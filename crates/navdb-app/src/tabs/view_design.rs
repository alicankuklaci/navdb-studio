//! View Designer: SELECT definition editor with data preview, explain and
//! SQL preview. Existing views are re-created with CREATE OR REPLACE
//! (MySQL / PostgreSQL) or DROP + CREATE (SQLite, renames).

use std::time::Instant;

use anyhow::Context;
use egui::{RichText, Ui};
use navdb_core::{ddl, sqlutil, Dialect, ObjectKind, Pending, QueryResult, Scope, Session};

use super::routine::{exec_statement, new_editor, run_plan, status_line, view_body, ResultsPane, SavePlan};
use super::Tab;
use crate::state::AppState;
use crate::ui::sql_editor::SqlEditor;
use crate::ui::{self, style::icon};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Definition,
    SqlPreview,
}

pub struct ViewDesignTab {
    id: String,
    conn: String,
    scope: Scope,
    dialect: Dialect,
    session: Option<Session>,
    /// Name on the server (existing views).
    view: Option<String>,
    name: String,
    editor: SqlEditor,
    saved_body: String,
    /// Original DDL (used to restore after a failed DROP + CREATE).
    orig_ddl: String,
    loading: Option<Pending<String>>,
    load_error: Option<String>,
    saving: Option<(Pending<()>, String, Instant)>,
    running: Option<(Pending<Vec<QueryResult>>, String, Instant, &'static str)>,
    results: ResultsPane,
    recreate: bool,
    page: Page,
    message: Option<(String, bool)>,
    confirm_close: bool,
    editor_height: f32,
}

impl ViewDesignTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, view: Option<String>) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let session = st.session(conn);
        if let Some(c) = st.conn_mut(conn) {
            c.load_objects(&scope, ObjectKind::Table);
        }
        let initial = if view.is_none() {
            match st.cached_tables(conn, &scope).first() {
                Some(t) => format!("SELECT *\nFROM {}", dialect.quote_ident(t)),
                None => "SELECT\n  1 AS id".to_string(),
            }
        } else {
            String::new()
        };
        let mut t = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            scope,
            dialect,
            session,
            view: view.clone(),
            name: view.unwrap_or_default(),
            editor: new_editor(st, initial.clone()),
            saved_body: initial,
            orig_ddl: String::new(),
            loading: None,
            load_error: None,
            saving: None,
            running: None,
            results: ResultsPane::default(),
            recreate: false,
            page: Page::Definition,
            message: None,
            confirm_close: false,
            editor_height: 280.0,
        };
        t.reload();
        t
    }

    fn reload(&mut self) {
        let (Some(s), Some(v)) = (&self.session, &self.view) else { return };
        let p = self.scope.path(v.clone());
        self.loading = Some(s.run(move |d| d.object_ddl(&p, ObjectKind::View)));
        self.load_error = None;
    }

    fn body(&self) -> String {
        self.editor.text.trim().trim_end_matches(';').trim_end().to_string()
    }

    fn plan(&self) -> Result<SavePlan, String> {
        let d = self.dialect;
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a view name.".into());
        }
        let body = self.body();
        if body.is_empty() {
            return Err("The view definition is empty.".into());
        }
        let path = self.scope.path(name.to_string());
        let create = ddl::create_view(d, &path, &body, false);
        let Some(old) = &self.view else {
            return Ok(SavePlan { pre: vec![], main: vec![create], restore: None });
        };
        let renamed = old != name;
        let old_path = self.scope.path(old.clone());
        let restore = if self.orig_ddl.trim().is_empty() { None } else { Some(self.orig_ddl.clone()) };
        if d == Dialect::Sqlite || renamed || self.recreate {
            Ok(SavePlan { pre: vec![format!("DROP VIEW IF EXISTS {}", d.qualify(&old_path))], main: vec![create], restore })
        } else {
            Ok(SavePlan { pre: vec![], main: vec![ddl::create_view(d, &path, &body, true)], restore: None })
        }
    }

    fn save(&mut self) {
        if self.saving.is_some() || self.loading.is_some() {
            return;
        }
        let plan = match self.plan() {
            Ok(p) => p,
            Err(e) => {
                self.message = Some((e, true));
                return;
            }
        };
        let Some(s) = &self.session else {
            self.message = Some(("Connection is not available.".into(), true));
            return;
        };
        let script = plan.script();
        let dialect = self.dialect;
        let scope = self.scope.clone();
        self.message = None;
        self.saving = Some((s.run(move |d| run_plan(d, dialect, &scope, &plan)), script, Instant::now()));
    }

    fn run(&mut self, explain: bool) {
        if self.running.is_some() {
            return;
        }
        let body = self.body();
        if body.is_empty() {
            return;
        }
        let Some(s) = &self.session else { return };
        let dialect = self.dialect;
        let scope = self.scope.clone();
        if explain {
            let b = body.clone();
            self.running = Some((s.run(move |d| Ok(vec![d.explain(Some(&scope), &b)?])), body, Instant::now(), "Explain"));
        } else {
            let sql = format!("SELECT * FROM (\n{}\n) {} LIMIT 100", body, dialect.quote_ident("_navdb_preview"));
            let q = sql.clone();
            self.running = Some((
                s.run(move |d| exec_statement(d, dialect, &scope, &q).with_context(|| "preview failed")),
                sql,
                Instant::now(),
                "Preview",
            ));
        }
    }

    fn poll(&mut self, st: &mut AppState) {
        if let Some(p) = &mut self.loading {
            if let Some(r) = p.poll() {
                self.loading = None;
                match r {
                    Ok(ddl) => {
                        let body = view_body(&ddl).unwrap_or_else(|| ddl.clone());
                        self.orig_ddl = ddl;
                        self.editor.text = body.clone();
                        self.saved_body = body;
                    }
                    Err(e) => self.load_error = Some(format!("{:#}", e)),
                }
            }
        }
        if let Some((p, _, _)) = &mut self.saving {
            if let Some(r) = p.poll() {
                let (_, script, started) = self.saving.take().unwrap();
                let ms = started.elapsed().as_millis();
                match r {
                    Ok(()) => {
                        let created = self.view.is_none();
                        st.record(&self.conn, Some(&self.scope), &script, true, if created { "view created" } else { "view saved" }, ms);
                        self.view = Some(self.name.trim().to_string());
                        self.name = self.name.trim().to_string();
                        self.saved_body = self.editor.text.clone();
                        self.orig_ddl = ddl::create_view(self.dialect, &self.scope.path(self.name.clone()), &self.body(), false);
                        st.refresh_scope(&self.conn, &self.scope);
                        st.toast(format!("View {} {}", self.name, if created { "created" } else { "saved" }));
                        self.message = Some((format!("Saved in {} ms", ms), false));
                        self.confirm_close = false;
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&self.conn, Some(&self.scope), &script, false, &m, ms);
                        self.message = Some((m, true));
                    }
                }
            }
        }
        if let Some((p, _, _, _)) = &mut self.running {
            if let Some(r) = p.poll() {
                let (_, sql, started, what) = self.running.take().unwrap();
                let ms = started.elapsed().as_millis();
                self.results.clear();
                match r {
                    Ok(res) => {
                        st.record(&self.conn, Some(&self.scope), &sql, true, what, ms);
                        self.results.set_results(what, res);
                        self.results.messages.push((format!("{} finished in {} ms", what, ms), false));
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&self.conn, Some(&self.scope), &sql, false, &m, ms);
                        self.results.messages.push((sql, false));
                        self.results.messages.push((m, true));
                    }
                }
            }
        }
    }
}

impl Tab for ViewDesignTab {
    fn title(&self) -> String {
        let n = self.view.clone().unwrap_or_else(|| if self.name.trim().is_empty() { "Untitled".into() } else { self.name.trim().to_string() });
        format!("{} {} (View)", icon::VIEW, n)
    }

    fn key(&self) -> String {
        match &self.view {
            Some(v) => format!("view_design:{}:{}", self.conn, self.scope.path(v.clone()).display()),
            None => format!("view_design:{}:new:{}", self.conn, self.id),
        }
    }

    fn dirty(&self) -> bool {
        if self.loading.is_some() {
            return false;
        }
        let renamed = match &self.view {
            Some(v) => v != self.name.trim(),
            None => !self.name.trim().is_empty(),
        };
        renamed || self.editor.text != self.saved_body
    }

    fn can_close(&mut self, _st: &mut AppState) -> bool {
        if self.dirty() && !self.confirm_close {
            self.confirm_close = true;
            self.message = Some(("Unsaved changes: Save them, or close again to discard.".into(), true));
            return false;
        }
        true
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.poll(st);
        let save_key = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        let run_key = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::R));

        ui.horizontal(|ui| {
            let busy = self.saving.is_some() || self.loading.is_some();
            if ui.add_enabled(!busy, egui::Button::new(format!("{} Save", icon::SAVE))).on_hover_text("Save (⌘S)").clicked() {
                self.save();
            }
            if self.saving.is_some() {
                ui.spinner();
            }
            let running = self.running.is_some();
            if ui.add_enabled(!running && !busy, egui::Button::new(format!("{} Preview", icon::RUN))).on_hover_text("Run the SELECT (first 100 rows, ⌘R)").clicked() {
                self.run(false);
            }
            if ui.add_enabled(!running && !busy, egui::Button::new(format!("{} Explain", icon::EXPLAIN))).clicked() {
                self.run(true);
            }
            if ui.button("Beautify").clicked() {
                self.editor.text = sqlutil::beautify(&self.editor.text, st.prefs().uppercase_keywords);
            }
            if self.view.is_some() && ui.add_enabled(!busy, egui::Button::new(icon::REFRESH)).on_hover_text("Reload (discards edits)").clicked() {
                self.reload();
            }
            ui.separator();
            ui.label("View name:");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(180.0).min_size(egui::vec2(180.0, 0.0)).hint_text("required"));
            if self.view.is_some() && self.dialect != Dialect::Sqlite {
                ui.checkbox(&mut self.recreate, "Drop & recreate").on_hover_text("Use DROP VIEW + CREATE VIEW instead of CREATE OR REPLACE (needed when columns are removed or change type)");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} · {}", st.conn_name(&self.conn), self.scope.display())).weak());
            });
        });
        if save_key {
            self.save();
        }
        if run_key {
            self.run(false);
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.page, Page::Definition, "Definition");
            ui.selectable_value(&mut self.page, Page::SqlPreview, "SQL Preview");
        });
        ui.separator();

        egui::TopBottomPanel::bottom(egui::Id::new(("view-status", &self.id))).show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if self.message.is_some() {
                    status_line(ui, &self.message);
                } else {
                    ui.label(RichText::new("Write the SELECT statement that defines the view.").weak().small());
                }
            });
        });

        if self.loading.is_some() {
            ui::loading(ui, "Loading view definition...");
            return;
        }
        if let Some(e) = self.load_error.clone() {
            ui::error_text(ui, &e);
            if ui.button("Retry").clicked() {
                self.reload();
            }
            return;
        }

        match self.page {
            Page::Definition => {
                let words = st.cached_tables(&self.conn, &self.scope);
                let id = self.id.clone();
                let max_h = (ui.available_height() - 80.0).max(120.0);
                let resp = egui::TopBottomPanel::top(egui::Id::new(("view-editor", &id)))
                    .resizable(true)
                    .default_height(self.editor_height.min(max_h))
                    .height_range(80.0..=max_h)
                    .show_inside(ui, |ui| {
                        self.editor.ui(ui, &format!("view-sql-{}", id), &words);
                    });
                self.editor_height = resp.response.rect.height();
                if self.running.is_some() {
                    ui::loading(ui, "Running...");
                }
                if self.results.is_empty() && self.running.is_none() {
                    ui.label(RichText::new("Press Preview to see the first 100 rows of the view, or Explain for the query plan.").weak());
                } else {
                    let rid = format!("view-{}", id);
                    self.results.show(ui, st, &rid);
                }
            }
            Page::SqlPreview => match self.plan() {
                Err(e) => ui::error_text(ui, &e),
                Ok(plan) => {
                    let script = plan.script();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Statements executed on Save").weak());
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(script.clone());
                        }
                    });
                    egui::ScrollArea::both().id_salt(("view-preview", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
                        let mut s = script.as_str();
                        ui.add(egui::TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                    });
                }
            },
        }
    }
}
