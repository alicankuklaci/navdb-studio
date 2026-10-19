//! Preferences window: General / Editor / Grid / Query pages.

use egui::RichText;
use navdb_core::config::Preferences;

use super::Dialog;
use crate::state::AppState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Editor,
    Grid,
    Query,
}

impl Page {
    const ALL: [Page; 4] = [Page::General, Page::Editor, Page::Grid, Page::Query];

    fn label(self) -> &'static str {
        match self {
            Page::General => "⚙  General",
            Page::Editor => "📝  Editor",
            Page::Grid => "⊞  Grid",
            Page::Query => "▶  Query",
        }
    }
}

pub struct PreferencesDialog {
    id: String,
    draft: Preferences,
    original: Preferences,
    page: Page,
}

impl PreferencesDialog {
    pub fn new(st: &mut AppState) -> Self {
        let p = st.ws.preferences.clone();
        Self { id: uuid::Uuid::new_v4().to_string(), draft: p.clone(), original: p, page: Page::General }
    }

    fn apply(&mut self, st: &mut AppState) {
        let mut p = self.draft.clone();
        p.tab_width = p.tab_width.clamp(1, 16);
        p.records_per_page = p.records_per_page.max(1);
        p.query_result_limit = p.query_result_limit.max(1);
        p.max_history = p.max_history.max(10);
        if p.date_format.trim().is_empty() {
            p.date_format = Preferences::default().date_format;
        }
        self.draft = p.clone();
        self.original = p.clone();
        st.ws.preferences = p;
        st.save_workspace();
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong());
    ui.separator();
}

fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    add(ui);
    ui.end_row();
}

impl Dialog for PreferencesDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        super::window("Preferences", &self.id).open(&mut open).default_width(620.0).default_height(420.0).show(ctx, |ui| {
            egui::TopBottomPanel::bottom(format!("{}-buttons", self.id)).show_separator_line(true).show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Restore Defaults").on_hover_text("Reset every option on all pages").clicked() {
                        self.draft = Preferences::default();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let changed = self.draft != self.original;
                        if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                            self.apply(st);
                            st.toast("Preferences saved");
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                        if ui.button(RichText::new("   OK   ").strong()).clicked() {
                            if changed {
                                self.apply(st);
                            }
                            close = true;
                        }
                    });
                });
            });
            egui::SidePanel::left(format!("{}-pages", self.id)).resizable(false).exact_width(130.0).show_inside(ui, |ui| {
                ui.add_space(4.0);
                for p in Page::ALL {
                    let r = ui.add_sized([120.0, 26.0], egui::SelectableLabel::new(self.page == p, p.label()));
                    if r.clicked() {
                        self.page = p;
                    }
                }
            });
            egui::CentralPanel::default().show_inside(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let p = &mut self.draft;
                    match self.page {
                        Page::General => {
                            section(ui, "Appearance");
                            ui.checkbox(&mut p.dark_mode, "Dark mode");
                            ui.checkbox(&mut p.show_object_info_pane, "Show information pane");
                            section(ui, "Behaviour");
                            ui.checkbox(&mut p.confirm_delete, "Ask for confirmation before deleting objects");
                            ui.checkbox(&mut p.open_tabs_in_new_window, "Open tabs in a new window")
                                .on_hover_text("Reserved: tabs currently always open in the main window");
                            ui.checkbox(&mut p.auto_save_queries, "Auto-save query editor contents");
                        }
                        Page::Editor => {
                            section(ui, "SQL Editor");
                            egui::Grid::new("pref-editor").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                                row(ui, "Font size", |ui| {
                                    ui.add(egui::Slider::new(&mut p.editor_font_size, 8.0..=28.0).step_by(0.5).suffix(" pt"));
                                });
                                row(ui, "Tab width", |ui| {
                                    ui.add(egui::DragValue::new(&mut p.tab_width).range(1..=16).suffix(" spaces"));
                                });
                            });
                            ui.add_space(6.0);
                            ui.checkbox(&mut p.show_line_numbers, "Show line numbers");
                            ui.checkbox(&mut p.word_wrap, "Word wrap");
                            ui.checkbox(&mut p.auto_complete, "Code completion");
                            ui.checkbox(&mut p.uppercase_keywords, "Uppercase keywords on completion");
                            ui.add_space(8.0);
                            ui.label(RichText::new("Preview").weak());
                            ui.label(
                                RichText::new(if p.uppercase_keywords { "SELECT * FROM customers WHERE id = 1;" } else { "select * from customers where id = 1;" })
                                    .monospace()
                                    .size(p.editor_font_size),
                            );
                            ui.label(RichText::new("Editor changes apply to newly opened editors.").weak().small());
                        }
                        Page::Grid => {
                            section(ui, "Data Grid");
                            egui::Grid::new("pref-grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                                row(ui, "Font size", |ui| {
                                    ui.add(egui::Slider::new(&mut p.grid_font_size, 8.0..=24.0).step_by(0.5).suffix(" pt"));
                                });
                                row(ui, "Records per page", |ui| {
                                    ui.add(egui::DragValue::new(&mut p.records_per_page).range(1..=1_000_000).speed(10));
                                });
                                row(ui, "Display NULL as", |ui| {
                                    ui.add(egui::TextEdit::singleline(&mut p.show_null_as).desired_width(140.0));
                                });
                                row(ui, "Date/time format", |ui| {
                                    ui.add(egui::TextEdit::singleline(&mut p.date_format).desired_width(180.0).font(egui::TextStyle::Monospace));
                                });
                                row(ui, "", |ui| {
                                    let sample = chrono::Local::now().format(&p.date_format);
                                    let mut s = String::new();
                                    use std::fmt::Write;
                                    let ok = write!(s, "{}", sample).is_ok();
                                    if ok {
                                        ui.label(RichText::new(format!("e.g. {}", s)).weak());
                                    } else {
                                        crate::ui::error_text(ui, "Invalid format string");
                                    }
                                });
                            });
                            ui.add_space(8.0);
                            ui.label(RichText::new(format!("Preview: {}  |  42  |  hello", p.show_null_as)).monospace().size(p.grid_font_size));
                        }
                        Page::Query => {
                            section(ui, "Execution");
                            ui.checkbox(&mut p.stop_on_error, "Stop running a script when an error occurs");
                            ui.checkbox(&mut p.auto_begin_transaction, "Automatically begin a transaction");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut p.limit_query_results, "Limit result rows to");
                                ui.add_enabled(p.limit_query_results, egui::DragValue::new(&mut p.query_result_limit).range(1..=10_000_000).speed(50));
                            });
                            section(ui, "History");
                            ui.horizontal(|ui| {
                                ui.label("Keep at most");
                                ui.add(egui::DragValue::new(&mut p.max_history).range(10..=100_000).speed(10));
                                ui.label("history entries");
                            });
                        }
                    }
                });
            });
        });
        open && !close
    }
}
