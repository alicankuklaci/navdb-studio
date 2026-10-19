//! Dump SQL File dialog, plus small helpers shared by the data-transfer style
//! tool windows (export/import wizards, execute SQL file, backup):
//! progress reporting, wizard step header, footer buttons, file manager access
//! and a statement runner used on worker threads.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Ui};
use navdb_core::driver::Driver;
use navdb_core::dump::{self, DumpOptions};
use navdb_core::{anyhow, bail, DbKind, Dialect, ObjectKind, Pending, Result, Scope, Session};

use super::Dialog;
use crate::state::{AppState, Loadable};
use crate::ui;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) struct LogLine {
    pub error: bool,
    pub text: String,
}

/// Progress of a background job, shared between the worker closure and the UI.
pub(crate) struct Progress {
    pub done: usize,
    pub total: usize,
    pub current: String,
    pub log: Vec<LogLine>,
    pub errors: usize,
    /// Set by the UI; workers check it between units of work.
    pub cancel: bool,
    pub started: Instant,
    pub finished: Option<Duration>,
}

pub(crate) type SharedProgress = Arc<Mutex<Progress>>;

impl Progress {
    pub fn shared() -> SharedProgress {
        Arc::new(Mutex::new(Progress {
            done: 0,
            total: 0,
            current: String::new(),
            log: Vec::new(),
            errors: 0,
            cancel: false,
            started: Instant::now(),
            finished: None,
        }))
    }

    pub fn info(&mut self, s: impl Into<String>) {
        self.log.push(LogLine { error: false, text: format!("[{}] {}", clock(), s.into()) });
    }

    pub fn err(&mut self, s: impl Into<String>) {
        self.errors += 1;
        self.log.push(LogLine { error: true, text: format!("[{}] {}", clock(), s.into()) });
    }

    pub fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(|| self.started.elapsed())
    }
}

fn clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

pub(crate) fn lock(p: &SharedProgress) -> MutexGuard<'_, Progress> {
    match p.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

pub(crate) fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{:.2}s", s)
    } else {
        format!("{}m {:02}s", (s / 60.0) as u64, (s as u64) % 60)
    }
}

/// One-line, length limited preview of a statement for logs.
pub(crate) fn preview_sql(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 160 {
        format!("{}…", one.chars().take(160).collect::<String>())
    } else {
        one
    }
}

/// A finished background job: mark the progress finished and return the outcome.
pub(crate) fn finish_job<T>(job: &mut Option<Pending<T>>, p: Option<&SharedProgress>) -> Option<std::result::Result<T, String>> {
    let r = job.as_mut()?.poll()?;
    *job = None;
    if let Some(p) = p {
        let mut g = lock(p);
        let el = g.started.elapsed();
        g.finished = Some(el);
        if let Err(e) = &r {
            g.err(format!("{:#}", e));
        }
    }
    Some(r.map_err(|e| format!("{:#}", e)))
}

/// Progress bar, counters and the scrolling log.
pub(crate) fn progress_view(ui: &mut Ui, p: &SharedProgress, running: bool, log_height: f32) {
    let g = lock(p);
    let frac = if g.total > 0 { g.done as f32 / g.total as f32 } else if g.finished.is_some() { 1.0 } else { 0.0 };
    let text = if g.total > 0 { format!("{} / {}", g.done, g.total) } else { String::new() };
    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(text).animate(running && g.total == 0));
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("Elapsed: {}", fmt_duration(g.elapsed()))).weak());
        ui.separator();
        let ec = if g.errors > 0 { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
        ui.label(RichText::new(format!("Errors: {}", g.errors)).color(ec));
        if running && !g.current.is_empty() {
            ui.separator();
            ui.spinner();
            ui.label(RichText::new(&g.current).weak());
        }
    });
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("job-log")
                .max_height(log_height)
                .min_scrolled_height(log_height)
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let err = ui.visuals().error_fg_color;
                    for l in &g.log {
                        let t = RichText::new(&l.text).monospace().size(11.5);
                        ui.add(egui::Label::new(if l.error { t.color(err) } else { t }).wrap());
                    }
                });
        });
}

/// Numbered step pills at the top of a wizard.
pub(crate) fn steps_header(ui: &mut Ui, steps: &[&str], current: usize) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, s) in steps.iter().enumerate() {
            let v = ui.visuals();
            let (fill, color) = if i == current {
                (v.selection.bg_fill, v.strong_text_color())
            } else if i < current {
                (v.faint_bg_color, v.text_color())
            } else {
                (Color32::TRANSPARENT, v.weak_text_color())
            };
            let mark = if i < current { "✔".to_string() } else { format!("{}", i + 1) };
            egui::Frame::new()
                .fill(fill)
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    let t = RichText::new(format!("{}  {}", mark, s)).color(color);
                    ui.label(if i == current { t.strong() } else { t });
                });
            if i + 1 < steps.len() {
                ui.label(RichText::new("›").weak());
            }
        }
    });
    ui.separator();
}

#[derive(Default)]
pub(crate) struct FooterClicks {
    pub back: bool,
    pub next: bool,
    pub close: bool,
}

/// Right aligned Back / Next|Start / Close buttons.
/// `next` = (label, enabled); `None` hides the button.
pub(crate) fn footer(ui: &mut Ui, back_enabled: bool, next: Option<(&str, bool)>, close_label: &str) -> FooterClicks {
    let mut c = FooterClicks::default();
    ui.separator();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(close_label).clicked() {
                c.close = true;
            }
            if let Some((label, enabled)) = next {
                if ui.add_enabled(enabled, egui::Button::new(RichText::new(label).strong())).clicked() {
                    c.next = true;
                }
            }
            if ui.add_enabled(back_enabled, egui::Button::new("‹ Back")).clicked() {
                c.back = true;
            }
        });
    });
    c
}

/// Open a file or folder with the system default application.
pub(crate) fn open_path(p: &Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(p).spawn();
    let _ = r;
}

/// Show a file selected in Finder / Explorer (parent folder elsewhere).
pub(crate) fn reveal_path(p: &Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(p)).spawn();
    let _ = r;
}

pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Make a string safe to use as a file name.
pub(crate) fn safe_file_name(s: &str) -> String {
    let v: String = s.chars().map(|c| if c.is_alphanumeric() || "-_.@ ".contains(c) { c } else { '_' }).collect();
    if v.trim().is_empty() {
        "untitled".into()
    } else {
        v
    }
}

pub(crate) fn begin_sql(d: Dialect) -> &'static str {
    match d {
        Dialect::MySql => "START TRANSACTION",
        _ => "BEGIN",
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RunOpts {
    pub continue_on_error: bool,
    pub transaction: bool,
}

/// Execute `stmts` one by one on a worker thread, updating `p`.
/// Returns (succeeded, failed, affected rows).
pub(crate) fn run_statements(
    drv: &mut dyn Driver,
    scope: Option<&Scope>,
    stmts: &[String],
    o: RunOpts,
    p: &SharedProgress,
) -> Result<(usize, usize, u64)> {
    let redis = drv.kind() == DbKind::Redis;
    let d = drv.dialect();
    let tx = o.transaction && !redis;
    // PostgreSQL aborts the whole transaction on error: protect statements with savepoints.
    let savepoints = tx && o.continue_on_error && d == Dialect::Postgres;
    {
        let mut g = lock(p);
        g.total = stmts.len();
        g.done = 0;
    }
    if tx {
        drv.execute(scope, begin_sql(d))?;
    }
    let (mut ok, mut failed, mut affected) = (0usize, 0usize, 0u64);
    for (i, s) in stmts.iter().enumerate() {
        if lock(p).cancel {
            if tx {
                let _ = drv.execute(scope, "ROLLBACK");
            }
            lock(p).err("Cancelled by user");
            bail!("cancelled");
        }
        if savepoints {
            let _ = drv.execute(scope, "SAVEPOINT navdb_sp");
        }
        match drv.execute(scope, s) {
            Ok(res) => {
                ok += 1;
                affected += res.iter().map(|r| r.affected_rows).sum::<u64>();
                if savepoints {
                    let _ = drv.execute(scope, "RELEASE SAVEPOINT navdb_sp");
                }
            }
            Err(e) => {
                failed += 1;
                if savepoints {
                    let _ = drv.execute(scope, "ROLLBACK TO SAVEPOINT navdb_sp");
                }
                lock(p).err(format!("[{}] {:#}\n    {}", i + 1, e, preview_sql(s)));
                if !o.continue_on_error {
                    if tx {
                        let _ = drv.execute(scope, "ROLLBACK");
                        lock(p).info("Transaction rolled back");
                    }
                    lock(p).done = i + 1;
                    return Err(anyhow!("stopped at statement {} of {}", i + 1, stmts.len()));
                }
            }
        }
        let mut g = lock(p);
        g.done = i + 1;
        g.current = format!("Statement {} / {}", i + 1, stmts.len());
    }
    if tx {
        drv.execute(scope, "COMMIT")?;
    }
    Ok((ok, failed, affected))
}

/// Split a script for the given connection kind (Redis: one command per line).
pub(crate) fn split_script(sql: &str, kind: DbKind) -> Vec<String> {
    if kind.base() == DbKind::Redis {
        sql.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("--"))
            .map(|l| l.to_string())
            .collect()
    } else {
        navdb_core::sqlutil::split_statements(sql, kind.dialect())
            .into_iter()
            .filter(|s| !navdb_core::sqlutil::strip_leading_comments(s).trim().is_empty())
            .collect()
    }
}

/// Read a text file, tolerating invalid UTF-8 and a BOM.
pub(crate) fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!("reading {}: {}", path.display(), e))?;
    let s = String::from_utf8_lossy(&bytes).to_string();
    Ok(s.strip_prefix('\u{feff}').map(|s| s.to_string()).unwrap_or(s))
}

/// Dump a scope into `path` on the calling (worker) thread.
pub(crate) fn dump_to_file(drv: &mut dyn Driver, scope: &Scope, o: &DumpOptions, path: &Path, p: &SharedProgress) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let f = std::fs::File::create(path).map_err(|e| anyhow!("creating {}: {}", path.display(), e))?;
    let mut w = std::io::BufWriter::new(f);
    let p2 = p.clone();
    let mut last = String::new();
    let mut cb = |done: usize, total: usize, cur: &str| {
        let mut g = lock(&p2);
        g.done = done;
        g.total = total;
        if !cur.is_empty() && cur != last {
            g.current = format!("Dumping {}", cur);
            g.info(format!("Dumping {}", cur));
            last = cur.to_string();
        }
    };
    dump::dump(drv, scope, o, &mut w, &mut cb)?;
    w.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dump SQL File dialog
// ---------------------------------------------------------------------------

pub struct DumpDialog {
    id: String,
    conn: String,
    conn_name: String,
    kind: DbKind,
    scope: Scope,
    session: Option<Session>,
    path: String,
    opts: DumpOptions,
    preselect: Vec<String>,
    tables: Loadable<Vec<String>>,
    selected: Vec<(String, bool)>,
    table_filter: String,
    progress: Option<SharedProgress>,
    job: Option<Pending<()>>,
    outcome: Option<std::result::Result<(), String>>,
    _worker: Option<Session>,
}

impl DumpDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, tables: Vec<String>, with_data: bool) -> Self {
        let session = st.session(conn);
        let kind = st.config(conn).map(|c| c.kind).unwrap_or_default();
        let loading = match &session {
            Some(s) if kind.is_sql() => {
                let sc = scope.clone();
                Loadable::Loading(s.run(move |d| Ok(d.list_objects(&sc, ObjectKind::Table)?.into_iter().map(|o| o.name).collect())))
            }
            _ => Loadable::NotLoaded,
        };
        // dumping picked tables: name the file after them and leave out views/routines,
        // which usually depend on tables outside the selection
        let file = match (&scope.schema, tables.as_slice()) {
            (_, [one]) => format!("{}.sql", one),
            (Some(s), _) => format!("{}_{}.sql", scope.db, s),
            (None, _) => format!("{}.sql", scope.db),
        };
        let whole_db = tables.is_empty();
        let path = home_dir().join(safe_file_name(&file)).display().to_string();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            kind,
            scope,
            session,
            path,
            opts: DumpOptions { data: with_data, include_views: whole_db, include_routines: whole_db, ..Default::default() },
            preselect: tables,
            tables: loading,
            selected: vec![],
            table_filter: String::new(),
            progress: None,
            job: None,
            outcome: None,
            _worker: None,
        }
    }

    fn start(&mut self) {
        let Some(base) = &self.session else { return };
        let worker = base.fork();
        let p = Progress::shared();
        let mut o = self.opts.clone();
        let all = self.selected.iter().all(|(_, s)| *s);
        o.tables = if all { vec![] } else { self.selected.iter().filter(|(_, s)| *s).map(|(n, _)| n.clone()).collect() };
        let scope = self.scope.clone();
        let path = PathBuf::from(&self.path);
        lock(&p).info(format!("Dumping {} to {}", scope.display(), path.display()));
        let p2 = p.clone();
        self.job = Some(worker.run(move |d| {
            dump_to_file(d, &scope, &o, &path, &p2)?;
            let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
            let mut g = lock(&p2);
            let el = g.started.elapsed();
            g.current.clear();
            g.info(format!("Finished successfully – {} written in {}", ui::format_bytes(size), fmt_duration(el)));
            Ok(())
        }));
        self._worker = Some(worker);
        self.progress = Some(p);
        self.outcome = None;
    }
}

impl Dialog for DumpDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        if self.tables.poll() {
            if let Some(t) = self.tables.ready() {
                self.selected = t.iter().map(|n| (n.clone(), self.preselect.is_empty() || self.preselect.contains(n))).collect();
            }
        }
        if let Some(r) = finish_job(&mut self.job, self.progress.as_ref()) {
            match &r {
                Ok(()) => st.toast(format!("Dump of {} finished", self.scope.display())),
                Err(e) => st.error(format!("Dump failed: {}", e)),
            }
            self.outcome = Some(r);
        }
        let running = self.job.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let title = if self.opts.data { "Dump SQL File (Structure + Data)" } else { "Dump SQL File (Structure Only)" };
        super::window(title, &self.id).open(&mut open).default_width(560.0).show(ctx, |ui| {
            ui.label(RichText::new(format!("{}  ›  {}", self.conn_name, self.scope.display())).strong());
            ui.add_space(4.0);
            if !self.kind.is_sql() {
                ui.label("Dump SQL File is only available for SQL databases.");
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            }
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new(("dump-grid", &self.id)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Output file:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(360.0).min_size(egui::vec2(360.0, 0.0)));
                        if ui.button("…").on_hover_text("Choose file").clicked() {
                            let name = Path::new(&self.path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                            if let Some(p) = ui::save_file_dialog(&name, "sql") {
                                self.path = p.display().to_string();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Content:");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.opts.structure, "Structure");
                        ui.checkbox(&mut self.opts.data, "Data");
                    });
                    ui.end_row();
                    ui.label("Options:");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut self.opts.drop_before_create, "Add DROP statements before CREATE");
                        ui.checkbox(&mut self.opts.include_views, "Include views");
                        ui.checkbox(&mut self.opts.include_routines, "Include functions / procedures");
                        ui.checkbox(&mut self.opts.include_triggers, "Include triggers (and indexes)");
                        ui.horizontal(|ui| {
                            ui.label("Records per INSERT:");
                            ui.add(egui::DragValue::new(&mut self.opts.rows_per_insert).range(1..=10000));
                        });
                    });
                    ui.end_row();
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Tables").strong());
                    ui.add(egui::TextEdit::singleline(&mut self.table_filter).hint_text("Filter").desired_width(140.0).min_size(egui::vec2(140.0, 0.0)));
                    if ui.small_button("Select All").clicked() {
                        self.selected.iter_mut().for_each(|s| s.1 = true);
                    }
                    if ui.small_button("Deselect All").clicked() {
                        self.selected.iter_mut().for_each(|s| s.1 = false);
                    }
                    let n = self.selected.iter().filter(|s| s.1).count();
                    ui.label(RichText::new(format!("{} of {} selected", n, self.selected.len())).weak());
                });
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("dump-tables").max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                        if ui::show_loadable(ui, &self.tables, "tables").is_some() {
                            let f = self.table_filter.to_lowercase();
                            for (name, sel) in self.selected.iter_mut() {
                                if f.is_empty() || name.to_lowercase().contains(&f) {
                                    ui.checkbox(sel, format!("{} {}", ObjectKind::Table.icon(), name));
                                }
                            }
                            if self.selected.is_empty() {
                                ui.label(RichText::new("No tables").weak());
                            }
                        }
                    });
                });
            });
            if let Some(p) = &self.progress {
                ui.add_space(6.0);
                progress_view(ui, p, running, 140.0);
            }
            let any = self.selected.iter().any(|s| s.1) || (self.opts.structure && self.opts.include_views);
            let can_start = !running && !self.path.trim().is_empty() && (self.opts.structure || self.opts.data) && any && self.tables.ready().is_some();
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!running, egui::Button::new("Close")).clicked() {
                        close = true;
                    }
                    if ui.add_enabled(can_start, egui::Button::new(RichText::new("Start").strong())).clicked() {
                        self.start();
                    }
                    if matches!(self.outcome, Some(Ok(()))) && ui.button("Reveal in Finder").clicked() {
                        reveal_path(Path::new(&self.path));
                    }
                });
            });
        });
        let _ = &self.conn;
        open && !close
    }
}
