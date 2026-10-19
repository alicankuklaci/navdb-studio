//! Execute SQL File: run a script file statement by statement.

use std::path::PathBuf;
use std::time::Duration;

use egui::RichText;
use navdb_core::{DbKind, Pending, Scope, Session};

use super::dump::{self as job, lock, Progress, RunOpts, SharedProgress};
use super::Dialog;
use crate::state::{Action, AppState};
use crate::ui;

pub struct ExecuteSqlDialog {
    id: String,
    conn: Option<String>,
    scope: Option<Scope>,
    path: String,
    continue_on_error: bool,
    multi_query: bool,
    use_tx: bool,
    progress: Option<SharedProgress>,
    job: Option<Pending<()>>,
    outcome: Option<std::result::Result<(), String>>,
    ran_on: Option<String>,
    _worker: Option<Session>,
}

impl ExecuteSqlDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Option<Scope>) -> Self {
        let conn = if st.config(conn).is_some() { Some(conn.to_string()) } else { None };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn,
            scope,
            path: String::new(),
            continue_on_error: false,
            multi_query: true,
            use_tx: false,
            progress: None,
            job: None,
            outcome: None,
            ran_on: None,
            _worker: None,
        }
    }

    fn start(&mut self, st: &mut AppState) {
        let Some(cid) = self.conn.clone() else { return };
        let Some(base) = st.session(&cid) else {
            st.error("connection not found");
            return;
        };
        let worker = base.fork();
        let p = Progress::shared();
        let path = PathBuf::from(self.path.trim());
        let scope = self.scope.clone();
        let ro = RunOpts { continue_on_error: self.continue_on_error, transaction: self.use_tx };
        lock(&p).info(format!(
            "Executing {} on {}{}",
            path.display(),
            st.conn_name(&cid),
            scope.as_ref().map(|s| format!(" › {}", s.display())).unwrap_or_default()
        ));
        let p2 = p.clone();
        self.job = Some(worker.run(move |d| {
            lock(&p2).current = "Reading file".into();
            let text = job::read_text(&path)?;
            let stmts = job::split_script(&text, d.kind());
            lock(&p2).info(format!("{} statement(s) found", stmts.len()));
            let (ok, failed, affected) = job::run_statements(d, scope.as_ref(), &stmts, ro, &p2)?;
            let mut g = lock(&p2);
            g.current.clear();
            let el = g.started.elapsed();
            g.info(format!(
                "Finished: processed {} queries, {} succeeded, {} error(s), {} row(s) affected, {}",
                ok + failed,
                ok,
                failed,
                affected,
                job::fmt_duration(el)
            ));
            Ok(())
        }));
        self._worker = Some(worker);
        self.progress = Some(p);
        self.outcome = None;
        self.ran_on = Some(cid);
    }
}

impl Dialog for ExecuteSqlDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        if let Some(r) = job::finish_job(&mut self.job, self.progress.as_ref()) {
            match &r {
                Ok(()) => {
                    let errs = self.progress.as_ref().map(|p| lock(p).errors).unwrap_or(0);
                    if errs == 0 {
                        st.toast("SQL file executed");
                    } else {
                        st.error(format!("SQL file executed with {} error(s)", errs));
                    }
                }
                Err(e) => st.error(format!("Execute SQL file failed: {}", e)),
            }
            if let Some(c) = self.ran_on.clone() {
                st.push(Action::RefreshConnection(c));
            }
            self.outcome = Some(r);
        }
        let running = self.job.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        super::window("Execute SQL File", &self.id).open(&mut open).default_width(600.0).show(ctx, |ui| {
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new(("exec-grid", &self.id)).num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Connection:");
                    ui.horizontal(|ui| {
                        ui::scope_picker(ui, &format!("exec-scope-{}", self.id), st, &mut self.conn, &mut self.scope);
                    });
                    ui.end_row();
                    ui.label("File:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(380.0).hint_text("/path/to/script.sql"));
                        if ui.button("Browse…").clicked() {
                            let redis = self.conn.as_deref().and_then(|c| st.config(c)).map(|c| c.kind == DbKind::Redis).unwrap_or(false);
                            let exts: &[&str] = if redis { &["txt", "redis"] } else { &["sql", "txt"] };
                            if let Some(p) = rfd::FileDialog::new().add_filter("SQL script", exts).add_filter("All files", &["*"]).pick_file() {
                                self.path = p.display().to_string();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Encoding:");
                    ui.label(RichText::new("UTF-8 (invalid byte sequences are replaced)").weak());
                    ui.end_row();
                    ui.label("Options:");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut self.continue_on_error, "Continue on error");
                        ui.checkbox(&mut self.multi_query, "Run multiple queries in each execution")
                            .on_hover_text("Statements are sent one by one so failures can be reported individually.");
                        ui.checkbox(&mut self.use_tx, "Run in a single transaction");
                    });
                    ui.end_row();
                });
            });
            let path_ok = std::path::Path::new(self.path.trim()).is_file();
            if !self.path.trim().is_empty() && !path_ok {
                ui::error_text(ui, "File not found");
            }
            if let Some(p) = &self.progress {
                ui.add_space(6.0);
                job::progress_view(ui, p, running, 200.0);
            }
            let can_start = !running && path_ok && self.conn.is_some();
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("Stop").clicked() {
                            if let Some(p) = &self.progress {
                                lock(p).cancel = true;
                            }
                        }
                    } else if ui.button("Close").clicked() {
                        close = true;
                    }
                    if ui.add_enabled(can_start, egui::Button::new(RichText::new("Start").strong())).clicked() {
                        self.start(st);
                    }
                });
            });
        });
        open && !close
    }
}
