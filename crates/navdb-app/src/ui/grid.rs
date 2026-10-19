//! Data grid used by the table viewer, query results and tool windows.

use std::collections::BTreeSet;

use egui::{Color32, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use navdb_core::{ColumnMeta, QueryResult, Value};

use super::style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    Normal,
    Modified,
    New,
    Deleted,
}

/// Anything the grid can display.
pub trait GridSource {
    fn columns(&self) -> &[ColumnMeta];
    fn row_count(&self) -> usize;
    fn cell(&self, row: usize, col: usize) -> &Value;
    fn row_status(&self, _row: usize) -> RowStatus {
        RowStatus::Normal
    }
    fn cell_modified(&self, _row: usize, _col: usize) -> bool {
        false
    }
}

static NULL: Value = Value::Null;

impl GridSource for QueryResult {
    fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
    fn row_count(&self) -> usize {
        self.rows.len()
    }
    fn cell(&self, row: usize, col: usize) -> &Value {
        self.rows.get(row).and_then(|r| r.get(col)).unwrap_or(&NULL)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GridEvent {
    Edited { row: usize, col: usize, value: Value },
    /// Header clicked: request (server- or client-side) sort.
    Sort { col: usize, descending: bool },
    DeleteRows(Vec<usize>),
    FilterByValue { col: usize, value: Value },
    CopyAsInsert(Vec<usize>),
    CopyAsUpdate(Vec<usize>),
    OpenCellEditor { row: usize, col: usize },
    AddRow,
    Selected { row: usize, col: usize },
}

#[derive(Clone, Debug, Default)]
pub struct GridState {
    pub cursor: Option<(usize, usize)>,
    pub selected_rows: BTreeSet<usize>,
    pub editing: Option<(usize, usize)>,
    pub edit_buf: String,
    pub edit_requested_focus: bool,
    /// Client-side sort: (column, descending).
    pub sort: Option<(usize, bool)>,
    /// Display order of rows (client-side sort); empty = natural order.
    pub order: Vec<usize>,
    pub scroll_to_row: Option<usize>,
}

impl GridState {
    pub fn reset(&mut self) {
        *self = GridState::default();
    }

    pub fn selected_or_cursor(&self) -> Vec<usize> {
        if !self.selected_rows.is_empty() {
            self.selected_rows.iter().copied().collect()
        } else {
            self.cursor.map(|(r, _)| vec![r]).unwrap_or_default()
        }
    }

    /// Recompute `order` for client-side sorting.
    pub fn apply_client_sort(&mut self, src: &dyn GridSource) {
        match self.sort {
            None => self.order.clear(),
            Some((col, desc)) => {
                let mut idx: Vec<usize> = (0..src.row_count()).collect();
                idx.sort_by(|&a, &b| {
                    let o = src.cell(a, col).cmp_loose(src.cell(b, col));
                    if desc {
                        o.reverse()
                    } else {
                        o
                    }
                });
                self.order = idx;
            }
        }
    }
}

pub struct GridOptions<'a> {
    pub editable: bool,
    /// When false, sorting is done client side via `GridState::order`.
    pub server_sort: bool,
    /// Current server-side sort (for header arrows).
    pub server_sort_state: Option<(usize, bool)>,
    pub null_text: &'a str,
    pub row_number_offset: usize,
    pub font_size: f32,
    pub max_cell_chars: usize,
}

impl Default for GridOptions<'_> {
    fn default() -> Self {
        Self {
            editable: false,
            server_sort: false,
            server_sort_state: None,
            null_text: "(Null)",
            row_number_offset: 0,
            font_size: 12.5,
            max_cell_chars: 200,
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' || c == '\r' { '¶' } else { c }).collect();
    if one_line.chars().count() > n {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    } else {
        one_line
    }
}

pub fn row_as_tsv(src: &dyn GridSource, row: usize) -> String {
    (0..src.columns().len()).map(|c| src.cell(row, c).edit_text()).collect::<Vec<_>>().join("\t")
}

/// Render the grid. Returns user events for the caller to apply.
pub fn data_grid(ui: &mut Ui, id: &str, src: &dyn GridSource, st: &mut GridState, opt: &GridOptions) -> Vec<GridEvent> {
    let mut events = Vec::new();
    let ncols = src.columns().len();
    let nrows = src.row_count();
    if st.order.len() != nrows && st.sort.is_some() && !opt.server_sort {
        st.apply_client_sort(src);
    }
    if ncols == 0 {
        ui.label(RichText::new("No columns").weak());
        return events;
    }

    let font = egui::FontId::monospace(opt.font_size);
    let row_h = opt.font_size + 9.0;
    // Tab moves editing to the next cell, which is drawn later in this same frame and must
    // not see the same key press again
    let editing_at_start = st.editing;

    // keyboard navigation
    let grid_has_focus = ui.memory(|m| m.focused().is_none());
    if st.editing.is_none() && grid_has_focus && ui.ui_contains_pointer() {
        if let Some((r, c)) = st.cursor {
            let mut nr = r as i64;
            let mut nc = c as i64;
            ui.input(|i| {
                if i.key_pressed(egui::Key::ArrowDown) {
                    nr += 1;
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    nr -= 1;
                }
                if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Tab) {
                    nc += 1;
                }
                if i.key_pressed(egui::Key::ArrowLeft) {
                    nc -= 1;
                }
            });
            let nr = nr.clamp(0, nrows.saturating_sub(1) as i64) as usize;
            let nc = nc.clamp(0, ncols.saturating_sub(1) as i64) as usize;
            if (nr, nc) != (r, c) {
                st.cursor = Some((nr, nc));
                st.selected_rows.clear();
                st.scroll_to_row = Some(nr);
            }
            // typing on a selected cell starts editing it with that text
            let typed: String = ui.input(|i| {
                if i.modifiers.command || i.modifiers.ctrl {
                    return String::new();
                }
                i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.as_str()) } else { None }).collect()
            });
            if !typed.is_empty() && opt.editable && nrows > 0 {
                let (r, c) = st.cursor.unwrap_or((0, 0));
                if src.row_status(r) != RowStatus::Deleted {
                    st.editing = Some((r, c));
                    st.edit_buf = typed;
                    st.edit_requested_focus = true;
                }
            }
            let (enter, copy, del) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::F2),
                    i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                    i.key_pressed(egui::Key::Delete) || (i.modifiers.command && i.key_pressed(egui::Key::Backspace)),
                )
            });
            if enter && opt.editable && nrows > 0 {
                let (r, c) = st.cursor.unwrap_or((0, 0));
                if src.row_status(r) != RowStatus::Deleted {
                    st.editing = Some((r, c));
                    st.edit_buf = src.cell(r, c).edit_text();
                    st.edit_requested_focus = true;
                }
            }
            if copy {
                let rows = st.selected_or_cursor();
                let text = if rows.len() > 1 {
                    rows.iter().map(|&r| row_as_tsv(src, r)).collect::<Vec<_>>().join("\n")
                } else if let Some((r, c)) = st.cursor {
                    src.cell(r, c).edit_text()
                } else {
                    String::new()
                };
                ui.ctx().copy_text(text);
            }
            if del && opt.editable {
                events.push(GridEvent::DeleteRows(st.selected_or_cursor()));
            }
        }
    }

    let mut table = TableBuilder::new(ui)
        .id_salt(id)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(44.0))
        .columns(Column::initial(130.0).at_least(40.0).clip(true).resizable(true), ncols)
        .min_scrolled_height(0.0)
        .auto_shrink([false, false]);
    if let Some(r) = st.scroll_to_row.take() {
        let pos = st.order.iter().position(|&x| x == r).unwrap_or(r);
        table = table.scroll_to_row(pos, None);
    }

    let sort_state = if opt.server_sort { opt.server_sort_state } else { st.sort };
    let mut header_clicked: Option<usize> = None;
    table
        .header(row_h + 2.0, |mut header| {
            header.col(|ui| {
                ui.label(RichText::new("#").weak());
            });
            for (ci, c) in src.columns().iter().enumerate() {
                header.col(|ui| {
                    let arrow = match sort_state {
                        Some((sc, desc)) if sc == ci => {
                            if desc {
                                " ⬇"
                            } else {
                                " ⬆"
                            }
                        }
                        _ => "",
                    };
                    let resp = ui.add(egui::Label::new(RichText::new(format!("{}{}", c.name, arrow)).strong()).sense(Sense::click()));
                    let resp = if c.type_name.is_empty() { resp } else { resp.on_hover_text(&c.type_name) };
                    if resp.clicked() {
                        header_clicked = Some(ci);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(row_h, nrows, |mut row| {
                let disp = row.index();
                let r = if st.order.len() == nrows { st.order[disp] } else { disp };
                let status = src.row_status(r);
                let selected = st.selected_rows.contains(&r) || st.cursor.map(|(cr, _)| cr == r).unwrap_or(false);
                row.set_selected(selected);
                let (_, num_resp) = row.col(|ui| {
                    let mark = match status {
                        RowStatus::New => "＊",
                        RowStatus::Modified => "✏",
                        RowStatus::Deleted => "✖",
                        RowStatus::Normal => "",
                    };
                    ui.label(RichText::new(format!("{}{}", mark, r + 1 + opt.row_number_offset)).weak().font(egui::FontId::proportional(opt.font_size - 1.5)));
                });
                if num_resp.clicked() {
                    let shift = num_resp.ctx.input(|i| i.modifiers.shift);
                    let cmd = num_resp.ctx.input(|i| i.modifiers.command);
                    select_row(st, r, shift, cmd);
                }
                for c in 0..ncols {
                    let (rect, resp) = row.col(|ui| {
                        let bg = match status {
                            RowStatus::New => Some(style::NEW_BG),
                            RowStatus::Deleted => Some(style::DELETED_BG),
                            _ if src.cell_modified(r, c) => Some(style::MODIFIED_BG),
                            _ => None,
                        };
                        if let Some(bg) = bg {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
                        }
                        if st.editing == Some((r, c)) {
                            let te = egui::TextEdit::singleline(&mut st.edit_buf).font(font.clone()).desired_width(f32::INFINITY);
                            let out = ui.add(te);
                            if st.edit_requested_focus {
                                out.request_focus();
                                st.edit_requested_focus = false;
                            }
                            let (enter, esc, tab) = if editing_at_start == Some((r, c)) {
                                ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Tab)))
                            } else {
                                (false, false, false)
                            };
                            if esc {
                                st.editing = None;
                            } else if enter || tab || out.lost_focus() {
                                let original = src.cell(r, c);
                                let new_val = if st.edit_buf.is_empty() && original.is_null() {
                                    Value::Null
                                } else {
                                    Value::Text(st.edit_buf.clone())
                                };
                                if new_val.edit_text() != original.edit_text() || (original.is_null() != new_val.is_null()) {
                                    events.push(GridEvent::Edited { row: r, col: c, value: new_val });
                                }
                                st.editing = None;
                                if tab && c + 1 < ncols {
                                    st.cursor = Some((r, c + 1));
                                    st.editing = Some((r, c + 1));
                                    st.edit_buf = src.cell(r, c + 1).edit_text();
                                    st.edit_requested_focus = true;
                                }
                            }
                        } else {
                            let v = src.cell(r, c);
                            let is_cursor = st.cursor == Some((r, c));
                            let text = if v.is_null() {
                                RichText::new(opt.null_text).italics().color(style::NULL_COLOR)
                            } else if let Value::Bytes(b) = v {
                                if std::str::from_utf8(b).is_err() {
                                    RichText::new(format!("(BLOB) {}", super::format_bytes(b.len() as i64))).color(style::NULL_COLOR)
                                } else {
                                    RichText::new(truncate(&v.display(), opt.max_cell_chars))
                                }
                            } else {
                                RichText::new(truncate(&v.display(), opt.max_cell_chars))
                            };
                            let text = text.font(font.clone());
                            let text = if is_cursor { text.strong() } else { text };
                            if v.is_numeric() {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.add(egui::Label::new(text).selectable(false).truncate());
                                });
                            } else {
                                ui.add(egui::Label::new(text).selectable(false).truncate());
                            }
                            if is_cursor {
                                ui.painter().rect_stroke(
                                    ui.max_rect(),
                                    0.0,
                                    egui::Stroke::new(1.5_f32, ui.visuals().selection.stroke.color),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    });
                    let _ = rect;
                    if resp.clicked() {
                        let (shift, cmd) = resp.ctx.input(|i| (i.modifiers.shift, i.modifiers.command));
                        if shift || cmd {
                            select_row(st, r, shift, cmd);
                        } else {
                            st.selected_rows.clear();
                        }
                        st.cursor = Some((r, c));
                        events.push(GridEvent::Selected { row: r, col: c });
                    }
                    if resp.double_clicked() && opt.editable && status != RowStatus::Deleted {
                        st.cursor = Some((r, c));
                        st.editing = Some((r, c));
                        st.edit_buf = src.cell(r, c).edit_text();
                        st.edit_requested_focus = true;
                    }
                    resp.context_menu(|ui| {
                        if st.cursor.map(|(cr, _)| cr) != Some(r) && !st.selected_rows.contains(&r) {
                            st.cursor = Some((r, c));
                            st.selected_rows.clear();
                        }
                        cell_menu(ui, src, st, r, c, opt, &mut events);
                    });
                }
            });
        });

    if let Some(ci) = header_clicked {
        let desc = match sort_state {
            Some((sc, d)) if sc == ci => !d,
            _ => false,
        };
        if opt.server_sort {
            events.push(GridEvent::Sort { col: ci, descending: desc });
        } else {
            // third click clears sorting
            if matches!(st.sort, Some((sc, true)) if sc == ci) {
                st.sort = None;
                st.order.clear();
            } else {
                st.sort = Some((ci, desc));
                st.apply_client_sort(src);
            }
            events.push(GridEvent::Sort { col: ci, descending: desc });
        }
    }
    events
}

fn select_row(st: &mut GridState, r: usize, shift: bool, cmd: bool) {
    if shift {
        let anchor = st.cursor.map(|(cr, _)| cr).unwrap_or(r);
        let (a, b) = if anchor <= r { (anchor, r) } else { (r, anchor) };
        st.selected_rows = (a..=b).collect();
    } else if cmd {
        if !st.selected_rows.insert(r) {
            st.selected_rows.remove(&r);
        }
    } else {
        st.selected_rows.clear();
        st.selected_rows.insert(r);
    }
    st.cursor = Some((r, st.cursor.map(|(_, c)| c).unwrap_or(0)));
}

fn cell_menu(ui: &mut Ui, src: &dyn GridSource, st: &mut GridState, r: usize, c: usize, opt: &GridOptions, events: &mut Vec<GridEvent>) {
    let v = src.cell(r, c).clone();
    if opt.editable {
        if ui.button("Edit Cell").clicked() {
            st.editing = Some((r, c));
            st.edit_buf = v.edit_text();
            st.edit_requested_focus = true;
            ui.close_menu();
        }
        if ui.button("Open in Editor…").clicked() {
            events.push(GridEvent::OpenCellEditor { row: r, col: c });
            ui.close_menu();
        }
        if ui.button("Set to NULL").clicked() {
            events.push(GridEvent::Edited { row: r, col: c, value: Value::Null });
            ui.close_menu();
        }
        if ui.button("Set to Empty String").clicked() {
            events.push(GridEvent::Edited { row: r, col: c, value: Value::Text(String::new()) });
            ui.close_menu();
        }
        ui.separator();
    } else if ui.button("View Cell…").clicked() {
        events.push(GridEvent::OpenCellEditor { row: r, col: c });
        ui.close_menu();
    }
    if ui.button("Copy").clicked() {
        ui.ctx().copy_text(v.edit_text());
        ui.close_menu();
    }
    if ui.button("Copy Field Name").clicked() {
        ui.ctx().copy_text(src.columns()[c].name.clone());
        ui.close_menu();
    }
    let rows = st.selected_or_cursor();
    if ui.button(format!("Copy {} Row(s) (Tab separated)", rows.len())).clicked() {
        let header = src.columns().iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t");
        let body = rows.iter().map(|&r| row_as_tsv(src, r)).collect::<Vec<_>>().join("\n");
        ui.ctx().copy_text(format!("{}\n{}", header, body));
        ui.close_menu();
    }
    if ui.button("Copy as INSERT Statement").clicked() {
        events.push(GridEvent::CopyAsInsert(rows.clone()));
        ui.close_menu();
    }
    if ui.button("Copy as UPDATE Statement").clicked() {
        events.push(GridEvent::CopyAsUpdate(rows.clone()));
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Filter by This Value").clicked() {
        events.push(GridEvent::FilterByValue { col: c, value: v.clone() });
        ui.close_menu();
    }
    if ui.button("Sort Ascending").clicked() {
        if opt.server_sort {
            events.push(GridEvent::Sort { col: c, descending: false });
        } else {
            st.sort = Some((c, false));
            st.apply_client_sort(src);
        }
        ui.close_menu();
    }
    if ui.button("Sort Descending").clicked() {
        if opt.server_sort {
            events.push(GridEvent::Sort { col: c, descending: true });
        } else {
            st.sort = Some((c, true));
            st.apply_client_sort(src);
        }
        ui.close_menu();
    }
    if opt.editable {
        ui.separator();
        if ui.button("Add Record").clicked() {
            events.push(GridEvent::AddRow);
            ui.close_menu();
        }
        if ui.button(RichText::new(format!("Delete {} Record(s)", rows.len())).color(Color32::from_rgb(200, 60, 60))).clicked() {
            events.push(GridEvent::DeleteRows(rows));
            ui.close_menu();
        }
    }
}

/// Form View: one record shown vertically.
pub fn form_view(ui: &mut Ui, src: &dyn GridSource, row: &mut usize, editable: bool, buffers: &mut Vec<String>) -> Vec<GridEvent> {
    let mut events = Vec::new();
    let n = src.row_count();
    ui.horizontal(|ui| {
        if ui.button("⏮").clicked() {
            *row = 0;
            buffers.clear();
        }
        if ui.button("◀").clicked() && *row > 0 {
            *row -= 1;
            buffers.clear();
        }
        ui.label(format!("Record {} of {}", if n == 0 { 0 } else { *row + 1 }, n));
        if ui.button("▶").clicked() && *row + 1 < n {
            *row += 1;
            buffers.clear();
        }
        if ui.button("⏭").clicked() && n > 0 {
            *row = n - 1;
            buffers.clear();
        }
        if editable && ui.button("➕ Add").clicked() {
            events.push(GridEvent::AddRow);
        }
    });
    ui.separator();
    if n == 0 {
        ui.label(RichText::new("No records").weak());
        return events;
    }
    if *row >= n {
        *row = n - 1;
    }
    let cols = src.columns();
    if buffers.len() != cols.len() {
        *buffers = (0..cols.len()).map(|c| src.cell(*row, c).edit_text()).collect();
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("form-view").num_columns(2).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
            for (c, meta) in cols.iter().enumerate() {
                ui.label(RichText::new(&meta.name).strong()).on_hover_text(&meta.type_name);
                let v = src.cell(*row, c);
                if editable {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut buffers[c])
                            .desired_width(420.0)
                            .hint_text(if v.is_null() { "(Null)" } else { "" }),
                    );
                    if resp.lost_focus() && buffers[c] != v.edit_text() {
                        events.push(GridEvent::Edited { row: *row, col: c, value: Value::Text(buffers[c].clone()) });
                    }
                } else if v.is_null() {
                    ui.label(RichText::new("(Null)").italics().color(style::NULL_COLOR));
                } else {
                    ui.add(egui::Label::new(v.display()).wrap());
                }
                ui.end_row();
            }
        });
    });
    events
}
