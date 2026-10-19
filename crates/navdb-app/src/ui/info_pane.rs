//! Right-hand information pane: General properties and DDL of the selection.

use egui::{RichText, Ui};
use navdb_core::{ObjectKind, Pending};

use crate::state::AppState;

#[derive(Default)]
pub struct InfoPane {
    pub show_ddl: bool,
    key: Option<(String, String, String)>,
    ddl: Option<Pending<String>>,
    ddl_text: Option<Result<String, String>>,
}

impl InfoPane {
    pub fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_ddl, false, "ℹ General");
            ui.selectable_value(&mut self.show_ddl, true, "DDL");
        });
        ui.separator();
        let sel = st.selection.clone();
        let Some(conn) = sel.conn.clone() else {
            ui.label(RichText::new("No object selected").weak());
            return;
        };
        let Some(cfg) = st.config(&conn).cloned() else { return };
        let Some(scope) = sel.scope.clone() else {
            // connection information
            let mut rows = vec![
                ("Name".to_string(), cfg.display_name()),
                ("Type".to_string(), cfg.kind.label().to_string()),
            ];
            if cfg.kind.is_file_based() {
                rows.push(("File".into(), cfg.database.clone()));
            } else {
                rows.push(("Host".into(), format!("{}:{}", cfg.host, cfg.port)));
                rows.push(("User".into(), cfg.user.clone()));
            }
            if let Some(oc) = st.conn(&conn) {
                rows.push(("Status".into(), "Connected".into()));
                if let Some(v) = oc.version.ready() {
                    rows.push(("Server Version".into(), v.clone()));
                }
                if let Some(dbs) = oc.databases.ready() {
                    rows.push(("Databases".into(), dbs.len().to_string()));
                }
            } else {
                rows.push(("Status".into(), "Closed".into()));
            }
            if cfg.ssh.enabled {
                rows.push(("SSH Tunnel".into(), format!("{}@{}:{}", cfg.ssh.user, cfg.ssh.host, cfg.ssh.port)));
            }
            if cfg.ssl.enabled {
                rows.push(("SSL".into(), "Enabled".into()));
            }
            super::kv_grid(ui, "conn-info", &rows);
            return;
        };
        let (Some(kind), Some(name)) = (sel.kind, sel.object.clone()) else {
            let mut rows = vec![("Database".to_string(), scope.db.clone())];
            if let Some(s) = &scope.schema {
                rows.push(("Schema".into(), s.clone()));
            }
            if let Some(oc) = st.conn(&conn) {
                for k in oc.object_kinds() {
                    if let Some(v) = oc.objects(&scope, k).and_then(|l| l.ready()) {
                        rows.push((k.label().to_string(), v.len().to_string()));
                    }
                }
            }
            super::kv_grid(ui, "db-info", &rows);
            return;
        };
        if !self.show_ddl {
            let info = st
                .conn(&conn)
                .and_then(|c| c.objects(&scope, kind))
                .and_then(|l| l.ready())
                .and_then(|v| v.iter().find(|o| o.name == name).cloned());
            ui.label(RichText::new(format!("{} {}", crate::ui::style::object_icon(kind), name)).heading());
            ui.label(RichText::new(kind.singular()).weak());
            ui.add_space(6.0);
            if let Some(o) = info {
                let mut rows = Vec::new();
                if let Some(r) = o.rows {
                    rows.push(("Rows".to_string(), r.to_string()));
                }
                if let Some(l) = o.data_length {
                    rows.push(("Data Length".into(), super::format_bytes(l)));
                }
                if !o.engine.is_empty() {
                    rows.push(("Engine".into(), o.engine.clone()));
                }
                if !o.created.is_empty() {
                    rows.push(("Create Date".into(), o.created.clone()));
                }
                if !o.modified.is_empty() {
                    rows.push(("Modified Date".into(), o.modified.clone()));
                }
                for (k, v) in &o.extra {
                    if !v.is_empty() {
                        rows.push((k.clone(), v.clone()));
                    }
                }
                if !o.comment.is_empty() {
                    rows.push(("Comment".into(), o.comment.clone()));
                }
                super::kv_grid(ui, "obj-info", &rows);
            }
            return;
        }
        // DDL
        let key = (conn.clone(), scope.display(), format!("{:?}:{}", kind, name));
        if self.key.as_ref() != Some(&key) {
            self.key = Some(key);
            self.ddl_text = None;
            if let Some(s) = st.session(&conn) {
                let p = scope.path(name.clone());
                self.ddl = Some(s.run(move |d| d.object_ddl(&p, kind)));
            }
        }
        if let Some(p) = &mut self.ddl {
            if let Some(r) = p.poll() {
                self.ddl_text = Some(r.map_err(|e| format!("{:#}", e)));
                self.ddl = None;
            }
        }
        if self.ddl.is_some() {
            super::loading(ui, "Loading DDL...");
            return;
        }
        match self.ddl_text.as_ref().unwrap_or(&Ok(String::new())) {
            Ok(t) => {
                if ui.small_button("📋 Copy").clicked() {
                    ui.ctx().copy_text(t.clone());
                }
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    let theme = egui_extras::syntax_highlighting::CodeTheme::from_memory(ui.ctx(), ui.style());
                    egui_extras::syntax_highlighting::code_view_ui(ui, &theme, t, if kind == ObjectKind::Key { "txt" } else { "sql" });
                });
            }
            Err(e) => super::error_text(ui, e),
        }
    }
}
