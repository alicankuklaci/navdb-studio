//! Redis key viewer / editor (string, hash, list, set, sorted set, stream)
//! and the "New Key" form.

use egui::{Color32, RichText, Ui};
use navdb_core::{Pending, QueryResult, Scope};

use super::Tab;
use crate::state::{Action, AppState, Loadable};
use crate::ui::style::icon;
use crate::ui::{error_text, loading};

/// Maximum number of list / sorted-set elements fetched at once.
const RANGE_LIMIT: usize = 1000;
const STREAM_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

impl KeyType {
    const ALL: [KeyType; 6] = [KeyType::String, KeyType::Hash, KeyType::List, KeyType::Set, KeyType::ZSet, KeyType::Stream];

    fn label(self) -> &'static str {
        match self {
            KeyType::String => "string",
            KeyType::Hash => "hash",
            KeyType::List => "list",
            KeyType::Set => "set",
            KeyType::ZSet => "zset",
            KeyType::Stream => "stream",
        }
    }

    fn color(self) -> Color32 {
        match self {
            KeyType::String => Color32::from_rgb(70, 130, 220),
            KeyType::Hash => Color32::from_rgb(160, 90, 210),
            KeyType::List => Color32::from_rgb(40, 160, 120),
            KeyType::Set => Color32::from_rgb(220, 140, 40),
            KeyType::ZSet => Color32::from_rgb(210, 70, 90),
            KeyType::Stream => Color32::from_rgb(90, 150, 160),
        }
    }

    fn parse(s: &str) -> Option<KeyType> {
        Self::ALL.iter().copied().find(|t| t.label() == s.trim())
    }

    /// Column headers for the element table: (a, b).
    fn headers(self) -> (&'static str, &'static str) {
        match self {
            KeyType::String => ("", "Value"),
            KeyType::Hash => ("Field", "Value"),
            KeyType::List => ("Index", "Value"),
            KeyType::Set => ("", "Member"),
            KeyType::ZSet => ("Member", "Score"),
            KeyType::Stream => ("ID", "Fields"),
        }
    }
}

/// One element of a collection key.
#[derive(Clone, Debug, Default)]
struct Elem {
    /// field / index / member / entry id
    a: String,
    /// value / score / formatted stream fields
    b: String,
    /// stream entry fields
    fields: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct KeyData {
    /// `None` when the key does not exist.
    ty: Option<KeyType>,
    raw_type: String,
    ttl: i64,
    /// String value.
    text: String,
    elems: Vec<Elem>,
    /// Total element count (LLEN, HLEN...).
    total: usize,
    encoding: String,
    memory: Option<i64>,
}

/// Inline element editor state.
#[derive(Clone, Debug, Default)]
struct ElemEdit {
    /// `None` = adding a new element.
    index: Option<usize>,
    a: String,
    b: String,
    /// List: push at head.
    head: bool,
    /// Stream: fields for XADD.
    fields: Vec<(String, String)>,
}

/// After a command batch finishes.
enum After {
    Reload,
    Renamed(String),
    Created(String),
}

pub struct RedisKeyTab {
    id: String,
    conn: String,
    db: String,
    key: String,
    is_new: bool,
    data: Loadable<KeyData>,
    op: Option<(Pending<()>, After, String, std::time::Instant)>,
    // string editor
    text: String,
    text_dirty: bool,
    json_view: bool,
    word_wrap: bool,
    // collection editor
    filter: String,
    selected: Option<usize>,
    edit: Option<ElemEdit>,
    // toolbar
    ttl_buf: String,
    rename_buf: Option<String>,
    // new key form
    new_name: String,
    new_type: KeyType,
    new_ttl: String,
    new_text: String,
    new_pairs: Vec<(String, String)>,
    new_lines: String,
}

fn q(s: &str) -> String {
    crate::commands::quote_redis(s).replace('\n', "\\n")
}

/// Values of an array reply (`#`, `Value` columns) or a single-value reply.
fn values(r: &QueryResult) -> Vec<String> {
    if r.columns.len() == 2 && r.columns[0].name == "#" {
        r.rows.iter().map(|row| row.get(1).map(|v| v.edit_text()).unwrap_or_default()).collect()
    } else {
        r.rows.iter().filter_map(|row| row.first()).filter(|v| !v.is_null()).map(|v| v.edit_text()).collect()
    }
}

/// Field/value pairs from a flat array reply (RESP2) or a map reply (RESP3).
fn pairs(r: &QueryResult) -> Vec<(String, String)> {
    if r.columns.len() == 2 && r.columns[0].name == "Field" {
        return r.rows.iter().map(|row| (row[0].edit_text(), row[1].edit_text())).collect();
    }
    let v = values(r);
    // RESP3 nested pairs come back as "a\nb" per element
    if !v.is_empty() && v.iter().all(|s| s.contains('\n')) {
        return v.iter().filter_map(|s| s.split_once('\n').map(|(a, b)| (a.to_string(), b.to_string()))).collect();
    }
    v.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect()
}

fn scalar(r: &QueryResult) -> String {
    r.rows.first().and_then(|row| row.first()).map(|v| v.edit_text()).unwrap_or_default()
}

fn exec1(d: &mut dyn navdb_core::driver::Driver, scope: &Scope, cmd: &str) -> navdb_core::Result<QueryResult> {
    let mut v = d.execute(Some(scope), cmd)?;
    v.pop().ok_or_else(|| navdb_core::anyhow!("no reply"))
}

fn load_key(d: &mut dyn navdb_core::driver::Driver, scope: &Scope, key: &str) -> navdb_core::Result<KeyData> {
    let k = q(key);
    let raw_type = scalar(&exec1(d, scope, &format!("TYPE {}", k))?);
    let ttl = scalar(&exec1(d, scope, &format!("TTL {}", k))?).trim().parse::<i64>().unwrap_or(-1);
    let ty = KeyType::parse(&raw_type);
    let mut data = KeyData { ty, raw_type: raw_type.clone(), ttl, text: String::new(), elems: vec![], total: 0, encoding: String::new(), memory: None };
    if raw_type == "none" {
        return Ok(data);
    }
    data.encoding = exec1(d, scope, &format!("OBJECT ENCODING {}", k)).map(|r| scalar(&r)).unwrap_or_default();
    data.memory = exec1(d, scope, &format!("MEMORY USAGE {}", k)).ok().and_then(|r| scalar(&r).trim().parse().ok());
    let count = |d: &mut dyn navdb_core::driver::Driver, cmd: &str| -> usize {
        exec1(d, scope, &format!("{} {}", cmd, k)).ok().and_then(|r| scalar(&r).trim().parse().ok()).unwrap_or(0)
    };
    match ty {
        Some(KeyType::String) => {
            let r = exec1(d, scope, &format!("GET {}", k))?;
            data.text = scalar(&r);
            data.total = data.text.len();
        }
        Some(KeyType::Hash) => {
            let r = exec1(d, scope, &format!("HGETALL {}", k))?;
            data.elems = pairs(&r).into_iter().map(|(a, b)| Elem { a, b, ..Default::default() }).collect();
            data.elems.sort_by(|x, y| x.a.cmp(&y.a));
            data.total = data.elems.len();
        }
        Some(KeyType::List) => {
            data.total = count(d, "LLEN");
            let r = exec1(d, scope, &format!("LRANGE {} 0 {}", k, RANGE_LIMIT - 1))?;
            data.elems = values(&r).into_iter().enumerate().map(|(i, b)| Elem { a: i.to_string(), b, ..Default::default() }).collect();
        }
        Some(KeyType::Set) => {
            data.total = count(d, "SCARD");
            let r = exec1(d, scope, &format!("SMEMBERS {}", k))?;
            let mut m = values(&r);
            m.sort();
            data.elems = m.into_iter().map(|b| Elem { b, ..Default::default() }).collect();
        }
        Some(KeyType::ZSet) => {
            data.total = count(d, "ZCARD");
            let r = exec1(d, scope, &format!("ZRANGE {} 0 {} WITHSCORES", k, RANGE_LIMIT - 1))?;
            data.elems = pairs(&r).into_iter().map(|(a, b)| Elem { a, b, ..Default::default() }).collect();
        }
        Some(KeyType::Stream) => {
            data.total = count(d, "XLEN");
            let r = exec1(d, scope, &format!("XRANGE {} - + COUNT {}", k, STREAM_LIMIT))?;
            for v in values(&r) {
                let mut lines = v.split('\n');
                let id = lines.next().unwrap_or_default().to_string();
                let rest: Vec<&str> = lines.collect();
                let fields: Vec<(String, String)> = rest.chunks(2).map(|c| (c[0].to_string(), c.get(1).copied().unwrap_or_default().to_string())).collect();
                let b = fields.iter().map(|(f, v)| format!("{}={}", f, v)).collect::<Vec<_>>().join(", ");
                data.elems.push(Elem { a: id, b, fields });
            }
        }
        None => {}
    }
    Ok(data)
}

fn format_ttl(ttl: i64) -> String {
    match ttl {
        -1 => "No limit".into(),
        -2 => "Expired".into(),
        t if t >= 86400 => format!("{} s ({}d {}h)", t, t / 86400, (t % 86400) / 3600),
        t if t >= 3600 => format!("{} s ({}h {}m)", t, t / 3600, (t % 3600) / 60),
        t if t >= 60 => format!("{} s ({}m {}s)", t, t / 60, t % 60),
        t => format!("{} s", t),
    }
}

fn badge(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::WHITE).strong().small());
        });
}

impl RedisKeyTab {
    pub fn new(st: &mut AppState, conn: &str, db: String, key: String) -> Self {
        let is_new = key.is_empty();
        let mut t = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            db,
            key,
            is_new,
            data: Loadable::NotLoaded,
            op: None,
            text: String::new(),
            text_dirty: false,
            json_view: false,
            word_wrap: true,
            filter: String::new(),
            selected: None,
            edit: None,
            ttl_buf: String::new(),
            rename_buf: None,
            new_name: String::new(),
            new_type: KeyType::String,
            new_ttl: String::new(),
            new_text: String::new(),
            new_pairs: vec![(String::new(), String::new())],
            new_lines: String::new(),
        };
        if !is_new {
            t.reload(st);
        }
        t
    }

    pub fn new_key(st: &mut AppState, conn: &str, db: String) -> Self {
        Self::new(st, conn, db, String::new())
    }

    fn scope(&self) -> Scope {
        Scope::new(self.db.clone(), None)
    }

    fn reload(&mut self, st: &mut AppState) {
        let Some(session) = st.session(&self.conn) else {
            self.data = Loadable::Failed("connection not found".into());
            return;
        };
        let scope = self.scope();
        let key = self.key.clone();
        self.data = Loadable::Loading(session.run(move |d| load_key(d, &scope, &key)));
        self.edit = None;
        self.selected = None;
    }

    /// Run redis-cli style commands sequentially on the shared session.
    fn exec(&mut self, st: &mut AppState, cmds: Vec<String>, after: After) {
        if self.op.is_some() {
            return;
        }
        let Some(session) = st.session(&self.conn) else {
            st.error("connection not found");
            return;
        };
        let scope = self.scope();
        let text = cmds.join("\n");
        let pending = session.run(move |d| {
            for c in &cmds {
                d.execute(Some(&scope), c)?;
            }
            Ok(())
        });
        self.op = Some((pending, after, text, std::time::Instant::now()));
    }

    fn poll(&mut self, st: &mut AppState) {
        if self.data.poll() {
            if let Some(d) = self.data.ready() {
                self.text = d.text.clone();
                self.text_dirty = false;
                self.json_view = false;
                self.ttl_buf = if d.ttl >= 0 { d.ttl.to_string() } else { String::new() };
            }
        }
        let done = match &mut self.op {
            Some((p, _, _, _)) => p.poll(),
            None => None,
        };
        if let Some(res) = done {
            let Some((_, after, text, started)) = self.op.take() else { return };
            let scope = self.scope();
            let elapsed = started.elapsed().as_millis();
            match res {
                Ok(()) => {
                    st.record(&self.conn, Some(&scope), &text, true, "OK", elapsed);
                    match after {
                        After::Reload => st.toast("Saved"),
                        After::Renamed(n) => {
                            self.key = n;
                            st.refresh_scope(&self.conn, &scope);
                            st.toast("Key renamed");
                        }
                        After::Created(n) => {
                            self.key = n;
                            self.is_new = false;
                            st.refresh_scope(&self.conn, &scope);
                            st.toast("Key created");
                        }
                    }
                    self.reload(st);
                }
                Err(e) => {
                    let msg = format!("{:#}", e);
                    st.record(&self.conn, Some(&scope), &text, false, &msg, elapsed);
                    st.error(msg);
                }
            }
        }
    }

    // ------------------------------------------------------------------ toolbar

    fn toolbar(&mut self, ui: &mut Ui, st: &mut AppState) {
        let busy = self.op.is_some() || self.data.is_loading();
        let k = q(&self.key);
        let mut cmds: Option<(Vec<String>, After)> = None;
        ui.horizontal(|ui| {
            if let Some(d) = self.data.ready() {
                match d.ty {
                    Some(t) => badge(ui, &t.label().to_uppercase(), t.color()),
                    None => badge(ui, &d.raw_type.to_uppercase(), Color32::GRAY),
                }
            }
            if let Some(buf) = &mut self.rename_buf {
                let r = ui.add(egui::TextEdit::singleline(buf).desired_width(260.0).min_size(egui::vec2(260.0, 0.0)).font(egui::TextStyle::Monospace));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("✔ Rename").clicked() || enter {
                    let n = buf.clone();
                    if !n.is_empty() && n != self.key {
                        cmds = Some((vec![format!("RENAMENX {} {}", k, q(&n))], After::Renamed(n)));
                    }
                    self.rename_buf = None;
                } else if ui.button("Cancel").clicked() {
                    self.rename_buf = None;
                }
            } else {
                ui.label(RichText::new(&self.key).monospace().strong().size(14.0));
                if ui.small_button("✏").on_hover_text("Rename key").clicked() {
                    self.rename_buf = Some(self.key.clone());
                }
                if ui.small_button("🗐").on_hover_text("Copy key name").clicked() {
                    ui.ctx().copy_text(self.key.clone());
                }
            }
            ui.separator();
            ui.label("TTL:");
            ui.add(egui::TextEdit::singleline(&mut self.ttl_buf).desired_width(70.0).min_size(egui::vec2(70.0, 0.0)).hint_text("seconds"));
            if ui.add_enabled(!busy, egui::Button::new("Set TTL")).on_hover_text("EXPIRE (empty or ≤ 0 = PERSIST)").clicked() {
                let c = match self.ttl_buf.trim().parse::<i64>() {
                    Ok(n) if n > 0 => format!("EXPIRE {} {}", k, n),
                    _ => format!("PERSIST {}", k),
                };
                cmds = Some((vec![c], After::Reload));
            }
            if ui.add_enabled(!busy, egui::Button::new("Persist")).clicked() {
                cmds = Some((vec![format!("PERSIST {}", k)], After::Reload));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new(format!("{} Delete", icon::DELETE))).clicked() {
                    let conn = self.conn.clone();
                    let scope = self.scope();
                    let tab_key = self.key();
                    let cmd = format!("DEL {}", k);
                    st.open_dialog(crate::dialogs::confirm::ConfirmDialog::new(
                        "Delete Key",
                        format!("Are you sure you want to delete key \"{}\"?", self.key),
                        Box::new(move |st: &mut AppState| {
                            st.run_task(&conn, Some(scope.clone()), cmd.clone(), "Key deleted", true);
                            st.push(Action::CloseTabByKey(tab_key.clone()));
                        }),
                    ));
                }
                if ui.add_enabled(!busy, egui::Button::new(format!("{} Refresh", icon::REFRESH))).clicked() {
                    self.reload(st);
                }
                if busy {
                    ui.spinner();
                }
                if let Some(d) = self.data.ready() {
                    let mut info = vec![format!("TTL: {}", format_ttl(d.ttl))];
                    if d.ty.is_some() && d.ty != Some(KeyType::String) {
                        info.push(format!("{} elements", d.total));
                    } else if d.ty == Some(KeyType::String) {
                        info.push(format!("{} bytes", d.total));
                    }
                    if !d.encoding.is_empty() {
                        info.push(format!("encoding: {}", d.encoding));
                    }
                    if let Some(m) = d.memory {
                        info.push(format!("memory: {}", crate::ui::format_bytes(m)));
                    }
                    // truncate instead of overflowing onto the TTL buttons in narrow windows
                    ui.add(egui::Label::new(RichText::new(info.join("  ·  ")).weak()).truncate());
                }
            });
        });
        if let Some((c, a)) = cmds {
            self.exec(st, c, a);
        }
    }

    // ------------------------------------------------------------------ string

    fn string_editor(&mut self, ui: &mut Ui, st: &mut AppState, ttl: i64) {
        let k = q(&self.key);
        let mut save = false;
        ui.horizontal(|ui| {
            let busy = self.op.is_some();
            if ui.add_enabled(!busy && self.text_dirty, egui::Button::new(format!("{} Save", icon::SAVE))).clicked() {
                save = true;
            }
            if ui.button("🗐 Copy Value").clicked() {
                ui.ctx().copy_text(self.text.clone());
            }
            let is_json = serde_json::from_str::<serde_json::Value>(&self.text).is_ok();
            let was = self.json_view;
            ui.add_enabled(is_json || self.json_view, egui::Checkbox::new(&mut self.json_view, "JSON pretty"));
            if was != self.json_view {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&self.text) {
                    let new = if self.json_view { serde_json::to_string_pretty(&v) } else { serde_json::to_string(&v) };
                    if let Ok(s) = new {
                        // pretty-printing is only a view change; compact form is what gets saved
                        self.text = s;
                    }
                }
            }
            ui.checkbox(&mut self.word_wrap, "Word wrap");
            if is_json {
                ui.label(RichText::new("✔ valid JSON").color(Color32::from_rgb(60, 160, 90)).small());
            }
            if self.text_dirty {
                ui.label(RichText::new("■ modified").color(Color32::from_rgb(220, 160, 40)).small());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} chars", self.text.chars().count())).weak());
            });
        });
        let save_key = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        let wrap = self.word_wrap;
        let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let job = egui::text::LayoutJob::simple(text.to_owned(), font, ui.visuals().text_color(), if wrap { wrap_width } else { f32::INFINITY });
            ui.fonts(|f| f.layout_job(job))
        };
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let te = egui::TextEdit::multiline(&mut self.text)
                .code_editor()
                .desired_width(if wrap { ui.available_width() } else { f32::INFINITY })
                .min_size(ui.available_size())
                .layouter(&mut layouter);
            if ui.add(te).changed() {
                self.text_dirty = true;
            }
        });
        if (save || (save_key && self.text_dirty)) && self.op.is_none() {
            let value = if self.json_view {
                serde_json::from_str::<serde_json::Value>(&self.text)
                    .ok()
                    .and_then(|v| serde_json::to_string(&v).ok())
                    .unwrap_or_else(|| self.text.clone())
            } else {
                self.text.clone()
            };
            let mut cmds = vec![format!("SET {} {}", k, q(&value))];
            if ttl > 0 {
                cmds.push(format!("EXPIRE {} {}", k, ttl));
            }
            self.exec(st, cmds, After::Reload);
        }
    }

    // ------------------------------------------------------------------ collections

    fn collection_editor(&mut self, ui: &mut Ui, st: &mut AppState, data: &KeyData, ty: KeyType) {
        let k = q(&self.key);
        let busy = self.op.is_some();
        let mut cmds: Option<Vec<String>> = None;
        ui.horizontal(|ui| {
            let add_label = match ty {
                KeyType::List => "Push",
                KeyType::Stream => "Add Entry",
                _ => "Add",
            };
            if ui.add_enabled(!busy, egui::Button::new(format!("{} {}", icon::ADD, add_label))).clicked() {
                self.edit = Some(ElemEdit {
                    index: None,
                    a: if ty == KeyType::Stream { "*".into() } else if ty == KeyType::ZSet { String::new() } else { String::new() },
                    b: if ty == KeyType::ZSet { "0".into() } else { String::new() },
                    head: false,
                    fields: vec![(String::new(), String::new())],
                });
            }
            let sel = self.selected.and_then(|i| data.elems.get(i).cloned().map(|e| (i, e)));
            if ui.add_enabled(!busy && sel.is_some() && ty != KeyType::Stream, egui::Button::new(format!("{} Edit", icon::DESIGN))).clicked() {
                if let Some((i, e)) = &sel {
                    self.edit = Some(ElemEdit { index: Some(*i), a: e.a.clone(), b: e.b.clone(), head: false, fields: vec![] });
                }
            }
            if ui.add_enabled(!busy && sel.is_some(), egui::Button::new(format!("{} Delete", icon::DELETE))).clicked() {
                if let Some((_, e)) = &sel {
                    cmds = Some(delete_cmds(ty, &k, e));
                    self.selected = None;
                }
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new("🗐 Copy")).clicked() {
                if let Some((_, e)) = &sel {
                    let t = match ty {
                        KeyType::Hash | KeyType::List | KeyType::Set => e.b.clone(),
                        KeyType::ZSet => e.a.clone(),
                        _ => format!("{} {}", e.a, e.b),
                    };
                    ui.ctx().copy_text(t);
                }
            }
            ui.separator();
            ui.label("🔍");
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(180.0).min_size(egui::vec2(180.0, 0.0)).hint_text("Filter"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let shown = data.elems.len();
                let note = if shown < data.total { format!("showing {} of {}", shown, data.total) } else { format!("{} elements", data.total.max(shown)) };
                ui.label(RichText::new(note).weak());
            });
        });

        // element editor
        if self.edit.is_some() {
            let mut close = false;
            let mut submit = false;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let Some(ed) = &mut self.edit else { return };
                let title = if ed.index.is_some() { "Edit element" } else { "New element" };
                ui.label(RichText::new(title).strong());
                egui::Grid::new(("redis-edit", &self.id)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    match ty {
                        KeyType::Hash => {
                            ui.label("Field");
                            ui.add(egui::TextEdit::singleline(&mut ed.a).desired_width(400.0).min_size(egui::vec2(400.0, 0.0)));
                            ui.end_row();
                            ui.label("Value");
                            ui.add(egui::TextEdit::multiline(&mut ed.b).desired_width(400.0).desired_rows(3));
                            ui.end_row();
                        }
                        KeyType::List => {
                            if ed.index.is_some() {
                                ui.label("Index");
                                ui.label(&ed.a);
                                ui.end_row();
                            } else {
                                ui.label("Position");
                                ui.horizontal(|ui| {
                                    ui.radio_value(&mut ed.head, true, "Head (LPUSH)");
                                    ui.radio_value(&mut ed.head, false, "Tail (RPUSH)");
                                });
                                ui.end_row();
                            }
                            ui.label("Value");
                            ui.add(egui::TextEdit::multiline(&mut ed.b).desired_width(400.0).desired_rows(3));
                            ui.end_row();
                        }
                        KeyType::Set => {
                            ui.label("Member");
                            ui.add(egui::TextEdit::multiline(&mut ed.b).desired_width(400.0).desired_rows(2));
                            ui.end_row();
                        }
                        KeyType::ZSet => {
                            ui.label("Member");
                            ui.add(egui::TextEdit::singleline(&mut ed.a).desired_width(400.0).min_size(egui::vec2(400.0, 0.0)));
                            ui.end_row();
                            ui.label("Score");
                            ui.add(egui::TextEdit::singleline(&mut ed.b).desired_width(140.0).min_size(egui::vec2(140.0, 0.0)));
                            ui.end_row();
                        }
                        KeyType::Stream => {
                            ui.label("ID");
                            ui.add(egui::TextEdit::singleline(&mut ed.a).desired_width(200.0).min_size(egui::vec2(200.0, 0.0)).hint_text("*"));
                            ui.end_row();
                            ui.label("Fields");
                            ui.vertical(|ui| {
                                pair_editor(ui, &mut ed.fields, "Field", "Value");
                            });
                            ui.end_row();
                        }
                        KeyType::String => {}
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("✔ Apply").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if submit {
                if let Some(ed) = self.edit.clone() {
                    let old = ed.index.and_then(|i| data.elems.get(i));
                    match upsert_cmds(ty, &k, &ed, old) {
                        Ok(c) => {
                            cmds = Some(c);
                            close = true;
                        }
                        Err(e) => st.error(e),
                    }
                }
            }
            if close {
                self.edit = None;
            }
        }

        // element table
        let (ha, hb) = ty.headers();
        let filter = self.filter.to_lowercase();
        let rows: Vec<usize> = (0..data.elems.len())
            .filter(|&i| {
                filter.is_empty() || {
                    let e = &data.elems[i];
                    e.a.to_lowercase().contains(&filter) || e.b.to_lowercase().contains(&filter)
                }
            })
            .collect();
        let row_h = 22.0;
        let show_a = !ha.is_empty();
        let mut dbl: Option<usize> = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let mut tb = egui_extras::TableBuilder::new(ui).striped(true).resizable(true).sense(egui::Sense::click()).column(egui_extras::Column::exact(50.0));
            if show_a {
                tb = tb.column(egui_extras::Column::initial(if ty == KeyType::Stream { 170.0 } else { 220.0 }).at_least(60.0).clip(true));
            }
            tb.column(egui_extras::Column::remainder().at_least(80.0).clip(true))
                .header(row_h, |mut h| {
                    h.col(|ui| {
                        ui.strong("#");
                    });
                    if show_a {
                        h.col(|ui| {
                            ui.strong(ha);
                        });
                    }
                    h.col(|ui| {
                        ui.strong(hb);
                    });
                })
                .body(|body| {
                    body.rows(row_h, rows.len(), |mut row| {
                        let i = rows[row.index()];
                        let e = &data.elems[i];
                        let selected = self.selected == Some(i);
                        row.set_selected(selected);
                        let mut cells = Vec::new();
                        cells.push(row.col(|ui| {
                            ui.label(RichText::new((i + 1).to_string()).weak());
                        }));
                        if show_a {
                            cells.push(row.col(|ui| {
                                ui.add(egui::Label::new(RichText::new(one_line(&e.a)).monospace()).truncate().selectable(false));
                            }));
                        }
                        cells.push(row.col(|ui| {
                            ui.add(egui::Label::new(RichText::new(one_line(&e.b)).monospace()).truncate().selectable(false));
                        }));
                        let resp = row.response();
                        if resp.clicked() || cells.iter().any(|(_, r)| r.clicked()) {
                            self.selected = Some(i);
                        }
                        if resp.double_clicked() || cells.iter().any(|(_, r)| r.double_clicked()) {
                            dbl = Some(i);
                        }
                    });
                });
        });
        if let Some(i) = dbl {
            if ty != KeyType::Stream && !busy {
                let e = &data.elems[i];
                self.edit = Some(ElemEdit { index: Some(i), a: e.a.clone(), b: e.b.clone(), head: false, fields: vec![] });
            }
        }
        // stream entry details
        if ty == KeyType::Stream {
            if let Some(e) = self.selected.and_then(|i| data.elems.get(i)) {
                ui.separator();
                ui.label(RichText::new(format!("Entry {}", e.a)).strong());
                let rows: Vec<(String, String)> = e.fields.clone();
                crate::ui::kv_grid(ui, &format!("stream-entry-{}", self.id), &rows);
            }
        }
        if let Some(c) = cmds {
            self.exec(st, c, After::Reload);
        }
    }

    // ------------------------------------------------------------------ new key form

    fn new_key_form(&mut self, ui: &mut Ui, st: &mut AppState) {
        ui.heading(format!("{} New Key", icon::KEY));
        ui.label(RichText::new(format!("{} · {}", st.conn_name(&self.conn), self.db)).weak());
        ui.add_space(8.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("new-key", &self.id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Key name");
                ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(360.0).min_size(egui::vec2(360.0, 0.0)).hint_text("e.g. user:1001"));
                ui.end_row();
                ui.label("Type");
                ui.horizontal(|ui| {
                    for t in KeyType::ALL {
                        ui.selectable_value(&mut self.new_type, t, t.label());
                    }
                });
                ui.end_row();
                ui.label("TTL (seconds)");
                ui.add(egui::TextEdit::singleline(&mut self.new_ttl).desired_width(120.0).min_size(egui::vec2(120.0, 0.0)).hint_text("no limit"));
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Initial value").strong());
            match self.new_type {
                KeyType::String => {
                    ui.add(egui::TextEdit::multiline(&mut self.new_text).code_editor().desired_width(f32::INFINITY).desired_rows(10));
                }
                KeyType::Hash => pair_editor(ui, &mut self.new_pairs, "Field", "Value"),
                KeyType::ZSet => pair_editor(ui, &mut self.new_pairs, "Member", "Score"),
                KeyType::Stream => pair_editor(ui, &mut self.new_pairs, "Field", "Value"),
                KeyType::List | KeyType::Set => {
                    ui.label(RichText::new("One element per line").weak());
                    ui.add(egui::TextEdit::multiline(&mut self.new_lines).code_editor().desired_width(f32::INFINITY).desired_rows(10));
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let busy = self.op.is_some();
                if ui.add_enabled(!busy && !self.new_name.trim().is_empty(), egui::Button::new(RichText::new(format!("{} Create Key", icon::SAVE)).strong())).clicked() {
                    match self.create_cmds() {
                        Ok(cmds) => {
                            let name = self.new_name.trim().to_string();
                            self.create(st, name, cmds);
                        }
                        Err(e) => st.error(e),
                    }
                }
                if busy {
                    ui.spinner();
                }
            });
        });
    }

    fn create_cmds(&self) -> Result<Vec<String>, String> {
        let name = self.new_name.trim();
        let k = q(name);
        let mut cmds = Vec::new();
        let filled: Vec<&(String, String)> = self.new_pairs.iter().filter(|(a, _)| !a.is_empty()).collect();
        let lines: Vec<&str> = self.new_lines.lines().filter(|l| !l.is_empty()).collect();
        match self.new_type {
            KeyType::String => cmds.push(format!("SET {} {}", k, q(&self.new_text))),
            KeyType::Hash => {
                if filled.is_empty() {
                    return Err("A hash needs at least one field".into());
                }
                let args: Vec<String> = filled.iter().map(|(f, v)| format!("{} {}", q(f), q(v))).collect();
                cmds.push(format!("HSET {} {}", k, args.join(" ")));
            }
            KeyType::List | KeyType::Set => {
                if lines.is_empty() {
                    return Err("Enter at least one element".into());
                }
                let args: Vec<String> = lines.iter().map(|l| q(l)).collect();
                let c = if self.new_type == KeyType::List { "RPUSH" } else { "SADD" };
                cmds.push(format!("{} {} {}", c, k, args.join(" ")));
            }
            KeyType::ZSet => {
                if filled.is_empty() {
                    return Err("A sorted set needs at least one member".into());
                }
                let mut args = Vec::new();
                for (m, s) in &filled {
                    let score: f64 = s.trim().parse().map_err(|_| format!("Invalid score for member \"{}\"", m))?;
                    args.push(format!("{} {}", score, q(m)));
                }
                cmds.push(format!("ZADD {} {}", k, args.join(" ")));
            }
            KeyType::Stream => {
                if filled.is_empty() {
                    return Err("A stream entry needs at least one field".into());
                }
                let args: Vec<String> = filled.iter().map(|(f, v)| format!("{} {}", q(f), q(v))).collect();
                cmds.push(format!("XADD {} * {}", k, args.join(" ")));
            }
        }
        if let Ok(n) = self.new_ttl.trim().parse::<i64>() {
            if n > 0 {
                cmds.push(format!("EXPIRE {} {}", k, n));
            }
        }
        Ok(cmds)
    }

    fn create(&mut self, st: &mut AppState, name: String, cmds: Vec<String>) {
        let Some(session) = st.session(&self.conn) else {
            st.error("connection not found");
            return;
        };
        let scope = self.scope();
        let text = cmds.join("\n");
        let check = format!("EXISTS {}", q(&name));
        let pending = session.run(move |d| {
            let r = exec1(d, &scope, &check)?;
            if scalar(&r).trim() != "0" {
                navdb_core::bail!("key already exists");
            }
            for c in &cmds {
                d.execute(Some(&scope), c)?;
            }
            Ok(())
        });
        self.op = Some((pending, After::Created(name), text, std::time::Instant::now()));
    }
}

fn one_line(s: &str) -> String {
    let t: String = s.chars().take(300).map(|c| if c == '\n' || c == '\r' { '¶' } else { c }).collect();
    t
}

/// Editable list of (a, b) pairs with add/remove buttons.
fn pair_editor(ui: &mut Ui, rows: &mut Vec<(String, String)>, ha: &str, hb: &str) {
    let mut remove = None;
    egui::Grid::new(ui.next_auto_id()).num_columns(3).spacing([6.0, 4.0]).show(ui, |ui| {
        ui.label(RichText::new(ha).weak());
        ui.label(RichText::new(hb).weak());
        ui.label("");
        ui.end_row();
        for (i, (a, b)) in rows.iter_mut().enumerate() {
            ui.add(egui::TextEdit::singleline(a).desired_width(200.0).min_size(egui::vec2(200.0, 0.0)));
            ui.add(egui::TextEdit::singleline(b).desired_width(300.0).min_size(egui::vec2(300.0, 0.0)));
            if ui.small_button(icon::DELETE).clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove {
        rows.remove(i);
    }
    if ui.small_button(format!("{} Add row", icon::ADD)).clicked() {
        rows.push((String::new(), String::new()));
    }
}

fn delete_cmds(ty: KeyType, k: &str, e: &Elem) -> Vec<String> {
    match ty {
        KeyType::Hash => vec![format!("HDEL {} {}", k, q(&e.a))],
        KeyType::Set => vec![format!("SREM {} {}", k, q(&e.b))],
        KeyType::ZSet => vec![format!("ZREM {} {}", k, q(&e.a))],
        KeyType::Stream => vec![format!("XDEL {} {}", k, q(&e.a))],
        KeyType::List => {
            // mark the exact index, then remove the marker
            let marker = format!("__navdb_deleted_{}__", uuid::Uuid::new_v4());
            vec![format!("LSET {} {} {}", k, e.a, q(&marker)), format!("LREM {} 1 {}", k, q(&marker))]
        }
        KeyType::String => vec![],
    }
}

fn upsert_cmds(ty: KeyType, k: &str, ed: &ElemEdit, old: Option<&Elem>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    match ty {
        KeyType::Hash => {
            if ed.a.is_empty() {
                return Err("Field name is required".into());
            }
            if let Some(o) = old {
                if o.a != ed.a {
                    out.push(format!("HDEL {} {}", k, q(&o.a)));
                }
            }
            out.push(format!("HSET {} {} {}", k, q(&ed.a), q(&ed.b)));
        }
        KeyType::List => match old {
            Some(o) => out.push(format!("LSET {} {} {}", k, o.a, q(&ed.b))),
            None => out.push(format!("{} {} {}", if ed.head { "LPUSH" } else { "RPUSH" }, k, q(&ed.b))),
        },
        KeyType::Set => {
            if let Some(o) = old {
                if o.b == ed.b {
                    return Ok(out);
                }
                out.push(format!("SREM {} {}", k, q(&o.b)));
            }
            out.push(format!("SADD {} {}", k, q(&ed.b)));
        }
        KeyType::ZSet => {
            let score: f64 = ed.b.trim().parse().map_err(|_| "Score must be a number".to_string())?;
            if let Some(o) = old {
                if o.a != ed.a {
                    out.push(format!("ZREM {} {}", k, q(&o.a)));
                }
            }
            out.push(format!("ZADD {} {} {}", k, score, q(&ed.a)));
        }
        KeyType::Stream => {
            let fields: Vec<String> = ed.fields.iter().filter(|(f, _)| !f.is_empty()).map(|(f, v)| format!("{} {}", q(f), q(v))).collect();
            if fields.is_empty() {
                return Err("At least one field is required".into());
            }
            let id = if ed.a.trim().is_empty() { "*".to_string() } else { ed.a.trim().to_string() };
            out.push(format!("XADD {} {} {}", k, id, fields.join(" ")));
        }
        KeyType::String => {}
    }
    Ok(out)
}

impl Tab for RedisKeyTab {
    fn title(&self) -> String {
        if self.is_new {
            "New Key".into()
        } else {
            format!("{} @{}", self.key, self.db)
        }
    }

    fn key(&self) -> String {
        if self.is_new {
            format!("redis:{}:{}:new:{}", self.conn, self.db, self.id)
        } else {
            format!("redis:{}:{}:{}", self.conn, self.db, self.key)
        }
    }

    fn dirty(&self) -> bool {
        self.text_dirty
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.poll(st);
        if self.is_new {
            self.new_key_form(ui, st);
            return;
        }
        self.toolbar(ui, st);
        ui.separator();
        let data = match &self.data {
            Loadable::NotLoaded => {
                self.reload(st);
                return;
            }
            Loadable::Loading(_) => {
                loading(ui, "Loading key...");
                return;
            }
            Loadable::Failed(e) => {
                let e = e.clone();
                error_text(ui, &e);
                if ui.button("Retry").clicked() {
                    self.reload(st);
                }
                return;
            }
            Loadable::Ready(d) => d.clone(),
        };
        match data.ty {
            None if data.raw_type == "none" => {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("This key does not exist (it may have expired or been deleted).").weak());
                });
            }
            None => {
                ui.label(format!("Keys of type \"{}\" are not supported by the editor.", data.raw_type));
            }
            Some(KeyType::String) => self.string_editor(ui, st, data.ttl),
            Some(t) => self.collection_editor(ui, st, &data, t),
        }
    }
}
