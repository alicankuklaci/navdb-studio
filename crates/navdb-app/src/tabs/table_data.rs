//! Table / View viewer: paged grid with editing, filter & sort, form view.

use std::collections::{HashMap, HashSet};

use egui::{RichText, Ui};
use navdb_core::dml::{self, FilterOp, FilterRule, SortSpec};
use navdb_core::{ColumnMeta, Dialect, ObjPath, ObjectKind, Pending, QueryResult, Session, TableDef, Value};

use super::Tab;
use crate::commands as cmd;
use crate::state::{AppState, Loadable};
use crate::ui::grid::{self, GridEvent, GridOptions, GridSource, GridState, RowStatus};
use crate::ui::{self, style::icon};

/// Working copy of a page with pending edits.
#[derive(Default)]
pub struct EditBuffer {
    pub base: QueryResult,
    pub edits: HashMap<(usize, usize), Value>,
    pub new_rows: Vec<Vec<Value>>,
    pub deleted: HashSet<usize>,
}

static NULL: Value = Value::Null;

impl EditBuffer {
    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty() || !self.new_rows.is_empty() || !self.deleted.is_empty()
    }

    pub fn clear(&mut self) {
        self.edits.clear();
        self.new_rows.clear();
        self.deleted.clear();
    }

    pub fn set(&mut self, row: usize, col: usize, v: Value) {
        let base_len = self.base.rows.len();
        if row >= base_len {
            if let Some(r) = self.new_rows.get_mut(row - base_len) {
                r[col] = v;
            }
        } else if self.base.rows[row][col] == v {
            self.edits.remove(&(row, col));
        } else {
            self.edits.insert((row, col), v);
        }
    }

    pub fn row_values(&self, row: usize) -> Vec<Value> {
        (0..self.base.columns.len()).map(|c| self.cell(row, c).clone()).collect()
    }
}

impl GridSource for EditBuffer {
    fn columns(&self) -> &[ColumnMeta] {
        &self.base.columns
    }
    fn row_count(&self) -> usize {
        self.base.rows.len() + self.new_rows.len()
    }
    fn cell(&self, row: usize, col: usize) -> &Value {
        let base_len = self.base.rows.len();
        if row >= base_len {
            return self.new_rows.get(row - base_len).and_then(|r| r.get(col)).unwrap_or(&NULL);
        }
        self.edits.get(&(row, col)).unwrap_or_else(|| self.base.rows.get(row).and_then(|r| r.get(col)).unwrap_or(&NULL))
    }
    fn row_status(&self, row: usize) -> RowStatus {
        if row >= self.base.rows.len() {
            RowStatus::New
        } else if self.deleted.contains(&row) {
            RowStatus::Deleted
        } else if self.edits.keys().any(|(r, _)| *r == row) {
            RowStatus::Modified
        } else {
            RowStatus::Normal
        }
    }
    fn cell_modified(&self, row: usize, col: usize) -> bool {
        self.edits.contains_key(&(row, col))
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ViewMode {
    Grid,
    Form,
}

pub struct TableDataTab {
    conn: String,
    path: ObjPath,
    kind: ObjectKind,
    dialect: Dialect,
    session: Option<Session>,
    read_only: bool,
    def: Loadable<TableDef>,
    page: Loadable<QueryResult>,
    buf: EditBuffer,
    total: Option<Pending<i64>>,
    total_rows: Option<i64>,
    page_index: usize,
    page_size: usize,
    grid: GridState,
    sort: Vec<SortSpec>,
    filters: Vec<FilterRule>,
    raw_where: String,
    use_raw_where: bool,
    show_filter: bool,
    applied_where: String,
    mode: ViewMode,
    form_row: usize,
    form_bufs: Vec<String>,
    saving: Option<Pending<Vec<QueryResult>>>,
    last_sql: String,
    message: Option<(String, bool)>,
    cell_editor: Option<(usize, usize, String)>,
    confirm_discard_close: bool,
    pending_copy: Option<String>,
}

impl TableDataTab {
    pub fn new(st: &mut AppState, conn: &str, path: ObjPath, kind: ObjectKind) -> Self {
        let cfg = st.config(conn).cloned();
        let dialect = cfg.as_ref().map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let page_size = st.prefs().records_per_page.max(10);
        let mut t = Self {
            conn: conn.to_string(),
            path,
            kind,
            dialect,
            session: st.session(conn),
            read_only: cfg.map(|c| c.read_only).unwrap_or(false) || kind != ObjectKind::Table,
            def: Loadable::NotLoaded,
            page: Loadable::NotLoaded,
            buf: EditBuffer::default(),
            total: None,
            total_rows: None,
            page_index: 0,
            page_size,
            grid: GridState::default(),
            sort: Vec::new(),
            filters: Vec::new(),
            raw_where: String::new(),
            use_raw_where: false,
            show_filter: false,
            applied_where: String::new(),
            mode: ViewMode::Grid,
            form_row: 0,
            form_bufs: Vec::new(),
            saving: None,
            last_sql: String::new(),
            message: None,
            cell_editor: None,
            confirm_discard_close: false,
            pending_copy: None,
        };
        t.load_def();
        t.reload(true);
        t
    }

    fn load_def(&mut self) {
        if self.kind != ObjectKind::Table {
            return;
        }
        if let Some(s) = &self.session {
            let p = self.path.clone();
            self.def = Loadable::Loading(s.run(move |d| d.table_def(&p)));
        }
    }

    fn reload(&mut self, count: bool) {
        let Some(s) = &self.session else { return };
        let sql = dml::select_page(self.dialect, &self.path, &self.applied_where, &self.sort, self.page_size, self.page_index * self.page_size);
        self.last_sql = sql.clone();
        let scope = self.path.scope();
        self.page = Loadable::Loading(s.run(move |d| d.query(Some(&scope), &sql)));
        if count {
            let csql = dml::count_rows(self.dialect, &self.path, &self.applied_where);
            let scope = self.path.scope();
            self.total = Some(s.run(move |d| {
                let r = d.query(Some(&scope), &csql)?;
                Ok(r.rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64)
            }));
        }
        self.grid.cursor = None;
        self.grid.selected_rows.clear();
        self.grid.editing = None;
    }

    fn key_columns(&self) -> Vec<usize> {
        let cols = &self.buf.base.columns;
        if let Some(def) = self.def.ready() {
            let pk = def.primary_key();
            if !pk.is_empty() {
                let idx: Vec<usize> = pk.iter().filter_map(|k| cols.iter().position(|c| &c.name == k)).collect();
                if idx.len() == pk.len() {
                    return idx;
                }
            }
        }
        (0..cols.len()).collect()
    }

    fn key_values(&self, row: usize, keys: &[usize]) -> Vec<(String, Value)> {
        keys.iter().map(|&k| (self.buf.base.columns[k].name.clone(), self.buf.base.rows[row][k].clone())).collect()
    }

    fn pending_statements(&self) -> Vec<String> {
        let d = self.dialect;
        let keys = self.key_columns();
        let cols: Vec<String> = self.buf.base.columns.iter().map(|c| c.name.clone()).collect();
        let mut out = Vec::new();
        let mut rows: Vec<usize> = self.buf.edits.keys().map(|(r, _)| *r).collect();
        rows.sort();
        rows.dedup();
        for r in rows {
            if self.buf.deleted.contains(&r) {
                continue;
            }
            let changes: Vec<(String, Value)> = self
                .buf
                .edits
                .iter()
                .filter(|((rr, _), _)| *rr == r)
                .map(|((_, c), v)| (cols[*c].clone(), v.clone()))
                .collect();
            out.push(dml::update_row(d, &self.path, &changes, &self.key_values(r, &keys)));
        }
        let mut del: Vec<usize> = self.buf.deleted.iter().copied().collect();
        del.sort();
        for r in del {
            out.push(dml::delete_row(d, &self.path, &self.key_values(r, &keys)));
        }
        for nr in &self.buf.new_rows {
            // skip untouched NULL columns so defaults / auto increment apply
            let pairs: Vec<(String, Value)> = cols.iter().cloned().zip(nr.iter().cloned()).filter(|(_, v)| !v.is_null()).collect();
            if pairs.is_empty() {
                out.push(match d {
                    Dialect::MySql => format!("INSERT INTO {} () VALUES ()", d.qualify(&self.path)),
                    _ => format!("INSERT INTO {} DEFAULT VALUES", d.qualify(&self.path)),
                });
            } else {
                let (c, v): (Vec<String>, Vec<Value>) = pairs.into_iter().unzip();
                out.push(dml::insert_row(d, &self.path, &c, &v));
            }
        }
        out
    }

    fn apply_changes(&mut self, st: &mut AppState) {
        let stmts = self.pending_statements();
        if stmts.is_empty() {
            return;
        }
        let Some(s) = &self.session else { return };
        let scope = self.path.scope();
        let sql = stmts.join(";\n");
        st.record(&self.conn, Some(&scope), &sql, true, "table edit", 0);
        let dialect = self.dialect;
        self.saving = Some(s.run(move |d| {
            let mut out = Vec::new();
            d.execute(Some(&scope), if dialect == Dialect::MySql { "START TRANSACTION" } else { "BEGIN" })?;
            for stmt in &stmts {
                match d.execute(Some(&scope), stmt) {
                    Ok(r) => out.extend(r),
                    Err(e) => {
                        let _ = d.execute(Some(&scope), "ROLLBACK");
                        return Err(e.context(format!("while executing: {}", stmt)));
                    }
                }
            }
            d.execute(Some(&scope), "COMMIT")?;
            Ok(out)
        }));
    }

    fn add_row(&mut self) {
        let n = self.buf.base.columns.len();
        let mut row = vec![Value::Null; n];
        // pre-fill defaults shown by the designer
        if let Some(def) = self.def.ready() {
            for (i, c) in self.buf.base.columns.iter().enumerate() {
                if let Some(cd) = def.column(&c.name) {
                    if !cd.nullable && cd.default.is_none() && !cd.auto_increment {
                        row[i] = Value::Text(String::new());
                    }
                }
            }
        }
        self.buf.new_rows.push(row);
        let r = self.buf.row_count() - 1;
        self.grid.cursor = Some((r, 0));
        self.grid.scroll_to_row = Some(r);
        self.form_row = r;
        self.form_bufs.clear();
    }

    fn handle_events(&mut self, st: &mut AppState, events: Vec<GridEvent>) {
        for e in events {
            match e {
                GridEvent::Edited { row, col, value } => {
                    if !self.read_only {
                        self.buf.set(row, col, value);
                    }
                }
                GridEvent::Sort { col, descending } => {
                    let name = self.buf.base.columns[col].name.clone();
                    let same = self.sort.first().map(|s| s.column == name && s.descending).unwrap_or(false);
                    self.sort = if same { vec![] } else { vec![SortSpec { column: name, descending }] };
                    self.reload(false);
                }
                GridEvent::DeleteRows(rows) => {
                    if self.read_only {
                        continue;
                    }
                    let base_len = self.buf.base.rows.len();
                    let mut new_remove: Vec<usize> = Vec::new();
                    for r in rows {
                        if r >= base_len {
                            new_remove.push(r - base_len);
                        } else if !self.buf.deleted.remove(&r) {
                            self.buf.deleted.insert(r);
                        }
                    }
                    new_remove.sort();
                    for i in new_remove.into_iter().rev() {
                        if i < self.buf.new_rows.len() {
                            self.buf.new_rows.remove(i);
                        }
                    }
                }
                GridEvent::FilterByValue { col, value } => {
                    let name = self.buf.base.columns[col].name.clone();
                    let mut rule = FilterRule::new(name);
                    if value.is_null() {
                        rule.op = FilterOp::IsNull;
                    } else {
                        rule.value = value.edit_text();
                    }
                    self.filters = vec![rule];
                    self.use_raw_where = false;
                    self.show_filter = true;
                    self.apply_filter();
                }
                GridEvent::CopyAsInsert(rows) => {
                    let cols: Vec<String> = self.buf.base.columns.iter().map(|c| c.name.clone()).collect();
                    let sql = rows
                        .iter()
                        .map(|&r| format!("{};", dml::insert_row(self.dialect, &self.path, &cols, &self.buf.row_values(r))))
                        .collect::<Vec<_>>()
                        .join("\n");
                    st.toast(format!("Copied {} INSERT statement(s)", rows.len()));
                    self.copy(sql);
                }
                GridEvent::CopyAsUpdate(rows) => {
                    let keys = self.key_columns();
                    let cols: Vec<String> = self.buf.base.columns.iter().map(|c| c.name.clone()).collect();
                    let sql = rows
                        .iter()
                        .filter(|&&r| r < self.buf.base.rows.len())
                        .map(|&r| {
                            let changes: Vec<(String, Value)> = cols.iter().cloned().zip(self.buf.row_values(r)).collect();
                            format!("{};", dml::update_row(self.dialect, &self.path, &changes, &self.key_values(r, &keys)))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.copy(sql);
                    st.toast("Copied UPDATE statement(s)");
                }
                GridEvent::OpenCellEditor { row, col } => {
                    self.cell_editor = Some((row, col, self.buf.cell(row, col).edit_text()));
                }
                GridEvent::AddRow => {
                    if !self.read_only {
                        self.add_row();
                    }
                }
                GridEvent::Selected { .. } => {}
            }
        }
    }

    fn copy(&mut self, text: String) {
        self.pending_copy = Some(text);
    }

    fn apply_filter(&mut self) {
        self.applied_where = if self.use_raw_where { self.raw_where.clone() } else { dml::filters_to_where(self.dialect, &self.filters) };
        self.page_index = 0;
        self.reload(true);
    }

    fn filter_panel(&mut self, ui: &mut Ui) {
        let cols: Vec<String> = self.buf.base.columns.iter().map(|c| c.name.clone()).collect();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.use_raw_where, false, "Filter Builder");
            ui.selectable_value(&mut self.use_raw_where, true, "WHERE clause");
        });
        if self.use_raw_where {
            ui.horizontal(|ui| {
                ui.label("WHERE");
                let r = ui.add(egui::TextEdit::singleline(&mut self.raw_where).desired_width(ui.available_width() - 160.0).font(egui::TextStyle::Monospace));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.apply_filter();
                }
                if ui.button("Apply").clicked() {
                    self.apply_filter();
                }
                if ui.button("Clear").clicked() {
                    self.raw_where.clear();
                    self.apply_filter();
                }
            });
            return;
        }
        let mut remove = None;
        for (i, f) in self.filters.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut f.enabled, "");
                if i > 0 {
                    egui::ComboBox::from_id_salt(("andor", i)).width(60.0).selected_text(if f.and { "AND" } else { "OR" }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.and, true, "AND");
                        ui.selectable_value(&mut f.and, false, "OR");
                    });
                }
                egui::ComboBox::from_id_salt(("fcol", i)).width(140.0).selected_text(&f.column).show_ui(ui, |ui| {
                    for c in &cols {
                        ui.selectable_value(&mut f.column, c.clone(), c);
                    }
                });
                egui::ComboBox::from_id_salt(("fop", i)).width(130.0).selected_text(f.op.label()).show_ui(ui, |ui| {
                    for op in FilterOp::ALL {
                        ui.selectable_value(&mut f.op, op, op.label());
                    }
                });
                if f.op.needs_value() {
                    let hint = match f.op {
                        FilterOp::In => "a, b, c",
                        FilterOp::Between => "low, high",
                        _ => "value",
                    };
                    ui.add(egui::TextEdit::singleline(&mut f.value).hint_text(hint).desired_width(200.0));
                }
                if ui.small_button("✖").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.filters.remove(i);
        }
        ui.horizontal(|ui| {
            if ui.button("➕ Add Condition").clicked() {
                self.filters.push(FilterRule::new(cols.first().cloned().unwrap_or_default()));
            }
            if ui.button("Apply Filter").clicked() {
                self.apply_filter();
            }
            if ui.button("Clear").clicked() {
                self.filters.clear();
                self.apply_filter();
            }
            let preview = dml::filters_to_where(self.dialect, &self.filters);
            if !preview.is_empty() {
                ui.label(RichText::new(format!("WHERE {}", preview)).monospace().weak());
            }
        });
    }
}

impl Tab for TableDataTab {
    fn title(&self) -> String {
        format!("{} {} @{}", crate::ui::style::object_icon(self.kind), self.path.name, self.path.db)
    }

    fn key(&self) -> String {
        format!("data:{}:{}", self.conn, self.path.display())
    }

    fn dirty(&self) -> bool {
        self.buf.is_dirty()
    }

    fn can_close(&mut self, _st: &mut AppState) -> bool {
        if self.buf.is_dirty() && !self.confirm_discard_close {
            self.confirm_discard_close = true;
            self.message = Some(("Unsaved changes: Apply or Discard them, or close again to discard.".into(), true));
            return false;
        }
        true
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        if let Some(t) = self.pending_copy.take() {
            ui.ctx().copy_text(t);
        }
        // poll background work
        self.def.poll();
        if self.page.poll() {
            if let Some(r) = self.page.ready() {
                self.buf.base = r.clone();
                self.buf.clear();
                self.grid.order.clear();
                self.form_bufs.clear();
                self.message = None;
            }
        }
        if let Some(t) = &mut self.total {
            if let Some(r) = t.poll() {
                self.total_rows = r.ok();
                self.total = None;
            }
        }
        if let Some(p) = &mut self.saving {
            if let Some(r) = p.poll() {
                self.saving = None;
                match r {
                    Ok(res) => {
                        let n: u64 = res.iter().map(|r| r.affected_rows).sum();
                        self.message = Some((format!("Changes applied ({} row(s) affected)", n), false));
                        self.reload(true);
                    }
                    Err(e) => self.message = Some((format!("{:#}", e), true)),
                }
            }
        }

        // toolbar
        ui.horizontal(|ui| {
            let editable = !self.read_only;
            if ui.add_enabled(editable, egui::Button::new("➕")).on_hover_text("Add record").clicked() {
                self.add_row();
            }
            if ui.add_enabled(editable, egui::Button::new("−")).on_hover_text("Delete selected records").clicked() {
                let rows = self.grid.selected_or_cursor();
                self.handle_events(st, vec![GridEvent::DeleteRows(rows)]);
            }
            let dirty = self.buf.is_dirty();
            if ui.add_enabled(dirty && self.saving.is_none(), egui::Button::new("✔ Apply")).on_hover_text("Apply changes (⌘S)").clicked() {
                self.apply_changes(st);
            }
            if ui.add_enabled(dirty, egui::Button::new("✖ Discard")).clicked() {
                self.buf.clear();
                self.confirm_discard_close = false;
            }
            if ui.button(icon::REFRESH).on_hover_text("Refresh").clicked() {
                self.reload(true);
            }
            ui.separator();
            ui.toggle_value(&mut self.show_filter, format!("{} Filter", icon::FILTER));
            if !self.sort.is_empty() && ui.button(format!("{} Clear sort", icon::SORT)).clicked() {
                self.sort.clear();
                self.reload(false);
            }
            ui.separator();
            ui.selectable_value(&mut self.mode, ViewMode::Grid, "⊞ Grid");
            ui.selectable_value(&mut self.mode, ViewMode::Form, "☰ Form");
            ui.separator();
            if self.kind == ObjectKind::Table && ui.button(format!("{} Import", icon::IMPORT)).clicked() {
                cmd::import_wizard(st, &self.conn, &self.path.scope(), Some(self.path.name.clone()));
            }
            if ui.button(format!("{} Export", icon::EXPORT)).clicked() {
                cmd::export_wizard(st, &self.conn, &self.path.scope(), vec![self.path.name.clone()]);
            }
            if self.kind == ObjectKind::Table && ui.button(format!("{} Design", icon::DESIGN)).clicked() {
                cmd::design_object(st, &self.conn, &self.path.scope(), ObjectKind::Table, Some(self.path.name.clone()));
            }
            if ui.button(format!("{} Chart", icon::CHARTS)).clicked() {
                cmd::charts(st, Some((self.conn.clone(), self.path.scope(), self.last_sql.clone())));
            }
            if self.read_only {
                ui.label(RichText::new("read-only").weak().italics());
            }
        });
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) && self.buf.is_dirty() {
            self.apply_changes(st);
        }
        if self.show_filter {
            egui::Frame::group(ui.style()).show(ui, |ui| self.filter_panel(ui));
        }

        // bottom bar: paging + sql
        egui::TopBottomPanel::bottom(egui::Id::new(("tbl-bottom", self.key()))).show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let total = self.total_rows;
                let pages = total.map(|t| ((t as usize).max(1) + self.page_size - 1) / self.page_size).unwrap_or(1).max(1);
                if ui.button("⏮").clicked() && self.page_index > 0 {
                    self.page_index = 0;
                    self.reload(false);
                }
                if ui.button("◀").clicked() && self.page_index > 0 {
                    self.page_index -= 1;
                    self.reload(false);
                }
                ui.label(format!("Page {} / {}", self.page_index + 1, pages));
                if ui.button("▶").clicked() && self.page_index + 1 < pages {
                    self.page_index += 1;
                    self.reload(false);
                }
                if ui.button("⏭").clicked() && self.page_index + 1 < pages {
                    self.page_index = pages - 1;
                    self.reload(false);
                }
                ui.label("Limit:");
                let mut ps = self.page_size;
                if ui.add(egui::DragValue::new(&mut ps).range(10..=100_000).speed(10)).lost_focus() && ps != self.page_size {
                    self.page_size = ps;
                    self.page_index = 0;
                    self.reload(false);
                }
                ui.separator();
                let shown = self.buf.base.rows.len();
                ui.label(match total {
                    Some(t) => format!("{} records shown, {} total", shown, t),
                    None => format!("{} records shown", shown),
                });
                if let Some((r, c)) = self.grid.cursor {
                    if let Some(col) = self.buf.base.columns.get(c) {
                        ui.separator();
                        ui.label(RichText::new(format!("Row {} · {}", r + 1, col.name)).weak());
                    }
                }
            });
            ui.horizontal(|ui| {
                if let Some((m, err)) = &self.message {
                    if *err {
                        ui::error_text(ui, m);
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(60, 160, 80), m);
                    }
                } else {
                    ui.label(RichText::new(&self.last_sql).monospace().weak().small());
                }
            });
        });

        // cell editor (long text / JSON)
        if let Some((r, c, mut text)) = self.cell_editor.take() {
            let mut keep = true;
            egui::TopBottomPanel::bottom(egui::Id::new(("cell-editor", self.key()))).resizable(true).default_height(160.0).show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let colname = self.buf.base.columns.get(c).map(|c| c.name.clone()).unwrap_or_default();
                    ui.strong(format!("Editing {} (row {})", colname, r + 1));
                    if ui.button("Pretty JSON").clicked() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            text = serde_json::to_string_pretty(&v).unwrap_or(text.clone());
                        }
                    }
                    if !self.read_only && ui.button("✔ Set").clicked() {
                        self.buf.set(r, c, Value::Text(text.clone()));
                        keep = false;
                    }
                    if !self.read_only && ui.button("Set NULL").clicked() {
                        self.buf.set(r, c, Value::Null);
                        keep = false;
                    }
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY).desired_rows(6));
                });
            });
            if keep {
                self.cell_editor = Some((r, c, text));
            }
        }

        match &self.page {
            Loadable::Loading(_) if self.buf.base.columns.is_empty() => {
                ui::loading(ui, "Loading data...");
                return;
            }
            Loadable::Failed(e) => {
                ui::error_text(ui, &e.clone());
                return;
            }
            _ => {}
        }
        let null_text = st.prefs().show_null_as.clone();
        let opt = GridOptions {
            editable: !self.read_only,
            server_sort: true,
            server_sort_state: self.sort.first().and_then(|s| self.buf.base.column_index(&s.column).map(|i| (i, s.descending))),
            null_text: &null_text,
            row_number_offset: self.page_index * self.page_size,
            font_size: st.prefs().grid_font_size,
            ..Default::default()
        };
        let events = match self.mode {
            ViewMode::Grid => grid::data_grid(ui, &self.key(), &self.buf, &mut self.grid, &opt),
            ViewMode::Form => {
                if let Some((r, _)) = self.grid.cursor {
                    if self.form_bufs.is_empty() {
                        self.form_row = r;
                    }
                }
                let ev = grid::form_view(ui, &self.buf, &mut self.form_row, !self.read_only, &mut self.form_bufs);
                self.grid.cursor = Some((self.form_row, 0));
                ev
            }
        };
        self.handle_events(st, events);
    }
}
