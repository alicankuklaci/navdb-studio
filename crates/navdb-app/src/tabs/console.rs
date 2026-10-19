//! Console: a terminal-like REPL on a dedicated connection with mysql-CLI
//! style table output.

use std::time::Instant;

use egui::text::{CCursor, CCursorRange};
use egui::{Color32, RichText};
use navdb_core::{DbKind, Pending, QueryResult, Scope, Session};

use super::Tab;
use crate::state::AppState;
use crate::ui::style::icon;

const MAX_ROWS: usize = 1000;
const MAX_CELL: usize = 200;
const MAX_SCROLLBACK: usize = 2000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Input,
    Output,
    Error,
    Info,
}

struct Entry {
    kind: LineKind,
    text: String,
}

pub struct ConsoleTab {
    id: String,
    conn: String,
    conn_name: String,
    kind: DbKind,
    host: String,
    port: u16,
    session: Option<Session>,
    scope: Option<Scope>,
    entries: Vec<Entry>,
    input: String,
    buffer: Vec<String>,
    history: Vec<String>,
    hist_pos: Option<usize>,
    running: Option<(Pending<Vec<QueryResult>>, Instant, String, Option<Scope>)>,
    focus: bool,
    move_cursor_end: bool,
    scroll_bottom: bool,
}

impl ConsoleTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        let session = st.session(conn).map(|s| s.fork());
        let scope = cfg.as_ref().map(|c| c.default_database()).filter(|d| !d.is_empty()).map(|d| Scope::new(d, None));
        let mut me = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            kind,
            host: cfg.as_ref().map(|c| c.host.clone()).unwrap_or_default(),
            port: cfg.as_ref().map(|c| c.port).unwrap_or_default(),
            session,
            scope,
            entries: Vec::new(),
            input: String::new(),
            buffer: Vec::new(),
            history: Vec::new(),
            hist_pos: None,
            running: None,
            focus: true,
            move_cursor_end: false,
            scroll_bottom: true,
        };
        let banner = match kind.base() {
            DbKind::Redis => "Redis console. One command per line. Type `clear` to clear the screen, `SELECT n` to switch database.".to_string(),
            DbKind::PostgreSql => "PostgreSQL console. End statements with `;`. `\\c db` switches database, `clear` clears the screen.".to_string(),
            DbKind::Sqlite => "SQLite console. End statements with `;`. `clear` clears the screen.".to_string(),
            DbKind::MySql | DbKind::MariaDb => "MySQL console. End statements with `;`. `USE db` switches database, `clear` clears the screen.".to_string(),
            DbKind::MongoDb => "MongoDB shell. One command per line, e.g. `db.users.find({age: {$gt: 30}})`. `use db` switches database, `show collections` lists collections.".to_string(),
            _ => format!("{} console. End statements with `;`. `USE db` switches database, `clear` clears the screen.", kind.label()),
        };
        me.info(format!("Connected to {} ({})", me.conn_name, kind.label()));
        me.info(banner);
        if let Some(s) = me.session.clone() {
            let p = s.run(|d| {
                let v = d.server_version()?;
                Ok(vec![QueryResult { sql: format!("Server version: {}", v), ..Default::default() }])
            });
            me.running = Some((p, Instant::now(), "\u{0}version".into(), None));
        } else {
            me.error("Connection not found".into());
        }
        me
    }

    fn push(&mut self, kind: LineKind, text: String) {
        self.entries.push(Entry { kind, text });
        if self.entries.len() > MAX_SCROLLBACK {
            let n = self.entries.len() - MAX_SCROLLBACK;
            self.entries.drain(0..n);
        }
        self.scroll_bottom = true;
    }

    fn info(&mut self, t: String) {
        self.push(LineKind::Info, t);
    }

    fn error(&mut self, t: String) {
        self.push(LineKind::Error, t);
    }

    /// Statements are buffered until `;` (SQL) instead of running per line.
    fn is_sql(&self) -> bool {
        self.kind.query_language() == "sql"
    }

    fn prompt(&self, continuation: bool) -> String {
        let db = self.scope.as_ref().map(|s| s.db.clone()).unwrap_or_default();
        match self.kind.base() {
            DbKind::MySql | DbKind::MariaDb => {
                if continuation {
                    "    -> ".into()
                } else {
                    let name = if self.kind.base() == DbKind::MariaDb { "MariaDB" } else { "mysql" };
                    format!("{} [{}]> ", name, if db.is_empty() { "(none)" } else { &db })
                }
            }
            DbKind::PostgreSql => format!("{}{} ", if db.is_empty() { "postgres" } else { &db }, if continuation { "->" } else { "=>" }),
            DbKind::Sqlite => {
                if continuation {
                    "   ...> ".into()
                } else {
                    "sqlite> ".into()
                }
            }
            DbKind::Redis => {
                let n = db.trim_start_matches("db");
                if n.is_empty() || n == "0" {
                    format!("{}:{}> ", self.host, self.port)
                } else {
                    format!("{}:{}[{}]> ", self.host, self.port, n)
                }
            }
            DbKind::MongoDb => format!("{}> ", if db.is_empty() { "test" } else { &db }),
            _ if continuation => "    -> ".into(),
            _ => format!("{} [{}]> ", self.kind.label().to_ascii_lowercase(), if db.is_empty() { "(none)" } else { &db }),
        }
    }

    /// Called when Enter is pressed in the input line.
    fn submit(&mut self) {
        let line = std::mem::take(&mut self.input);
        let continuation = !self.buffer.is_empty();
        self.push(LineKind::Input, format!("{}{}", self.prompt(continuation), line));
        self.hist_pos = None;
        let trimmed = line.trim();
        if trimmed.is_empty() && !continuation {
            return;
        }
        // local commands (only at the start of a statement)
        if !continuation {
            let lower = trimmed.trim_end_matches(';').trim().to_ascii_lowercase();
            if lower == "clear" || lower == "cls" || lower == "\\! clear" {
                self.remember(trimmed);
                self.entries.clear();
                return;
            }
            if lower == "help" || lower == "\\?" || lower == "\\h" {
                self.remember(trimmed);
                self.info(
                    "Commands: clear | help | USE db (MySQL) | \\c db (PostgreSQL) | SELECT n (Redis)\n\
                     Up/Down browse the command history. SQL statements end with ';'."
                        .into(),
                );
                return;
            }
            if self.is_sql() {
                let word = |p: &str| lower.strip_prefix(p).map(|r| r.trim().to_string());
                let target = if matches!(self.kind.base(), DbKind::MySql | DbKind::MariaDb) {
                    word("use ").or_else(|| word("\\u "))
                } else if self.kind.base() == DbKind::PostgreSql {
                    word("\\c ").or_else(|| word("\\connect "))
                } else if !matches!(self.kind.base(), DbKind::Sqlite | DbKind::Oracle | DbKind::Trino) {
                    word("use ")
                } else {
                    None
                };
                if let Some(_t) = target {
                    // keep original case of the database name
                    let orig = trimmed.trim_end_matches(';').trim();
                    let name = orig.split_once(char::is_whitespace).map(|(_, r)| r.trim()).unwrap_or("");
                    let name = name.trim_matches('`').trim_matches('"').to_string();
                    self.remember(trimmed);
                    if name.is_empty() {
                        self.error("Database name expected".into());
                    } else {
                        self.switch_db(name);
                    }
                    return;
                }
            }
        }
        if !self.is_sql() {
            self.remember(trimmed);
            let lower = trimmed.to_ascii_lowercase();
            if self.kind.base() == DbKind::MongoDb {
                if let Some(name) = trimmed.strip_prefix("use ") {
                    self.switch_db(name.trim().trim_end_matches(';').to_string());
                    return;
                }
            }
            let new_scope = if let Some(n) = lower.strip_prefix("select ") {
                n.trim().parse::<u32>().ok().map(|n| Scope::new(format!("db{}", n), None))
            } else {
                None
            };
            self.execute(trimmed.to_string(), new_scope);
            return;
        }
        self.buffer.push(line.clone());
        let full = self.buffer.join("\n");
        let ends = full.trim_end().ends_with(';') || full.trim_end().ends_with("\\g") || full.trim_end().ends_with("\\G");
        if ends {
            self.buffer.clear();
            let mut sql = full.trim().to_string();
            if sql.ends_with("\\g") || sql.ends_with("\\G") {
                sql.truncate(sql.len() - 2);
            }
            self.remember(&sql);
            self.execute(sql, None);
        }
    }

    fn remember(&mut self, cmd: &str) {
        let c = cmd.trim().to_string();
        if c.is_empty() {
            return;
        }
        if self.history.last() != Some(&c) {
            self.history.push(c);
        }
        if self.history.len() > 500 {
            self.history.remove(0);
        }
    }

    fn switch_db(&mut self, db: String) {
        let Some(s) = self.session.clone() else { return };
        let scope = Scope::new(db.clone(), None);
        let sc = scope.clone();
        let probe = self.kind.ping_query();
        let p = s.run(move |d| d.execute(Some(&sc), probe));
        self.running = Some((p, Instant::now(), format!("\u{0}use {}", db), Some(scope)));
    }

    fn execute(&mut self, sql: String, new_scope: Option<Scope>) {
        let Some(s) = self.session.clone() else {
            self.error("Not connected".into());
            return;
        };
        let scope = self.scope.clone();
        let q = sql.clone();
        let p = s.run(move |d| d.execute(scope.as_ref(), &q));
        self.running = Some((p, Instant::now(), sql, new_scope));
    }

    fn poll(&mut self, st: &mut AppState) {
        let Some((p, started, sql, new_scope)) = &mut self.running else { return };
        let Some(res) = p.poll() else { return };
        let elapsed = started.elapsed();
        let sql = std::mem::take(sql);
        let new_scope = new_scope.take();
        self.running = None;
        if let Some(meta) = sql.strip_prefix('\u{0}') {
            // internal commands: version banner / database switch
            match res {
                Ok(r) => {
                    if meta == "version" {
                        if let Some(r) = r.first() {
                            self.info(r.sql.clone());
                        }
                    } else if let Some(sc) = new_scope {
                        let msg = if self.kind.base() == DbKind::PostgreSql {
                            format!("You are now connected to database \"{}\".", sc.db)
                        } else {
                            "Database changed".to_string()
                        };
                        self.scope = Some(sc);
                        self.push(LineKind::Output, msg);
                    }
                }
                Err(e) => self.error(format!("ERROR: {:#}", e)),
            }
            return;
        }
        let secs = elapsed.as_secs_f64();
        match res {
            Ok(results) => {
                st.record(&self.conn, self.scope.as_ref(), &sql, true, &format!("{} statement(s)", results.len()), elapsed.as_millis());
                if let Some(sc) = new_scope {
                    self.scope = Some(sc);
                }
                for r in &results {
                    let text = self.format_result(r, secs / results.len().max(1) as f64);
                    self.push(LineKind::Output, text);
                }
            }
            Err(e) => {
                let m = format!("{:#}", e);
                st.record(&self.conn, self.scope.as_ref(), &sql, false, &m, elapsed.as_millis());
                self.error(format!("ERROR: {}", m));
            }
        }
    }

    fn format_result(&self, r: &QueryResult, secs: f64) -> String {
        if self.kind.base() == DbKind::Redis {
            return format_redis(r);
        }
        if !r.is_query {
            return format!("Query OK, {} row{} affected ({:.3} sec)\n", r.affected_rows, if r.affected_rows == 1 { "" } else { "s" }, secs);
        }
        if r.rows.is_empty() {
            return format!("Empty set ({:.3} sec)\n", secs);
        }
        let mut out = ascii_table(r, MAX_ROWS);
        let n = r.rows.len();
        if n > MAX_ROWS {
            out.push_str(&format!("({} rows, showing the first {})\n", n, MAX_ROWS));
        }
        out.push_str(&format!("{} row{} in set ({:.3} sec)\n", n, if n == 1 { "" } else { "s" }, secs));
        out
    }

    fn all_text(&self) -> String {
        self.entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    fn history_nav(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        let pos = match (self.hist_pos, up) {
            (None, true) => Some(self.history.len() - 1),
            (None, false) => None,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) => {
                if p + 1 >= self.history.len() {
                    None
                } else {
                    Some(p + 1)
                }
            }
        };
        self.hist_pos = pos;
        self.input = pos.map(|p| self.history[p].clone()).unwrap_or_default();
        self.move_cursor_end = true;
    }
}

fn cell_text(v: &navdb_core::Value) -> String {
    let s = if v.is_null() { "NULL".to_string() } else { v.display() };
    let s: String = s.chars().map(|c| match c {
        '\n' => '↵',
        '\r' => ' ',
        '\t' => ' ',
        c => c,
    }).collect();
    if s.chars().count() > MAX_CELL {
        format!("{}…", s.chars().take(MAX_CELL).collect::<String>())
    } else {
        s
    }
}

/// mysql-CLI style table.
fn ascii_table(r: &QueryResult, max_rows: usize) -> String {
    let rows: Vec<Vec<String>> = r.rows.iter().take(max_rows).map(|row| row.iter().map(cell_text).collect()).collect();
    let mut widths: Vec<usize> = r.columns.iter().map(|c| c.name.chars().count()).collect();
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let numeric: Vec<bool> = (0..r.columns.len())
        .map(|i| r.rows.iter().take(max_rows).filter(|row| !row[i].is_null()).all(|row| row[i].is_numeric()) && !r.rows.is_empty())
        .collect();
    let sep = {
        let mut s = String::from("+");
        for w in &widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s.push('\n');
        s
    };
    let pad = |s: &str, w: usize, right: bool| {
        let n = s.chars().count();
        if right {
            format!("{}{}", " ".repeat(w - n), s)
        } else {
            format!("{}{}", s, " ".repeat(w - n))
        }
    };
    let mut out = sep.clone();
    out.push('|');
    for (i, c) in r.columns.iter().enumerate() {
        out.push(' ');
        out.push_str(&pad(&c.name, widths[i], false));
        out.push_str(" |");
    }
    out.push('\n');
    out.push_str(&sep);
    for row in &rows {
        out.push('|');
        for (i, w) in widths.iter().enumerate() {
            let c = row.get(i).map(|s| s.as_str()).unwrap_or("");
            out.push(' ');
            out.push_str(&pad(c, *w, numeric.get(i).copied().unwrap_or(false)));
            out.push_str(" |");
        }
        out.push('\n');
    }
    out.push_str(&sep);
    out
}

/// redis-cli style reply formatting.
fn format_redis(r: &QueryResult) -> String {
    let quote = |v: &navdb_core::Value| -> String {
        match v {
            navdb_core::Value::Null => "(nil)".into(),
            navdb_core::Value::Int(i) => format!("(integer) {}", i),
            navdb_core::Value::UInt(i) => format!("(integer) {}", i),
            other => {
                let s = other.display();
                if s == "OK" || s == "PONG" || s == "QUEUED" {
                    s
                } else {
                    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
                }
            }
        }
    };
    let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
    if names.len() == 1 {
        return r.rows.first().and_then(|row| row.first()).map(quote).unwrap_or_else(|| "(nil)".into());
    }
    if r.rows.is_empty() {
        return "(empty array)".into();
    }
    let total = r.rows.len();
    let mut out = String::new();
    if names.first().map(|s| s.as_str()) == Some("Field") {
        for (i, row) in r.rows.iter().take(MAX_ROWS).enumerate() {
            out.push_str(&format!("{}) {}\n{}) {}\n", i * 2 + 1, quote(&row[0]), i * 2 + 2, quote(&row[1])));
        }
    } else {
        let w = total.to_string().len();
        for (i, row) in r.rows.iter().take(MAX_ROWS).enumerate() {
            let v = row.get(1).map(quote).unwrap_or_default();
            out.push_str(&format!("{:>w$}) {}\n", i + 1, v, w = w));
        }
    }
    if total > MAX_ROWS {
        out.push_str(&format!("... ({} items, showing the first {})\n", total, MAX_ROWS));
    }
    out.trim_end().to_string()
}

impl Tab for ConsoleTab {
    fn title(&self) -> String {
        format!("{} Console - {}", icon::CONSOLE, self.conn_name)
    }

    fn key(&self) -> String {
        format!("console:{}", self.conn)
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        self.poll(st);
        let font = egui::FontId::monospace(st.prefs().editor_font_size.max(9.0));
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::CONSOLE, self.conn_name)).strong());
            if let Some(s) = &self.scope {
                ui.label(RichText::new(format!("· {}", s.display())).weak());
            }
            ui.separator();
            if ui.button("📋 Copy All").clicked() {
                ui.ctx().copy_text(self.all_text());
                st.toast("Console output copied");
            }
            if ui.button("🗑 Clear").clicked() {
                self.entries.clear();
                self.focus = true;
            }
            if self.running.is_some() {
                ui.spinner();
                ui.label(RichText::new("running…").weak());
            }
        });
        ui.separator();

        let dark = ui.visuals().dark_mode;
        let bg = if dark { Color32::from_rgb(24, 24, 27) } else { Color32::from_rgb(250, 250, 248) };
        let out_color = if dark { Color32::from_rgb(220, 220, 220) } else { Color32::from_rgb(30, 30, 30) };
        let in_color = if dark { Color32::from_rgb(120, 200, 255) } else { Color32::from_rgb(20, 90, 170) };
        let info_color = if dark { Color32::from_rgb(140, 140, 140) } else { Color32::from_rgb(120, 120, 120) };
        let err_color = ui.visuals().error_fg_color;

        egui::Frame::new().fill(bg).inner_margin(egui::Margin::same(8)).corner_radius(4.0).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let input_h = font.size + 14.0;
            let scroll_h = (ui.available_height() - input_h).max(60.0);
            let sa = egui::ScrollArea::both().id_salt(format!("console-{}", self.id)).auto_shrink([false, false]).max_height(scroll_h).stick_to_bottom(true);
            sa.show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for e in &self.entries {
                    let color = match e.kind {
                        LineKind::Input => in_color,
                        LineKind::Output => out_color,
                        LineKind::Error => err_color,
                        LineKind::Info => info_color,
                    };
                    ui.add(egui::Label::new(RichText::new(&e.text).font(font.clone()).color(color)).extend().selectable(true));
                }
                if self.scroll_bottom {
                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                }
            });
            self.scroll_bottom = false;
            ui.horizontal(|ui| {
                let prompt = self.prompt(!self.buffer.is_empty());
                ui.label(RichText::new(prompt).font(font.clone()).color(in_color));
                let te_id = egui::Id::new(("console-input", &self.id));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.input)
                        .id(te_id)
                        .font(font.clone())
                        .text_color(out_color)
                        .frame(false)
                        .desired_width(f32::INFINITY)
                        .lock_focus(true)
                        .hint_text(if self.running.is_some() { "running…" } else { "" }),
                );
                if self.focus {
                    resp.request_focus();
                    self.focus = false;
                }
                let (enter, up, down, ctrl_l) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.modifiers.command && i.key_pressed(egui::Key::L),
                    )
                });
                if resp.has_focus() || resp.lost_focus() {
                    if up && resp.has_focus() {
                        self.history_nav(true);
                    }
                    if down && resp.has_focus() {
                        self.history_nav(false);
                    }
                    if ctrl_l {
                        self.entries.clear();
                    }
                }
                if enter && resp.lost_focus() {
                    if self.running.is_none() {
                        self.submit();
                    }
                    self.focus = true;
                }
                if self.move_cursor_end {
                    self.move_cursor_end = false;
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), te_id) {
                        let c = CCursor::new(self.input.chars().count());
                        state.cursor.set_char_range(Some(CCursorRange::one(c)));
                        state.store(ui.ctx(), te_id);
                    }
                }
            });
        });
        // typing anywhere in the console focuses the input line
        let typed = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_))));
        if typed && ui.memory(|m| m.focused().is_none()) && ui.ui_contains_pointer() {
            self.focus = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use navdb_core::{ColumnMeta, Value};

    #[test]
    fn table_layout() {
        let r = QueryResult {
            columns: vec![ColumnMeta::new("id"), ColumnMeta::new("name")],
            rows: vec![vec![Value::Int(1), Value::Text("alice".into())], vec![Value::Int(22), Value::Null]],
            is_query: true,
            ..Default::default()
        };
        let t = ascii_table(&r, 10);
        assert_eq!(t, "+----+-------+\n| id | name  |\n+----+-------+\n|  1 | alice |\n| 22 | NULL  |\n+----+-------+\n");
    }
}
