//! Query editor tab: SQL editor, multiple result sets, explain, history,
//! snippets, saved queries.

use std::collections::HashMap;

use egui::{RichText, Ui};
use navdb_core::config::{SavedQuery, Snippet};
use navdb_core::complete::{self, Item, ItemKind, NoCatalog};
use navdb_core::{sqlutil, DbKind, Dialect, Pending, QueryResult, Scope, Session};

use super::Tab;
use crate::catalog::AppCatalog;
use crate::commands as cmd;
use crate::state::AppState;
use crate::ui::grid::{self, GridOptions, GridState};
use crate::ui::sql_editor::{Assist, SqlEditor};
use crate::ui::{self, style::icon};

struct ResultTab {
    title: String,
    result: QueryResult,
    grid: GridState,
    truncated: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum Pane {
    Message,
    Result(usize),
    Explain,
}

pub struct QueryTab {
    id: String,
    conn: Option<String>,
    scope: Option<Scope>,
    session: Option<Session>,
    session_conn: Option<String>,
    editor: SqlEditor,
    saved_id: Option<String>,
    name: String,
    saved_text: String,
    running: Option<(Pending<Vec<Result<QueryResult, String>>>, std::time::Instant, String)>,
    explaining: Option<Pending<QueryResult>>,
    results: Vec<ResultTab>,
    explain: Option<ResultTab>,
    messages: Vec<(String, bool)>,
    pane: Pane,
    columns_cache: HashMap<String, Vec<String>>,
    columns_loading: Option<(String, Pending<Vec<String>>)>,
    dialect: Dialect,
    kind: DbKind,
    editor_height: f32,
}

impl QueryTab {
    pub fn new(st: &mut AppState, conn: Option<String>, scope: Option<Scope>, sql: String, saved_id: Option<String>) -> Self {
        let p = st.prefs().clone();
        let mut editor = SqlEditor::new(sql.clone());
        editor.font_size = p.editor_font_size;
        editor.auto_complete = p.auto_complete;
        editor.uppercase_keywords = p.uppercase_keywords;
        let name = saved_id
            .as_ref()
            .and_then(|id| st.ws.queries.iter().find(|q| &q.id == id).map(|q| q.name.clone()))
            .unwrap_or_else(|| "Untitled".into());
        let (dialect, kind) = conn.as_ref().and_then(|c| st.config(c)).map(|c| (c.dialect(), c.kind)).unwrap_or((Dialect::MySql, DbKind::MySql));
        editor.language = kind.query_language().into();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn,
            scope,
            session: None,
            session_conn: None,
            editor,
            saved_id,
            name,
            saved_text: sql,
            running: None,
            explaining: None,
            results: Vec::new(),
            explain: None,
            messages: Vec::new(),
            pane: Pane::Message,
            columns_cache: HashMap::new(),
            columns_loading: None,
            dialect,
            kind,
            editor_height: 260.0,
        }
    }

    fn ensure_session(&mut self, st: &mut AppState) -> Option<Session> {
        let conn = self.conn.clone()?;
        if self.session_conn.as_deref() != Some(&conn) {
            let shared = st.session(&conn)?;
            // each query tab owns a dedicated connection
            self.session = Some(shared.fork());
            self.session_conn = Some(conn.clone());
            if let Some(c) = st.config(&conn) {
                self.dialect = c.dialect();
                self.kind = c.kind;
                self.editor.language = c.kind.query_language().into();
            }
        }
        self.session.clone()
    }

    fn run(&mut self, st: &mut AppState, sql: String) {
        if sql.trim().is_empty() {
            return;
        }
        let Some(session) = self.ensure_session(st) else {
            self.messages = vec![("Select a connection first.".into(), true)];
            return;
        };
        let scope = self.scope.clone();
        let stop_on_error = st.prefs().stop_on_error;
        let dialect = self.dialect;
        let is_redis = self.kind.base() == DbKind::Redis;
        // MongoDB shell scripts and search console requests are split by the driver
        let whole = matches!(self.kind.base(), DbKind::MongoDb | DbKind::OpenSearch);
        let text = sql.clone();
        let pending = session.run(move |d| {
            let stmts: Vec<String> = if is_redis {
                text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
            } else if whole {
                vec![text.clone()]
            } else {
                sqlutil::split_statements(&text, dialect)
            };
            let mut out = Vec::new();
            for s in stmts {
                match d.execute(scope.as_ref(), &s) {
                    Ok(mut rs) => {
                        if rs.is_empty() {
                            rs.push(QueryResult { sql: s.clone(), ..Default::default() });
                        }
                        out.extend(rs.into_iter().map(Ok));
                    }
                    Err(e) => {
                        out.push(Err(format!("{}\n> {:#}", s, e)));
                        if stop_on_error {
                            break;
                        }
                    }
                }
            }
            Ok(out)
        });
        self.running = Some((pending, std::time::Instant::now(), sql));
        self.messages.clear();
    }

    fn explain(&mut self, st: &mut AppState) {
        let sql = {
            let sel = self.editor.selected_text();
            if sel.trim().is_empty() {
                sqlutil::statement_at(&self.editor.text, self.editor.cursor, self.dialect).unwrap_or_default()
            } else {
                sel
            }
        };
        if sql.trim().is_empty() {
            return;
        }
        let Some(session) = self.ensure_session(st) else { return };
        let scope = self.scope.clone();
        self.explaining = Some(session.run(move |d| d.explain(scope.as_ref(), &sql)));
    }

    fn save(&mut self, st: &mut AppState, ask_name: bool) {
        let Some(conn) = self.conn.clone() else {
            st.error("Select a connection before saving the query");
            return;
        };
        let db = self.scope.as_ref().map(|s| s.display()).unwrap_or_default();
        if let (Some(id), false) = (&self.saved_id, ask_name) {
            if let Some(q) = st.ws.queries.iter_mut().find(|q| &q.id == id) {
                q.sql = self.editor.text.clone();
                q.connection_id = conn;
                q.database = db;
                q.modified = chrono::Local::now();
            }
            self.saved_text = self.editor.text.clone();
            st.save_workspace();
            st.toast("Query saved");
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.saved_id = Some(id.clone());
        let sql = self.editor.text.clone();
        self.saved_text = sql.clone();
        let default_name = if self.name == "Untitled" { format!("Query {}", st.ws.queries.len() + 1) } else { self.name.clone() };
        st.open_dialog(crate::dialogs::prompt::PromptDialog::new(
            "Save Query",
            "Query name:",
            default_name,
            Box::new(move |st: &mut AppState, name: String| {
                let name = if name.is_empty() { "Untitled".into() } else { name };
                st.ws.queries.push(SavedQuery { id, name, connection_id: conn, database: db, sql, modified: chrono::Local::now() });
                st.save_workspace();
                st.toast("Query saved");
            }),
        ));
    }

    fn completion_words(&mut self, st: &mut AppState) -> Vec<String> {
        let (Some(conn), Some(scope)) = (self.conn.clone(), self.scope.clone()) else { return vec![] };
        if let Some(oc) = st.conn_mut(&conn) {
            oc.load_objects(&scope, navdb_core::ObjectKind::Table);
        }
        let mut words = st.cached_tables(&conn, &scope);
        // columns of tables referenced in the text
        let lower = self.editor.text.to_lowercase();
        for t in words.clone() {
            let tl = t.to_lowercase();
            if lower.contains(&tl) {
                if let Some(cols) = self.columns_cache.get(&t) {
                    words.extend(cols.iter().cloned());
                } else if self.columns_loading.is_none() {
                    if let Some(s) = st.session(&conn) {
                        let p = scope.path(t.clone());
                        self.columns_loading = Some((t.clone(), s.run(move |d| Ok(d.columns(&p)?.into_iter().map(|c| c.name).collect()))));
                    }
                }
            }
        }
        if let Some((t, p)) = &mut self.columns_loading {
            if let Some(r) = p.poll() {
                self.columns_cache.insert(t.clone(), r.unwrap_or_default());
                self.columns_loading = None;
            }
        }
        words.extend(st.ws.snippets.iter().map(|s| s.name.clone()));
        words
    }

    fn snippet_menu(&mut self, ui: &mut Ui, st: &mut AppState) {
        let builtin: Vec<(&str, &str)> = vec![
            ("SELECT", "SELECT *\nFROM table_name\nWHERE condition;"),
            ("INSERT", "INSERT INTO table_name (col1, col2)\nVALUES (val1, val2);"),
            ("UPDATE", "UPDATE table_name\nSET col1 = val1\nWHERE condition;"),
            ("DELETE", "DELETE FROM table_name\nWHERE condition;"),
            ("CREATE TABLE", "CREATE TABLE table_name (\n  id INT PRIMARY KEY,\n  name VARCHAR(100)\n);"),
            ("JOIN", "SELECT a.*, b.*\nFROM a\nJOIN b ON b.a_id = a.id;"),
            ("GROUP BY", "SELECT col, COUNT(*)\nFROM table_name\nGROUP BY col\nORDER BY 2 DESC;"),
            ("CTE", "WITH cte AS (\n  SELECT * FROM table_name\n)\nSELECT * FROM cte;"),
        ];
        ui.label(RichText::new("Built-in").weak());
        for (n, body) in builtin {
            if ui.button(n).clicked() {
                self.editor.insert_at_cursor(body);
                ui.close_menu();
            }
        }
        ui.separator();
        ui.label(RichText::new("My snippets").weak());
        let snippets = st.ws.snippets.clone();
        for s in &snippets {
            ui.horizontal(|ui| {
                if ui.button(&s.name).clicked() {
                    self.editor.insert_at_cursor(&s.body);
                    ui.close_menu();
                }
                if ui.small_button("✖").clicked() {
                    st.ws.snippets.retain(|x| x.name != s.name);
                    st.save_workspace();
                }
            });
        }
        ui.separator();
        if ui.button("Save Selection as Snippet…").clicked() {
            let body = {
                let s = self.editor.selected_text();
                if s.is_empty() { self.editor.text.clone() } else { s }
            };
            st.open_dialog(crate::dialogs::prompt::PromptDialog::new(
                "New Snippet",
                "Snippet name:",
                String::new(),
                Box::new(move |st: &mut AppState, name: String| {
                    if !name.is_empty() {
                        st.ws.snippets.push(Snippet { name, body: body.clone() });
                        st.save_workspace();
                    }
                }),
            ));
            ui.close_menu();
        }
    }

    fn poll(&mut self, st: &mut AppState) {
        if let Some((p, started, sql)) = &mut self.running {
            if let Some(r) = p.poll() {
                let elapsed = started.elapsed().as_millis();
                let sql = sql.clone();
                self.running = None;
                let limit = if st.prefs().limit_query_results { st.prefs().query_result_limit } else { usize::MAX };
                self.results.clear();
                self.messages.clear();
                let conn = self.conn.clone().unwrap_or_default();
                match r {
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&conn, self.scope.as_ref(), &sql, false, &m, elapsed);
                        self.messages.push((m, true));
                    }
                    Ok(list) => {
                        for item in list {
                            match item {
                                Ok(mut res) => {
                                    st.record(&conn, self.scope.as_ref(), &res.sql, true, &res.summary(), res.elapsed_ms);
                                    self.messages.push((format!("{}\n> OK — {}", res.sql, res.summary()), false));
                                    if res.is_query {
                                        let truncated = res.rows.len() > limit;
                                        res.rows.truncate(limit);
                                        let n = self.results.len() + 1;
                                        self.results.push(ResultTab { title: format!("Result {}", n), result: res, grid: GridState::default(), truncated });
                                    }
                                }
                                Err(m) => {
                                    st.record(&conn, self.scope.as_ref(), &m, false, &m, 0);
                                    self.messages.push((m, true));
                                }
                            }
                        }
                        self.messages.push((format!("Total time: {} ms", elapsed), false));
                        // DDL changes the navigator content
                        let up = sql.to_ascii_uppercase();
                        if ["CREATE ", "DROP ", "ALTER ", "RENAME "].iter().any(|k| up.contains(k)) {
                            if let Some(s) = &self.scope {
                                st.refresh_scope(&conn, s);
                            }
                            if up.contains("DATABASE") || up.contains("SCHEMA") {
                                st.push(crate::state::Action::RefreshConnection(conn.clone()));
                            }
                        }
                    }
                }
                let has_error = self.messages.iter().any(|(_, e)| *e);
                self.pane = if has_error || self.results.is_empty() { Pane::Message } else { Pane::Result(0) };
            }
        }
        if let Some(p) = &mut self.explaining {
            if let Some(r) = p.poll() {
                self.explaining = None;
                match r {
                    Ok(res) => {
                        self.explain = Some(ResultTab { title: "Explain".into(), result: res, grid: GridState::default(), truncated: false });
                        self.pane = Pane::Explain;
                    }
                    Err(e) => {
                        self.messages = vec![(format!("{:#}", e), true)];
                        self.pane = Pane::Message;
                    }
                }
            }
        }
    }
}

impl Tab for QueryTab {
    fn title(&self) -> String {
        format!("{} {}", icon::QUERY, self.name)
    }

    fn key(&self) -> String {
        match &self.saved_id {
            Some(id) => format!("query:{}", id),
            None => format!("query-new:{}", self.id),
        }
    }

    fn dirty(&self) -> bool {
        self.editor.text != self.saved_text && !self.editor.text.trim().is_empty()
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.poll(st);
        // keyboard shortcuts
        let (run_all, run_cur, save) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::R),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
            )
        });

        ui.horizontal(|ui| {
            let mut conn = self.conn.clone();
            let mut scope = self.scope.clone();
            if ui::scope_picker(ui, &format!("q-{}", self.id), st, &mut conn, &mut scope) {
                if conn != self.conn {
                    self.columns_cache.clear();
                }
                self.conn = conn;
                self.scope = scope;
            }
            ui.separator();
            let busy = self.running.is_some();
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(format!("{} Run", icon::RUN)).strong())).on_hover_text("Run all (⌘R)").clicked() {
                let sel = self.editor.selected_text();
                let sql = if sel.trim().is_empty() { self.editor.text.clone() } else { sel };
                self.run(st, sql);
            }
            if ui.add_enabled(!busy, egui::Button::new("▶ Current")).on_hover_text("Run current statement (⌘↵)").clicked() {
                if let Some(s) = sqlutil::statement_at(&self.editor.text, self.editor.cursor, self.dialect) {
                    self.run(st, s);
                }
            }
            if busy {
                ui.spinner();
                if let Some((_, started, _)) = &self.running {
                    ui.label(format!("{:.1}s", started.elapsed().as_secs_f32()));
                }
                if ui.button(format!("{} Stop", icon::STOP)).on_hover_text("Close the session (query is abandoned)").clicked() {
                    self.running = None;
                    self.session = None;
                    self.session_conn = None;
                    self.messages = vec![("Query stopped: the session was closed.".into(), true)];
                }
            }
            if self.kind.base() != DbKind::Redis && ui.button(format!("{} Explain", icon::EXPLAIN)).clicked() {
                self.explain(st);
            }
            ui.separator();
            ui.menu_button("✨ Format", |ui| {
                if ui.button("Beautify SQL").clicked() {
                    let upper = st.prefs().uppercase_keywords;
                    let sel = self.editor.selected_text();
                    if sel.is_empty() {
                        self.editor.text = sqlutil::beautify(&self.editor.text, upper);
                    } else {
                        self.editor.text = self.editor.text.replacen(&sel, &sqlutil::beautify(&sel, upper), 1);
                    }
                    ui.close_menu();
                }
                if ui.button("Minify SQL").clicked() {
                    self.editor.text = sqlutil::minify(&self.editor.text);
                    ui.close_menu();
                }
                if ui.button("Comment / Uncomment  ⌘/").clicked() {
                    self.editor.toggle_comment_selection();
                    ui.close_menu();
                }
                if ui.button("UPPERCASE").clicked() {
                    self.editor.text = self.editor.text.to_uppercase();
                    ui.close_menu();
                }
                if ui.button("lowercase").clicked() {
                    self.editor.text = self.editor.text.to_lowercase();
                    ui.close_menu();
                }
            });
            ui.menu_button("✂ Snippets", |ui| self.snippet_menu(ui, st));
            ui.separator();
            if ui.button(format!("{} Save", icon::SAVE)).on_hover_text("Save query (⌘S)").clicked() {
                self.save(st, false);
            }
            ui.menu_button("☰", |ui| {
                if ui.button("Save As…").clicked() {
                    self.save(st, true);
                    ui.close_menu();
                }
                if ui.button("Export SQL to File…").clicked() {
                    if let Some(p) = ui::save_file_dialog(&format!("{}.sql", self.name), "sql") {
                        match std::fs::write(&p, &self.editor.text) {
                            Ok(_) => st.toast(format!("Saved {}", p.display())),
                            Err(e) => st.error(e.to_string()),
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("Load SQL File…").clicked() {
                    if let Some(p) = ui::open_file_dialog(&["sql", "txt"]) {
                        if let Ok(s) = std::fs::read_to_string(&p) {
                            self.editor.text = s;
                        }
                    }
                    ui.close_menu();
                }
                if ui.button("Find / Replace  ⌘F").clicked() {
                    self.editor.find_open = true;
                    ui.close_menu();
                }
                if ui.button("History Log").clicked() {
                    cmd::history(st);
                    ui.close_menu();
                }
            });
        });
        if run_all && self.running.is_none() {
            let sel = self.editor.selected_text();
            let sql = if sel.trim().is_empty() { self.editor.text.clone() } else { sel };
            self.run(st, sql);
        }
        if run_cur && self.running.is_none() {
            if let Some(s) = sqlutil::statement_at(&self.editor.text, self.editor.cursor, self.dialect) {
                self.run(st, s);
            }
        }
        if save {
            self.save(st, false);
        }
        ui.separator();

        let tab_id = self.id.clone();
        let sql_engine = self.kind.query_language() == "sql";
        let words = if sql_engine { Vec::new() } else { self.completion_words(st) };
        let snippets: Vec<Item> = st
            .ws
            .snippets
            .iter()
            .map(|s| Item { label: s.name.clone(), insert: s.body.clone(), cursor_back: 0, kind: ItemKind::Snippet, detail: "snippet".into(), score: 50 })
            .collect();
        let (conn, scope, dialect) = (self.conn.clone(), self.scope.clone(), self.dialect);
        egui::TopBottomPanel::top(egui::Id::new(("q-editor", &tab_id)))
            .resizable(true)
            .default_height(self.editor_height)
            .height_range(80.0..=ui.available_height() - 80.0)
            .show_inside(ui, |ui| {
                let id = format!("sql-{}", tab_id);
                if !sql_engine {
                    self.editor.ui(ui, &id, &words);
                    return;
                }
                let mut assist = SqlAssist { st: &mut *st, conn: conn.as_deref(), scope: scope.as_ref(), dialect, snippets: &snippets };
                self.editor.ui_with(ui, &id, &mut assist);
            });

        // result panes
        ui.horizontal(|ui| {
            let has_err = self.messages.iter().any(|(_, e)| *e);
            let msg_label = if has_err { RichText::new("Message ⚠").color(ui.visuals().error_fg_color) } else { RichText::new("Message") };
            if ui.selectable_label(self.pane == Pane::Message, msg_label).clicked() {
                self.pane = Pane::Message;
            }
            for (i, r) in self.results.iter().enumerate() {
                if ui.selectable_label(self.pane == Pane::Result(i), &r.title).clicked() {
                    self.pane = Pane::Result(i);
                }
            }
            if self.explain.is_some() && ui.selectable_label(self.pane == Pane::Explain, "Explain").clicked() {
                self.pane = Pane::Explain;
            }
            if self.explaining.is_some() {
                ui.spinner();
            }
            if let Pane::Result(i) = self.pane {
                if let Some(r) = self.results.get(i) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(format!("{} Chart", icon::CHARTS)).clicked() {
                            if let Some(c) = &self.conn {
                                cmd::charts(st, Some((c.clone(), self.scope.clone().unwrap_or_default(), r.result.sql.clone())));
                            }
                        }
                        if ui.button(format!("{} Export", icon::EXPORT)).clicked() {
                            cmd::export_result(st, r.result.clone(), &self.name, self.dialect);
                        }
                        let note = if r.truncated { " (truncated by result limit)" } else { "" };
                        ui.label(RichText::new(format!("{} rows · {} ms{}", r.result.rows.len(), r.result.elapsed_ms, note)).weak());
                    });
                }
            }
        });
        ui.separator();
        let null_text = st.prefs().show_null_as.clone();
        let font_size = st.prefs().grid_font_size;
        match self.pane {
            Pane::Message => {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    if self.messages.is_empty() {
                        ui.label(RichText::new("Run a query to see messages here.").weak());
                    }
                    for (m, err) in &self.messages {
                        let t = RichText::new(m).monospace();
                        ui.label(if *err { t.color(ui.visuals().error_fg_color) } else { t });
                        ui.add_space(4.0);
                    }
                });
            }
            Pane::Result(i) => {
                if let Some(r) = self.results.get_mut(i) {
                    let opt = GridOptions { null_text: &null_text, font_size, ..Default::default() };
                    let _ = grid::data_grid(ui, &format!("res-{}-{}", self.id, i), &r.result, &mut r.grid, &opt);
                }
            }
            Pane::Explain => {
                if let Some(r) = &mut self.explain {
                    let opt = GridOptions { null_text: &null_text, font_size, max_cell_chars: 400, ..Default::default() };
                    let _ = grid::data_grid(ui, &format!("explain-{}", self.id), &r.result, &mut r.grid, &opt);
                }
            }
        }
    }
}

/// Completion, signature and hover services over the tab's connection.
struct SqlAssist<'a> {
    st: &'a mut AppState,
    conn: Option<&'a str>,
    scope: Option<&'a Scope>,
    dialect: Dialect,
    snippets: &'a [Item],
}

impl SqlAssist<'_> {
    fn with_catalog<R>(&mut self, f: impl FnOnce(&mut dyn complete::Catalog) -> R) -> R {
        match (self.conn, self.scope) {
            (Some(conn), Some(scope)) => f(&mut AppCatalog { st: &mut *self.st, conn, scope }),
            _ => f(&mut NoCatalog),
        }
    }
}

impl Assist for SqlAssist<'_> {
    fn complete(&mut self, text: &str, cursor: usize, explicit: bool) -> Option<complete::Completion> {
        let (dialect, snippets) = (self.dialect, self.snippets);
        self.with_catalog(|cat| complete::complete(text, cursor, dialect, cat, explicit, snippets))
    }

    fn signature(&mut self, text: &str, cursor: usize) -> Option<complete::SignatureHelp> {
        complete::signature_help(text, cursor, self.dialect)
    }

    fn hover(&mut self, text: &str, at: usize) -> Option<complete::Hover> {
        let dialect = self.dialect;
        self.with_catalog(|cat| complete::hover(text, at, dialect, cat))
    }
}
