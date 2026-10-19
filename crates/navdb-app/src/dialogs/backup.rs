//! Backup / Restore: timestamped SQL dumps (and SQLite file copies) stored under
//! `backups_dir()/<connection id>/<database>/` with a JSON sidecar per backup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::RichText;
use navdb_core::dump::DumpOptions;
use navdb_core::{anyhow, ConnectionConfig, DbKind, Pending, Result, Scope, Session};
use serde::{Deserialize, Serialize};

use super::dump::{self as job, lock, Progress, RunOpts, SharedProgress};
use super::Dialog;
use crate::state::AppState;
use crate::ui;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Sidecar {
    comment: String,
    created: String,
    connection: String,
    database: String,
    server: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Sql,
    Sqlite,
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    kind: FileKind,
    created: String,
    size: u64,
    comment: String,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Panel {
    None,
    NewBackup,
    ConfirmRestore,
    ConfirmDelete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Backup,
    Restore,
    Extract,
}

pub struct BackupDialog {
    id: String,
    conn: String,
    conn_name: String,
    cfg: Option<ConnectionConfig>,
    kind: DbKind,
    scope: Scope,
    session: Option<Session>,
    dir: PathBuf,
    entries: Vec<Entry>,
    selected: Option<usize>,
    panel: Panel,
    comment: String,
    sqlite_copy: bool,
    structure_only: bool,
    continue_on_error: bool,
    progress: Option<SharedProgress>,
    job: Option<(JobKind, Pending<String>)>,
    _worker: Option<Session>,
}

fn sidecar_path(p: &Path) -> PathBuf {
    p.with_extension("json")
}

fn scan(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let path = e.path();
        let kind = match path.extension().and_then(|s| s.to_str()) {
            Some("sql") => FileKind::Sql,
            Some("sqlite") => FileKind::Sqlite,
            _ => continue,
        };
        let meta = e.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let side: Sidecar = std::fs::read_to_string(sidecar_path(&path)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let created = if !side.created.is_empty() {
            side.created.clone()
        } else {
            meta.and_then(|m| m.modified().ok())
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default()
        };
        out.push(Entry { path, kind, created, size, comment: side.comment });
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then(b.path.cmp(&a.path)));
    out
}

/// SQL text of a backup file (SQLite copies are dumped on the fly).
fn backup_sql(e_path: &Path, kind: FileKind, p: &SharedProgress) -> Result<String> {
    match kind {
        FileKind::Sql => job::read_text(e_path),
        FileKind::Sqlite => {
            let cfg = ConnectionConfig { database: e_path.display().to_string(), read_only: true, ..ConnectionConfig::new(DbKind::Sqlite) };
            let mut drv = navdb_core::driver::connect(&cfg)?;
            let mut buf = Vec::new();
            lock(p).info("Reading SQLite backup file");
            navdb_core::dump::dump(drv.as_mut(), &Scope::new("main", None), &DumpOptions::default(), &mut buf, &mut |_, _, _| {})?;
            Ok(String::from_utf8_lossy(&buf).to_string())
        }
    }
}

impl BackupDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        let session = st.session(conn);
        let sub = match &scope.schema {
            Some(s) => format!("{}.{}", scope.db, s),
            None => scope.db.clone(),
        };
        let dir = navdb_core::config::backups_dir().join(job::safe_file_name(conn)).join(job::safe_file_name(&sub));
        let entries = scan(&dir);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            cfg,
            kind,
            scope,
            session,
            dir,
            entries,
            selected: None,
            panel: Panel::None,
            comment: String::new(),
            sqlite_copy: kind == DbKind::Sqlite,
            structure_only: false,
            continue_on_error: false,
            progress: None,
            job: None,
            _worker: None,
        }
    }

    fn rescan(&mut self) {
        let sel = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        self.entries = scan(&self.dir);
        self.selected = sel.and_then(|p| self.entries.iter().position(|e| e.path == p));
    }

    fn selected_entry(&self) -> Option<Entry> {
        self.selected.and_then(|i| self.entries.get(i)).cloned()
    }

    fn begin(&mut self, kind: JobKind, p: SharedProgress, pending: Pending<String>, worker: Option<Session>) {
        self.job = Some((kind, pending));
        self.progress = Some(p);
        self._worker = worker;
        self.panel = Panel::None;
    }

    fn start_backup(&mut self) {
        let Some(base) = &self.session else { return };
        let worker = base.fork();
        let p = Progress::shared();
        let stamp = chrono::Local::now();
        let stem = format!("backup_{}", stamp.format("%Y%m%d_%H%M%S"));
        let sql_path = self.dir.join(format!("{}.sql", stem));
        let file_path = self.dir.join(format!("{}.sqlite", stem));
        let side = Sidecar {
            comment: self.comment.trim().to_string(),
            created: stamp.format("%Y-%m-%d %H:%M:%S").to_string(),
            connection: self.conn_name.clone(),
            database: self.scope.display(),
            server: String::new(),
        };
        let copy = self.sqlite_copy && self.kind == DbKind::Sqlite;
        let o = DumpOptions { data: !self.structure_only, ..Default::default() };
        let scope = self.scope.clone();
        let dir = self.dir.clone();
        lock(&p).info(format!("Backing up {} to {}", scope.display(), dir.display()));
        let p2 = p.clone();
        let pending = worker.run(move |d| {
            std::fs::create_dir_all(&dir)?;
            let mut side = side;
            side.server = d.server_version().unwrap_or_default();
            job::dump_to_file(d, &scope, &o, &sql_path, &p2)?;
            lock(&p2).info(format!("SQL backup written: {}", sql_path.display()));
            if copy {
                lock(&p2).current = "Copying database file".into();
                let dl = d.dialect();
                let sql = format!("VACUUM {} INTO {}", dl.quote_ident(&scope.db), dl.string_literal(&file_path.display().to_string()));
                match d.execute(None, &sql) {
                    Ok(_) => {
                        lock(&p2).info(format!("Database file copied: {}", file_path.display()));
                        let _ = std::fs::write(sidecar_path(&file_path), serde_json::to_string_pretty(&side)?);
                    }
                    Err(e) => lock(&p2).err(format!("VACUUM INTO failed: {:#}", e)),
                }
            }
            std::fs::write(sidecar_path(&sql_path), serde_json::to_string_pretty(&side)?)?;
            let mut g = lock(&p2);
            g.current.clear();
            let el = g.started.elapsed();
            g.info(format!("Backup finished in {}", job::fmt_duration(el)));
            Ok("Backup finished".to_string())
        });
        self.comment.clear();
        self.begin(JobKind::Backup, p, pending, Some(worker));
    }

    fn start_restore(&mut self) {
        let Some(e) = self.selected_entry() else { return };
        let Some(base) = &self.session else { return };
        let worker = base.fork();
        let p = Progress::shared();
        let scope = self.scope.clone();
        let kind = self.kind;
        let ro = RunOpts { continue_on_error: self.continue_on_error, transaction: false };
        lock(&p).info(format!("Restoring {} into {}", e.path.display(), scope.display()));
        let p2 = p.clone();
        let pending = worker.run(move |d| {
            lock(&p2).current = "Reading backup".into();
            let sql = backup_sql(&e.path, e.kind, &p2)?;
            let stmts = job::split_script(&sql, kind);
            lock(&p2).info(format!("{} statement(s) to execute", stmts.len()));
            let (ok, failed, _) = job::run_statements(d, Some(&scope), &stmts, ro, &p2)?;
            let mut g = lock(&p2);
            g.current.clear();
            let el = g.started.elapsed();
            g.info(format!("Restore finished: {} succeeded, {} failed, {}", ok, failed, job::fmt_duration(el)));
            Ok(if failed == 0 { "Restore finished".to_string() } else { format!("Restore finished with {} error(s)", failed) })
        });
        self.begin(JobKind::Restore, p, pending, Some(worker));
    }

    fn start_extract(&mut self, st: &AppState) {
        let Some(e) = self.selected_entry() else { return };
        let stem = e.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "backup".into());
        let name = format!("{}_{}.sql", job::safe_file_name(&self.scope.db), stem);
        let Some(target) = ui::save_file_dialog(&name, "sql") else { return };
        let p = Progress::shared();
        let p2 = p.clone();
        lock(&p).info(format!("Extracting SQL to {}", target.display()));
        let pending = navdb_core::session::spawn_task(st.notify.clone(), move || {
            match e.kind {
                FileKind::Sql => {
                    std::fs::copy(&e.path, &target).map_err(|er| anyhow!("copy failed: {}", er))?;
                }
                FileKind::Sqlite => {
                    let sql = backup_sql(&e.path, e.kind, &p2)?;
                    std::fs::write(&target, sql)?;
                }
            }
            lock(&p2).info(format!("SQL saved to {}", target.display()));
            Ok("SQL extracted".to_string())
        });
        self.begin(JobKind::Extract, p, pending, None);
    }

    fn delete_selected(&mut self, st: &mut AppState) {
        let Some(e) = self.selected_entry() else { return };
        match std::fs::remove_file(&e.path) {
            Ok(()) => {
                // keep the sidecar while a sibling backup file of the same stem still uses it
                let other = if e.kind == FileKind::Sql { e.path.with_extension("sqlite") } else { e.path.with_extension("sql") };
                if !other.exists() {
                    let _ = std::fs::remove_file(sidecar_path(&e.path));
                }
                st.toast("Backup deleted");
            }
            Err(er) => st.error(format!("Delete failed: {}", er)),
        }
        self.selected = None;
        self.panel = Panel::None;
        self.rescan();
    }

    fn list_ui(&mut self, ui: &mut egui::Ui, running: bool) {
        use egui_extras::{Column, TableBuilder};
        let entries = &self.entries;
        let mut clicked = None;
        let mut double = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_min_height(200.0);
            ui.set_max_height(260.0);
            if entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("No backups yet. Click \"New Backup\" to create one.").weak());
                });
                return;
            }
            TableBuilder::new(ui)
                .id_salt(("bk-table", &self.id))
                .striped(true)
                .sense(egui::Sense::click())
                .max_scroll_height(220.0)
                .column(Column::initial(150.0).at_least(100.0))
                .column(Column::initial(80.0).at_least(60.0))
                .column(Column::initial(70.0).at_least(50.0))
                .column(Column::remainder().at_least(120.0))
                .header(20.0, |mut h| {
                    for t in ["Date", "Size", "Type", "Comment"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, entries.len(), |mut row| {
                        let i = row.index();
                        let e = &entries[i];
                        row.set_selected(self.selected == Some(i));
                        row.col(|ui| {
                            ui.add(egui::Label::new(&e.created).selectable(false));
                        });
                        row.col(|ui| {
                            ui.add(egui::Label::new(ui::format_bytes(e.size as i64)).selectable(false));
                        });
                        row.col(|ui| {
                            ui.add(egui::Label::new(if e.kind == FileKind::Sql { "SQL" } else { "SQLite" }).selectable(false));
                        });
                        row.col(|ui| {
                            let t = if e.comment.is_empty() { RichText::new("—").weak() } else { RichText::new(&e.comment) };
                            ui.add(egui::Label::new(t).selectable(false).truncate());
                        });
                        let r = row.response();
                        if r.clicked() {
                            clicked = Some(i);
                        }
                        if r.double_clicked() {
                            double = Some(i);
                        }
                    });
                });
        });
        if !running {
            if let Some(i) = clicked {
                self.selected = Some(i);
                if matches!(self.panel, Panel::ConfirmDelete | Panel::ConfirmRestore) {
                    self.panel = Panel::None;
                }
            }
            if let Some(i) = double {
                self.selected = Some(i);
                self.panel = Panel::ConfirmRestore;
            }
        }
    }
}

impl Dialog for BackupDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        let mut finished = None;
        if let Some((kind, pending)) = &mut self.job {
            if let Some(r) = pending.poll() {
                finished = Some((*kind, r));
            }
        }
        if let Some((kind, r)) = finished {
            self.job = None;
            if let Some(p) = &self.progress {
                let mut g = lock(p);
                let el = g.started.elapsed();
                g.finished = Some(el);
                g.current.clear();
                if let Err(e) = &r {
                    g.err(format!("{:#}", e));
                }
            }
            match r {
                Ok(msg) => st.toast(msg),
                Err(e) => st.error(format!("{:#}", e)),
            }
            match kind {
                JobKind::Backup => {
                    self.rescan();
                    if !self.entries.is_empty() {
                        self.selected = Some(0);
                    }
                }
                JobKind::Restore => st.refresh_scope(&self.conn, &self.scope),
                JobKind::Extract => {}
            }
        }
        let running = self.job.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        super::window("Backup / Restore", &self.id).open(&mut open).default_width(640.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}  ›  {}", self.conn_name, self.scope.display())).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(format!("{} Refresh", crate::ui::style::icon::REFRESH)).clicked() {
                        self.rescan();
                    }
                });
            });
            if !self.kind.is_sql() || self.cfg.is_none() {
                ui.add_space(6.0);
                ui.label("Backups are only available for SQL databases.");
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            }
            ui.add_space(4.0);
            let has_sel = self.selected_entry().is_some();
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(format!("{} New Backup", crate::ui::style::icon::ADD)).clicked() {
                        self.panel = if self.panel == Panel::NewBackup { Panel::None } else { Panel::NewBackup };
                    }
                    if ui.add_enabled(has_sel, egui::Button::new("⟲ Restore…")).clicked() {
                        self.panel = Panel::ConfirmRestore;
                    }
                    if ui.add_enabled(has_sel, egui::Button::new("Extract SQL…")).clicked() {
                        self.start_extract(st);
                    }
                    if ui.add_enabled(has_sel, egui::Button::new("Reveal in Finder")).clicked() {
                        if let Some(e) = self.selected_entry() {
                            job::reveal_path(&e.path);
                        }
                    }
                    if ui.add_enabled(has_sel, egui::Button::new(format!("{} Delete", crate::ui::style::icon::DELETE))).clicked() {
                        self.panel = Panel::ConfirmDelete;
                    }
                });
            });
            ui.add_space(4.0);
            self.list_ui(ui, running);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} backup file(s) in", self.entries.len())).weak());
                if ui.link(RichText::new(self.dir.display().to_string()).small()).clicked() {
                    let _ = std::fs::create_dir_all(&self.dir);
                    job::open_path(&self.dir);
                }
            });

            // action panels
            match self.panel {
                Panel::None => {}
                Panel::NewBackup => {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.label(RichText::new("New Backup").strong());
                        ui.horizontal(|ui| {
                            ui.label("Comment:");
                            ui.add(egui::TextEdit::singleline(&mut self.comment).desired_width(380.0).min_size(egui::vec2(380.0, 0.0)).hint_text("optional"));
                        });
                        ui.checkbox(&mut self.structure_only, "Structure only (no data)");
                        if self.kind == DbKind::Sqlite {
                            ui.checkbox(&mut self.sqlite_copy, "Also save a copy of the database file (VACUUM INTO)");
                        }
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("Start Backup").strong()).clicked() {
                                self.start_backup();
                            }
                            if ui.button("Cancel").clicked() {
                                self.panel = Panel::None;
                            }
                        });
                    });
                }
                Panel::ConfirmRestore => {
                    if let Some(e) = self.selected_entry() {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.label(RichText::new("Restore Backup").strong());
                            ui.add(
                                egui::Label::new(format!(
                                    "Execute backup from {} into \"{}\"? Existing objects with the same names will be dropped and recreated.",
                                    e.created,
                                    self.scope.display()
                                ))
                                .wrap(),
                            );
                            ui.checkbox(&mut self.continue_on_error, "Continue on error");
                            ui.horizontal(|ui| {
                                if ui.button(RichText::new("Restore").strong().color(ui.visuals().warn_fg_color)).clicked() {
                                    self.start_restore();
                                }
                                if ui.button("Cancel").clicked() {
                                    self.panel = Panel::None;
                                }
                            });
                        });
                    }
                }
                Panel::ConfirmDelete => {
                    if let Some(e) = self.selected_entry() {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.label(format!("Delete backup {} ({})? This cannot be undone.", e.created, e.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()));
                            ui.horizontal(|ui| {
                                if ui.button(RichText::new("Delete").color(ui.visuals().error_fg_color)).clicked() {
                                    self.delete_selected(st);
                                }
                                if ui.button("Cancel").clicked() {
                                    self.panel = Panel::None;
                                }
                            });
                        });
                    }
                }
            }

            if let Some(p) = &self.progress {
                ui.add_space(6.0);
                job::progress_view(ui, p, running, 130.0);
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        let restoring = matches!(self.job, Some((JobKind::Restore, _)));
                        if ui.add_enabled(restoring, egui::Button::new("Stop")).clicked() {
                            if let Some(p) = &self.progress {
                                lock(p).cancel = true;
                            }
                        }
                    } else if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        });
        open && !close
    }
}
