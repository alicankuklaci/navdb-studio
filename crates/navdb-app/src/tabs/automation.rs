//! Automation: batch jobs made of steps (run query, backup, export table),
//! run on demand or on a simple interval while the application is open.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{Color32, RichText};
use navdb_core::config::{self, AutomationJob, AutomationStep, ConnectionConfig};
use navdb_core::driver::{self, Driver};
use navdb_core::export::{self, ExportFormat, ExportOptions};
use navdb_core::{anyhow, dump, Pending, Scope};

use super::Tab;
use crate::state::AppState;
use crate::ui::style::icon;

type Log = Arc<Mutex<Vec<String>>>;

struct HistoryItem {
    conn: String,
    scope: Option<Scope>,
    sql: String,
    ok: bool,
    message: String,
    elapsed_ms: u128,
}

struct Outcome {
    ok: bool,
    status: String,
    history: Vec<HistoryItem>,
}

struct Run {
    log: Log,
    pending: Pending<Outcome>,
    started: chrono::DateTime<chrono::Local>,
}

fn now_str() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn log_line(log: &Log, s: impl Into<String>) {
    if let Ok(mut l) = log.lock() {
        l.push(s.into());
    }
}

fn step_kind(s: &AutomationStep) -> &'static str {
    match s {
        AutomationStep::RunQuery { .. } => "Run Query",
        AutomationStep::Backup { .. } => "Backup",
        AutomationStep::ExportTable { .. } => "Export Table",
    }
}

fn step_conn(s: &AutomationStep) -> &str {
    match s {
        AutomationStep::RunQuery { connection_id, .. } | AutomationStep::Backup { connection_id, .. } | AutomationStep::ExportTable { connection_id, .. } => connection_id,
    }
}

fn parse_format(s: &str) -> Option<ExportFormat> {
    ExportFormat::from_extension(s).or_else(|| ExportFormat::ALL.iter().copied().find(|f| format!("{:?}", f).eq_ignore_ascii_case(s)))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn scope_of(cfg: &ConnectionConfig, database: &str) -> Scope {
    let db = if database.trim().is_empty() { cfg.default_database() } else { database.trim().to_string() };
    let mut s = crate::commands::parse_scope(&db);
    if cfg.kind.has_schemas() && s.schema.is_none() {
        s.schema = cfg.kind.default_schema().map(String::from);
    }
    s
}

fn run_step(
    step: &AutomationStep,
    cfgs: &HashMap<String, ConnectionConfig>,
    drivers: &mut HashMap<String, Box<dyn Driver>>,
    log: &Log,
    history: &mut Vec<HistoryItem>,
) -> navdb_core::Result<String> {
    let conn_id = step_conn(step).to_string();
    let cfg = cfgs.get(&conn_id).ok_or_else(|| anyhow!("connection not found (was it deleted?)"))?;
    if !drivers.contains_key(&conn_id) {
        log_line(log, format!("  connecting to {} ...", cfg.display_name()));
        drivers.insert(conn_id.clone(), driver::connect(cfg)?);
    }
    let d = drivers.get_mut(&conn_id).ok_or_else(|| anyhow!("not connected"))?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    match step {
        AutomationStep::RunQuery { database, sql, .. } => {
            let scope = if database.trim().is_empty() { None } else { Some(scope_of(cfg, database)) };
            let start = Instant::now();
            match d.execute(scope.as_ref(), sql) {
                Ok(results) => {
                    for r in &results {
                        let first = r.sql.lines().next().unwrap_or("").chars().take(80).collect::<String>();
                        log_line(log, format!("  {}  ->  {}", first, r.summary()));
                    }
                    history.push(HistoryItem {
                        conn: conn_id.clone(),
                        scope: scope.clone(),
                        sql: sql.clone(),
                        ok: true,
                        message: format!("automation: {} statement(s)", results.len()),
                        elapsed_ms: start.elapsed().as_millis(),
                    });
                    Ok(format!("{} statement(s) executed", results.len()))
                }
                Err(e) => {
                    history.push(HistoryItem {
                        conn: conn_id.clone(),
                        scope,
                        sql: sql.clone(),
                        ok: false,
                        message: format!("{:#}", e),
                        elapsed_ms: start.elapsed().as_millis(),
                    });
                    Err(e)
                }
            }
        }
        AutomationStep::Backup { database, output_dir, .. } => {
            let scope = scope_of(cfg, database);
            let dir = if output_dir.trim().is_empty() { config::backups_dir() } else { std::path::PathBuf::from(output_dir.trim()) };
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}_{}.sql", sanitize(&scope.display()), stamp));
            let f = std::fs::File::create(&path)?;
            let mut w = std::io::BufWriter::new(f);
            let mut last = String::new();
            dump::dump(d.as_mut(), &scope, &dump::DumpOptions::default(), &mut w, &mut |done, total, cur| {
                if cur != last {
                    last = cur.to_string();
                    log_line(log, format!("  [{}/{}] {}", done, total, cur));
                }
            })?;
            use std::io::Write;
            w.flush()?;
            let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
            Ok(format!("backup written to {} ({})", path.display(), crate::ui::format_bytes(size)))
        }
        AutomationStep::ExportTable { database, table, format, path, .. } => {
            let fmt = parse_format(format).ok_or_else(|| anyhow!("unknown export format '{}'", format))?;
            let scope = scope_of(cfg, database);
            let obj = scope.path(table.clone());
            let dialect = d.dialect();
            let r = d.query(Some(&scope), &format!("SELECT * FROM {}", dialect.qualify(&obj)))?;
            let mut out = if path.trim().is_empty() {
                config::backups_dir().join(format!("{}_{}.{}", sanitize(table), stamp, fmt.extension()))
            } else {
                std::path::PathBuf::from(path.trim().replace("{date}", &stamp).replace("{table}", table))
            };
            if out.is_dir() {
                out = out.join(format!("{}_{}.{}", sanitize(table), stamp, fmt.extension()));
            }
            if let Some(p) = out.parent() {
                if !p.as_os_str().is_empty() {
                    std::fs::create_dir_all(p)?;
                }
            }
            let opts = ExportOptions { dialect, table: obj, ..Default::default() };
            export::export_to_file(&r, fmt, &opts, &out)?;
            Ok(format!("{} row(s) exported to {}", r.rows.len(), out.display()))
        }
    }
}

fn run_job(job: AutomationJob, cfgs: HashMap<String, ConnectionConfig>, log: Log, stop_on_error: bool) -> Outcome {
    let start = Instant::now();
    log_line(&log, format!("[{}] ▶ Job \"{}\" started ({} step(s))", now_str(), job.name, job.steps.len()));
    let mut drivers: HashMap<String, Box<dyn Driver>> = HashMap::new();
    let mut history = Vec::new();
    let mut failed = 0;
    for (i, step) in job.steps.iter().enumerate() {
        log_line(&log, format!("[{}] Step {}: {}", now_str(), i + 1, step_kind(step)));
        match run_step(step, &cfgs, &mut drivers, &log, &mut history) {
            Ok(msg) => log_line(&log, format!("  ✔ {}", msg)),
            Err(e) => {
                failed += 1;
                log_line(&log, format!("  ✖ {:#}", e));
                // drop a possibly broken connection
                drivers.remove(step_conn(step));
                if stop_on_error {
                    log_line(&log, "  stopping: an error occurred");
                    break;
                }
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let (ok, status) = if failed == 0 {
        (true, format!("Succeeded ({} step(s), {:.1} s)", job.steps.len(), secs))
    } else {
        (false, format!("Failed ({} error(s), {:.1} s)", failed, secs))
    };
    log_line(&log, format!("[{}] {} {}", now_str(), if ok { "■" } else { "✖" }, status));
    Outcome { ok, status, history }
}

fn fmt_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

/// Seconds until the job is due (<= 0 = due now); None when manual.
fn due_in(job: &AutomationJob) -> Option<i64> {
    if job.interval_minutes == 0 || job.steps.is_empty() {
        return None;
    }
    let interval = (job.interval_minutes * 60) as i64;
    Some(match job.last_run {
        None => 0,
        Some(t) => interval - (chrono::Local::now() - t).num_seconds(),
    })
}

pub struct AutomationTab {
    selected: Option<String>,
    runs: HashMap<String, Run>,
    logs: HashMap<String, Log>,
    dirty: bool,
}

impl AutomationTab {
    pub fn new(st: &mut AppState) -> Self {
        Self { selected: st.ws.automation.first().map(|j| j.id.clone()), runs: HashMap::new(), logs: HashMap::new(), dirty: false }
    }

    fn start(&mut self, st: &mut AppState, job_id: &str) {
        if self.runs.contains_key(job_id) {
            return;
        }
        let Some(job) = st.ws.automation.iter().find(|j| j.id == job_id).cloned() else { return };
        if job.steps.is_empty() {
            st.error("The job has no steps");
            return;
        }
        let cfgs: HashMap<String, ConnectionConfig> = st.ws.connections.iter().map(|c| (c.id.clone(), c.clone())).collect();
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let stop = st.prefs().stop_on_error;
        let pending = navdb_core::session::spawn_task(st.notify.clone(), move || Ok(run_job(job, cfgs, l, stop)));
        self.logs.insert(job_id.to_string(), log.clone());
        self.runs.insert(job_id.to_string(), Run { log, pending, started: chrono::Local::now() });
    }

    fn poll(&mut self, st: &mut AppState) {
        let mut finished = Vec::new();
        for (id, r) in self.runs.iter_mut() {
            if let Some(res) = r.pending.poll() {
                finished.push((id.clone(), r.started, res));
            }
        }
        for (id, started, res) in finished {
            if let Some(run) = self.runs.remove(&id) {
                self.logs.insert(id.clone(), run.log);
            }
            let (ok, status, history) = match res {
                Ok(o) => (o.ok, o.status, o.history),
                Err(e) => (false, format!("Failed: {:#}", e), vec![]),
            };
            for h in history {
                st.record(&h.conn, h.scope.as_ref(), &h.sql, h.ok, &h.message, h.elapsed_ms);
            }
            let name = st.ws.automation.iter().find(|j| j.id == id).map(|j| j.name.clone()).unwrap_or_default();
            if let Some(j) = st.ws.automation.iter_mut().find(|j| j.id == id) {
                j.last_run = Some(started);
                j.last_status = status.clone();
            }
            st.save_workspace();
            if ok {
                st.toast(format!("Automation \"{}\": {}", name, status));
            } else {
                st.error(format!("Automation \"{}\": {}", name, status));
            }
        }
        // scheduler
        let due: Vec<String> = st
            .ws
            .automation
            .iter()
            .filter(|j| !self.runs.contains_key(&j.id))
            .filter(|j| due_in(j).map(|s| s <= 0).unwrap_or(false))
            .map(|j| j.id.clone())
            .collect();
        for id in due {
            // mark immediately so a failing start does not retrigger every frame
            if let Some(j) = st.ws.automation.iter_mut().find(|j| j.id == id) {
                j.last_run = Some(chrono::Local::now());
            }
            self.start(st, &id);
        }
    }

    fn job_list(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Jobs").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let sel = self.selected.clone();
                if ui.add_enabled(sel.is_some(), egui::Button::new(icon::DELETE).small()).on_hover_text("Delete job").clicked() {
                    if let Some(id) = sel.clone() {
                        let name = st.ws.automation.iter().find(|j| j.id == id).map(|j| j.name.clone()).unwrap_or_default();
                        st.open_dialog(crate::dialogs::confirm::ConfirmDialog::new(
                            "Delete Job",
                            format!("Delete automation job \"{}\"?", name),
                            Box::new(move |st: &mut AppState| {
                                st.ws.automation.retain(|j| j.id != id);
                                st.save_workspace();
                            }),
                        ));
                    }
                }
                if ui.add_enabled(sel.is_some(), egui::Button::new("📋").small()).on_hover_text("Duplicate job").clicked() {
                    if let Some(j) = sel.and_then(|id| st.ws.automation.iter().find(|j| j.id == id).cloned()) {
                        let mut c = j;
                        c.id = uuid::Uuid::new_v4().to_string();
                        c.name = format!("{} copy", c.name);
                        c.last_run = None;
                        c.last_status.clear();
                        c.interval_minutes = 0;
                        self.selected = Some(c.id.clone());
                        st.ws.automation.push(c);
                        st.save_workspace();
                    }
                }
                if ui.add(egui::Button::new(format!("{} New", icon::ADD)).small()).clicked() {
                    let n = st.ws.automation.len() + 1;
                    let j = AutomationJob {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: format!("Job {}", n),
                        steps: vec![],
                        interval_minutes: 0,
                        last_run: None,
                        last_status: String::new(),
                    };
                    self.selected = Some(j.id.clone());
                    st.ws.automation.push(j);
                    st.save_workspace();
                }
            });
        });
        ui.separator();
        if st.ws.automation.is_empty() {
            ui.label(RichText::new("No jobs yet. Click \"New\" to create one.").weak());
            return;
        }
        egui::ScrollArea::vertical().id_salt("auto-jobs").auto_shrink([false, false]).show(ui, |ui| {
            for j in &st.ws.automation {
                let running = self.runs.contains_key(&j.id);
                let selected = self.selected.as_deref() == Some(&j.id);
                let status_icon = if running {
                    "⏳"
                } else if j.last_status.is_empty() {
                    "○"
                } else if j.last_status.starts_with("Succeeded") {
                    "✔"
                } else {
                    "✖"
                };
                let sched = match due_in(j) {
                    None => "manual".to_string(),
                    Some(_) if running => "running…".to_string(),
                    Some(s) if s <= 0 => format!("every {} min · due now", j.interval_minutes),
                    Some(s) => format!("every {} min · next in {}", j.interval_minutes, fmt_duration(s)),
                };
                let r = ui.add_sized(
                    [ui.available_width(), 38.0],
                    egui::SelectableLabel::new(
                        selected,
                        RichText::new(format!("{} {}  {}\n      {}", status_icon, icon::AUTOMATION, j.name, sched)),
                    ),
                );
                if r.clicked() {
                    self.selected = Some(j.id.clone());
                }
            }
        });
    }

    fn conn_combo(ui: &mut egui::Ui, id: String, st: &AppState, conn: &mut String) {
        let label = if conn.is_empty() { "Select connection".to_string() } else { st.conn_name(conn) };
        egui::ComboBox::from_id_salt(id).selected_text(label).width(180.0).show_ui(ui, |ui| {
            for (cid, name) in st.connection_choices() {
                if ui.selectable_label(*conn == cid, name).clicked() {
                    *conn = cid;
                }
            }
        });
    }

    fn db_field(ui: &mut egui::Ui, id: String, st: &mut AppState, conn: &str, db: &mut String, hint: &str) {
        ui.add(egui::TextEdit::singleline(db).hint_text(hint).desired_width(150.0).min_size(egui::vec2(150.0, 0.0)));
        ui.menu_button("⏷", |ui| {
            if conn.is_empty() {
                ui.label("Select a connection first");
                return;
            }
            if st.session(conn).is_none() {
                ui.label("Connection not found");
                return;
            }
            let Some(oc) = st.conn_mut(conn) else { return };
            if oc.databases.is_loading() {
                ui.spinner();
            }
            let dbs = oc.visible_databases();
            egui::ScrollArea::vertical().id_salt(id).max_height(300.0).show(ui, |ui| {
                for d in dbs {
                    if ui.selectable_label(*db == d, &d).clicked() {
                        *db = d;
                        ui.close_menu();
                    }
                }
            });
        });
    }

    fn table_field(ui: &mut egui::Ui, id: String, st: &mut AppState, conn: &str, db: &str, table: &mut String) {
        ui.add(egui::TextEdit::singleline(table).hint_text("table").desired_width(150.0).min_size(egui::vec2(150.0, 0.0)));
        ui.menu_button("⏷", |ui| {
            let Some(cfg) = st.config(conn).cloned() else {
                ui.label("Select a connection first");
                return;
            };
            if st.session(conn).is_none() {
                return;
            }
            let scope = scope_of(&cfg, db);
            let tables = match st.conn_mut(conn) {
                Some(oc) => {
                    oc.load_objects(&scope, navdb_core::ObjectKind::Table);
                    if oc.objects(&scope, navdb_core::ObjectKind::Table).map(|l| l.is_loading()).unwrap_or(false) {
                        ui.spinner();
                    }
                    st.cached_tables(conn, &scope)
                }
                None => vec![],
            };
            egui::ScrollArea::vertical().id_salt(id).max_height(300.0).show(ui, |ui| {
                for t in tables {
                    if ui.selectable_label(*table == t, &t).clicked() {
                        *table = t;
                        ui.close_menu();
                    }
                }
            });
        });
    }

    fn steps_ui(ui: &mut egui::Ui, st: &mut AppState, job: &mut AutomationJob) {
        let mut action: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 delete)
        let n = job.steps.len();
        let jid = job.id.clone();
        for (i, step) in job.steps.iter_mut().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Step {}", i + 1)).strong());
                    let current = step_kind(step);
                    let conn = step_conn(step).to_string();
                    egui::ComboBox::from_id_salt(format!("auto-kind-{}-{}", jid, i)).selected_text(current).width(120.0).show_ui(ui, |ui| {
                        if ui.selectable_label(current == "Run Query", "Run Query").clicked() && current != "Run Query" {
                            *step = AutomationStep::RunQuery { connection_id: conn.clone(), database: String::new(), sql: String::new() };
                        }
                        if ui.selectable_label(current == "Backup", "Backup").clicked() && current != "Backup" {
                            *step = AutomationStep::Backup { connection_id: conn.clone(), database: String::new(), output_dir: String::new() };
                        }
                        if ui.selectable_label(current == "Export Table", "Export Table").clicked() && current != "Export Table" {
                            *step = AutomationStep::ExportTable {
                                connection_id: conn.clone(),
                                database: String::new(),
                                table: String::new(),
                                format: "csv".into(),
                                path: String::new(),
                            };
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icon::DELETE).on_hover_text("Remove step").clicked() {
                            action = Some((i, 0));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("⏷").small()).on_hover_text("Move down").clicked() {
                            action = Some((i, 1));
                        }
                        if ui.add_enabled(i > 0, egui::Button::new("⏶").small()).on_hover_text("Move up").clicked() {
                            action = Some((i, -1));
                        }
                    });
                });
                let sid = format!("{}-{}", jid, i);
                match step {
                    AutomationStep::RunQuery { connection_id, database, sql } => {
                        ui.horizontal(|ui| {
                            ui.label("Connection");
                            Self::conn_combo(ui, format!("auto-c-{}", sid), st, connection_id);
                            ui.label("Database");
                            Self::db_field(ui, format!("auto-d-{}", sid), st, connection_id, database, "(default)");
                        });
                        ui.add(
                            egui::TextEdit::multiline(sql)
                                .code_editor()
                                .hint_text("SQL statements (separate with ;)")
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                    }
                    AutomationStep::Backup { connection_id, database, output_dir } => {
                        ui.horizontal(|ui| {
                            ui.label("Connection");
                            Self::conn_combo(ui, format!("auto-c-{}", sid), st, connection_id);
                            ui.label("Database");
                            Self::db_field(ui, format!("auto-d-{}", sid), st, connection_id, database, "(default)");
                        });
                        ui.horizontal(|ui| {
                            ui.label("Output folder");
                            ui.add(egui::TextEdit::singleline(output_dir).hint_text(config::backups_dir().display().to_string()).desired_width(320.0).min_size(egui::vec2(320.0, 0.0)));
                            if ui.button("…").clicked() {
                                if let Some(p) = crate::ui::pick_folder_dialog() {
                                    *output_dir = p.display().to_string();
                                }
                            }
                        });
                        ui.label(RichText::new("A timestamped .sql dump (structure + data) is written on every run.").weak().small());
                    }
                    AutomationStep::ExportTable { connection_id, database, table, format, path } => {
                        ui.horizontal(|ui| {
                            ui.label("Connection");
                            Self::conn_combo(ui, format!("auto-c-{}", sid), st, connection_id);
                            ui.label("Database");
                            let conn = connection_id.clone();
                            Self::db_field(ui, format!("auto-d-{}", sid), st, &conn, database, "(default)");
                        });
                        ui.horizontal(|ui| {
                            ui.label("Table");
                            let conn = connection_id.clone();
                            Self::table_field(ui, format!("auto-t-{}", sid), st, &conn, database, table);
                            ui.label("Format");
                            let cur = parse_format(format).map(|f| f.label()).unwrap_or("Select format");
                            egui::ComboBox::from_id_salt(format!("auto-f-{}", sid)).selected_text(cur).width(170.0).show_ui(ui, |ui| {
                                for f in ExportFormat::ALL {
                                    if ui.selectable_label(parse_format(format) == Some(f), f.label()).clicked() {
                                        *format = f.extension().to_string();
                                    }
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.label("Output file");
                            ui.add(egui::TextEdit::singleline(path).hint_text("empty = backups folder; {date} and {table} are replaced").desired_width(360.0).min_size(egui::vec2(360.0, 0.0)));
                            if ui.button("…").clicked() {
                                let ext = parse_format(format).map(|f| f.extension()).unwrap_or("csv");
                                let name = format!("{}.{}", if table.is_empty() { "export" } else { table.as_str() }, ext);
                                if let Some(p) = crate::ui::save_file_dialog(&name, ext) {
                                    *path = p.display().to_string();
                                }
                            }
                        });
                    }
                }
            });
            ui.add_space(2.0);
        }
        if let Some((i, op)) = action {
            match op {
                0 => {
                    job.steps.remove(i);
                }
                -1 if i > 0 => job.steps.swap(i, i - 1),
                1 if i + 1 < job.steps.len() => job.steps.swap(i, i + 1),
                _ => {}
            }
        }
        let default_conn = st.selection.conn.clone().or_else(|| st.ws.connections.first().map(|c| c.id.clone())).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.menu_button(format!("{} Add Step", icon::ADD), |ui| {
                if ui.button("Run Query").clicked() {
                    job.steps.push(AutomationStep::RunQuery { connection_id: default_conn.clone(), database: String::new(), sql: String::new() });
                    ui.close_menu();
                }
                if ui.button("Backup").clicked() {
                    job.steps.push(AutomationStep::Backup { connection_id: default_conn.clone(), database: String::new(), output_dir: String::new() });
                    ui.close_menu();
                }
                if ui.button("Export Table").clicked() {
                    job.steps.push(AutomationStep::ExportTable {
                        connection_id: default_conn.clone(),
                        database: String::new(),
                        table: String::new(),
                        format: "csv".into(),
                        path: String::new(),
                    });
                    ui.close_menu();
                }
            });
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let Some(id) = self.selected.clone() else {
            ui.label(RichText::new("Select or create a job.").weak());
            return;
        };
        let Some(idx) = st.ws.automation.iter().position(|j| j.id == id) else {
            self.selected = None;
            return;
        };
        let mut original = st.ws.automation[idx].clone();
        let mut job = original.clone();
        let running = self.runs.contains_key(&id);
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut job.name).desired_width(240.0).min_size(egui::vec2(240.0, 0.0)));
            ui.separator();
            if ui.add_enabled(!running && !job.steps.is_empty(), egui::Button::new(RichText::new(format!("{} Run Now", icon::RUN)).strong())).clicked() {
                // the run picks up the job from the workspace: store edits first
                st.ws.automation[idx] = job.clone();
                original = job.clone();
                st.save_workspace();
                self.dirty = false;
                self.start(st, &id);
            }
            if running {
                ui.spinner();
                ui.label(RichText::new("running…").weak());
            }
            if ui.add_enabled(self.dirty, egui::Button::new(format!("{} Save", icon::SAVE))).clicked() {
                st.save_workspace();
                self.dirty = false;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Schedule: run every");
            ui.add(egui::DragValue::new(&mut job.interval_minutes).range(0..=10_080).suffix(" min"));
            ui.label(RichText::new("(0 = manual only; runs while NavDB Studio and this tab are open)").weak().small());
        });
        ui.horizontal(|ui| {
            let last = job.last_run.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|| "never".into());
            ui.label(RichText::new(format!("Last run: {}", last)).weak());
            if !job.last_status.is_empty() {
                let c = if job.last_status.starts_with("Succeeded") { Color32::from_rgb(40, 160, 70) } else { ui.visuals().error_fg_color };
                ui.colored_label(c, &job.last_status);
            }
            if let Some(s) = due_in(&job) {
                if !running {
                    ui.label(RichText::new(if s <= 0 { "· due now".to_string() } else { format!("· next run in {}", fmt_duration(s)) }).weak());
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt(format!("auto-steps-{}", id)).auto_shrink([false, false]).show(ui, |ui| {
            if job.steps.is_empty() {
                ui.label(RichText::new("This job has no steps yet.").weak());
            }
            Self::steps_ui(ui, st, &mut job);
        });
        if job != original {
            if let Some(j) = st.ws.automation.iter_mut().find(|j| j.id == id) {
                *j = job;
            }
            self.dirty = true;
        }
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        let lines = self
            .selected
            .as_ref()
            .and_then(|id| self.logs.get(id))
            .and_then(|l| l.lock().ok().map(|v| v.clone()))
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            if ui.small_button("📋 Copy").clicked() {
                ui.ctx().copy_text(lines.join("\n"));
            }
            if ui.small_button("Clear").clicked() {
                if let Some(id) = &self.selected {
                    if !self.runs.contains_key(id) {
                        self.logs.remove(id);
                    }
                }
            }
        });
        egui::ScrollArea::both().id_salt("auto-log").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if lines.is_empty() {
                ui.label(RichText::new("Run the job to see its output here.").weak());
            }
            for l in &lines {
                let color = if l.trim_start().starts_with('✖') {
                    ui.visuals().error_fg_color
                } else if l.trim_start().starts_with('✔') {
                    Color32::from_rgb(40, 160, 70)
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(l).monospace().color(color));
            }
        });
    }
}

impl Tab for AutomationTab {
    fn title(&self) -> String {
        format!("{} Automation", icon::AUTOMATION)
    }

    fn key(&self) -> String {
        "automation".into()
    }

    fn dirty(&self) -> bool {
        self.dirty
    }

    fn can_close(&mut self, st: &mut AppState) -> bool {
        if self.dirty {
            st.save_workspace();
            self.dirty = false;
        }
        true
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        self.poll(st);
        // keep "next run in" texts and scheduling current
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        egui::SidePanel::left("auto-left").resizable(true).default_width(250.0).width_range(180.0..=500.0).show_inside(ui, |ui| {
            self.job_list(ui, st);
        });
        egui::TopBottomPanel::bottom("auto-log-panel").resizable(true).default_height(170.0).height_range(60.0..=600.0).show_inside(ui, |ui| {
            self.log_panel(ui);
        });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.editor(ui, st);
        });
    }
}
