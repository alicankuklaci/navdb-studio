//! Navigation pane: connections → databases → (schemas) → object groups → objects.

use egui::collapsing_header::CollapsingState;
use egui::{RichText, Ui};
use navdb_core::{ConnectionConfig, DbKind, ObjectKind, Scope};

use super::style::{self, icon};
use crate::commands as cmd;
use crate::state::{Action, AppState, Loadable, Selection};

#[derive(Default)]
pub struct TreeState {
    pub filter: String,
}

pub fn show(ui: &mut Ui, st: &mut AppState, ts: &mut TreeState) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut ts.filter).hint_text("🔍 Search").desired_width(f32::INFINITY));
    });
    ui.separator();
    let conns: Vec<ConnectionConfig> = st.ws.connections.clone();
    if conns.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No connections yet").weak());
            if ui.button(format!("{} New Connection", icon::CONNECTION)).clicked() {
                cmd::new_connection(st, DbKind::MySql);
            }
        });
        return;
    }
    let mut groups: Vec<String> = conns.iter().map(|c| c.group.clone()).collect();
    groups.sort();
    groups.dedup();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for g in &groups {
            let members: Vec<&ConnectionConfig> = conns.iter().filter(|c| &c.group == g).collect();
            if g.is_empty() {
                for c in members {
                    connection_node(ui, st, ts, c);
                }
            } else {
                let id = ui.make_persistent_id(("group", g));
                CollapsingState::load_with_default_open(ui.ctx(), id, true)
                    .show_header(ui, |ui| {
                        ui.label(RichText::new(format!("📁 {}", g)).strong());
                    })
                    .body(|ui| {
                        for c in members {
                            connection_node(ui, st, ts, c);
                        }
                    });
            }
        }
    });
}

/// Ask a collapsing node to open (Some(true)), close or toggle (None) on the next frame.
/// Writing the state directly would be overwritten when the header stores its own copy.
fn request_open(ctx: &egui::Context, id: egui::Id, open: Option<bool>) {
    ctx.data_mut(|d| d.insert_temp(id.with("open-req"), open));
}

fn load_state(ui: &Ui, id: egui::Id, default_open: bool) -> CollapsingState {
    let mut cs = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    if let Some(req) = ui.ctx().data_mut(|d| d.remove_temp::<Option<bool>>(id.with("open-req"))) {
        let v = req.unwrap_or(!cs.is_open());
        cs.set_open(v);
        ui.ctx().request_repaint();
    }
    cs
}

fn matches(filter: &str, name: &str) -> bool {
    filter.is_empty() || name.to_lowercase().contains(&filter.to_lowercase())
}

fn connection_node(ui: &mut Ui, st: &mut AppState, ts: &TreeState, c: &ConnectionConfig) {
    let id = ui.make_persistent_id(("conn", &c.id));
    let is_open = st.conns.contains_key(&c.id);
    let mut cs = load_state(ui, id, false);
    if !is_open && cs.is_open() && !st.actions.iter().any(|a| matches!(a, Action::OpenConnection(x) if x == &c.id)) {
        cs.set_open(false);
    }
    let selected = st.selection.conn.as_deref() == Some(&c.id) && st.selection.scope.is_none();
    let header = cs.show_header(ui, |ui| {
        let color = c.color.map(|[r, g, b]| egui::Color32::from_rgb(r, g, b)).unwrap_or_else(|| style::kind_color(c.kind));
        let dot = if is_open { "■" } else { "○" };
        ui.label(RichText::new(dot).color(color).small());
        ui.add(style::kind_logo(c.kind, 15.0));
        let text = RichText::new(c.display_name());
        let text = if is_open { text.strong() } else { text };
        let resp = ui.selectable_label(selected, text);
        drop_focus(&resp);
        let resp = if let Some(err) = st.conn(&c.id).and_then(|o| o.databases.error().map(|s| s.to_string())) {
            ui.colored_label(ui.visuals().error_fg_color, "⚠").on_hover_text(&err);
            resp
        } else {
            resp
        };
        if resp.clicked() {
            st.selection = Selection { conn: Some(c.id.clone()), ..Default::default() };
        }
        if resp.double_clicked() {
            if is_open {
                st.push(Action::CloseConnection(c.id.clone()));
            } else {
                st.push(Action::OpenConnection(c.id.clone()));
                request_open(ui.ctx(), id, Some(true));
            }
        }
        resp.context_menu(|ui| connection_menu(ui, st, c, is_open, id));
    });
    let open_now = header.is_open();
    if open_now && !is_open {
        st.push(Action::OpenConnection(c.id.clone()));
    }
    header.body(|ui| {
        let Some(oc) = st.conn_mut(&c.id) else { return };
        if let Loadable::Loading(_) = oc.databases {
            super::loading(ui, "Connecting...");
            return;
        }
        if let Some(e) = oc.databases.error() {
            super::error_text(ui, e);
            return;
        }
        let dbs = oc.visible_databases();
        for db in dbs {
            database_node(ui, st, ts, c, &db);
        }
    });
}

fn connection_menu(ui: &mut Ui, st: &mut AppState, c: &ConnectionConfig, is_open: bool, node_id: egui::Id) {
    if is_open {
        if ui.button("Close Connection").clicked() {
            st.push(Action::CloseConnection(c.id.clone()));
            ui.close_menu();
        }
    } else if ui.button("Open Connection").clicked() {
        st.push(Action::OpenConnection(c.id.clone()));
        request_open(ui.ctx(), node_id, Some(true));
        ui.close_menu();
    }
    if ui.button("Edit Connection…").clicked() {
        cmd::edit_connection(st, &c.id);
        ui.close_menu();
    }
    if ui.button("New Connection…").clicked() {
        cmd::new_connection(st, c.kind);
        ui.close_menu();
    }
    if ui.button("Delete Connection").clicked() {
        cmd::delete_connection(st, &c.id);
        ui.close_menu();
    }
    if ui.button("Duplicate Connection").clicked() {
        cmd::duplicate_connection(st, &c.id);
        ui.close_menu();
    }
    ui.separator();
    if c.kind.is_sql() && ui.button("New Database…").clicked() {
        cmd::new_database(st, &c.id);
        ui.close_menu();
    }
    if ui.button(format!("{} New Query", icon::NEW_QUERY)).clicked() {
        cmd::new_query(st, Some(c.id.clone()), None);
        ui.close_menu();
    }
    if ui.button(format!("{} Console", icon::CONSOLE)).clicked() {
        cmd::console(st, &c.id);
        ui.close_menu();
    }
    if ui.button("Execute SQL File…").clicked() {
        cmd::execute_sql_file(st, &c.id, None);
        ui.close_menu();
    }
    ui.separator();
    if ui.button(format!("{} Server Monitor", icon::MONITOR)).clicked() {
        cmd::server_monitor(st, &c.id);
        ui.close_menu();
    }
    if c.kind.is_sql() && c.kind.base() != DbKind::Sqlite && ui.button(format!("{} Users", icon::USER)).clicked() {
        cmd::users(st, &c.id);
        ui.close_menu();
    }
    ui.separator();
    ui.menu_button("Color", |ui| {
        let colors: [(&str, Option<[u8; 3]>); 7] = [
            ("None", None),
            ("Red", Some([220, 60, 60])),
            ("Orange", Some([240, 150, 40])),
            ("Yellow", Some([220, 200, 40])),
            ("Green", Some([60, 170, 80])),
            ("Blue", Some([50, 120, 220])),
            ("Purple", Some([150, 80, 200])),
        ];
        for (name, col) in colors {
            if ui.button(name).clicked() {
                if let Some(x) = st.ws.connections.iter_mut().find(|x| x.id == c.id) {
                    x.color = col;
                }
                st.save_workspace();
                ui.close_menu();
            }
        }
    });
    ui.menu_button("Move to Group", |ui| {
        let mut groups: Vec<String> = st.ws.connections.iter().map(|c| c.group.clone()).filter(|g| !g.is_empty()).collect();
        groups.sort();
        groups.dedup();
        if ui.button("(No group)").clicked() {
            set_group(st, &c.id, String::new());
            ui.close_menu();
        }
        for g in groups {
            if ui.button(&g).clicked() {
                set_group(st, &c.id, g.clone());
                ui.close_menu();
            }
        }
        if ui.button("New Group…").clicked() {
            let id = c.id.clone();
            st.open_dialog(crate::dialogs::prompt::PromptDialog::new(
                "New Group",
                "Group name:",
                String::new(),
                Box::new(move |st: &mut AppState, name: String| set_group(st, &id, name)),
            ));
            ui.close_menu();
        }
    });
    if is_open && ui.button(format!("{} Refresh", icon::REFRESH)).clicked() {
        st.push(Action::RefreshConnection(c.id.clone()));
        ui.close_menu();
    }
}

fn set_group(st: &mut AppState, id: &str, g: String) {
    if let Some(x) = st.ws.connections.iter_mut().find(|x| x.id == id) {
        x.group = g;
    }
    st.save_workspace();
}

fn database_node(ui: &mut Ui, st: &mut AppState, ts: &TreeState, c: &ConnectionConfig, db: &str) {
    let id = ui.make_persistent_id(("db", &c.id, db));
    let scope = Scope::new(db, None);
    let selected = st.selection.conn.as_deref() == Some(&c.id) && st.selection.scope.as_ref() == Some(&scope) && st.selection.kind.is_none();
    let is_default = c.kind.base() == DbKind::Sqlite || c.default_database() == db;
    let cs = load_state(ui, id, false);
    let header = cs.show_header(ui, |ui| {
        let resp = ui.selectable_label(selected, format!("{} {}", icon::DATABASE, db));
        drop_focus(&resp);
        if resp.clicked() {
            st.selection = Selection { conn: Some(c.id.clone()), scope: Some(scope.clone()), kind: Some(ObjectKind::Table), object: None };
        }
        if resp.double_clicked() {
            request_open(ui.ctx(), id, None);
        }
        resp.context_menu(|ui| database_menu(ui, st, c, db));
        let _ = is_default;
    });
    header.body(|ui| {
        if c.kind.has_schemas() {
            let Some(oc) = st.conn_mut(&c.id) else { return };
            oc.load_schemas(db);
            let schemas = match oc.schemas.get(db) {
                Some(Loadable::Ready(v)) => v.clone(),
                Some(Loadable::Failed(e)) => {
                    super::error_text(ui, &e.clone());
                    return;
                }
                _ => {
                    super::loading(ui, "Loading schemas...");
                    return;
                }
            };
            for s in schemas {
                let sc = Scope::new(db, Some(s.clone()));
                let sid = ui.make_persistent_id(("schema", &c.id, db, &s));
                let sel = st.selection.scope.as_ref() == Some(&sc) && st.selection.conn.as_deref() == Some(&c.id) && st.selection.kind.is_none();
                CollapsingState::load_with_default_open(ui.ctx(), sid, s == "public")
                    .show_header(ui, |ui| {
                        let r = ui.selectable_label(sel, format!("{} {}", icon::SCHEMA, s));
                        drop_focus(&r);
                        if r.clicked() {
                            st.selection = Selection { conn: Some(c.id.clone()), scope: Some(sc.clone()), kind: Some(ObjectKind::Table), object: None };
                        }
                        r.context_menu(|ui| {
                            scope_menu(ui, st, c, &sc);
                            ui.separator();
                            if ui.button("New Schema…").clicked() {
                                cmd::new_schema(st, &c.id, db);
                                ui.close_menu();
                            }
                            if ui.button("Delete Schema").clicked() {
                                cmd::drop_schema(st, &c.id, db, &s);
                                ui.close_menu();
                            }
                        });
                    })
                    .body(|ui| kind_groups(ui, st, ts, c, &sc));
            }
        } else {
            kind_groups(ui, st, ts, c, &scope);
        }
    });
}

fn database_menu(ui: &mut Ui, st: &mut AppState, c: &ConnectionConfig, db: &str) {
    let scope = Scope::new(db, None);
    if c.kind.is_sql() {
        if c.kind.base() != DbKind::Sqlite || db != "main" {
            if ui.button("Edit Database…").clicked() {
                cmd::edit_database(st, &c.id, db);
                ui.close_menu();
            }
        }
        if ui.button("New Database…").clicked() {
            cmd::new_database(st, &c.id);
            ui.close_menu();
        }
        if ui.button("Delete Database").clicked() {
            cmd::drop_database(st, &c.id, db);
            ui.close_menu();
        }
        if c.kind.has_schemas() && ui.button("New Schema…").clicked() {
            cmd::new_schema(st, &c.id, db);
            ui.close_menu();
        }
        ui.separator();
    }
    scope_menu(ui, st, c, &scope);
}

/// Menu entries shared by databases and schemas.
fn scope_menu(ui: &mut Ui, st: &mut AppState, c: &ConnectionConfig, scope: &Scope) {
    if ui.button(format!("{} New Query", icon::NEW_QUERY)).clicked() {
        cmd::new_query(st, Some(c.id.clone()), Some(scope.clone()));
        ui.close_menu();
    }
    if !c.kind.is_sql() {
        if ui.button("New Key…").clicked() {
            cmd::design_object(st, &c.id, scope, ObjectKind::Key, None);
            ui.close_menu();
        }
        if ui.button(format!("{} Refresh", icon::REFRESH)).clicked() {
            st.refresh_scope(&c.id, scope);
            ui.close_menu();
        }
        return;
    }
    if ui.button(format!("{} New Table", icon::TABLE)).clicked() {
        cmd::design_object(st, &c.id, scope, ObjectKind::Table, None);
        ui.close_menu();
    }
    if ui.button("Execute SQL File…").clicked() {
        cmd::execute_sql_file(st, &c.id, Some(scope.clone()));
        ui.close_menu();
    }
    ui.menu_button("Dump SQL File", |ui| {
        if ui.button("Structure + Data…").clicked() {
            cmd::dump_sql(st, &c.id, scope, vec![], true);
            ui.close_menu();
        }
        if ui.button("Structure Only…").clicked() {
            cmd::dump_sql(st, &c.id, scope, vec![], false);
            ui.close_menu();
        }
    });
    if ui.button(format!("{} Backup / Restore…", icon::BACKUP)).clicked() {
        cmd::backup(st, &c.id, scope);
        ui.close_menu();
    }
    ui.separator();
    ui.menu_button(format!("{} Tools", icon::TOOLS), |ui| {
        if ui.button("Data Transfer…").clicked() {
            cmd::data_transfer(st, Some((c.id.clone(), scope.clone())));
            ui.close_menu();
        }
        if ui.button("Data Synchronization…").clicked() {
            cmd::data_sync(st, Some((c.id.clone(), scope.clone())));
            ui.close_menu();
        }
        if ui.button("Structure Synchronization…").clicked() {
            cmd::structure_sync(st, Some((c.id.clone(), scope.clone())));
            ui.close_menu();
        }
        if ui.button("Data Generation…").clicked() {
            cmd::data_generation(st, &c.id, scope, vec![]);
            ui.close_menu();
        }
        if ui.button("Find in Database…").clicked() {
            cmd::find_in_database(st, &c.id, scope);
            ui.close_menu();
        }
    });
    if ui.button(format!("{} Reverse Database to Model", icon::MODEL)).clicked() {
        cmd::er_model(st, &c.id, scope);
        ui.close_menu();
    }
    if ui.button("Import Wizard…").clicked() {
        cmd::import_wizard(st, &c.id, scope, None);
        ui.close_menu();
    }
    if ui.button(format!("{} Refresh", icon::REFRESH)).clicked() {
        st.refresh_scope(&c.id, scope);
        ui.close_menu();
    }
}

fn kind_groups(ui: &mut Ui, st: &mut AppState, ts: &TreeState, c: &ConnectionConfig, scope: &Scope) {
    let kinds = st.conn(&c.id).map(|o| o.object_kinds()).unwrap_or_default();
    for kind in kinds {
        let gid = ui.make_persistent_id(("kind", &c.id, scope, kind as u8));
        let default_open = kind == ObjectKind::Table || kind == ObjectKind::Key;
        let sel = st.selection.conn.as_deref() == Some(&c.id)
            && st.selection.scope.as_ref() == Some(scope)
            && st.selection.kind == Some(kind)
            && st.selection.object.is_none();
        let cs = CollapsingState::load_with_default_open(ui.ctx(), gid, default_open);
        let header = cs.show_header(ui, |ui| {
            let count = st
                .conn(&c.id)
                .and_then(|o| o.objects(scope, kind))
                .and_then(|l| l.ready())
                .map(|v| format!(" ({})", v.len()))
                .unwrap_or_default();
            let r = ui.selectable_label(sel, format!("{} {}{}", crate::ui::style::object_icon(kind), kind.label(), count));
            drop_focus(&r);
            if r.clicked() {
                st.selection = Selection { conn: Some(c.id.clone()), scope: Some(scope.clone()), kind: Some(kind), object: None };
            }
            r.context_menu(|ui| {
                if c.kind.is_sql() || kind == ObjectKind::Key {
                    if ui.button(format!("New {}", kind.singular())).clicked() {
                        cmd::design_object(st, &c.id, scope, kind, None);
                        ui.close_menu();
                    }
                }
                if kind == ObjectKind::Table {
                    if ui.button("Import Wizard…").clicked() {
                        cmd::import_wizard(st, &c.id, scope, None);
                        ui.close_menu();
                    }
                    if ui.button("Export Wizard…").clicked() {
                        cmd::export_wizard(st, &c.id, scope, vec![]);
                        ui.close_menu();
                    }
                }
                if ui.button(format!("{} Refresh", icon::REFRESH)).clicked() {
                    st.refresh_scope(&c.id, scope);
                    ui.close_menu();
                }
            });
        });
        let open = header.is_open();
        if open {
            if let Some(oc) = st.conn_mut(&c.id) {
                oc.load_objects(scope, kind);
            }
        }
        header.body(|ui| {
            let list = match st.conn(&c.id).and_then(|o| o.objects(scope, kind)) {
                Some(Loadable::Ready(v)) => v.clone(),
                Some(Loadable::Failed(e)) => {
                    super::error_text(ui, &e.clone());
                    return;
                }
                _ => {
                    super::loading(ui, "Loading...");
                    return;
                }
            };
            for o in list.iter().filter(|o| matches(&ts.filter, &o.name)) {
                object_leaf(ui, st, c, scope, kind, &o.name);
            }
        });
    }
    // saved queries for this connection/database
    if c.kind.is_sql() {
        let queries: Vec<(String, String)> = st
            .ws
            .queries
            .iter()
            .filter(|q| q.connection_id == c.id && (q.database.is_empty() || q.database == scope.display() || q.database == scope.db))
            .map(|q| (q.id.clone(), q.name.clone()))
            .collect();
        let gid = ui.make_persistent_id(("queries", &c.id, scope));
        CollapsingState::load_with_default_open(ui.ctx(), gid, false)
            .show_header(ui, |ui| {
                let r = ui.label(format!("{} Queries ({})", icon::QUERY, queries.len()));
                r.context_menu(|ui| {
                    if ui.button("New Query").clicked() {
                        cmd::new_query(st, Some(c.id.clone()), Some(scope.clone()));
                        ui.close_menu();
                    }
                });
            })
            .body(|ui| {
                for (qid, name) in queries.iter().filter(|(_, n)| matches(&ts.filter, n)) {
                    let r = ui.selectable_label(false, format!("{} {}", icon::QUERY, name));
                    drop_focus(&r);
                    if r.double_clicked() {
                        cmd::open_saved_query(st, qid);
                    }
                    r.context_menu(|ui| {
                        if ui.button("Open Query").clicked() {
                            cmd::open_saved_query(st, qid);
                            ui.close_menu();
                        }
                        if ui.button("Delete Query").clicked() {
                            st.ws.queries.retain(|q| &q.id != qid);
                            st.save_workspace();
                            ui.close_menu();
                        }
                    });
                }
            });
    }
}

fn object_leaf(ui: &mut Ui, st: &mut AppState, c: &ConnectionConfig, scope: &Scope, kind: ObjectKind, name: &str) {
    let sel = st.selection.conn.as_deref() == Some(&c.id)
        && st.selection.scope.as_ref() == Some(scope)
        && st.selection.object.as_deref() == Some(name);
    let r = ui.selectable_label(sel, format!("   {} {}", crate::ui::style::object_icon(kind), name));
    drop_focus(&r);
    if r.clicked() {
        st.selection = Selection { conn: Some(c.id.clone()), scope: Some(scope.clone()), kind: Some(kind), object: Some(name.to_string()) };
    }
    if r.double_clicked() {
        cmd::open_object(st, &c.id, scope, kind, name);
    }
    r.context_menu(|ui| object_menu(ui, st, c, scope, kind, name));
}

/// Context menu for a single object (also used by the Objects tab).
pub fn object_menu(ui: &mut Ui, st: &mut AppState, c: &ConnectionConfig, scope: &Scope, kind: ObjectKind, name: &str) {
    let conn = c.id.as_str();
    match kind {
        ObjectKind::Table => {
            if ui.button("Open Table").clicked() {
                cmd::open_object(st, conn, scope, kind, name);
                ui.close_menu();
            }
            if ui.button("Design Table").clicked() {
                cmd::design_object(st, conn, scope, kind, Some(name.to_string()));
                ui.close_menu();
            }
            if ui.button("New Table").clicked() {
                cmd::design_object(st, conn, scope, kind, None);
                ui.close_menu();
            }
            if ui.button("Delete Table").clicked() {
                cmd::drop_objects(st, conn, scope, kind, vec![name.to_string()]);
                ui.close_menu();
            }
            if ui.button("Empty Table").clicked() {
                cmd::empty_tables(st, conn, scope, vec![name.to_string()], false);
                ui.close_menu();
            }
            if c.kind.base() != DbKind::Sqlite && ui.button("Truncate Table").clicked() {
                cmd::empty_tables(st, conn, scope, vec![name.to_string()], true);
                ui.close_menu();
            }
            ui.menu_button("Duplicate Table", |ui| {
                if ui.button("Structure + Data").clicked() {
                    cmd::duplicate_table(st, conn, scope, name, true);
                    ui.close_menu();
                }
                if ui.button("Structure Only").clicked() {
                    cmd::duplicate_table(st, conn, scope, name, false);
                    ui.close_menu();
                }
            });
            ui.separator();
            if ui.button("Import Wizard…").clicked() {
                cmd::import_wizard(st, conn, scope, Some(name.to_string()));
                ui.close_menu();
            }
            if ui.button("Export Wizard…").clicked() {
                cmd::export_wizard(st, conn, scope, vec![name.to_string()]);
                ui.close_menu();
            }
            ui.menu_button("Dump SQL File", |ui| {
                if ui.button("Structure + Data…").clicked() {
                    cmd::dump_sql(st, conn, scope, vec![name.to_string()], true);
                    ui.close_menu();
                }
                if ui.button("Structure Only…").clicked() {
                    cmd::dump_sql(st, conn, scope, vec![name.to_string()], false);
                    ui.close_menu();
                }
            });
            if ui.button("Generate Data…").clicked() {
                cmd::data_generation(st, conn, scope, vec![name.to_string()]);
                ui.close_menu();
            }
            ui.separator();
        }
        ObjectKind::View | ObjectKind::MaterializedView => {
            if ui.button(format!("Open {}", kind.singular())).clicked() {
                cmd::open_object(st, conn, scope, kind, name);
                ui.close_menu();
            }
            if ui.button(format!("Design {}", kind.singular())).clicked() {
                cmd::design_object(st, conn, scope, kind, Some(name.to_string()));
                ui.close_menu();
            }
            if ui.button(format!("New {}", kind.singular())).clicked() {
                cmd::design_object(st, conn, scope, kind, None);
                ui.close_menu();
            }
            if ui.button(format!("Delete {}", kind.singular())).clicked() {
                cmd::drop_objects(st, conn, scope, kind, vec![name.to_string()]);
                ui.close_menu();
            }
            if ui.button("Export Wizard…").clicked() {
                cmd::export_wizard(st, conn, scope, vec![name.to_string()]);
                ui.close_menu();
            }
            ui.separator();
        }
        _ => {
            if ui.button(format!("Design {}", kind.singular())).clicked() {
                cmd::open_object(st, conn, scope, kind, name);
                ui.close_menu();
            }
            if ui.button(format!("New {}", kind.singular())).clicked() {
                cmd::design_object(st, conn, scope, kind, None);
                ui.close_menu();
            }
            if ui.button(format!("Delete {}", kind.singular())).clicked() {
                cmd::drop_objects(st, conn, scope, kind, vec![name.to_string()]);
                ui.close_menu();
            }
            ui.separator();
        }
    }
    if kind != ObjectKind::Function && kind != ObjectKind::Procedure && kind != ObjectKind::Index && ui.button("Rename").clicked() {
        cmd::rename_object(st, conn, scope, kind, name);
        ui.close_menu();
    }
    if ui.button("Copy Name").clicked() {
        ui.ctx().copy_text(name.to_string());
        ui.close_menu();
    }
    if ui.button(format!("{} Refresh", icon::REFRESH)).clicked() {
        st.refresh_scope(conn, scope);
        ui.close_menu();
    }
}

/// Navigator rows don't take keyboard focus; otherwise a menu click that lands on a row
/// leaves it drawn with the focus frame.
fn drop_focus(r: &egui::Response) {
    if r.has_focus() {
        r.surrender_focus();
    }
}
