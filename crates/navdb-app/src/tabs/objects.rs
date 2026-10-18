//! "Objects" tab: list of objects for the navigator selection.

use egui::{RichText, Ui};
use egui_extras::{Column, TableBuilder};
use navdb_core::{DbKind, ObjectInfo, ObjectKind};

use super::Tab;
use crate::commands as cmd;
use crate::state::{AppState, Loadable, Selection};
use crate::ui::{self, style::icon};

pub struct ObjectsTab {
    selected: Vec<String>,
    detail: bool,
}

impl ObjectsTab {
    pub fn new() -> Self {
        Self { selected: Vec::new(), detail: true }
    }
}

impl Tab for ObjectsTab {
    fn title(&self) -> String {
        "Objects".into()
    }

    fn key(&self) -> String {
        "objects".into()
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        let sel = st.selection.clone();
        let Some(conn) = sel.conn.clone() else {
            welcome(ui, st);
            return;
        };
        let Some(cfg) = st.config(&conn).cloned() else { return };
        let Some(scope) = sel.scope.clone() else {
            ui.heading(format!("{} {}", ui::style::kind_icon(cfg.kind), cfg.display_name()));
            ui.label(RichText::new("Open the connection and select a database to see its objects.").weak());
            ui.horizontal(|ui| {
                if !st.conns.contains_key(&conn) && ui.button("Open Connection").clicked() {
                    st.push(crate::state::Action::OpenConnection(conn.clone()));
                }
                if ui.button("Edit Connection…").clicked() {
                    cmd::edit_connection(st, &conn);
                }
                if ui.button("New Query").clicked() {
                    cmd::new_query(st, Some(conn.clone()), None);
                }
            });
            if let Some(oc) = st.conn(&conn) {
                ui.add_space(8.0);
                let dbs = oc.visible_databases();
                ui.label(RichText::new(format!("{} database(s)", dbs.len())).strong());
                let mut pick = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for d in &dbs {
                            if ui.button(format!("{} {}", icon::DATABASE, d)).clicked() {
                                pick = Some(d.clone());
                            }
                        }
                    });
                });
                if let Some(d) = pick {
                    st.selection = Selection {
                        conn: Some(conn.clone()),
                        scope: Some(navdb_core::Scope::new(d, if cfg.kind.has_schemas() { Some("public".into()) } else { None })),
                        kind: Some(if cfg.kind == DbKind::Redis { ObjectKind::Key } else { ObjectKind::Table }),
                        object: None,
                    };
                }
            }
            return;
        };
        if st.session(&conn).is_none() {
            return;
        }
        let kinds = st.conn(&conn).map(|c| c.object_kinds()).unwrap_or_default();
        let mut kind = sel.kind.unwrap_or(if cfg.kind == DbKind::Redis { ObjectKind::Key } else { ObjectKind::Table });
        if !kinds.is_empty() && !kinds.contains(&kind) {
            kind = kinds[0];
        }
        if let Some(oc) = st.conn_mut(&conn) {
            oc.load_objects(&scope, kind);
        }

        // toolbar
        ui.horizontal(|ui| {
            for k in &kinds {
                if ui.selectable_label(*k == kind, format!("{} {}", k.icon(), k.label())).clicked() {
                    st.selection.kind = Some(*k);
                    st.selection.object = None;
                    self.selected.clear();
                }
            }
        });
        ui.horizontal(|ui| {
            let one = self.selected.first().cloned();
            let open_label = match kind {
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::Key => "Open",
                _ => "Design",
            };
            if ui.add_enabled(one.is_some(), egui::Button::new(format!("{} {} {}", icon::RUN, open_label, kind.singular()))).clicked() {
                if let Some(n) = &one {
                    cmd::open_object(st, &conn, &scope, kind, n);
                }
            }
            if matches!(kind, ObjectKind::Table | ObjectKind::View)
                && ui.add_enabled(one.is_some(), egui::Button::new(format!("{} Design {}", icon::DESIGN, kind.singular()))).clicked()
            {
                cmd::design_object(st, &conn, &scope, kind, one.clone());
            }
            if ui.button(format!("{} New {}", icon::ADD, kind.singular())).clicked() {
                cmd::design_object(st, &conn, &scope, kind, None);
            }
            if ui.add_enabled(!self.selected.is_empty(), egui::Button::new(format!("{} Delete {}", icon::DELETE, kind.singular()))).clicked() {
                cmd::drop_objects(st, &conn, &scope, kind, self.selected.clone());
            }
            if kind == ObjectKind::Table {
                ui.separator();
                if ui.button(format!("{} Import Wizard", icon::IMPORT)).clicked() {
                    cmd::import_wizard(st, &conn, &scope, one.clone());
                }
                if ui.button(format!("{} Export Wizard", icon::EXPORT)).clicked() {
                    cmd::export_wizard(st, &conn, &scope, self.selected.clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(icon::REFRESH).on_hover_text("Refresh").clicked() {
                    st.refresh_scope(&conn, &scope);
                }
                ui.toggle_value(&mut self.detail, "☰ Detail");
                ui.add(egui::TextEdit::singleline(&mut st.object_filter).hint_text("🔍 Filter").desired_width(160.0));
            });
        });
        ui.separator();

        let list: Vec<ObjectInfo> = match st.conn(&conn).and_then(|c| c.objects(&scope, kind)) {
            Some(Loadable::Ready(v)) => v.clone(),
            Some(Loadable::Failed(e)) => {
                ui::error_text(ui, &e.clone());
                return;
            }
            _ => {
                ui::loading(ui, "Loading objects...");
                return;
            }
        };
        let filter = st.object_filter.to_lowercase();
        let list: Vec<ObjectInfo> = list.into_iter().filter(|o| filter.is_empty() || o.name.to_lowercase().contains(&filter)).collect();

        let mut clicked: Option<(String, bool, bool)> = None;
        let mut double: Option<String> = None;
        let mut menu_target: Option<String> = None;
        if !self.detail {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for o in &list {
                        let sel = self.selected.contains(&o.name);
                        let r = ui.selectable_label(sel, format!("{} {}", kind.icon(), o.name));
                        if r.clicked() {
                            let m = ui.input(|i| i.modifiers);
                            clicked = Some((o.name.clone(), m.command, m.shift));
                        }
                        if r.double_clicked() {
                            double = Some(o.name.clone());
                        }
                        if r.secondary_clicked() {
                            menu_target = Some(o.name.clone());
                        }
                        r.context_menu(|ui| crate::ui::tree::object_menu(ui, st, &cfg, &scope, kind, &o.name));
                    }
                });
            });
        } else {
            let show_rows = matches!(kind, ObjectKind::Table | ObjectKind::MaterializedView);
            let extra_keys: Vec<String> = list.first().map(|o| o.extra.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
            let mut tb = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(egui::Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::initial(240.0).at_least(80.0).clip(true));
            if show_rows {
                tb = tb.column(Column::initial(90.0)).column(Column::initial(100.0));
            }
            tb = tb.columns(Column::initial(120.0).clip(true), 2 + extra_keys.len()).column(Column::remainder().clip(true));
            tb.header(22.0, |mut h| {
                h.col(|ui| {
                    ui.strong("Name");
                });
                if show_rows {
                    h.col(|ui| {
                        ui.strong("Rows");
                    });
                    h.col(|ui| {
                        ui.strong("Data Length");
                    });
                }
                h.col(|ui| {
                    ui.strong(if cfg.kind == DbKind::MySql || cfg.kind == DbKind::MariaDb { "Engine" } else { "Created" });
                });
                h.col(|ui| {
                    ui.strong("Modified");
                });
                for k in &extra_keys {
                    h.col(|ui| {
                        ui.strong(k);
                    });
                }
                h.col(|ui| {
                    ui.strong("Comment");
                });
            })
            .body(|body| {
                body.rows(22.0, list.len(), |mut row| {
                    let o = &list[row.index()];
                    row.set_selected(self.selected.contains(&o.name));
                    let mut resps = Vec::new();
                    resps.push(row.col(|ui| {
                        ui.label(format!("{} {}", kind.icon(), o.name));
                    }).1);
                    if show_rows {
                        resps.push(row.col(|ui| {
                            ui.label(o.rows.map(|r| r.to_string()).unwrap_or_default());
                        }).1);
                        resps.push(row.col(|ui| {
                            ui.label(o.data_length.map(ui::format_bytes).unwrap_or_default());
                        }).1);
                    }
                    resps.push(row.col(|ui| {
                        ui.label(if o.engine.is_empty() { &o.created } else { &o.engine });
                    }).1);
                    resps.push(row.col(|ui| {
                        ui.label(&o.modified);
                    }).1);
                    for k in &extra_keys {
                        let v = o.extra.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()).unwrap_or_default();
                        resps.push(row.col(|ui| {
                            ui.label(v);
                        }).1);
                    }
                    resps.push(row.col(|ui| {
                        ui.label(RichText::new(&o.comment).weak());
                    }).1);
                    for r in resps {
                        if r.clicked() {
                            let m = r.ctx.input(|i| i.modifiers);
                            clicked = Some((o.name.clone(), m.command, m.shift));
                        }
                        if r.double_clicked() {
                            double = Some(o.name.clone());
                        }
                        r.context_menu(|ui| crate::ui::tree::object_menu(ui, st, &cfg, &scope, kind, &o.name));
                    }
                });
            });
        }
        if let Some((name, cmd_key, shift)) = clicked {
            if cmd_key {
                if let Some(p) = self.selected.iter().position(|x| x == &name) {
                    self.selected.remove(p);
                } else {
                    self.selected.push(name.clone());
                }
            } else if shift && !self.selected.is_empty() {
                let a = list.iter().position(|o| o.name == self.selected[0]).unwrap_or(0);
                let b = list.iter().position(|o| o.name == name).unwrap_or(0);
                let (a, b) = (a.min(b), a.max(b));
                self.selected = list[a..=b].iter().map(|o| o.name.clone()).collect();
            } else {
                self.selected = vec![name.clone()];
            }
            st.selection.object = Some(name);
            st.selection.kind = Some(kind);
        }
        if let Some(n) = double {
            cmd::open_object(st, &conn, &scope, kind, &n);
        }
        let _ = menu_target;
        // keyboard: delete key on selected objects
        if !self.selected.is_empty() && ui.input(|i| i.key_pressed(egui::Key::Delete)) && ui.memory(|m| m.focused().is_none()) {
            cmd::drop_objects(st, &conn, &scope, kind, self.selected.clone());
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(RichText::new(format!("{} {}", list.len(), kind.label().to_lowercase())).small().weak());
        });
    }
}

fn welcome(ui: &mut Ui, st: &mut AppState) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.heading(RichText::new("NavDB Studio").size(30.0));
        ui.label(RichText::new("Database development & administration for MySQL, MariaDB, PostgreSQL, SQLite and Redis").weak());
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            let w = ui.available_width();
            ui.add_space((w - 5.0 * 130.0).max(0.0) / 2.0);
            for k in DbKind::ALL {
                if ui.add_sized([120.0, 60.0], egui::Button::new(RichText::new(format!("{}\n{}", ui::style::kind_icon(k), k.label())).size(15.0))).clicked() {
                    cmd::new_connection(st, k);
                }
            }
        });
        ui.add_space(20.0);
        ui.label(RichText::new("Create a connection, then double-click it in the navigator to connect.").weak());
        if !st.ws.connections.is_empty() {
            ui.add_space(14.0);
            ui.label(RichText::new("Recent connections").strong());
            let conns: Vec<(String, String, DbKind)> = st.ws.connections.iter().map(|c| (c.id.clone(), c.display_name(), c.kind)).collect();
            for (id, name, kind) in conns.into_iter().take(10) {
                if ui.link(format!("{} {}", ui::style::kind_icon(kind), name)).clicked() {
                    st.push(crate::state::Action::OpenConnection(id.clone()));
                    st.selection = Selection { conn: Some(id), ..Default::default() };
                }
            }
        }
    });
}
