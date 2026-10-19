//! Data Transfer: copy tables (structure and/or data) between connections,
//! databases and even different database kinds, or into a SQL file.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use egui::RichText;
use navdb_core::driver::{self, Driver};
use navdb_core::{ddl, dml, ConnectionConfig, Dialect, ObjectKind, Result, Scope};

use super::Dialog;
use crate::state::AppState;
use crate::ui;

use common::*;

/// Helpers shared by the Data Transfer, Structure/Data Synchronization and
/// Data Generation tools.
pub(crate) mod common {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use egui::{Color32, RichText, Ui};
    use navdb_core::driver::Driver;
    use navdb_core::{Dialect, ObjPath, ObjectInfo, ObjectKind, Result, Scope, TableDef, Value};

    use crate::state::{AppState, Loadable};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum LogLevel {
        Info,
        Ok,
        Warn,
        Error,
    }

    /// Progress of a background job, shared with the UI through a mutex.
    #[derive(Default)]
    pub struct Progress {
        pub log: Vec<(LogLevel, String)>,
        pub done: usize,
        pub total: usize,
        pub current: String,
        pub cancel: bool,
        pub errors: usize,
    }

    pub type Shared = Arc<Mutex<Progress>>;

    pub fn new_shared() -> Shared {
        Arc::new(Mutex::new(Progress::default()))
    }

    pub fn plog(p: &Shared, lvl: LogLevel, msg: impl Into<String>) {
        if let Ok(mut g) = p.lock() {
            if lvl == LogLevel::Error {
                g.errors += 1;
            }
            g.log.push((lvl, msg.into()));
        }
    }

    pub fn set_current(p: &Shared, msg: impl Into<String>) {
        if let Ok(mut g) = p.lock() {
            g.current = msg.into();
        }
    }

    pub fn set_counts(p: &Shared, done: usize, total: usize) {
        if let Ok(mut g) = p.lock() {
            g.done = done;
            g.total = total;
        }
    }

    pub fn cancelled(p: &Shared) -> bool {
        p.lock().map(|g| g.cancel).unwrap_or(false)
    }

    pub fn request_cancel(p: &Shared) {
        if let Ok(mut g) = p.lock() {
            g.cancel = true;
        }
    }

    pub fn error_count(p: &Shared) -> usize {
        p.lock().map(|g| g.errors).unwrap_or(0)
    }

    /// Lifecycle of a background job started by a tool window.
    pub struct Job {
        pub pending: Option<navdb_core::Pending<()>>,
        pub progress: Shared,
        pub started: Option<Instant>,
        pub elapsed: Option<Duration>,
        pub result: Option<std::result::Result<(), String>>,
    }

    impl Default for Job {
        fn default() -> Self {
            Self { pending: None, progress: new_shared(), started: None, elapsed: None, result: None }
        }
    }

    impl Job {
        pub fn running(&self) -> bool {
            self.pending.is_some()
        }

        pub fn finished(&self) -> bool {
            self.result.is_some()
        }

        /// Reset and start a job; `f` gets the shared progress.
        pub fn start(&mut self, st: &AppState, f: impl FnOnce(Shared) -> Result<()> + Send + 'static) {
            self.progress = new_shared();
            self.started = Some(Instant::now());
            self.elapsed = None;
            self.result = None;
            let p = self.progress.clone();
            self.pending = Some(navdb_core::session::spawn_task(st.notify.clone(), move || f(p)));
        }

        /// Poll; returns true exactly once, when the job just finished.
        pub fn poll(&mut self, ctx: &egui::Context) -> bool {
            if let Some(p) = &mut self.pending {
                if let Some(r) = p.poll() {
                    self.pending = None;
                    self.elapsed = self.started.map(|s| s.elapsed());
                    if let Err(e) = &r {
                        plog(&self.progress, LogLevel::Error, format!("{:#}", e));
                    }
                    self.result = Some(r.map_err(|e| format!("{:#}", e)));
                    return true;
                }
                ctx.request_repaint_after(Duration::from_millis(150));
            }
            false
        }

        pub fn cancel(&self) {
            request_cancel(&self.progress);
        }

        pub fn elapsed_text(&self) -> String {
            let d = self.elapsed.or_else(|| self.started.map(|s| s.elapsed())).unwrap_or_default();
            format_duration(d)
        }
    }

    pub fn format_duration(d: Duration) -> String {
        let s = d.as_secs_f64();
        if s < 60.0 {
            format!("{:.2} s", s)
        } else {
            format!("{}m {:02}s", (s / 60.0) as u64, (s as u64) % 60)
        }
    }

    /// Progress bar + status + scrolling log.
    pub fn job_ui(ui: &mut Ui, job: &Job, unit: &str, log_height: f32) {
        let (done, total, current, log, errors) = match job.progress.lock() {
            Ok(g) => (g.done, g.total, g.current.clone(), g.log.clone(), g.errors),
            Err(_) => return,
        };
        let frac = if total > 0 { done as f32 / total as f32 } else if job.finished() { 1.0 } else { 0.0 };
        ui.horizontal(|ui| {
            if job.running() {
                ui.spinner();
            }
            let text = if total > 0 { format!("{} / {} {}", done, total, unit) } else { String::new() };
            ui.add(egui::ProgressBar::new(frac).text(text).desired_width(ui.available_width() - 4.0));
        });
        ui.horizontal(|ui| {
            let status = match &job.result {
                None if job.running() => RichText::new(if current.is_empty() { "Working…".to_string() } else { current }),
                None => RichText::new("Not started").weak(),
                Some(Ok(())) if errors == 0 => RichText::new("✔ Finished successfully").color(Color32::from_rgb(60, 170, 90)),
                Some(Ok(())) => RichText::new(format!("⚠ Finished with {} error(s)", errors)).color(Color32::from_rgb(220, 150, 40)),
                Some(Err(_)) => RichText::new("✖ Failed").color(ui.visuals().error_fg_color),
            };
            ui.label(status);
            if job.started.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("Elapsed: {}", job.elapsed_text())).weak());
                });
            }
        });
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(4.0)
            .inner_margin(6.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("job-log")
                    .max_height(log_height)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (lvl, msg) in &log {
                            let (icon, color) = match lvl {
                                LogLevel::Info => ("•", ui.visuals().weak_text_color()),
                                LogLevel::Ok => ("✔", Color32::from_rgb(60, 170, 90)),
                                LogLevel::Warn => ("⚠", Color32::from_rgb(220, 150, 40)),
                                LogLevel::Error => ("✖", ui.visuals().error_fg_color),
                            };
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(icon).color(color).monospace());
                                ui.label(RichText::new(msg).monospace().size(12.0));
                            });
                        }
                    });
            });
    }

    /// Step / page selector shown at the top of the tool windows.
    pub fn step_bar<T: PartialEq + Copy>(ui: &mut Ui, steps: &[(T, &str)], current: &mut T, enabled: impl Fn(T) -> bool) {
        ui.horizontal(|ui| {
            for (i, (s, label)) in steps.iter().enumerate() {
                if i > 0 {
                    ui.label(RichText::new("›").weak());
                }
                let sel = *current == *s;
                let text = RichText::new(format!("{}  {}", i + 1, label)).strong();
                let r = ui.add_enabled(enabled(*s), egui::SelectableLabel::new(sel, text));
                if r.clicked() {
                    *current = *s;
                }
            }
        });
    }

    /// Read-only code view with scrolling.
    pub fn code_view(ui: &mut Ui, id: &str, text: &str, height: f32) {
        egui::ScrollArea::both().id_salt(id).max_height(height).auto_shrink([false, false]).show(ui, |ui| {
            let mut t = text;
            ui.add(
                egui::TextEdit::multiline(&mut t)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(((height / 15.0) as usize).max(3)),
            );
        });
    }

    /// Small "Source"/"Target" header with a picker.
    pub fn endpoint(ui: &mut Ui, st: &mut AppState, title: &str, id: &str, conn: &mut Option<String>, scope: &mut Option<Scope>) -> bool {
        let mut changed = false;
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong());
            ui.horizontal(|ui| {
                changed = crate::ui::scope_picker(ui, id, st, conn, scope);
            });
            if let Some(c) = conn.as_ref().and_then(|c| st.config(c)) {
                ui.label(RichText::new(format!("{} {}", crate::ui::style::kind_icon(c.kind), c.kind.label())).weak().small());
            }
        });
        changed
    }

    pub enum Tables {
        NoScope,
        Loading,
        Failed(String),
        Ready(Vec<ObjectInfo>),
    }

    /// Tables of a scope from the navigator cache (loading them if needed).
    pub fn scope_tables(st: &mut AppState, conn: &Option<String>, scope: &Option<Scope>) -> Tables {
        let (Some(c), Some(s)) = (conn, scope) else { return Tables::NoScope };
        if st.session(c).is_none() {
            return Tables::Failed("connection not found".into());
        }
        let Some(oc) = st.conn_mut(c) else { return Tables::NoScope };
        oc.load_objects(s, ObjectKind::Table);
        match oc.objects(s, ObjectKind::Table) {
            Some(Loadable::Ready(v)) => Tables::Ready(v.clone()),
            Some(Loadable::Failed(e)) => Tables::Failed(e.clone()),
            _ => Tables::Loading,
        }
    }

    pub fn begin_sql(d: Dialect) -> &'static str {
        match d {
            Dialect::MySql => "START TRANSACTION",
            _ => "BEGIN",
        }
    }

    /// List the tables of a scope and read their definitions.
    pub fn load_defs(drv: &mut dyn Driver, scope: &Scope, p: &Shared, side: &str) -> Result<Vec<TableDef>> {
        set_current(p, format!("Reading {} table list…", side));
        let tables = drv.list_objects(scope, ObjectKind::Table)?;
        let mut out = Vec::new();
        for (i, t) in tables.iter().enumerate() {
            if cancelled(p) {
                anyhow::bail!("cancelled");
            }
            set_current(p, format!("Reading {} table {} ({}/{})", side, t.name, i + 1, tables.len()));
            match drv.table_def(&scope.path(t.name.clone())) {
                Ok(d) => out.push(d),
                Err(e) => plog(p, LogLevel::Error, format!("{} table {}: {:#}", side, t.name, e)),
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Cross-dialect type mapping
    // ------------------------------------------------------------------

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Cat {
        SmallInt,
        Int,
        BigInt,
        Decimal,
        Float,
        Double,
        Bool,
        Char,
        Varchar,
        Text,
        Blob,
        Date,
        Time,
        DateTime,
        Json,
        Uuid,
    }

    fn split_type(ty: &str) -> (String, String, bool) {
        let lower = ty.trim().to_ascii_lowercase();
        let unsigned = lower.contains("unsigned");
        let cleaned = lower.replace("unsigned", "").replace("zerofill", "");
        let (base, args) = match cleaned.find('(') {
            Some(p) => {
                let close = cleaned[p..].find(')').map(|c| p + c + 1).unwrap_or(cleaned.len());
                let rest = format!("{} {}", &cleaned[..p], &cleaned[close..]);
                (rest, cleaned[p..close].to_string())
            }
            None => (cleaned.clone(), String::new()),
        };
        let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
        (base, args.replace(' ', ""), unsigned)
    }

    fn classify(from: Dialect, ty: &str) -> (Cat, String) {
        let (base, args, unsigned) = split_type(ty);
        if base.ends_with("[]") || base.starts_with('_') {
            return (Cat::Text, String::new());
        }
        let first = base.split(' ').next().unwrap_or("").to_string();
        let cat = match first.as_str() {
            "tinyint" if args == "(1)" => Cat::Bool,
            "tinyint" | "smallint" | "int2" | "smallserial" => {
                if unsigned {
                    Cat::Int
                } else {
                    Cat::SmallInt
                }
            }
            "int" | "integer" | "int4" | "mediumint" | "serial" | "year" | "serial4" => {
                if unsigned || from == Dialect::Sqlite {
                    Cat::BigInt
                } else {
                    Cat::Int
                }
            }
            "bigint" | "int8" | "bigserial" | "serial8" => {
                if unsigned {
                    return (Cat::Decimal, "(20,0)".into());
                }
                Cat::BigInt
            }
            "decimal" | "numeric" | "dec" | "fixed" | "number" => Cat::Decimal,
            "money" => return (Cat::Decimal, "(19,2)".into()),
            "float" | "float4" => Cat::Float,
            "real" => {
                if from == Dialect::Postgres {
                    Cat::Float
                } else {
                    Cat::Double
                }
            }
            "double" | "float8" => Cat::Double,
            "bool" | "boolean" => Cat::Bool,
            "bit" if args.is_empty() || args == "(1)" => Cat::Bool,
            "bit" => Cat::BigInt,
            "char" | "nchar" | "bpchar" => Cat::Char,
            "character" if base.contains("varying") => Cat::Varchar,
            "character" => Cat::Char,
            "varchar" | "nvarchar" | "varchar2" | "nvarchar2" | "varying" => Cat::Varchar,
            "enum" | "set" => return (Cat::Varchar, "(255)".into()),
            "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "bytea" => Cat::Blob,
            "date" => Cat::Date,
            "time" | "timetz" => Cat::Time,
            "json" | "jsonb" => Cat::Json,
            "uuid" => Cat::Uuid,
            f if f.starts_with("timestamp") || f.starts_with("datetime") => Cat::DateTime,
            f if f.contains("int") => Cat::BigInt,
            f if f.contains("char") || f.contains("clob") => Cat::Varchar,
            _ => Cat::Text,
        };
        (cat, args)
    }

    fn is_int(c: Cat) -> bool {
        matches!(c, Cat::SmallInt | Cat::Int | Cat::BigInt)
    }

    /// Map a column type from one SQL dialect to another.
    /// `keyed`: the column is part of a key/index (MySQL needs bounded types there).
    pub fn map_type(from: Dialect, to: Dialect, ty: &str, auto_inc: bool, keyed: bool) -> String {
        if from == to {
            return ty.to_string();
        }
        if ty.trim().is_empty() {
            return match to {
                Dialect::Sqlite => "TEXT".into(),
                Dialect::MySql => if keyed { "varchar(255)" } else { "longtext" }.into(),
                Dialect::Postgres | Dialect::Generic => "text".into(),
                Dialect::SqlServer => if keyed { "nvarchar(255)" } else { "nvarchar(max)" }.into(),
                Dialect::Oracle => if keyed { "VARCHAR2(255)" } else { "CLOB" }.into(),
            };
        }
        let (mut cat, args) = classify(from, ty);
        if auto_inc && !is_int(cat) {
            cat = Cat::BigInt;
        }
        // SQLite AUTOINCREMENT only works on INTEGER PRIMARY KEY
        let len = |def: &str| if args.is_empty() { def.to_string() } else { args.clone() };
        match to {
            Dialect::Sqlite => match cat {
                Cat::SmallInt | Cat::Int | Cat::BigInt | Cat::Bool => "INTEGER".into(),
                Cat::Decimal => "NUMERIC".into(),
                Cat::Float | Cat::Double => "REAL".into(),
                Cat::Char | Cat::Varchar => {
                    if args.is_empty() {
                        "TEXT".into()
                    } else {
                        format!("VARCHAR{}", args)
                    }
                }
                Cat::Text | Cat::Json | Cat::Uuid => "TEXT".into(),
                Cat::Blob => "BLOB".into(),
                Cat::Date => "DATE".into(),
                Cat::Time => "TIME".into(),
                Cat::DateTime => "DATETIME".into(),
            },
            Dialect::MySql => match cat {
                Cat::SmallInt => "smallint".into(),
                Cat::Int => "int".into(),
                Cat::BigInt => "bigint".into(),
                Cat::Decimal => format!("decimal{}", len("(20,6)")),
                Cat::Float => "float".into(),
                Cat::Double => "double".into(),
                Cat::Bool => "tinyint(1)".into(),
                Cat::Char => format!("char{}", len("(1)")),
                Cat::Varchar => format!("varchar{}", len("(255)")),
                Cat::Text => if keyed { "varchar(255)" } else { "longtext" }.into(),
                Cat::Json => if keyed { "varchar(255)" } else { "json" }.into(),
                Cat::Uuid => "char(36)".into(),
                Cat::Blob => if keyed { "varbinary(255)" } else { "longblob" }.into(),
                Cat::Date => "date".into(),
                Cat::Time => "time".into(),
                Cat::DateTime => "datetime".into(),
            },
            Dialect::Postgres => match cat {
                Cat::SmallInt => "smallint".into(),
                Cat::Int => "integer".into(),
                Cat::BigInt => "bigint".into(),
                Cat::Decimal => format!("numeric{}", args),
                Cat::Float => "real".into(),
                Cat::Double => "double precision".into(),
                Cat::Bool => "boolean".into(),
                Cat::Char => format!("char{}", len("(1)")),
                Cat::Varchar => format!("varchar{}", args),
                Cat::Text => "text".into(),
                Cat::Json => "json".into(),
                Cat::Uuid => "uuid".into(),
                Cat::Blob => "bytea".into(),
                Cat::Date => "date".into(),
                Cat::Time => "time".into(),
                Cat::DateTime => "timestamp".into(),
            },
            Dialect::SqlServer => match cat {
                Cat::SmallInt => "smallint".into(),
                Cat::Int => "int".into(),
                Cat::BigInt => "bigint".into(),
                Cat::Decimal => format!("decimal{}", len("(20,6)")),
                Cat::Float => "real".into(),
                Cat::Double => "float".into(),
                Cat::Bool => "bit".into(),
                Cat::Char => format!("nchar{}", len("(1)")),
                Cat::Varchar => format!("nvarchar{}", len("(255)")),
                Cat::Text | Cat::Json => if keyed { "nvarchar(255)" } else { "nvarchar(max)" }.into(),
                Cat::Uuid => "uniqueidentifier".into(),
                Cat::Blob => if keyed { "varbinary(255)" } else { "varbinary(max)" }.into(),
                Cat::Date => "date".into(),
                Cat::Time => "time".into(),
                Cat::DateTime => "datetime2".into(),
            },
            Dialect::Oracle => match cat {
                Cat::SmallInt | Cat::Int | Cat::BigInt => "NUMBER(19)".into(),
                Cat::Bool => "NUMBER(1)".into(),
                Cat::Decimal => format!("NUMBER{}", args),
                Cat::Float => "BINARY_FLOAT".into(),
                Cat::Double => "BINARY_DOUBLE".into(),
                Cat::Char => format!("CHAR{}", len("(1)")),
                Cat::Varchar => format!("VARCHAR2{}", len("(255)")),
                Cat::Text | Cat::Json => if keyed { "VARCHAR2(255)" } else { "CLOB" }.into(),
                Cat::Uuid => "VARCHAR2(36)".into(),
                Cat::Blob => "BLOB".into(),
                Cat::Date | Cat::DateTime => "TIMESTAMP".into(),
                Cat::Time => "VARCHAR2(16)".into(),
            },
            Dialect::Generic => match cat {
                Cat::SmallInt => "SMALLINT".into(),
                Cat::Int => "INTEGER".into(),
                Cat::BigInt => "BIGINT".into(),
                Cat::Decimal => format!("DECIMAL{}", len("(20,6)")),
                Cat::Float => "REAL".into(),
                Cat::Double => "DOUBLE".into(),
                Cat::Bool => "BOOLEAN".into(),
                Cat::Char | Cat::Varchar | Cat::Text | Cat::Uuid => "VARCHAR".into(),
                Cat::Json => "JSON".into(),
                Cat::Blob => "BLOB".into(),
                Cat::Date => "DATE".into(),
                Cat::Time => "TIME".into(),
                Cat::DateTime => "TIMESTAMP".into(),
            },
        }
    }

    fn convert_default(def: &str, to: Dialect, dst_ty: &str) -> Option<String> {
        let mut d = def.trim().to_string();
        if d.is_empty() {
            return None;
        }
        // strip PostgreSQL casts: 'abc'::character varying, (0)::numeric
        if let Some(p) = d.find("::") {
            if d.starts_with('\'') || d.starts_with('(') || d.starts_with('-') || d.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                d = d[..p].to_string();
            }
        }
        let d = d.trim().trim_start_matches('(').trim_end_matches(')').trim().to_string();
        let upper = d.to_ascii_uppercase();
        let dst = dst_ty.to_ascii_lowercase();
        let dst_bool = dst.contains("bool") || dst == "tinyint(1)";
        let dst_dt = dst.contains("time") || dst.contains("date");
        if upper == "NULL" {
            return None;
        }
        if let Ok(n) = d.parse::<f64>() {
            if dst_bool && to == Dialect::Postgres {
                return Some(if n != 0.0 { "TRUE" } else { "FALSE" }.into());
            }
            return Some(d);
        }
        if upper == "TRUE" || upper == "FALSE" {
            return Some(if to == Dialect::Postgres { upper } else if upper == "TRUE" { "1".into() } else { "0".into() });
        }
        if d.starts_with('\'') && d.ends_with('\'') && d.len() >= 2 {
            if to == Dialect::MySql && (dst.contains("text") || dst.contains("blob") || dst == "json") {
                return None;
            }
            if dst_bool {
                let inner = d[1..d.len() - 1].to_ascii_lowercase();
                let t = matches!(inner.as_str(), "t" | "true" | "1" | "y" | "yes");
                return Some(match to {
                    Dialect::Postgres => if t { "TRUE" } else { "FALSE" }.into(),
                    _ => if t { "1" } else { "0" }.into(),
                });
            }
            return Some(d);
        }
        let ts = upper.starts_with("CURRENT_TIMESTAMP") || upper.starts_with("NOW(") || upper.starts_with("LOCALTIMESTAMP") || upper == "DATETIME('NOW')";
        if ts && dst_dt {
            return Some("CURRENT_TIMESTAMP".into());
        }
        if upper == "CURRENT_DATE" && dst.contains("date") && to != Dialect::MySql {
            return Some("CURRENT_DATE".into());
        }
        None
    }

    /// Retarget a table definition into `target` and convert it to dialect `to`.
    /// Returns the definition plus notes about dropped features.
    pub fn convert_table(def: &TableDef, from: Dialect, to: Dialect, target: &Scope) -> (TableDef, Vec<String>) {
        let mut notes = Vec::new();
        let mut t = def.clone();
        let src_db = def.path.db.clone();
        let src_schema = def.path.schema.clone();
        t.path = target.path(def.path.name.clone());
        for fk in t.foreign_keys.iter_mut() {
            let same_place = fk.ref_db.is_empty() || fk.ref_db == src_db || (fk.ref_schema.is_some() && fk.ref_schema == src_schema);
            if same_place || from != to {
                fk.ref_db = target.db.clone();
                fk.ref_schema = target.schema.clone();
            }
        }
        if from == to {
            return (t, notes);
        }
        let mut keyed: HashSet<String> = t.primary_key().into_iter().collect();
        for i in &t.indexes {
            keyed.extend(i.columns.iter().cloned());
        }
        for f in &t.foreign_keys {
            keyed.extend(f.columns.iter().cloned());
        }
        let pk = t.primary_key();
        for c in t.columns.iter_mut() {
            let mapped = map_type(from, to, &c.data_type, c.auto_increment, keyed.contains(&c.name));
            c.default = c.default.as_deref().and_then(|d| convert_default(d, to, &mapped));
            c.data_type = mapped;
            c.collation.clear();
            c.extra.clear();
            if to == Dialect::Sqlite && c.auto_increment && !(pk.len() == 1 && pk[0] == c.name) {
                c.auto_increment = false;
            }
            if !to.supports_comments() {
                c.comment.clear();
            }
        }
        t.engine.clear();
        t.charset.clear();
        t.collation.clear();
        t.auto_increment = None;
        if !to.supports_comments() {
            t.comment.clear();
        }
        let tname = t.path.name.clone();
        for idx in t.indexes.iter_mut() {
            idx.method.clear();
            // MySQL index names are per table; PG/SQLite need unique names per schema
            if from == Dialect::MySql && to != Dialect::MySql && !idx.primary && !idx.name.contains(&tname) {
                idx.name = format!("{}_{}", tname, idx.name);
            }
        }
        if !t.triggers.is_empty() {
            notes.push(format!("{} trigger(s) skipped (dialect specific)", t.triggers.len()));
            t.triggers.clear();
        }
        if !t.checks.is_empty() {
            notes.push(format!("{} check constraint(s) skipped (dialect specific)", t.checks.len()));
            t.checks.clear();
        }
        (t, notes)
    }

    fn decode_hex(s: &str) -> Option<Vec<u8>> {
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
    }

    /// Adjust a value read with source column type `src_ty` for target type `dst_ty`.
    pub fn fix_value(v: Value, src_ty: &str, dst_ty: &str) -> Value {
        let dst_bool = dst_ty.contains("bool");
        if dst_bool {
            return match v {
                Value::Int(n) => Value::Bool(n != 0),
                Value::UInt(n) => Value::Bool(n != 0),
                Value::Float(f) => Value::Bool(f != 0.0),
                Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "t" | "true" | "1" | "y" | "yes" | "on" => Value::Bool(true),
                    "f" | "false" | "0" | "n" | "no" | "off" => Value::Bool(false),
                    _ => Value::Text(s),
                },
                other => other,
            };
        }
        if src_ty.contains("bool") {
            return match v {
                Value::Bool(b) => Value::Int(b as i64),
                Value::Text(s) => match s.as_str() {
                    "t" | "true" => Value::Int(1),
                    "f" | "false" => Value::Int(0),
                    _ => Value::Text(s),
                },
                other => other,
            };
        }
        if src_ty == "bytea" {
            if let Value::Text(s) = &v {
                if let Some(h) = s.strip_prefix("\\x") {
                    if let Some(b) = decode_hex(h) {
                        return Value::Bytes(b);
                    }
                }
            }
        }
        v
    }

    /// Where a foreign key points, resolved against the table's scope.
    pub fn fk_target(scope: &Scope, fk: &navdb_core::ForeignKeyDef) -> ObjPath {
        let db = if fk.ref_db.is_empty() { scope.db.clone() } else { fk.ref_db.clone() };
        let schema = fk.ref_schema.clone().or_else(|| scope.schema.clone());
        ObjPath::new(db, schema, fk.ref_table.clone())
    }

    pub fn exec(d: &mut dyn Driver, scope: &Scope, sql: &str) -> Result<()> {
        d.execute(Some(scope), sql).map(|_| ())
    }

    pub fn truncate_sql(s: &str, n: usize) -> String {
        let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if one.chars().count() > n {
            format!("{}…", one.chars().take(n).collect::<String>())
        } else {
            one
        }
    }

    pub fn same_endpoint(a: &(Option<String>, Option<Scope>), b: &(Option<String>, Option<Scope>)) -> bool {
        a.0.is_some() && a.0 == b.0 && a.1 == b.1
    }

    pub fn dialect_badge(ui: &mut Ui, from: Dialect, to: Dialect) {
        if from != to {
            ui.label(
                RichText::new(format!("ℹ {:?} → {:?}: column types will be converted between dialects", from, to))
                    .color(Color32::from_rgb(90, 140, 220)),
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn types() {
            assert_eq!(map_type(Dialect::MySql, Dialect::Sqlite, "int(11)", false, false), "INTEGER");
            assert_eq!(map_type(Dialect::MySql, Dialect::Sqlite, "varchar(50)", false, false), "VARCHAR(50)");
            assert_eq!(map_type(Dialect::MySql, Dialect::Postgres, "tinyint(1)", false, false), "boolean");
            assert_eq!(map_type(Dialect::MySql, Dialect::Postgres, "int unsigned", false, false), "bigint");
            assert_eq!(map_type(Dialect::Postgres, Dialect::MySql, "character varying(30)", false, false), "varchar(30)");
            assert_eq!(map_type(Dialect::Postgres, Dialect::MySql, "timestamp without time zone", false, false), "datetime");
            assert_eq!(map_type(Dialect::Postgres, Dialect::MySql, "text", false, true), "varchar(255)");
            assert_eq!(map_type(Dialect::Postgres, Dialect::MySql, "serial", true, false), "int");
            assert_eq!(map_type(Dialect::Postgres, Dialect::MySql, "numeric(10,2)", false, false), "decimal(10,2)");
            assert_eq!(map_type(Dialect::Sqlite, Dialect::Postgres, "INTEGER", true, false), "bigint");
            assert_eq!(map_type(Dialect::Postgres, Dialect::Sqlite, "double precision", false, false), "REAL");
        }

        #[test]
        fn defaults() {
            assert_eq!(convert_default("'abc'::character varying", Dialect::MySql, "varchar(10)"), Some("'abc'".into()));
            assert_eq!(convert_default("0", Dialect::Postgres, "boolean"), Some("FALSE".into()));
            assert_eq!(convert_default("now()", Dialect::MySql, "datetime"), Some("CURRENT_TIMESTAMP".into()));
            assert_eq!(convert_default("gen_random_uuid()", Dialect::MySql, "char(36)"), None);
        }

        #[test]
        fn values() {
            assert_eq!(fix_value(Value::Int(1), "tinyint(1)", "boolean"), Value::Bool(true));
            assert_eq!(fix_value(Value::Text("f".into()), "boolean", "tinyint(1)"), Value::Int(0));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    General,
    Objects,
    Options,
    Summary,
}

#[derive(Clone)]
struct TransferOptions {
    create_tables: bool,
    drop_target: bool,
    create_records: bool,
    batch: usize,
    use_tx: bool,
    continue_on_error: bool,
    convert_types: bool,
    foreign_keys: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            create_tables: true,
            drop_target: false,
            create_records: true,
            batch: 1000,
            use_tx: true,
            continue_on_error: true,
            convert_types: true,
            foreign_keys: true,
        }
    }
}

enum TransferTarget {
    Conn(ConnectionConfig, Scope),
    File(PathBuf, Dialect),
}

struct TransferJob {
    src_cfg: ConnectionConfig,
    src_scope: Scope,
    target: TransferTarget,
    tables: Vec<String>,
    opts: TransferOptions,
}

pub struct DataTransferDialog {
    id: String,
    step: Step,
    src_conn: Option<String>,
    src_scope: Option<Scope>,
    to_file: bool,
    dst_conn: Option<String>,
    dst_scope: Option<Scope>,
    file_path: String,
    file_dialect: Dialect,
    selected: BTreeSet<String>,
    selection_for: Option<(String, Scope)>,
    filter: String,
    opts: TransferOptions,
    job: Job,
    /// Target of the running/finished job (for navigator refresh).
    job_target: Option<(String, Scope)>,
}

impl DataTransferDialog {
    pub fn new(st: &mut AppState, source: Option<(String, Scope)>) -> Self {
        let (src_conn, src_scope) = match source {
            Some((c, s)) => (Some(c), Some(s)),
            None => (st.selection.conn.clone(), st.selection.scope.clone()),
        };
        let file_dialect = src_conn.as_ref().and_then(|c| st.config(c)).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            step: Step::General,
            src_conn,
            src_scope,
            to_file: false,
            dst_conn: None,
            dst_scope: None,
            file_path: String::new(),
            file_dialect,
            selected: BTreeSet::new(),
            selection_for: None,
            filter: String::new(),
            opts: TransferOptions::default(),
            job: Job::default(),
            job_target: None,
        }
    }

    fn src_dialect(&self, st: &AppState) -> Option<Dialect> {
        self.src_conn.as_ref().and_then(|c| st.config(c)).map(|c| c.dialect())
    }

    fn dst_dialect(&self, st: &AppState) -> Option<Dialect> {
        if self.to_file {
            Some(self.file_dialect)
        } else {
            self.dst_conn.as_ref().and_then(|c| st.config(c)).map(|c| c.dialect())
        }
    }

    fn validate(&self, st: &AppState) -> std::result::Result<(), String> {
        if self.src_conn.is_none() || self.src_scope.is_none() {
            return Err("Select a source connection and database.".into());
        }
        if let Some(c) = self.src_conn.as_ref().and_then(|c| st.config(c)) {
            if !c.kind.is_sql() {
                return Err(format!("{} is not supported as a transfer source.", c.kind.label()));
            }
        }
        if self.to_file {
            if self.file_path.trim().is_empty() {
                return Err("Choose a target SQL file.".into());
            }
        } else {
            if self.dst_conn.is_none() || self.dst_scope.is_none() {
                return Err("Select a target connection and database.".into());
            }
            if let Some(c) = self.dst_conn.as_ref().and_then(|c| st.config(c)) {
                if !c.kind.is_sql() {
                    return Err(format!("{} is not supported as a transfer target.", c.kind.label()));
                }
            }
            if same_endpoint(&(self.src_conn.clone(), self.src_scope.clone()), &(self.dst_conn.clone(), self.dst_scope.clone())) {
                return Err("Source and target are the same database.".into());
            }
        }
        if self.selected.is_empty() {
            return Err("Select at least one table on the Objects page.".into());
        }
        if !self.opts.create_tables && !self.opts.create_records && !self.opts.drop_target {
            return Err("Nothing to do: enable \"Create tables\" or \"Create records\".".into());
        }
        Ok(())
    }

    fn page_general(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        if endpoint(ui, st, "Source", &format!("dt-src-{}", self.id), &mut self.src_conn, &mut self.src_scope) {
            self.selection_for = None;
        }
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Target").strong());
                ui.add_space(12.0);
                ui.radio_value(&mut self.to_file, false, "Connection");
                ui.radio_value(&mut self.to_file, true, "SQL file");
            });
            if self.to_file {
                ui.horizontal(|ui| {
                    ui.label("File:");
                    ui.add(egui::TextEdit::singleline(&mut self.file_path).desired_width(360.0).min_size(egui::vec2(360.0, 0.0)).hint_text("/path/to/transfer.sql"));
                    if ui.button("Browse…").clicked() {
                        let name = self.src_scope.as_ref().map(|s| format!("{}.sql", s.db)).unwrap_or_else(|| "transfer.sql".into());
                        if let Some(p) = ui::save_file_dialog(&name, "sql") {
                            self.file_path = p.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("SQL dialect:");
                    egui::ComboBox::from_id_salt(format!("dt-fd-{}", self.id)).selected_text(dialect_name(self.file_dialect)).show_ui(ui, |ui| {
                        for d in [Dialect::MySql, Dialect::Postgres, Dialect::Sqlite] {
                            ui.selectable_value(&mut self.file_dialect, d, dialect_name(d));
                        }
                    });
                });
            } else {
                ui.horizontal(|ui| {
                    ui::scope_picker(ui, &format!("dt-dst-{}", self.id), st, &mut self.dst_conn, &mut self.dst_scope);
                });
                if let Some(c) = self.dst_conn.as_ref().and_then(|c| st.config(c)) {
                    ui.label(RichText::new(format!("{} {}", ui::style::kind_icon(c.kind), c.kind.label())).weak().small());
                }
            }
        });
        ui.add_space(6.0);
        if let (Some(a), Some(b)) = (self.src_dialect(st), self.dst_dialect(st)) {
            dialect_badge(ui, a, b);
        }
        if !self.to_file && same_endpoint(&(self.src_conn.clone(), self.src_scope.clone()), &(self.dst_conn.clone(), self.dst_scope.clone())) {
            ui::error_text(ui, "Source and target are the same database.");
        }
    }

    fn page_objects(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let tables = match scope_tables(st, &self.src_conn, &self.src_scope) {
            Tables::NoScope => {
                ui.label("Select a source database on the General page first.");
                return;
            }
            Tables::Loading => {
                ui::loading(ui, "Loading tables…");
                return;
            }
            Tables::Failed(e) => {
                ui::error_text(ui, &e);
                return;
            }
            Tables::Ready(v) => v,
        };
        let key = self.src_conn.clone().zip(self.src_scope.clone());
        if self.selection_for != key {
            self.selected = tables.iter().map(|t| t.name.clone()).collect();
            self.selection_for = key;
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter tables").desired_width(200.0).min_size(egui::vec2(200.0, 0.0)));
            if ui.button("Select All").clicked() {
                self.selected = tables.iter().map(|t| t.name.clone()).collect();
            }
            if ui.button("Unselect All").clicked() {
                self.selected.clear();
            }
            ui.label(RichText::new(format!("{} of {} selected", self.selected.len(), tables.len())).weak());
        });
        ui.separator();
        let f = self.filter.to_lowercase();
        egui::ScrollArea::vertical().id_salt("dt-objs").auto_shrink([false, false]).max_height(330.0).show(ui, |ui| {
            egui::Grid::new(format!("dt-objgrid-{}", self.id)).striped(true).num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                for t in tables.iter().filter(|t| f.is_empty() || t.name.to_lowercase().contains(&f)) {
                    let mut on = self.selected.contains(&t.name);
                    if ui.checkbox(&mut on, format!("{} {}", ObjectKind::Table.icon(), t.name)).changed() {
                        if on {
                            self.selected.insert(t.name.clone());
                        } else {
                            self.selected.remove(&t.name);
                        }
                    }
                    ui.label(RichText::new(t.rows.map(|r| format!("{} rows", r)).unwrap_or_default()).weak());
                    ui.end_row();
                }
            });
        });
    }

    fn page_options(&mut self, ui: &mut egui::Ui, st: &AppState) {
        let o = &mut self.opts;
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("Structure").strong());
            cols[0].checkbox(&mut o.create_tables, "Create tables");
            cols[0].add_enabled(o.create_tables, egui::Checkbox::new(&mut o.drop_target, "Drop target table before create"));
            cols[0].add_enabled(o.create_tables, egui::Checkbox::new(&mut o.foreign_keys, "Create foreign keys (after data)"));
            cols[0].checkbox(&mut o.convert_types, "Convert data types across dialects");
            cols[1].label(RichText::new("Data").strong());
            cols[1].checkbox(&mut o.create_records, "Create records");
            cols[1].add_enabled_ui(o.create_records, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Insert in batches of");
                    ui.add(egui::DragValue::new(&mut o.batch).range(1..=50_000).speed(10.0));
                    ui.label("rows");
                });
                ui.checkbox(&mut o.use_tx, "Use transactions (one per table)");
            });
            cols[1].checkbox(&mut o.continue_on_error, "Continue on error");
        });
        ui.add_space(8.0);
        if let (Some(a), Some(b)) = (self.src_dialect(st), self.dst_dialect(st)) {
            dialect_badge(ui, a, b);
            if a != b && !self.opts.convert_types {
                ui::error_text(ui, "Without type conversion the source column types are used verbatim and may fail on the target.");
            }
        }
    }

    fn page_summary(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let src = match (&self.src_conn, &self.src_scope) {
            (Some(c), Some(s)) => format!("{} / {}", st.conn_name(c), s.display()),
            _ => "-".into(),
        };
        let dst = if self.to_file {
            format!("SQL file {} ({})", self.file_path, dialect_name(self.file_dialect))
        } else {
            match (&self.dst_conn, &self.dst_scope) {
                (Some(c), Some(s)) => format!("{} / {}", st.conn_name(c), s.display()),
                _ => "-".into(),
            }
        };
        let o = &self.opts;
        let yn = |b: bool| if b { "Yes" } else { "No" }.to_string();
        let rows = vec![
            ("Source".to_string(), src),
            ("Target".to_string(), dst),
            ("Tables".to_string(), format!("{} ({})", self.selected.len(), truncate_sql(&self.selected.iter().cloned().collect::<Vec<_>>().join(", "), 120))),
            ("Create tables".to_string(), format!("{}{}", yn(o.create_tables), if o.drop_target { " (drop first)" } else { "" })),
            ("Create records".to_string(), if o.create_records { format!("Yes, batches of {}", o.batch) } else { "No".into() }),
            ("Transactions".to_string(), yn(o.use_tx)),
            ("Continue on error".to_string(), yn(o.continue_on_error)),
        ];
        ui::kv_grid(ui, &format!("dt-sum-{}", self.id), &rows);
        ui.add_space(8.0);
        if let Err(e) = self.validate(st) {
            ui::error_text(ui, &e);
        }
        if self.job.started.is_some() {
            ui.separator();
            job_ui(ui, &self.job, "tables", 200.0);
        }
    }

    fn start(&mut self, st: &mut AppState) {
        let (Some(sc), Some(ss)) = (self.src_conn.clone(), self.src_scope.clone()) else { return };
        let Some(src_cfg) = st.config(&sc).cloned() else { return };
        let target = if self.to_file {
            self.job_target = None;
            TransferTarget::File(PathBuf::from(self.file_path.trim()), self.file_dialect)
        } else {
            let (Some(dc), Some(ds)) = (self.dst_conn.clone(), self.dst_scope.clone()) else { return };
            let Some(cfg) = st.config(&dc).cloned() else { return };
            self.job_target = Some((dc, ds.clone()));
            TransferTarget::Conn(cfg, ds)
        };
        let job = TransferJob {
            src_cfg,
            src_scope: ss,
            target,
            tables: self.selected.iter().cloned().collect(),
            opts: self.opts.clone(),
        };
        self.job.start(st, move |p| run_transfer(job, p));
    }
}

fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::MySql => "MySQL / MariaDB",
        Dialect::Postgres => "PostgreSQL",
        Dialect::Sqlite => "SQLite",
        Dialect::SqlServer => "SQL Server",
        Dialect::Oracle => "Oracle",
        Dialect::Generic => "SQL",
    }
}

impl Dialog for DataTransferDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        if self.job.poll(ctx) {
            if let Some((c, s)) = self.job_target.clone() {
                st.refresh_scope(&c, &s);
            }
            match &self.job.result {
                Some(Ok(())) if error_count(&self.job.progress) == 0 => st.toast(format!("Data transfer finished in {}", self.job.elapsed_text())),
                Some(Ok(())) => st.error(format!("Data transfer finished with {} error(s)", error_count(&self.job.progress))),
                _ => st.error("Data transfer failed"),
            }
        }
        let mut open = true;
        let mut close_clicked = false;
        super::window("Data Transfer", &self.id).open(&mut open).default_width(700.0).show(ctx, |ui| {
            let running = self.job.running();
            ui.add_enabled_ui(!running, |ui| {
                step_bar(
                    ui,
                    &[(Step::General, "General"), (Step::Objects, "Objects"), (Step::Options, "Options"), (Step::Summary, "Summary")],
                    &mut self.step,
                    |_| true,
                );
            });
            ui.separator();
            ui.allocate_ui(egui::vec2(ui.available_width(), 400.0), |ui| {
                ui.set_min_height(400.0);
                ui.vertical(|ui| {
                    ui.add_enabled_ui(!running || self.step == Step::Summary, |ui| match self.step {
                        Step::General => self.page_general(ui, st),
                        Step::Objects => self.page_objects(ui, st),
                        Step::Options => self.page_options(ui, st),
                        Step::Summary => self.page_summary(ui, st),
                    });
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    if running {
                        self.job.cancel();
                    }
                    close_clicked = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("■ Cancel").clicked() {
                            self.job.cancel();
                        }
                        return;
                    }
                    match self.step {
                        Step::Summary => {
                            let ok = self.validate(st).is_ok();
                            let label = if self.job.finished() { "▶ Start Again" } else { "▶ Start" };
                            if ui.add_enabled(ok, egui::Button::new(RichText::new(label).strong())).clicked() {
                                self.start(st);
                            }
                        }
                        _ => {
                            if ui.button("Next ›").clicked() {
                                self.step = match self.step {
                                    Step::General => Step::Objects,
                                    Step::Objects => Step::Options,
                                    _ => Step::Summary,
                                };
                            }
                        }
                    }
                    if self.step != Step::General && ui.button("‹ Back").clicked() {
                        self.step = match self.step {
                            Step::Summary => Step::Options,
                            Step::Options => Step::Objects,
                            _ => Step::General,
                        };
                    }
                });
            });
        });
        if close_clicked {
            open = false;
        }
        if !open && self.job.running() {
            self.job.cancel();
        }
        open
    }
}

// ----------------------------------------------------------------------
// Worker
// ----------------------------------------------------------------------

enum Sink {
    Db(Box<dyn Driver>, Scope),
    File(std::io::BufWriter<std::fs::File>),
}

impl Sink {
    fn exec(&mut self, sql: &str) -> Result<()> {
        match self {
            Sink::Db(d, s) => exec(d.as_mut(), s, sql),
            Sink::File(w) => {
                writeln!(w, "{};\n", sql.trim_end().trim_end_matches(';'))?;
                Ok(())
            }
        }
    }
}

fn run_transfer(job: TransferJob, p: Shared) -> Result<()> {
    let sd = job.src_cfg.dialect();
    plog(&p, LogLevel::Info, format!("Connecting to source {}…", job.src_cfg.display_name()));
    let mut src = driver::connect(&job.src_cfg)?;
    let (mut sink, td, target_scope) = match &job.target {
        TransferTarget::Conn(cfg, scope) => {
            plog(&p, LogLevel::Info, format!("Connecting to target {}…", cfg.display_name()));
            (Sink::Db(driver::connect(cfg)?, scope.clone()), cfg.dialect(), scope.clone())
        }
        TransferTarget::File(path, d) => {
            plog(&p, LogLevel::Info, format!("Writing to {}", path.display()));
            let mut w = std::io::BufWriter::new(std::fs::File::create(path)?);
            writeln!(w, "-- NavDB Studio data transfer")?;
            writeln!(w, "-- Source: {} / {}", job.src_cfg.display_name(), job.src_scope.display())?;
            writeln!(w, "-- Date: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))?;
            let schema = if *d == Dialect::Postgres { job.src_scope.schema.clone() } else { None };
            (Sink::File(w), *d, Scope::new("", schema))
        }
    };
    let total = job.tables.len();
    set_counts(&p, 0, total);
    match td {
        Dialect::MySql => {
            let _ = sink.exec("SET FOREIGN_KEY_CHECKS=0");
        }
        Dialect::Sqlite => {
            let _ = sink.exec("PRAGMA foreign_keys=OFF");
        }
        _ => {}
    }
    let mut deferred: Vec<(String, Vec<String>)> = Vec::new();
    let mut outcome: Result<()> = Ok(());
    let mut total_rows: u64 = 0;
    for (i, name) in job.tables.iter().enumerate() {
        if cancelled(&p) {
            plog(&p, LogLevel::Warn, "Cancelled by user");
            break;
        }
        set_current(&p, format!("Transferring {}…", name));
        let started = Instant::now();
        match transfer_table(src.as_mut(), &mut sink, &job, sd, td, &target_scope, name, &p, &mut deferred) {
            Ok(rows) => {
                total_rows += rows;
                let what = if job.opts.create_records { format!("{} row(s) copied", rows) } else { "structure created".into() };
                plog(&p, LogLevel::Ok, format!("{}: {} in {}", name, what, format_duration(started.elapsed())));
            }
            Err(e) => {
                plog(&p, LogLevel::Error, format!("{}: {:#}", name, e));
                if !job.opts.continue_on_error {
                    outcome = Err(anyhow::anyhow!("stopped after error in table {}", name));
                    break;
                }
            }
        }
        set_counts(&p, i + 1, total);
    }
    if outcome.is_ok() && !deferred.is_empty() && !cancelled(&p) {
        set_current(&p, "Creating foreign keys…");
        for (table, stmts) in &deferred {
            for s in stmts {
                if let Err(e) = sink.exec(s) {
                    plog(&p, LogLevel::Error, format!("{}: foreign key: {:#}", table, e));
                }
            }
        }
        plog(&p, LogLevel::Info, format!("Foreign keys processed for {} table(s)", deferred.len()));
    }
    match td {
        Dialect::MySql => {
            let _ = sink.exec("SET FOREIGN_KEY_CHECKS=1");
        }
        Dialect::Sqlite => {
            let _ = sink.exec("PRAGMA foreign_keys=ON");
        }
        _ => {}
    }
    if let Sink::File(w) = &mut sink {
        w.flush()?;
    }
    plog(&p, LogLevel::Info, format!("Total: {} row(s)", total_rows));
    set_current(&p, "Done");
    outcome
}

#[allow(clippy::too_many_arguments)]
fn transfer_table(
    src: &mut dyn Driver,
    sink: &mut Sink,
    job: &TransferJob,
    sd: Dialect,
    td: Dialect,
    target_scope: &Scope,
    name: &str,
    p: &Shared,
    deferred: &mut Vec<(String, Vec<String>)>,
) -> Result<u64> {
    let o = &job.opts;
    let spath = job.src_scope.path(name);
    let def = src.table_def(&spath)?;
    let from = if o.convert_types { sd } else { td };
    let (mut tdef, notes) = convert_table(&def, from, td, target_scope);
    for n in notes {
        plog(p, LogLevel::Warn, format!("{}: {}", name, n));
    }
    // FKs are added after all data is in place (SQLite keeps them inline: it
    // does not validate references at CREATE time and cannot ALTER them in).
    let fks = if td != Dialect::Sqlite { std::mem::take(&mut tdef.foreign_keys) } else { vec![] };
    if o.create_tables {
        if o.drop_target {
            sink.exec(&format!("DROP TABLE IF EXISTS {}", td.qualify(&tdef.path)))?;
        }
        for s in ddl::create_table(td, &tdef) {
            sink.exec(&s)?;
        }
        if o.foreign_keys && !fks.is_empty() {
            let mut with = tdef.clone();
            with.foreign_keys = fks;
            let stmts = ddl::alter_table(td, &tdef, &with);
            if !stmts.is_empty() {
                deferred.push((name.to_string(), stmts));
            }
        }
    }
    if !o.create_records {
        return Ok(0);
    }
    let pk = def.primary_key();
    let sort: Vec<dml::SortSpec> = pk.iter().map(|c| dml::SortSpec { column: c.clone(), descending: false }).collect();
    let src_types: Vec<(String, String)> = def.columns.iter().map(|c| (c.name.clone(), c.data_type.to_ascii_lowercase())).collect();
    let dst_types: Vec<(String, String)> = tdef.columns.iter().map(|c| (c.name.clone(), c.data_type.to_ascii_lowercase())).collect();
    let type_of = |list: &[(String, String)], n: &str| list.iter().find(|(c, _)| c.eq_ignore_ascii_case(n)).map(|(_, t)| t.clone()).unwrap_or_default();
    let batch = o.batch.max(1);
    let mut offset = 0usize;
    let mut copied: u64 = 0;
    if o.use_tx {
        sink.exec(begin_sql(td))?;
    }
    let result: Result<()> = (|| {
        loop {
            if cancelled(p) {
                anyhow::bail!("cancelled");
            }
            let page = src.query(Some(&job.src_scope), &dml::select_page(sd, &spath, "", &sort, batch, offset))?;
            if page.rows.is_empty() {
                break;
            }
            let cols: Vec<String> = page.columns.iter().map(|c| c.name.clone()).collect();
            let conv: Vec<(String, String)> = cols.iter().map(|c| (type_of(&src_types, c), type_of(&dst_types, c))).collect();
            let n = page.rows.len();
            let rows: Vec<Vec<navdb_core::Value>> = page
                .rows
                .into_iter()
                .map(|r| r.into_iter().enumerate().map(|(i, v)| fix_value(v, &conv[i].0, &conv[i].1)).collect())
                .collect();
            let sql = dml::insert_rows(td, &tdef.path, &cols, &rows);
            match sink.exec(&sql) {
                Ok(()) => copied += n as u64,
                Err(e) if !o.use_tx && o.continue_on_error => {
                    plog(p, LogLevel::Error, format!("{}: rows {}..{}: {:#}", name, offset + 1, offset + n, e));
                }
                Err(e) => return Err(e),
            }
            offset += n;
            set_current(p, format!("Transferring {}… {} row(s)", name, copied));
            if n < batch {
                break;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if o.use_tx {
                sink.exec("COMMIT")?;
            }
        }
        Err(e) => {
            if o.use_tx {
                let _ = sink.exec("ROLLBACK");
            }
            return Err(e);
        }
    }
    // keep identity sequences in step with copied ids
    if td == Dialect::Postgres && copied > 0 && matches!(sink, Sink::Db(..)) {
        for c in tdef.columns.iter().filter(|c| c.auto_increment) {
            let q = td.quote_ident(&c.name);
            let t = td.qualify(&tdef.path);
            let sql = format!(
                "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE((SELECT MAX({}) FROM {}), 1))",
                td.string_literal(&t),
                td.string_literal(&c.name),
                q,
                t
            );
            let _ = sink.exec(&sql);
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use navdb_core::DbKind;

    fn sqlite(path: &std::path::Path) -> ConnectionConfig {
        ConnectionConfig { database: path.display().to_string(), ..ConnectionConfig::new(DbKind::Sqlite) }
    }

    #[test]
    fn sqlite_to_sqlite_transfer() {
        let dir = std::env::temp_dir().join(format!("navdb-dt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.db"), dir.join("b.db"));
        let src = sqlite(&a);
        let mut d = driver::connect(&src).unwrap();
        d.execute(None, "CREATE TABLE p(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL); CREATE TABLE c(id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES p(id), v REAL)").unwrap();
        for i in 0..25 {
            d.execute(None, &format!("INSERT INTO p(name) VALUES ('n{}'); INSERT INTO c VALUES ({}, {}, 1.5)", i, i, i + 1)).unwrap();
        }
        drop(d);
        let job = TransferJob {
            src_cfg: src,
            src_scope: Scope::new("main", None),
            target: TransferTarget::Conn(sqlite(&b), Scope::new("main", None)),
            tables: vec!["c".into(), "p".into()],
            opts: TransferOptions { batch: 7, ..Default::default() },
        };
        let p = new_shared();
        run_transfer(job, p.clone()).unwrap();
        assert_eq!(error_count(&p), 0, "{:?}", p.lock().unwrap().log);
        let mut t = driver::connect(&sqlite(&b)).unwrap();
        assert_eq!(t.count(&ObjPathT::new("main", None, "p")).unwrap(), 25);
        assert_eq!(t.count(&ObjPathT::new("main", None, "c")).unwrap(), 25);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use navdb_core::ObjPath as ObjPathT;
}
