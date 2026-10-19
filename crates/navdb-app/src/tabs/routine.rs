//! Object Designer for functions, procedures, triggers, events, sequences,
//! materialized views and indexes: DDL editor with per-dialect templates,
//! save (drop/create or CREATE OR REPLACE) and an "Execute" runner for
//! functions / procedures.
//!
//! This module also hosts small helpers shared by the other designers
//! (statement execution, a light SQL tokenizer and a result pane).

use std::time::Instant;

use anyhow::Context;
use egui::{RichText, Ui};
use navdb_core::driver::Driver;
use navdb_core::{Dialect, ObjPath, ObjectKind, Pending, QueryResult, Result, Scope, Session};

use super::Tab;
use crate::state::{AppState, Loadable};
use crate::ui::grid::{self, GridOptions, GridState};
use crate::ui::sql_editor::SqlEditor;
use crate::ui::{self, style::icon};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Delimiter used to send MySQL compound statements (BEGIN ... END) through
/// the statement splitter in one piece.
const MYSQL_DELIM: &str = "$$navdb$$";

/// Execute one statement (or, for PostgreSQL/SQLite, a script) in `scope`.
/// MySQL statements containing `;` (routine bodies) are wrapped in a
/// `DELIMITER` directive so the driver does not split them.
pub(crate) fn exec_statement(d: &mut dyn Driver, dialect: Dialect, scope: &Scope, stmt: &str) -> Result<Vec<QueryResult>> {
    let s = stmt.trim();
    if dialect == Dialect::MySql {
        let has_directive = s.lines().any(|l| l.trim_start().to_ascii_uppercase().starts_with("DELIMITER "));
        if has_directive {
            return d.execute(Some(scope), s);
        }
        let s = s.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
        if s.contains(';') {
            return d.execute(Some(scope), &format!("DELIMITER {}\n{}\n{}", MYSQL_DELIM, s, MYSQL_DELIM));
        }
        return d.execute(Some(scope), s);
    }
    d.execute(Some(scope), s)
}

/// Statements to apply an object change, with an optional restore script
/// used when the object was dropped but the new definition failed.
#[derive(Clone, Default)]
pub(crate) struct SavePlan {
    pub pre: Vec<String>,
    pub main: Vec<String>,
    pub restore: Option<String>,
}

impl SavePlan {
    pub fn script(&self) -> String {
        let mut all: Vec<String> = self.pre.clone();
        all.extend(self.main.iter().cloned());
        all.iter().map(|s| format!("{};", s.trim().trim_end_matches(';').trim_end())).collect::<Vec<_>>().join("\n\n")
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.main.is_empty()
    }
}

/// Run a [`SavePlan`] on the worker thread.
pub(crate) fn run_plan(d: &mut dyn Driver, dialect: Dialect, scope: &Scope, plan: &SavePlan) -> Result<()> {
    for s in &plan.pre {
        exec_statement(d, dialect, scope, s).with_context(|| format!("while executing: {}", s))?;
    }
    for s in &plan.main {
        if let Err(e) = exec_statement(d, dialect, scope, s) {
            if !plan.pre.is_empty() {
                if let Some(r) = &plan.restore {
                    if exec_statement(d, dialect, scope, r).is_ok() {
                        return Err(e.context("the new definition failed; the original definition was restored"));
                    }
                    return Err(e.context("the new definition failed and the original could NOT be restored"));
                }
            }
            return Err(e);
        }
    }
    Ok(())
}

/// PostgreSQL routine names in the navigator carry their identity signature
/// (`f(integer, text)`); this returns the bare name.
pub(crate) fn strip_signature(name: &str) -> &str {
    match name.find('(') {
        Some(p) => name[..p].trim(),
        None => name,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TokKind {
    Word,
    /// Quoted identifier ("x" or `x`).
    Quoted,
    /// String literal or dollar-quoted body.
    Str,
    Punct,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok {
    pub start: usize,
    pub end: usize,
    pub kind: TokKind,
}

/// Very small SQL tokenizer: words, quoted identifiers, strings, single-char
/// punctuation. Comments and whitespace are skipped.
pub(crate) fn tokenize(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i < n && !(b[i] == b'*' && i + 1 < n && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        let start = i;
        if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < n {
                if b[i] == b'\\' && c == b'\'' {
                    i += 2;
                    continue;
                }
                if b[i] == c {
                    if i + 1 < n && b[i + 1] == c {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(n);
            out.push(Tok { start, end, kind: if c == b'\'' { TokKind::Str } else { TokKind::Quoted } });
            i = end;
            continue;
        }
        if c == b'$' {
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j < n && b[j] == b'$' {
                let tag = &s[i..=j];
                let body_start = j + 1;
                let end = match s[body_start..].find(tag) {
                    Some(p) => body_start + p + tag.len(),
                    None => n,
                };
                out.push(Tok { start, end, kind: TokKind::Str });
                i = end;
                continue;
            }
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            out.push(Tok { start, end: i, kind: TokKind::Word });
            continue;
        }
        i += 1;
        out.push(Tok { start, end: i, kind: TokKind::Punct });
    }
    out
}

pub(crate) fn tok_text<'a>(s: &'a str, t: &Tok) -> &'a str {
    &s[t.start..t.end]
}

fn is_word(s: &str, t: &Tok, w: &str) -> bool {
    t.kind == TokKind::Word && tok_text(s, t).eq_ignore_ascii_case(w)
}

fn is_punct(s: &str, t: &Tok, p: &str) -> bool {
    t.kind == TokKind::Punct && tok_text(s, t) == p
}

/// Remove identifier quotes.
pub(crate) fn unquote(t: &str) -> String {
    let t = t.trim();
    if t.len() >= 2 && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('`') && t.ends_with('`'))) {
        let q = &t[..1];
        t[1..t.len() - 1].replace(&format!("{}{}", q, q), q)
    } else {
        t.to_string()
    }
}

/// Index of the token holding the object-kind keyword of the first
/// `CREATE ... <KIND>` statement in `toks`.
fn find_create_kind(s: &str, toks: &[Tok], kind: ObjectKind) -> Option<usize> {
    let kw: Vec<&str> = kind.sql_keyword().split(' ').collect();
    for (ci, t) in toks.iter().enumerate() {
        if !is_word(s, t, "CREATE") {
            continue;
        }
        let mut j = ci + 1;
        while j < toks.len() && j < ci + 16 {
            let tj = &toks[j];
            if is_punct(s, tj, "(") || is_punct(s, tj, ";") {
                break;
            }
            if kw.iter().enumerate().all(|(k, w)| toks.get(j + k).map(|x| is_word(s, x, w)).unwrap_or(false)) {
                return Some(j + kw.len() - 1);
            }
            j += 1;
        }
    }
    None
}

/// Name tokens (start, end-exclusive) of a possibly qualified name starting at `i`.
fn qualified_name_at(s: &str, toks: &[Tok], mut i: usize) -> Option<(usize, usize)> {
    // skip IF NOT EXISTS / CONCURRENTLY
    while i < toks.len() && (is_word(s, &toks[i], "IF") || is_word(s, &toks[i], "NOT") || is_word(s, &toks[i], "EXISTS") || is_word(s, &toks[i], "CONCURRENTLY")) {
        i += 1;
    }
    let first = i;
    let t = toks.get(i)?;
    if !matches!(t.kind, TokKind::Word | TokKind::Quoted) {
        return None;
    }
    i += 1;
    while i + 1 < toks.len() && is_punct(s, &toks[i], ".") && matches!(toks[i + 1].kind, TokKind::Word | TokKind::Quoted) {
        i += 2;
    }
    Some((first, i))
}

/// Object name (last part, unquoted) of the first `CREATE <kind>` statement.
pub(crate) fn parse_object_name(s: &str, kind: ObjectKind) -> Option<String> {
    let toks = tokenize(s);
    let k = find_create_kind(s, &toks, kind)?;
    let (_, end) = qualified_name_at(s, &toks, k + 1)?;
    Some(unquote(tok_text(s, &toks[end - 1])))
}

/// Table text after `ON` in a `CREATE TRIGGER` / `CREATE INDEX` statement.
pub(crate) fn parse_on_table(s: &str, kind: ObjectKind) -> Option<String> {
    let toks = tokenize(s);
    let k = find_create_kind(s, &toks, kind)?;
    let on = (k + 1..toks.len()).find(|&i| is_word(s, &toks[i], "ON"))?;
    let mut i = on + 1;
    if toks.get(i).map(|t| is_word(s, t, "ONLY")).unwrap_or(false) {
        i += 1;
    }
    let (a, b) = qualified_name_at(s, &toks, i)?;
    Some(s[toks[a].start..toks[b - 1].end].to_string())
}

/// Text after the first top-level `AS` following the `VIEW` keyword.
pub(crate) fn view_body(s: &str) -> Option<String> {
    let toks = tokenize(s);
    let mut seen_view = false;
    let mut depth = 0i32;
    for t in &toks {
        if is_punct(s, t, "(") {
            depth += 1;
        } else if is_punct(s, t, ")") {
            depth -= 1;
        } else if !seen_view && is_word(s, t, "VIEW") {
            seen_view = true;
        } else if seen_view && depth == 0 && is_word(s, t, "AS") {
            let body = s[t.end..].trim();
            return Some(body.trim_end_matches(|c: char| c == ';' || c.is_whitespace()).to_string());
        }
    }
    None
}

/// `CREATE FUNCTION` → `CREATE OR REPLACE FUNCTION` (PostgreSQL).
fn pg_or_replace(s: &str) -> String {
    let toks = tokenize(s);
    for (i, t) in toks.iter().enumerate() {
        if is_word(s, t, "CREATE") {
            if let Some(n) = toks.get(i + 1) {
                if is_word(s, n, "FUNCTION") || is_word(s, n, "PROCEDURE") {
                    return format!("{} OR REPLACE{}", &s[..t.end], &s[t.end..]);
                }
            }
            break;
        }
    }
    s.to_string()
}

/// One routine parameter of the Execute panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Param {
    pub mode: String,
    pub name: String,
    pub ty: String,
    pub value: String,
    pub null: bool,
    /// Value is an SQL expression (not quoted).
    pub raw: bool,
}

impl Param {
    fn is_out(&self) -> bool {
        self.mode == "OUT"
    }
    fn is_inout(&self) -> bool {
        self.mode == "INOUT"
    }

    fn literal(&self, d: Dialect) -> String {
        if self.null {
            return "NULL".into();
        }
        let v = self.value.trim();
        if self.raw || v.eq_ignore_ascii_case("null") || (!v.is_empty() && v.parse::<f64>().is_ok()) {
            if v.is_empty() {
                "NULL".into()
            } else {
                v.to_string()
            }
        } else {
            d.string_literal(&self.value)
        }
    }
}

/// Parse the parameter list of a `CREATE FUNCTION/PROCEDURE` statement.
pub(crate) fn parse_params(s: &str, kind: ObjectKind) -> Vec<Param> {
    let toks = tokenize(s);
    let Some(k) = find_create_kind(s, &toks, kind) else { return vec![] };
    let Some((_, end)) = qualified_name_at(s, &toks, k + 1) else { return vec![] };
    if !toks.get(end).map(|t| is_punct(s, t, "(")).unwrap_or(false) {
        return vec![];
    }
    let mut depth = 0i32;
    let mut groups: Vec<Vec<Tok>> = vec![vec![]];
    for t in &toks[end..] {
        if is_punct(s, t, "(") {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if is_punct(s, t, ")") {
            depth -= 1;
            if depth == 0 {
                break;
            }
        } else if depth == 1 && is_punct(s, t, ",") {
            groups.push(vec![]);
            continue;
        }
        if let Some(g) = groups.last_mut() {
            g.push(*t);
        }
    }
    let mut out = Vec::new();
    for (n, g) in groups.into_iter().enumerate() {
        if g.is_empty() {
            continue;
        }
        let mut i = 0;
        let mut mode = "IN".to_string();
        if let Some(t) = g.first() {
            let w = tok_text(s, t).to_ascii_uppercase();
            if t.kind == TokKind::Word && matches!(w.as_str(), "IN" | "OUT" | "INOUT" | "VARIADIC") {
                mode = w;
                i = 1;
            }
        }
        // stop at DEFAULT / '='
        let stop = (i..g.len()).find(|&j| is_word(s, &g[j], "DEFAULT") || is_punct(s, &g[j], "=")).unwrap_or(g.len());
        let rest = &g[i..stop];
        if rest.is_empty() {
            continue;
        }
        let (name, ty) = if rest.len() == 1 {
            (format!("${}", n + 1), tok_text(s, &rest[0]).to_string())
        } else {
            let ty = s[rest[1].start..rest[rest.len() - 1].end].to_string();
            (unquote(tok_text(s, &rest[0])), ty)
        };
        out.push(Param { mode, name, ty, ..Default::default() });
    }
    out
}

/// Result sets + messages panel used by the designers.
#[derive(Default)]
pub(crate) struct ResultsPane {
    pub sets: Vec<(String, QueryResult, GridState)>,
    pub messages: Vec<(String, bool)>,
    /// None = messages page.
    pub active: Option<usize>,
}

impl ResultsPane {
    pub fn clear(&mut self) {
        self.sets.clear();
        self.messages.clear();
        self.active = None;
    }

    pub fn set_results(&mut self, title: &str, results: Vec<QueryResult>) {
        self.sets.clear();
        let n = results.iter().filter(|r| r.is_query).count();
        let mut i = 0;
        for r in results {
            if r.is_query {
                i += 1;
                let t = if n > 1 { format!("{} {}", title, i) } else { title.to_string() };
                self.sets.push((t, r, GridState::default()));
            } else {
                self.messages.push((format!("OK, {} row(s) affected · {} ms", r.affected_rows, r.elapsed_ms), false));
            }
        }
        self.active = if self.sets.is_empty() { None } else { Some(0) };
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty() && self.messages.is_empty()
    }

    pub fn show(&mut self, ui: &mut Ui, st: &AppState, id: &str) {
        ui.horizontal(|ui| {
            let has_err = self.messages.iter().any(|(_, e)| *e);
            let label = if has_err { RichText::new("Message ⚠").color(ui.visuals().error_fg_color) } else { RichText::new("Message") };
            if ui.selectable_label(self.active.is_none(), label).clicked() {
                self.active = None;
            }
            for (i, (t, _, _)) in self.sets.iter().enumerate() {
                if ui.selectable_label(self.active == Some(i), t).clicked() {
                    self.active = Some(i);
                }
            }
            if let Some(i) = self.active {
                if let Some((_, r, _)) = self.sets.get(i) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{} rows · {} ms", r.rows.len(), r.elapsed_ms)).weak());
                    });
                }
            }
        });
        ui.separator();
        let null_text = st.prefs().show_null_as.clone();
        let font_size = st.prefs().grid_font_size;
        match self.active.and_then(|i| self.sets.get_mut(i).map(|s| (i, s))) {
            Some((i, (_, r, g))) => {
                let opt = GridOptions { null_text: &null_text, font_size, max_cell_chars: 400, ..Default::default() };
                let _ = grid::data_grid(ui, &format!("{}-res-{}", id, i), &*r, g, &opt);
            }
            None => {
                egui::ScrollArea::vertical().id_salt(format!("{}-msgs", id)).auto_shrink([false, false]).show(ui, |ui| {
                    if self.messages.is_empty() {
                        ui.label(RichText::new("No messages.").weak());
                    }
                    for (m, err) in &self.messages {
                        let t = RichText::new(m).monospace();
                        ui.label(if *err { t.color(ui.visuals().error_fg_color) } else { t });
                    }
                });
            }
        }
    }
}

/// Inline status line (success green / error red).
pub(crate) fn status_line(ui: &mut Ui, msg: &Option<(String, bool)>) {
    if let Some((m, err)) = msg {
        if *err {
            ui::error_text(ui, m);
        } else {
            ui.colored_label(egui::Color32::from_rgb(60, 160, 80), m);
        }
    }
}

/// New SqlEditor configured from the user's preferences.
pub(crate) fn new_editor(st: &AppState, text: impl Into<String>) -> SqlEditor {
    let p = st.prefs();
    let mut e = SqlEditor::new(text);
    e.font_size = p.editor_font_size;
    e.auto_complete = p.auto_complete;
    e.uppercase_keywords = p.uppercase_keywords;
    e
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

fn template(d: Dialect, kind: ObjectKind, scope: &Scope, table: Option<&str>) -> String {
    let q = |n: &str| d.quote_ident(n);
    let qual = |n: &str| d.qualify(&scope.path(n));
    let tbl = table.map(|t| qual(t)).unwrap_or_else(|| qual("your_table"));
    let tbl_short = table.map(|t| q(t)).unwrap_or_else(|| q("your_table"));
    match (d, kind) {
        (Dialect::MySql, ObjectKind::Function) => format!(
            "CREATE FUNCTION {}(p_value INT)\nRETURNS INT\nDETERMINISTIC\nBEGIN\n  DECLARE v_result INT;\n  SET v_result = p_value * 2;\n  RETURN v_result;\nEND",
            q("new_function")
        ),
        (Dialect::MySql, ObjectKind::Procedure) => format!(
            "CREATE PROCEDURE {}(IN p_schema VARCHAR(64), OUT p_count INT)\nBEGIN\n  SELECT COUNT(*) INTO p_count\n  FROM information_schema.TABLES\n  WHERE TABLE_SCHEMA = p_schema;\nEND",
            q("new_procedure")
        ),
        (Dialect::MySql, ObjectKind::Trigger) => format!(
            "CREATE TRIGGER {} BEFORE INSERT ON {}\nFOR EACH ROW\nBEGIN\n  -- SET NEW.created_at = NOW();\nEND",
            q("new_trigger"),
            tbl
        ),
        (Dialect::MySql, ObjectKind::Event) => format!(
            "CREATE EVENT {}\nON SCHEDULE EVERY 1 DAY\nSTARTS CURRENT_TIMESTAMP + INTERVAL 1 HOUR\nON COMPLETION PRESERVE\nENABLE\nCOMMENT ''\nDO\nBEGIN\n  -- DELETE FROM log WHERE created_at < NOW() - INTERVAL 30 DAY;\nEND",
            q("new_event")
        ),
        (Dialect::MySql, ObjectKind::Index) => format!("CREATE INDEX {} ON {} ({}) USING BTREE", q("new_index"), tbl, q("column_name")),
        (Dialect::MySql, _) => format!("CREATE {} {} ...", kind.sql_keyword(), q("new_object")),

        (Dialect::Postgres, ObjectKind::Function) => format!(
            "CREATE OR REPLACE FUNCTION {}(p_value integer)\nRETURNS integer\nLANGUAGE plpgsql\nAS $$\nDECLARE\n  v_result integer;\nBEGIN\n  v_result := p_value * 2;\n  RETURN v_result;\nEND;\n$$;",
            qual("new_function")
        ),
        (Dialect::Postgres, ObjectKind::Procedure) => format!(
            "CREATE OR REPLACE PROCEDURE {}(IN p_id integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  RAISE NOTICE 'id = %', p_id;\nEND;\n$$;",
            qual("new_procedure")
        ),
        (Dialect::Postgres, ObjectKind::Trigger) => format!(
            "-- trigger function\nCREATE OR REPLACE FUNCTION {f}()\nRETURNS trigger\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  -- NEW.updated_at := now();\n  RETURN NEW;\nEND;\n$$;\n\nCREATE TRIGGER {n}\nBEFORE INSERT OR UPDATE ON {t}\nFOR EACH ROW EXECUTE FUNCTION {f}();",
            f = qual("new_trigger_fn"),
            n = q("new_trigger"),
            t = tbl
        ),
        (Dialect::Postgres, ObjectKind::Sequence) => format!(
            "CREATE SEQUENCE {}\n  INCREMENT 1\n  MINVALUE 1\n  MAXVALUE 9223372036854775807\n  START 1\n  CACHE 1;",
            qual("new_sequence")
        ),
        (Dialect::Postgres, ObjectKind::MaterializedView) => format!(
            "CREATE MATERIALIZED VIEW {} AS\nSELECT *\nFROM {}\nWITH DATA;",
            qual("new_materialized_view"),
            tbl
        ),
        (Dialect::Postgres, ObjectKind::Index) => {
            format!("CREATE INDEX {} ON {} USING btree ({});", q("new_index"), tbl, q("column_name"))
        }
        (Dialect::Postgres, _) => format!("CREATE {} {} ...;", kind.sql_keyword(), qual("new_object")),

        (Dialect::Sqlite, ObjectKind::Trigger) => format!(
            "CREATE TRIGGER {} AFTER INSERT ON {}\nFOR EACH ROW\nBEGIN\n  SELECT 1;\nEND;",
            q("new_trigger"),
            tbl_short
        ),
        (Dialect::Sqlite, ObjectKind::Index) => format!("CREATE INDEX {} ON {} ({});", q("new_index"), tbl_short, q("column_name")),
        (Dialect::Sqlite, _) => format!("CREATE {} {} ...;", kind.sql_keyword(), q("new_object")),
    }
}

// ---------------------------------------------------------------------------
// Tab
// ---------------------------------------------------------------------------

pub struct RoutineTab {
    id: String,
    conn: String,
    scope: Scope,
    kind: ObjectKind,
    dialect: Dialect,
    session: Option<Session>,
    /// Navigator name of the saved object (PG routines include the signature).
    name: Option<String>,
    editor: SqlEditor,
    load: Loadable<String>,
    /// DDL as loaded from the server (restore script).
    orig_ddl: String,
    saved_text: String,
    saving: Option<(Pending<()>, String, Instant)>,
    message: Option<(String, bool)>,
    exec_open: bool,
    params: Vec<Param>,
    executing: Option<(Pending<Vec<QueryResult>>, String, Instant)>,
    results: ResultsPane,
    show_preview: bool,
    confirm_close: bool,
}

impl RoutineTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, kind: ObjectKind, name: Option<String>) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let session = st.session(conn);
        if let Some(c) = st.conn_mut(conn) {
            c.load_objects(&scope, ObjectKind::Table);
        }
        let text = if name.is_none() {
            let tables = st.cached_tables(conn, &scope);
            template(dialect, kind, &scope, tables.first().map(|s| s.as_str()))
        } else {
            String::new()
        };
        let editor = new_editor(st, text.clone());
        let mut t = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            scope,
            kind,
            dialect,
            session,
            name,
            editor,
            load: Loadable::NotLoaded,
            orig_ddl: String::new(),
            saved_text: text,
            saving: None,
            message: None,
            exec_open: false,
            params: Vec::new(),
            executing: None,
            results: ResultsPane::default(),
            show_preview: false,
            confirm_close: false,
        };
        t.reload();
        t
    }

    fn path(&self) -> Option<ObjPath> {
        self.name.as_ref().map(|n| self.scope.path(n.clone()))
    }

    fn display_name(&self) -> String {
        self.name.as_deref().map(|n| strip_signature(n).to_string()).unwrap_or_else(|| "Untitled".into())
    }

    fn reload(&mut self) {
        let (Some(s), Some(p)) = (&self.session, self.path()) else { return };
        let kind = self.kind;
        let dialect = self.dialect;
        let scope = self.scope.clone();
        self.load = Loadable::Loading(s.run(move |d| match d.object_ddl(&p, kind) {
            Ok(x) => Ok(x),
            Err(e) if dialect == Dialect::Postgres && kind == ObjectKind::Index => {
                let sql = format!(
                    "SELECT indexdef FROM pg_indexes WHERE schemaname = {} AND indexname = {}",
                    dialect.string_literal(p.schema.as_deref().unwrap_or("public")),
                    dialect.string_literal(&p.name)
                );
                let r = d.query(Some(&scope), &sql)?;
                if r.rows.is_empty() {
                    return Err(e);
                }
                Ok(format!("{};", r.text(0, 0)))
            }
            Err(e) => Err(e),
        }));
    }

    fn is_routine(&self) -> bool {
        matches!(self.kind, ObjectKind::Function | ObjectKind::Procedure)
    }

    fn can_execute(&self) -> bool {
        self.is_routine() && self.dialect != Dialect::Sqlite && self.name.is_some()
    }

    /// DROP ... IF EXISTS for the saved object.
    fn drop_sql(&self) -> Option<String> {
        let p = self.path()?;
        let d = self.dialect;
        let kw = self.kind.sql_keyword();
        Some(match (d, self.kind) {
            (Dialect::Postgres, ObjectKind::Trigger) => {
                let table = parse_on_table(&self.orig_ddl, ObjectKind::Trigger)?;
                format!("DROP TRIGGER IF EXISTS {} ON {}", d.quote_ident(&p.name), table)
            }
            (Dialect::MySql, ObjectKind::Index) => {
                let table = parse_on_table(&self.orig_ddl, ObjectKind::Index)?;
                format!("DROP INDEX {} ON {}", d.quote_ident(&p.name), table)
            }
            (Dialect::Sqlite, _) => format!("DROP {} IF EXISTS {}", kw, d.qualify(&p)),
            _ => format!("DROP {} IF EXISTS {}", kw, d.qualify(&p)),
        })
    }

    fn plan(&self) -> SavePlan {
        let text = self.editor.text.trim().to_string();
        if text.is_empty() {
            return SavePlan::default();
        }
        let d = self.dialect;
        if self.name.is_none() {
            let text = if d == Dialect::Postgres && self.is_routine() { pg_or_replace(&text) } else { text };
            return SavePlan { pre: vec![], main: vec![text], restore: None };
        }
        let restore = if self.orig_ddl.trim().is_empty() { None } else { Some(self.orig_ddl.clone()) };
        match (d, self.kind) {
            (Dialect::Postgres, ObjectKind::Function | ObjectKind::Procedure) => SavePlan { pre: vec![], main: vec![pg_or_replace(&text)], restore: None },
            (Dialect::Postgres, ObjectKind::Sequence) => {
                let same_name = parse_object_name(&text, ObjectKind::Sequence).as_deref() == self.name.as_deref();
                let toks = tokenize(&text);
                let starts_create = toks.first().map(|t| is_word(&text, t, "CREATE")).unwrap_or(false)
                    && toks.get(1).map(|t| is_word(&text, t, "SEQUENCE")).unwrap_or(false);
                if same_name && starts_create && toks.len() > 1 {
                    // keep dependent column defaults: ALTER instead of DROP/CREATE
                    let alter = format!("ALTER{}", &text[toks[0].end..]);
                    SavePlan { pre: vec![], main: vec![alter], restore: None }
                } else {
                    SavePlan { pre: self.drop_sql().into_iter().collect(), main: vec![text], restore }
                }
            }
            _ => SavePlan { pre: self.drop_sql().into_iter().collect(), main: vec![text], restore },
        }
    }

    fn save(&mut self) {
        if self.saving.is_some() {
            return;
        }
        let plan = self.plan();
        if plan.is_empty() {
            self.message = Some(("Nothing to save: the definition is empty.".into(), true));
            return;
        }
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

    fn open_execute(&mut self) {
        let parsed = parse_params(&self.editor.text, self.kind);
        // keep entered values for parameters that still exist
        let old = std::mem::take(&mut self.params);
        self.params = parsed
            .into_iter()
            .map(|mut p| {
                if let Some(o) = old.iter().find(|o| o.name == p.name) {
                    p.value = o.value.clone();
                    p.null = o.null;
                    p.raw = o.raw;
                }
                p
            })
            .collect();
        self.exec_open = true;
    }

    fn execute(&mut self) {
        let (Some(s), Some(p)) = (&self.session, self.path()) else { return };
        let d = self.dialect;
        let base = strip_signature(&p.name).to_string();
        let target = d.qualify(&self.scope.path(base));
        let mut stmts: Vec<String> = Vec::new();
        match (d, self.kind) {
            (Dialect::MySql, ObjectKind::Procedure) => {
                let mut args = Vec::new();
                let mut outs = Vec::new();
                for prm in &self.params {
                    if prm.is_out() || prm.is_inout() {
                        let var = format!("@_{}", prm.name.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect::<String>());
                        if prm.is_inout() {
                            stmts.push(format!("SET {} = {}", var, prm.literal(d)));
                        }
                        args.push(var.clone());
                        outs.push(format!("{} AS {}", var, d.quote_ident(&prm.name)));
                    } else {
                        args.push(prm.literal(d));
                    }
                }
                stmts.push(format!("CALL {}({})", target, args.join(", ")));
                if !outs.is_empty() {
                    stmts.push(format!("SELECT {}", outs.join(", ")));
                }
            }
            (Dialect::MySql, _) => {
                let args: Vec<String> = self.params.iter().map(|p| p.literal(d)).collect();
                stmts.push(format!("SELECT {}({}) AS {}", target, args.join(", "), d.quote_ident("result")));
            }
            (_, ObjectKind::Procedure) => {
                let args: Vec<String> = self.params.iter().map(|p| if p.is_out() { "NULL".into() } else { p.literal(d) }).collect();
                stmts.push(format!("CALL {}({})", target, args.join(", ")));
            }
            _ => {
                let args: Vec<String> = self.params.iter().filter(|p| !p.is_out()).map(|p| p.literal(d)).collect();
                stmts.push(format!("SELECT * FROM {}({})", target, args.join(", ")));
            }
        }
        let script = stmts.iter().map(|s| format!("{};", s)).collect::<Vec<_>>().join("\n");
        let scope = self.scope.clone();
        self.executing = Some((
            s.run(move |drv| {
                let mut out = Vec::new();
                for stmt in &stmts {
                    out.extend(drv.execute(Some(&scope), stmt).with_context(|| format!("while executing: {}", stmt))?);
                }
                Ok(out)
            }),
            script,
            Instant::now(),
        ));
    }

    fn poll(&mut self, st: &mut AppState) {
        if self.load.poll() {
            match &self.load {
                Loadable::Ready(ddl) => {
                    self.orig_ddl = ddl.clone();
                    self.editor.text = ddl.clone();
                    self.saved_text = ddl.clone();
                }
                Loadable::Failed(e) => self.message = Some((e.clone(), true)),
                _ => {}
            }
        }
        if let Some((p, _, _)) = &mut self.saving {
            if let Some(r) = p.poll() {
                let (_, script, started) = self.saving.take().unwrap();
                let ms = started.elapsed().as_millis();
                match r {
                    Ok(()) => {
                        st.record(&self.conn, Some(&self.scope), &script, true, &format!("{} saved", self.kind.singular()), ms);
                        let text = self.editor.text.clone();
                        let parsed = parse_object_name(&text, self.kind);
                        let renamed = match (&self.name, &parsed) {
                            (Some(old), Some(new)) => strip_signature(old) != new,
                            (None, _) => true,
                            _ => false,
                        };
                        if renamed {
                            if let Some(n) = parsed {
                                self.name = Some(n);
                            }
                        }
                        self.saved_text = text.clone();
                        self.orig_ddl = text;
                        st.refresh_scope(&self.conn, &self.scope);
                        st.toast(format!("{} {} saved", self.kind.singular(), self.display_name()));
                        self.message = Some((format!("Saved in {} ms", ms), false));
                        // reload the canonical definition from the server
                        if self.name.is_some() && !(self.dialect == Dialect::Postgres && self.is_routine() && renamed) {
                            self.reload();
                        }
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&self.conn, Some(&self.scope), &script, false, &m, ms);
                        self.message = Some((m, true));
                    }
                }
            }
        }
        if let Some((p, _, _)) = &mut self.executing {
            if let Some(r) = p.poll() {
                let (_, script, started) = self.executing.take().unwrap();
                let ms = started.elapsed().as_millis();
                self.results.clear();
                match r {
                    Ok(res) => {
                        st.record(&self.conn, Some(&self.scope), &script, true, "OK", ms);
                        self.results.messages.push((script.clone(), false));
                        self.results.set_results("Result", res);
                        self.results.messages.push((format!("Finished in {} ms", ms), false));
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&self.conn, Some(&self.scope), &script, false, &m, ms);
                        self.results.messages.push((script, false));
                        self.results.messages.push((m, true));
                    }
                }
            }
        }
    }

    fn params_panel(&mut self, ui: &mut Ui) {
        let mut run = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(format!("{} Execute {}", icon::RUN, self.display_name()));
                ui.label(RichText::new("Values are quoted automatically unless they are numbers, NULL or marked as expression.").weak().small());
            });
            if self.params.is_empty() {
                ui.label(RichText::new("This routine has no parameters.").weak());
            } else {
                egui::Grid::new(("routine-params", &self.id)).num_columns(6).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                    ui.label(RichText::new("Mode").weak());
                    ui.label(RichText::new("Name").weak());
                    ui.label(RichText::new("Type").weak());
                    ui.label(RichText::new("Value").weak());
                    ui.label(RichText::new("NULL").weak());
                    ui.label(RichText::new("Expression").weak());
                    ui.end_row();
                    for p in self.params.iter_mut() {
                        ui.label(&p.mode);
                        ui.label(RichText::new(&p.name).strong());
                        ui.label(RichText::new(&p.ty).monospace());
                        let enabled = !p.null && (!p.is_out() || p.is_inout());
                        let hint = if p.is_out() { "(output)" } else { "value" };
                        let r = ui.add_enabled(enabled, egui::TextEdit::singleline(&mut p.value).desired_width(220.0).hint_text(hint));
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            run = true;
                        }
                        ui.checkbox(&mut p.null, "");
                        ui.checkbox(&mut p.raw, "");
                        ui.end_row();
                    }
                });
            }
            ui.horizontal(|ui| {
                let busy = self.executing.is_some();
                if ui.add_enabled(!busy, egui::Button::new(format!("{} Run", icon::RUN))).clicked() {
                    run = true;
                }
                if ui.button("Re-read parameters").clicked() {
                    self.open_execute();
                }
                if ui.button("Close").clicked() {
                    self.exec_open = false;
                }
                if busy {
                    ui.spinner();
                }
            });
        });
        if run && self.executing.is_none() {
            self.execute();
        }
    }
}

impl Tab for RoutineTab {
    fn title(&self) -> String {
        format!("{} {} ({})", crate::ui::style::object_icon(self.kind), self.display_name(), self.kind.singular())
    }

    fn key(&self) -> String {
        match self.path() {
            Some(p) => format!("routine:{}:{:?}:{}", self.conn, self.kind, p.display()),
            None => format!("routine:new:{}", self.id),
        }
    }

    fn dirty(&self) -> bool {
        !self.load.is_loading() && self.editor.text != self.saved_text
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

        // toolbar
        ui.horizontal(|ui| {
            let busy = self.saving.is_some();
            if ui.add_enabled(!busy && !self.load.is_loading(), egui::Button::new(format!("{} Save", icon::SAVE))).on_hover_text("Save (⌘S)").clicked() {
                self.save();
            }
            if busy {
                ui.spinner();
            }
            if self.is_routine() && self.dialect != Dialect::Sqlite {
                let tip = if self.name.is_some() { "Execute with parameters" } else { "Save the routine first" };
                if ui.add_enabled(self.can_execute(), egui::Button::new(format!("{} Execute", icon::RUN))).on_hover_text(tip).clicked() {
                    if self.dirty() {
                        self.message = Some(("Note: executing the saved version; unsaved edits are not applied.".into(), false));
                    }
                    self.open_execute();
                }
            }
            if self.name.is_some() && ui.button(format!("{} Reload", icon::REFRESH)).on_hover_text("Reload the definition from the server (discards edits)").clicked() {
                self.reload();
            }
            if ui.add_enabled(self.dirty(), egui::Button::new("↶ Revert")).clicked() {
                self.editor.text = self.saved_text.clone();
            }
            ui.separator();
            ui.toggle_value(&mut self.show_preview, "SQL Preview");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} · {}", st.conn_name(&self.conn), self.scope.display())).weak());
            });
        });
        if save_key {
            self.save();
        }
        if self.exec_open {
            self.params_panel(ui);
        }
        ui.separator();

        // status
        egui::TopBottomPanel::bottom(egui::Id::new(("routine-status", &self.id))).show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if self.message.is_some() {
                    status_line(ui, &self.message);
                } else {
                    let hint = match (self.dialect, self.kind) {
                        (Dialect::MySql, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger | ObjectKind::Event) if self.name.is_some() => {
                            "Saving drops and re-creates the object (restored automatically if the new definition fails)."
                        }
                        (Dialect::Postgres, ObjectKind::Function | ObjectKind::Procedure) => "Saving uses CREATE OR REPLACE.",
                        (Dialect::Postgres, ObjectKind::Sequence) if self.name.is_some() => "Saving alters the sequence in place.",
                        _ if self.name.is_some() => "Saving drops and re-creates the object.",
                        _ => "Edit the template and press Save to create the object.",
                    };
                    ui.label(RichText::new(hint).weak().small());
                }
            });
        });

        if !self.results.is_empty() || self.executing.is_some() {
            egui::TopBottomPanel::bottom(egui::Id::new(("routine-results", &self.id)))
                .resizable(true)
                .default_height(220.0)
                .show_inside(ui, |ui| {
                    if self.executing.is_some() {
                        ui::loading(ui, "Executing...");
                    }
                    let id = format!("routine-{}", self.id);
                    self.results.show(ui, st, &id);
                });
        }

        if self.show_preview {
            let script = self.plan().script();
            egui::TopBottomPanel::bottom(egui::Id::new(("routine-preview", &self.id)))
                .resizable(true)
                .default_height(160.0)
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Statements executed on Save");
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(script.clone());
                        }
                    });
                    egui::ScrollArea::both().id_salt(("routine-preview-scroll", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
                        let mut s = script.as_str();
                        ui.add(egui::TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                    });
                });
        }

        if self.load.is_loading() {
            ui::loading(ui, &format!("Loading {} definition...", self.kind.singular().to_lowercase()));
            return;
        }
        let words = st.cached_tables(&self.conn, &self.scope);
        self.editor.ui(ui, &format!("routine-editor-{}", self.id), &words);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_params() {
        let s = "CREATE DEFINER=`root`@`%` PROCEDURE `p`(IN a INT, OUT b VARCHAR(10), INOUT c DECIMAL(10,2))\nBEGIN SELECT 1; END";
        assert_eq!(parse_object_name(s, ObjectKind::Procedure).as_deref(), Some("p"));
        let p = parse_params(s, ObjectKind::Procedure);
        assert_eq!(p.len(), 3);
        assert_eq!(p[1].mode, "OUT");
        assert_eq!(p[2].ty, "DECIMAL(10,2)");
        let pg = "CREATE OR REPLACE FUNCTION public.f(a integer, text DEFAULT 'x')\n RETURNS integer AS $$ select 1 $$";
        assert_eq!(parse_object_name(pg, ObjectKind::Function).as_deref(), Some("f"));
        let p = parse_params(pg, ObjectKind::Function);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].name, "$2");
        let trg = "CREATE OR REPLACE FUNCTION x() RETURNS trigger AS $$ begin end $$;\nCREATE TRIGGER t BEFORE INSERT ON public.tbl FOR EACH ROW EXECUTE FUNCTION x();";
        assert_eq!(parse_object_name(trg, ObjectKind::Trigger).as_deref(), Some("t"));
        assert_eq!(parse_on_table(trg, ObjectKind::Trigger).as_deref(), Some("public.tbl"));
        assert_eq!(pg_or_replace("CREATE FUNCTION f() x"), "CREATE OR REPLACE FUNCTION f() x");
        assert_eq!(
            view_body("CREATE ALGORITHM=UNDEFINED DEFINER=`r`@`%` VIEW `v` AS select `a` AS `b` from t;").as_deref(),
            Some("select `a` AS `b` from t")
        );
        assert_eq!(strip_signature("f(integer, text)"), "f");
    }
}
