//! History Log: every statement executed by the application, newest first.

use egui::{Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use super::Tab;
use crate::state::AppState;
use crate::ui::style::icon;

pub struct HistoryTab {
    filter: String,
    errors_only: bool,
    /// Selected entry, identified by (time, sql) so it survives new entries being appended.
    selected: Option<(chrono::DateTime<chrono::Local>, String)>,
}

impl Default for HistoryTab {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryTab {
    pub fn new() -> Self {
        Self { filter: String::new(), errors_only: false, selected: None }
    }

    /// Indices into `st.history`, newest first, after filtering.
    fn visible(&self, st: &AppState) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..st.history.len())
            .rev()
            .filter(|&i| {
                let e = &st.history[i];
                if self.errors_only && e.ok {
                    return false;
                }
                f.is_empty()
                    || e.sql.to_lowercase().contains(&f)
                    || e.connection.to_lowercase().contains(&f)
                    || e.database.to_lowercase().contains(&f)
                    || e.message.to_lowercase().contains(&f)
            })
            .collect()
    }

    fn selected_index(&self, st: &AppState) -> Option<usize> {
        let (t, sql) = self.selected.as_ref()?;
        st.history.iter().rposition(|e| &e.time == t && &e.sql == sql)
    }
}

fn first_line(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or("").trim();
    let n = line.chars().count();
    let mut out: String = line.chars().take(160).collect();
    if n > 160 || s.trim().lines().count() > 1 {
        out.push_str(" …");
    }
    out
}

impl Tab for HistoryTab {
    fn title(&self) -> String {
        format!("{} History Log", icon::HISTORY)
    }

    fn key(&self) -> String {
        "history".into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let sel = self.selected_index(st);
        // toolbar
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search SQL, connection, database, message").desired_width(280.0).min_size(egui::vec2(280.0, 0.0)));
            if !self.filter.is_empty() && ui.small_button("✖").on_hover_text("Clear search").clicked() {
                self.filter.clear();
            }
            ui.checkbox(&mut self.errors_only, "Errors only");
            ui.separator();
            let has_sel = sel.is_some();
            if ui.add_enabled(has_sel, egui::Button::new("📋 Copy SQL")).clicked() {
                if let Some(i) = sel {
                    ui.ctx().copy_text(st.history[i].sql.clone());
                    st.toast("SQL copied");
                }
            }
            if ui.add_enabled(has_sel, egui::Button::new(format!("{} Open in New Query", icon::NEW_QUERY))).clicked() {
                if let Some(i) = sel {
                    let sql = st.history[i].sql.clone();
                    crate::commands::new_query_with(st, None, None, sql);
                }
            }
            ui.separator();
            if ui.add_enabled(!st.history.is_empty(), egui::Button::new(format!("{} Clear History", icon::DELETE))).clicked() {
                st.open_dialog(crate::dialogs::confirm::ConfirmDialog::new(
                    "Clear History",
                    "Delete all entries of the history log? This cannot be undone.",
                    Box::new(|st: &mut AppState| {
                        st.history.clear();
                        if let Err(e) = std::fs::write(navdb_core::config::history_path(), "") {
                            st.error(format!("Could not truncate history file: {}", e));
                        } else {
                            st.toast("History cleared");
                        }
                    }),
                ));
            }
        });
        ui.separator();

        let rows = self.visible(st);
        let total = st.history.len();
        let errors = st.history.iter().filter(|e| !e.ok).count();

        // details panel
        egui::TopBottomPanel::bottom("history-details")
            .resizable(true)
            .default_height(190.0)
            .height_range(80.0..=600.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                match sel {
                    None => {
                        ui.label(RichText::new("Select an entry to see the full statement.").weak());
                    }
                    Some(i) => {
                        let e = &st.history[i];
                        ui.horizontal_wrapped(|ui| {
                            if e.ok {
                                ui.colored_label(Color32::from_rgb(40, 160, 70), "✔ Success");
                            } else {
                                ui.colored_label(ui.visuals().error_fg_color, "✖ Error");
                            }
                            ui.separator();
                            ui.label(e.time.format("%Y-%m-%d %H:%M:%S").to_string());
                            ui.separator();
                            ui.label(format!("{} / {}", e.connection, if e.database.is_empty() { "-" } else { &e.database }));
                            ui.separator();
                            ui.label(format!("{} ms", e.elapsed_ms));
                        });
                        if !e.message.is_empty() {
                            let c = if e.ok { ui.visuals().weak_text_color() } else { ui.visuals().error_fg_color };
                            ui.add(egui::Label::new(RichText::new(&e.message).color(c)).wrap());
                        }
                        ui.separator();
                        let mut sql = e.sql.clone();
                        egui::ScrollArea::vertical().id_salt("history-sql").auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut sql)
                                    .code_editor()
                                    .desired_width(f32::INFINITY)
                                    .interactive(true),
                            );
                        });
                    }
                }
            });

        egui::TopBottomPanel::bottom("history-count").show_separator_line(false).show_inside(ui, |ui| {
            ui.label(RichText::new(format!("{} of {} entries  ·  {} error(s)", rows.len(), total, errors)).weak().small());
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if rows.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(if total == 0 { "No statements have been executed yet." } else { "No entries match the filter." }).weak());
                });
                return;
            }
            let row_h = 22.0;
            let mut clicked: Option<usize> = None;
            let mut dbl: Option<usize> = None;
            TableBuilder::new(ui)
                .id_salt("history-table")
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(140.0).at_least(80.0))
                .column(Column::initial(130.0).at_least(60.0).clip(true))
                .column(Column::initial(110.0).at_least(50.0).clip(true))
                .column(Column::exact(50.0))
                .column(Column::initial(70.0).at_least(50.0))
                .column(Column::remainder().at_least(150.0).clip(true))
                .auto_shrink([false, false])
                .header(22.0, |mut h| {
                    for t in ["Time", "Connection", "Database", "Status", "Duration", "SQL"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_h, rows.len(), |mut row| {
                        let i = rows[row.index()];
                        let e = &st.history[i];
                        row.set_selected(sel == Some(i));
                        row.col(|ui| {
                            ui.label(e.time.format("%Y-%m-%d %H:%M:%S").to_string());
                        });
                        row.col(|ui| {
                            ui.label(&e.connection);
                        });
                        row.col(|ui| {
                            ui.label(&e.database);
                        });
                        row.col(|ui| {
                            if e.ok {
                                ui.colored_label(Color32::from_rgb(40, 160, 70), "✔");
                            } else {
                                ui.colored_label(ui.visuals().error_fg_color, "✖").on_hover_text(&e.message);
                            }
                        });
                        row.col(|ui| {
                            ui.label(format!("{} ms", e.elapsed_ms));
                        });
                        row.col(|ui| {
                            ui.add(egui::Label::new(RichText::new(first_line(&e.sql)).monospace()).truncate());
                        });
                        let r = row.response();
                        if r.clicked() {
                            clicked = Some(i);
                        }
                        if r.double_clicked() {
                            dbl = Some(i);
                        }
                    });
                });
            if let Some(i) = clicked {
                let e = &st.history[i];
                self.selected = Some((e.time, e.sql.clone()));
            }
            if let Some(i) = dbl {
                let sql = st.history[i].sql.clone();
                crate::commands::new_query_with(st, None, None, sql);
            }
        });
    }
}
