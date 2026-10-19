//! Find in Database: search table data or object structure (names, columns,
//! definitions) of one database/schema.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::RichText;
use navdb_core::driver::Driver;
use navdb_core::{DbKind, Dialect, ObjectKind, Pending, Scope, Session};

use super::Tab;
use crate::state::{AppState, Loadable};
use crate::ui::style::icon;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Contains,
    WholeWord,
    Exact,
    Regex,
}

impl Mode {
    const ALL: [Mode; 4] = [Mode::Contains, Mode::WholeWord, Mode::Exact, Mode::Regex];
    fn label(self) -> &'static str {
        match self {
            Mode::Contains => "Contains",
            Mode::WholeWord => "Whole word",
            Mode::Exact => "Exact match",
            Mode::Regex => "Regular expression",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Data,
    Structure,
}

#[derive(Clone)]
struct Hit {
    label: String,
    detail: String,
}

#[derive(Clone)]
struct Group {
    kind: ObjectKind,
    name: String,
    hits: Vec<Hit>,
    /// WHERE clause reproducing the data matches (data search only).
    where_sql: Option<String>,
    truncated: bool,
    error: Option<String>,
}

#[derive(Default)]
struct Progress {
    done: usize,
    total: usize,
    current: String,
}

#[derive(Clone)]
struct Opts {
    text: String,
    mode: Mode,
    case: bool,
    limit: usize,
    tables: Vec<String>,
    kind: DbKind,
    scope: Scope,
}

fn like_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if c == '\\' || c == '%' || c == '_' {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// SQL condition testing one column.
fn condition(o: &Opts, col: &str) -> String {
    let d = o.kind.dialect();
    let c = d.quote_ident(col);
    let lit = |s: &str| d.string_literal(s);
    match o.mode {
        Mode::Contains | Mode::WholeWord => {
            let pat = lit(&format!("%{}%", like_escape(&o.text)));
            match d {
                Dialect::MySql => {
                    if o.case {
                        format!("CAST({} AS BINARY) LIKE {}", c, pat)
                    } else {
                        format!("{} LIKE {}", c, pat)
                    }
                }
                Dialect::Postgres => format!("CAST({} AS TEXT) {} {}", c, if o.case { "LIKE" } else { "ILIKE" }, pat),
                Dialect::Sqlite => {
                    if o.case {
                        format!("instr({}, {}) > 0", c, lit(&o.text))
                    } else {
                        format!("{} LIKE {} ESCAPE '\\'", c, pat)
                    }
                }
            }
        }
        Mode::Exact => {
            let v = lit(&o.text);
            match d {
                Dialect::MySql => {
                    if o.case {
                        format!("CAST({} AS BINARY) = {}", c, v)
                    } else {
                        format!("{} = {}", c, v)
                    }
                }
                Dialect::Postgres => {
                    if o.case {
                        format!("CAST({} AS TEXT) = {}", c, v)
                    } else {
                        format!("LOWER(CAST({} AS TEXT)) = LOWER({})", c, v)
                    }
                }
                Dialect::Sqlite => {
                    if o.case {
                        format!("{} = {}", c, v)
                    } else {
                        format!("{} = {} COLLATE NOCASE", c, v)
                    }
                }
            }
        }
        Mode::Regex => match d {
            Dialect::MySql if o.kind == DbKind::MySql => format!("REGEXP_LIKE({}, {}, '{}')", c, lit(&o.text), if o.case { "c" } else { "i" }),
            Dialect::MySql => format!("CAST({} AS CHAR) REGEXP {}", c, lit(&format!("{}{}", if o.case { "(?-i)" } else { "(?i)" }, o.text))),
            Dialect::Postgres => format!("CAST({} AS TEXT) {} {}", c, if o.case { "~" } else { "~*" }, lit(&o.text)),
            Dialect::Sqlite => format!("{} REGEXP {}", c, lit(&o.text)),
        },
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Client-side check used to find matching cells / names. Regex cannot be
/// evaluated client side and always returns true (the server already filtered).
fn text_matches(hay: &str, o: &Opts) -> bool {
    let (h, n) = if o.case { (hay.to_string(), o.text.clone()) } else { (hay.to_lowercase(), o.text.to_lowercase()) };
    match o.mode {
        Mode::Contains => h.contains(&n),
        Mode::Exact => h == n,
        Mode::Regex => true,
        Mode::WholeWord => {
            if n.is_empty() {
                return false;
            }
            let mut start = 0;
            while let Some(pos) = h[start..].find(&n) {
                let a = start + pos;
                let b = a + n.len();
                let before_ok = h[..a].chars().next_back().map(|c| !is_word_char(c)).unwrap_or(true);
                let after_ok = h[b..].chars().next().map(|c| !is_word_char(c)).unwrap_or(true);
                if before_ok && after_ok {
                    return true;
                }
                start = a + h[a..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                if start >= h.len() {
                    break;
                }
            }
            false
        }
    }
}

fn is_textish(type_name: &str, kind: DbKind) -> bool {
    let t = type_name.to_ascii_lowercase();
    if t.is_empty() {
        return kind == DbKind::Sqlite;
    }
    if t.contains("blob") || t.contains("binary") || t.contains("bytea") {
        return false;
    }
    ["char", "text", "json", "enum", "set", "uuid", "xml", "clob", "string", "name", "citext", "inet"].iter().any(|k| t.contains(k))
}

fn snippet(s: &str, o: &Opts) -> String {
    let one: String = s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
    let lower = one.to_lowercase();
    let pos = if o.case { one.find(&o.text) } else { lower.find(&o.text.to_lowercase()) };
    let chars: Vec<char> = one.chars().collect();
    let src = if o.case { &one } else { &lower };
    let center = pos.map(|p| src[..p].chars().count()).unwrap_or(0).min(chars.len());
    let start = center.saturating_sub(40);
    let end = (center + o.text.chars().count() + 60).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out = format!("…{}", out);
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn search_data(d: &mut dyn Driver, o: &Opts, prog: &Mutex<Progress>, out: &Mutex<Vec<Group>>, cancel: &AtomicBool) {
    let dialect = d.dialect();
    for t in &o.tables {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(mut p) = prog.lock() {
            p.current = t.clone();
        }
        let path = o.scope.path(t.clone());
        let res = (|| -> navdb_core::Result<Option<Group>> {
            let cols = d.columns(&path)?;
            let text_cols: Vec<String> = cols.iter().filter(|c| is_textish(&c.data_type, o.kind)).map(|c| c.name.clone()).collect();
            if text_cols.is_empty() {
                return Ok(None);
            }
            let where_sql = text_cols.iter().map(|c| condition(o, c)).collect::<Vec<_>>().join(" OR ");
            let fetch = if o.mode == Mode::WholeWord { o.limit * 4 + 1 } else { o.limit + 1 };
            let sql = format!("SELECT * FROM {} WHERE {}{}", dialect.qualify(&path), where_sql, dialect.limit_clause(fetch, 0));
            let r = d.query(Some(&o.scope), &sql)?;
            let idx: Vec<usize> = text_cols.iter().filter_map(|c| r.column_index(c)).collect();
            let mut hits = Vec::new();
            let mut truncated = r.rows.len() >= fetch;
            for (ri, row) in r.rows.iter().enumerate() {
                let matched: Vec<usize> = idx.iter().copied().filter(|&i| !row[i].is_null() && text_matches(&row[i].display(), o)).collect();
                if matched.is_empty() {
                    continue;
                }
                if hits.len() >= o.limit {
                    truncated = true;
                    break;
                }
                let label = matched
                    .iter()
                    .take(3)
                    .map(|&i| format!("{}: {}", r.columns[i].name, snippet(&row[i].display(), o)))
                    .collect::<Vec<_>>()
                    .join("   |   ");
                let detail = r
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{} = {}", c.name, r.text(ri, i)))
                    .collect::<Vec<_>>()
                    .join("\n");
                hits.push(Hit { label, detail });
            }
            if hits.is_empty() {
                return Ok(None);
            }
            Ok(Some(Group { kind: ObjectKind::Table, name: t.clone(), hits, where_sql: Some(where_sql), truncated, error: None }))
        })();
        match res {
            Ok(Some(g)) => out.lock().map(|mut v| v.push(g)).unwrap_or(()),
            Ok(None) => {}
            Err(e) => out
                .lock()
                .map(|mut v| {
                    v.push(Group { kind: ObjectKind::Table, name: t.clone(), hits: vec![], where_sql: None, truncated: false, error: Some(format!("{:#}", e)) })
                })
                .unwrap_or(()),
        }
        if let Ok(mut p) = prog.lock() {
            p.done += 1;
        }
    }
}

fn search_structure(d: &mut dyn Driver, o: &Opts, prog: &Mutex<Progress>, out: &Mutex<Vec<Group>>, cancel: &AtomicBool) {
    let kinds: Vec<ObjectKind> = d.object_kinds().into_iter().filter(|k| *k != ObjectKind::Key).collect();
    let mut objects: Vec<(ObjectKind, String)> = Vec::new();
    for k in &kinds {
        if let Ok(mut p) = prog.lock() {
            p.current = format!("listing {}", k.label().to_lowercase());
        }
        if let Ok(list) = d.list_objects(&o.scope, *k) {
            for obj in list {
                if *k == ObjectKind::Table && !o.tables.contains(&obj.name) {
                    continue;
                }
                objects.push((*k, obj.name));
            }
        }
    }
    let with_columns = |k: ObjectKind| matches!(k, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView);
    let with_ddl = |k: ObjectKind| {
        matches!(k, ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger | ObjectKind::Event)
    };
    if let Ok(mut p) = prog.lock() {
        p.total = objects.len();
        p.done = 0;
    }
    for (k, name) in objects {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(mut p) = prog.lock() {
            p.current = name.clone();
        }
        let mut hits = Vec::new();
        let bare = name.split('(').next().unwrap_or(&name).to_string();
        if text_matches(&bare, o) {
            hits.push(Hit { label: format!("{} name", k.singular()), detail: name.clone() });
        }
        let path = o.scope.path(bare.clone());
        if with_columns(k) {
            if let Ok(cols) = d.columns(&path) {
                for c in cols {
                    if text_matches(&c.name, o) {
                        hits.push(Hit { label: format!("Column: {} ({})", c.name, c.data_type), detail: c.comment.clone() });
                    } else if !c.comment.is_empty() && text_matches(&c.comment, o) && o.mode != Mode::Exact {
                        hits.push(Hit { label: format!("Column comment: {} - {}", c.name, snippet(&c.comment, o)), detail: c.comment.clone() });
                    }
                }
            }
        }
        if with_ddl(k) {
            if let Ok(ddl) = d.object_ddl(&o.scope.path(name.clone()), k).or_else(|_| d.object_ddl(&path, k)) {
                if o.mode != Mode::Exact {
                    for (ln, line) in ddl.lines().enumerate() {
                        if text_matches(line, o) {
                            hits.push(Hit { label: format!("Definition line {}: {}", ln + 1, snippet(line.trim(), o)), detail: ddl.clone() });
                            if hits.len() > 50 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        if !hits.is_empty() {
            if let Ok(mut v) = out.lock() {
                v.push(Group { kind: k, name, hits, where_sql: None, truncated: false, error: None });
            }
        }
        if let Ok(mut p) = prog.lock() {
            p.done += 1;
        }
    }
}

enum UiAction {
    Open(ObjectKind, String),
    Query(String, String),
}

pub struct FindInDbTab {
    id: String,
    conn: String,
    conn_name: String,
    scope: Scope,
    kind: DbKind,
    session: Option<Session>,
    text: String,
    mode: Mode,
    target: Target,
    case: bool,
    limit: usize,
    tables: Loadable<Vec<String>>,
    selected: BTreeSet<String>,
    table_filter: String,
    running: Option<Pending<()>>,
    started: Option<Instant>,
    elapsed: f64,
    progress: Arc<Mutex<Progress>>,
    results: Arc<Mutex<Vec<Group>>>,
    cancel: Arc<AtomicBool>,
    searched: Option<Opts>,
    detail: Option<String>,
    focus: bool,
}

impl FindInDbTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        let kind = st.config(conn).map(|c| c.kind).unwrap_or_default();
        let session = st.session(conn).map(|s| s.fork());
        let tables = match &session {
            Some(s) if kind.is_sql() => {
                let sc = scope.clone();
                Loadable::Loading(s.run(move |d| Ok(d.list_objects(&sc, ObjectKind::Table)?.into_iter().map(|o| o.name).collect())))
            }
            _ => Loadable::NotLoaded,
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            scope,
            kind,
            session,
            text: String::new(),
            mode: Mode::Contains,
            target: Target::Data,
            case: false,
            limit: 100,
            tables,
            selected: BTreeSet::new(),
            table_filter: String::new(),
            running: None,
            started: None,
            elapsed: 0.0,
            progress: Arc::new(Mutex::new(Progress::default())),
            results: Arc::new(Mutex::new(Vec::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            searched: None,
            detail: None,
            focus: true,
        }
    }

    fn start(&mut self, st: &mut AppState) {
        if self.text.is_empty() || self.running.is_some() {
            return;
        }
        let Some(s) = &self.session else {
            st.error("Connection not found");
            return;
        };
        let all = self.tables.ready().cloned().unwrap_or_default();
        let tables: Vec<String> = all.iter().filter(|t| self.selected.contains(*t)).cloned().collect();
        if tables.is_empty() && self.target == Target::Data {
            st.error("Select at least one table");
            return;
        }
        let o = Opts {
            text: self.text.clone(),
            mode: if self.target == Target::Structure && self.mode == Mode::Regex { Mode::Contains } else { self.mode },
            case: self.case,
            limit: self.limit.max(1),
            tables,
            kind: self.kind,
            scope: self.scope.clone(),
        };
        self.progress = Arc::new(Mutex::new(Progress { done: 0, total: o.tables.len(), current: String::new() }));
        self.results = Arc::new(Mutex::new(Vec::new()));
        self.cancel = Arc::new(AtomicBool::new(false));
        self.detail = None;
        let (prog, out, cancel) = (self.progress.clone(), self.results.clone(), self.cancel.clone());
        let target = self.target;
        let opts = o.clone();
        self.running = Some(s.run(move |d| {
            match target {
                Target::Data => search_data(d, &opts, &prog, &out, &cancel),
                Target::Structure => search_structure(d, &opts, &prog, &out, &cancel),
            }
            Ok(())
        }));
        self.searched = Some(o);
        self.started = Some(Instant::now());
    }

    fn poll(&mut self, st: &mut AppState) {
        if self.tables.poll() {
            if let Some(v) = self.tables.ready() {
                self.selected = v.iter().cloned().collect();
            }
        }
        if let Some(p) = &mut self.running {
            if let Some(r) = p.poll() {
                self.running = None;
                self.elapsed = self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                if let Err(e) = r {
                    st.error(format!("Search failed: {:#}", e));
                }
            }
        }
    }

    fn tables_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Tables").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("None").clicked() {
                    self.selected.clear();
                }
                if ui.small_button("All").clicked() {
                    if let Some(v) = self.tables.ready() {
                        self.selected = v.iter().cloned().collect();
                    }
                }
            });
        });
        ui.add(egui::TextEdit::singleline(&mut self.table_filter).hint_text("Filter tables").desired_width(f32::INFINITY));
        ui.separator();
        match &self.tables {
            Loadable::Loading(_) => crate::ui::loading(ui, "Loading tables..."),
            Loadable::Failed(e) => crate::ui::error_text(ui, e),
            Loadable::Ready(v) => {
                let f = self.table_filter.to_lowercase();
                let n = v.len();
                ui.label(RichText::new(format!("{} of {} selected", self.selected.len(), n)).weak().small());
                egui::ScrollArea::vertical().id_salt(format!("find-tables-{}", self.id)).auto_shrink([false, false]).show(ui, |ui| {
                    for t in v.iter().filter(|t| f.is_empty() || t.to_lowercase().contains(&f)) {
                        let mut on = self.selected.contains(t);
                        if ui.checkbox(&mut on, format!("{} {}", icon::TABLE, t)).changed() {
                            if on {
                                self.selected.insert(t.clone());
                            } else {
                                self.selected.remove(t);
                            }
                        }
                    }
                });
            }
            Loadable::NotLoaded => {}
        }
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) -> Option<UiAction> {
        let mut action = None;
        let groups = self.results.lock().map(|g| g.clone()).unwrap_or_default();
        let total_hits: usize = groups.iter().map(|g| g.hits.len()).sum();
        let (done, total, current) = self.progress.lock().map(|p| (p.done, p.total, p.current.clone())).unwrap_or_default();
        if self.running.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).desired_width(260.0).text(format!("{}/{}", done, total)));
                ui.label(RichText::new(format!("Searching {}", current)).weak());
            });
        } else if let Some(o) = &self.searched {
            let cancelled = self.cancel.load(Ordering::Relaxed);
            ui.label(
                RichText::new(format!(
                    "{} match(es) in {} object(s) for \"{}\" ({}{}){} - {:.2} s",
                    total_hits,
                    groups.iter().filter(|g| g.error.is_none()).count(),
                    o.text,
                    o.mode.label().to_lowercase(),
                    if o.case { ", case sensitive" } else { "" },
                    if cancelled { " - stopped" } else { "" },
                    self.elapsed
                ))
                .weak(),
            );
        }
        ui.separator();
        if groups.is_empty() {
            if self.searched.is_some() && self.running.is_none() {
                ui.add_space(12.0);
                ui.label(RichText::new("No matches found.").weak());
            }
            return None;
        }
        egui::ScrollArea::vertical().id_salt(format!("find-res-{}", self.id)).auto_shrink([false, false]).show(ui, |ui| {
            for (gi, g) in groups.iter().enumerate() {
                let title = if let Some(e) = &g.error {
                    RichText::new(format!("{} {}  -  error: {}", crate::ui::style::object_icon(g.kind), g.name, e)).color(ui.visuals().error_fg_color)
                } else {
                    RichText::new(format!(
                        "{} {}  ({} match{}{})",
                        crate::ui::style::object_icon(g.kind),
                        g.name,
                        g.hits.len(),
                        if g.hits.len() == 1 { "" } else { "es" },
                        if g.truncated { ", limit reached" } else { "" }
                    ))
                    .strong()
                };
                let header = egui::CollapsingHeader::new(title).id_salt(format!("find-g-{}-{}", self.id, gi)).default_open(groups.len() <= 20).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let open_label = match g.kind {
                            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => "Open",
                            _ => "Design",
                        };
                        if ui.small_button(format!("{} {}", icon::TABLE, open_label)).clicked() {
                            action = Some(UiAction::Open(g.kind, g.name.clone()));
                        }
                        if let Some(w) = &g.where_sql {
                            if ui.small_button(format!("{} Query matches", icon::NEW_QUERY)).clicked() {
                                action = Some(UiAction::Query(g.name.clone(), w.clone()));
                            }
                        }
                    });
                    for h in &g.hits {
                        let r = ui.add(egui::Label::new(RichText::new(&h.label).monospace()).truncate().sense(egui::Sense::click()));
                        let r = if h.detail.is_empty() { r } else { r.on_hover_text(RichText::new(&h.detail).monospace()) };
                        if r.clicked() {
                            self.detail = Some(h.detail.clone());
                        }
                        if r.double_clicked() {
                            action = Some(UiAction::Open(g.kind, g.name.clone()));
                        }
                    }
                });
                header.header_response.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        action = Some(UiAction::Open(g.kind, g.name.clone()));
                        ui.close_menu();
                    }
                    if let Some(w) = &g.where_sql {
                        if ui.button("Query matches in new query").clicked() {
                            action = Some(UiAction::Query(g.name.clone(), w.clone()));
                            ui.close_menu();
                        }
                    }
                    if ui.button("Copy name").clicked() {
                        ui.ctx().copy_text(g.name.clone());
                        ui.close_menu();
                    }
                });
                if header.header_response.double_clicked() {
                    action = Some(UiAction::Open(g.kind, g.name.clone()));
                }
            }
        });
        action
    }
}

impl Tab for FindInDbTab {
    fn title(&self) -> String {
        format!("🔍 Find - {}", self.scope.display())
    }

    fn key(&self) -> String {
        format!("find:{}:{}", self.conn, self.scope.display())
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        self.poll(st);
        if self.running.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("🔍 Find in {} / {}", self.conn_name, self.scope.display())).strong());
        });
        if !self.kind.is_sql() {
            ui.add_space(10.0);
            ui.label(RichText::new("Find in Database is available for SQL connections. Use the key filter in the navigator for Redis.").weak());
            return;
        }
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.text).hint_text("Text to find").desired_width(260.0));
            if self.focus {
                r.request_focus();
                self.focus = false;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if self.running.is_some() {
                if ui.button(format!("{} Stop", icon::STOP)).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            } else if ui.add_enabled(!self.text.is_empty(), egui::Button::new(RichText::new("🔍 Find").strong())).clicked() || enter {
                self.start(st);
            }
            ui.separator();
            egui::ComboBox::from_id_salt(format!("find-mode-{}", self.id)).selected_text(self.mode.label()).width(140.0).show_ui(ui, |ui| {
                for m in Mode::ALL {
                    ui.selectable_value(&mut self.mode, m, m.label());
                }
            });
            ui.checkbox(&mut self.case, "Case sensitive");
        });
        ui.horizontal(|ui| {
            ui.label("Search in:");
            ui.radio_value(&mut self.target, Target::Data, "Data");
            ui.radio_value(&mut self.target, Target::Structure, "Structure");
            ui.separator();
            if self.target == Target::Data {
                ui.label("Max matches per table:");
                ui.add(egui::DragValue::new(&mut self.limit).range(1..=100_000).speed(5));
            } else {
                ui.label(RichText::new("Object names, column names and definitions").weak());
                if self.mode == Mode::Regex {
                    ui.label(RichText::new("(regex is evaluated as 'contains' for structure)").weak().small());
                }
            }
        });
        ui.separator();

        egui::SidePanel::left(format!("find-left-{}", self.id)).resizable(true).default_width(220.0).width_range(150.0..=500.0).show_inside(ui, |ui| {
            self.tables_panel(ui);
        });
        if let Some(d) = self.detail.clone() {
            let mut close = false;
            egui::TopBottomPanel::bottom(format!("find-detail-{}", self.id)).resizable(true).default_height(150.0).show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Details").strong());
                    if ui.small_button("📋").on_hover_text("Copy").clicked() {
                        ui.ctx().copy_text(d.clone());
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").clicked() {
                            close = true;
                        }
                    });
                });
                let mut t = d.clone();
                egui::ScrollArea::vertical().id_salt(format!("find-detail-s-{}", self.id)).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut t).code_editor().desired_width(f32::INFINITY));
                });
            });
            if close {
                self.detail = None;
            }
        }
        let mut action = None;
        egui::CentralPanel::default().show_inside(ui, |ui| {
            action = self.results_ui(ui);
        });
        match action {
            Some(UiAction::Open(kind, name)) => {
                let scope = self.scope.clone();
                crate::commands::open_object(st, &self.conn, &scope, kind, &name);
            }
            Some(UiAction::Query(table, where_sql)) => {
                let d = self.kind.dialect();
                let sql = format!("SELECT * FROM {}\nWHERE {}", d.qualify(&self.scope.path(table)), where_sql.replace(" OR ", "\n   OR "));
                crate::commands::new_query_with(st, Some(self.conn.clone()), Some(self.scope.clone()), sql);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(text: &str, mode: Mode, case: bool) -> Opts {
        Opts { text: text.into(), mode, case, limit: 10, tables: vec![], kind: DbKind::MySql, scope: Scope::new("db", None) }
    }

    #[test]
    fn matching() {
        assert!(text_matches("Hello World", &o("world", Mode::Contains, false)));
        assert!(!text_matches("Hello World", &o("world", Mode::Contains, true)));
        assert!(text_matches("a cat sat", &o("cat", Mode::WholeWord, false)));
        assert!(!text_matches("concatenate", &o("cat", Mode::WholeWord, false)));
        assert!(text_matches("concat cat", &o("cat", Mode::WholeWord, false)));
        assert!(text_matches("ABC", &o("abc", Mode::Exact, false)));
        assert_eq!(condition(&o("50%", Mode::Contains, false), "c"), "`c` LIKE '%50\\\\%%'");
    }
}
