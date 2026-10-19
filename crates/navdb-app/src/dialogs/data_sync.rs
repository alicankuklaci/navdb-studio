//! Data Synchronization: compare rows of same-name tables (by primary key) and
//! insert/update/delete target rows so they match the source.

use egui::RichText;
use navdb_core::driver;
use navdb_core::sync::{self, DataDiff};
use navdb_core::{dml, ColumnMeta, ConnectionConfig, Dialect, ObjPath, ObjectKind, Pending, QueryResult, Scope};

use super::data_transfer::common::*;
use super::Dialog;
use crate::state::AppState;
use crate::ui;
use crate::ui::grid::{self, GridOptions, GridState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Mapping,
    Result,
    Script,
    Deploy,
}

#[derive(Clone)]
struct TableMap {
    name: String,
    key: Vec<String>,
    checked: bool,
    /// Why the table cannot be synchronized (no primary key...).
    problem: Option<String>,
}

struct TableResult {
    name: String,
    target: ObjPath,
    diff: DataDiff,
    error: Option<String>,
    apply: bool,
    insert: bool,
    update: bool,
    delete: bool,
}

struct Mapping {
    tables: Vec<TableMap>,
    only_source: Vec<String>,
    only_target: Vec<String>,
}

type Endpoints = (String, Scope, String, Scope);

pub struct DataSyncDialog {
    id: String,
    page: Page,
    src_conn: Option<String>,
    src_scope: Option<Scope>,
    dst_conn: Option<String>,
    dst_scope: Option<Scope>,
    mapping_job: Option<Pending<Mapping>>,
    mapping: Option<Mapping>,
    mapping_for: Option<Endpoints>,
    mapping_error: Option<String>,
    compare_job: Option<Pending<Vec<TableResult>>>,
    compare_progress: Shared,
    compared_for: Option<Endpoints>,
    results: Vec<TableResult>,
    sel: Option<usize>,
    viewer: usize,
    viewer_cache: Option<(usize, usize, QueryResult)>,
    grid: GridState,
    script: String,
    script_count: usize,
    script_dirty: bool,
    use_tx: bool,
    continue_on_error: bool,
    deploy: Job,
}

const PREVIEW_LIMIT: usize = 400_000;

impl DataSyncDialog {
    pub fn new(st: &mut AppState, source: Option<(String, Scope)>) -> Self {
        let (src_conn, src_scope) = match source {
            Some((c, s)) => (Some(c), Some(s)),
            None => (st.selection.conn.clone(), st.selection.scope.clone()),
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            page: Page::Mapping,
            src_conn,
            src_scope,
            dst_conn: None,
            dst_scope: None,
            mapping_job: None,
            mapping: None,
            mapping_for: None,
            mapping_error: None,
            compare_job: None,
            compare_progress: new_shared(),
            compared_for: None,
            results: Vec::new(),
            sel: None,
            viewer: 0,
            viewer_cache: None,
            grid: GridState::default(),
            script: String::new(),
            script_count: 0,
            script_dirty: true,
            use_tx: true,
            continue_on_error: true,
            deploy: Job::default(),
        }
    }

    fn endpoints(&self) -> Option<Endpoints> {
        Some((self.src_conn.clone()?, self.src_scope.clone()?, self.dst_conn.clone()?, self.dst_scope.clone()?))
    }

    fn cfgs(&self, st: &AppState) -> Option<(ConnectionConfig, ConnectionConfig)> {
        let (sc, _, dc, _) = self.endpoints()?;
        Some((st.config(&sc)?.clone(), st.config(&dc)?.clone()))
    }

    fn validate(&self, st: &AppState) -> std::result::Result<(), String> {
        let Some((sc, ss, dc, ds)) = self.endpoints() else { return Err("Select source and target databases.".into()) };
        for c in [&sc, &dc] {
            if let Some(cfg) = st.config(c) {
                if !cfg.kind.is_sql() {
                    return Err(format!("{} is not supported.", cfg.kind.label()));
                }
            }
        }
        if sc == dc && ss == ds {
            return Err("Source and target are the same database.".into());
        }
        Ok(())
    }

    fn load_mapping(&mut self, st: &mut AppState) {
        let Some(ep) = self.endpoints() else { return };
        let Some((scfg, dcfg)) = self.cfgs(st) else { return };
        self.mapping_for = Some(ep.clone());
        self.mapping = None;
        self.mapping_error = None;
        let (_, ss, _, ds) = ep;
        self.mapping_job = Some(navdb_core::session::spawn_task(st.notify.clone(), move || {
            let mut s = driver::connect(&scfg)?;
            let mut d = driver::connect(&dcfg)?;
            let src: Vec<String> = s.list_objects(&ss, ObjectKind::Table)?.into_iter().map(|o| o.name).collect();
            let dst: Vec<String> = d.list_objects(&ds, ObjectKind::Table)?.into_iter().map(|o| o.name).collect();
            let mut tables = Vec::new();
            for name in src.iter().filter(|n| dst.contains(n)) {
                let (key, problem) = match s.table_def(&ss.path(name.clone())) {
                    Ok(def) => {
                        let pk = def.primary_key();
                        if pk.is_empty() {
                            (pk, Some("no primary key".to_string()))
                        } else {
                            match d.table_def(&ds.path(name.clone())) {
                                Ok(tdef) => {
                                    let missing: Vec<String> = def.columns.iter().filter(|c| tdef.column(&c.name).is_none()).map(|c| c.name.clone()).collect();
                                    if missing.is_empty() {
                                        (pk, None)
                                    } else {
                                        (pk, Some(format!("target lacks column(s) {}", missing.join(", "))))
                                    }
                                }
                                Err(e) => (pk, Some(format!("{:#}", e))),
                            }
                        }
                    }
                    Err(e) => (vec![], Some(format!("{:#}", e))),
                };
                tables.push(TableMap { name: name.clone(), checked: problem.is_none(), key, problem });
            }
            Ok(Mapping {
                tables,
                only_source: src.iter().filter(|n| !dst.contains(n)).cloned().collect(),
                only_target: dst.iter().filter(|n| !src.contains(n)).cloned().collect(),
            })
        }));
    }

    fn start_compare(&mut self, st: &mut AppState) {
        let Some(ep) = self.endpoints() else { return };
        let Some((scfg, dcfg)) = self.cfgs(st) else { return };
        let Some(m) = &self.mapping else { return };
        let tables: Vec<TableMap> = m.tables.iter().filter(|t| t.checked && t.problem.is_none()).cloned().collect();
        self.compare_progress = new_shared();
        self.compared_for = Some(ep.clone());
        let p = self.compare_progress.clone();
        let (_, ss, _, ds) = ep;
        self.compare_job = Some(navdb_core::session::spawn_task(st.notify.clone(), move || {
            let sd = scfg.dialect();
            let td = dcfg.dialect();
            set_current(&p, "Connecting…");
            let mut s = driver::connect(&scfg)?;
            let mut d = driver::connect(&dcfg)?;
            let mut out = Vec::new();
            set_counts(&p, 0, tables.len());
            for (i, t) in tables.iter().enumerate() {
                if cancelled(&p) {
                    anyhow::bail!("cancelled");
                }
                set_current(&p, format!("Comparing {} ({}/{})", t.name, i + 1, tables.len()));
                let sort: Vec<dml::SortSpec> = t.key.iter().map(|k| dml::SortSpec { column: k.clone(), descending: false }).collect();
                let spath = ss.path(t.name.clone());
                let tpath = ds.path(t.name.clone());
                let res = (|| -> navdb_core::Result<DataDiff> {
                    let a = s.query(Some(&ss), &dml::select_page(sd, &spath, "", &sort, 0, 0))?;
                    let b = d.query(Some(&ds), &dml::select_page(td, &tpath, "", &sort, 0, 0))?;
                    Ok(sync::compare_data(&t.name, &t.key, &a, &b))
                })();
                let (diff, error) = match res {
                    Ok(dd) => (dd, None),
                    Err(e) => {
                        plog(&p, LogLevel::Error, format!("{}: {:#}", t.name, e));
                        (DataDiff { table: t.name.clone(), ..Default::default() }, Some(format!("{:#}", e)))
                    }
                };
                let apply = error.is_none() && diff.total_changes() > 0;
                out.push(TableResult { name: t.name.clone(), target: tpath, diff, error, apply, insert: true, update: true, delete: true });
                set_counts(&p, i + 1, tables.len());
            }
            Ok(out)
        }));
    }

    fn dst_dialect(&self, st: &AppState) -> Dialect {
        self.compared_for.as_ref().and_then(|e| st.config(&e.2)).map(|c| c.dialect()).unwrap_or(Dialect::MySql)
    }

    /// (table, statements) for every applied table.
    fn statements(&self, d: Dialect) -> Vec<(String, Vec<String>)> {
        self.results
            .iter()
            .filter(|r| r.apply && r.error.is_none())
            .map(|r| (r.name.clone(), sync::data_script(d, &r.target, &r.diff, r.insert, r.update, r.delete)))
            .filter(|(_, s)| !s.is_empty())
            .collect()
    }

    fn rebuild_script(&mut self, d: Dialect) {
        if !self.script_dirty {
            return;
        }
        let mut out = String::new();
        let mut count = 0;
        for (t, stmts) in self.statements(d) {
            count += stmts.len();
            if out.len() < PREVIEW_LIMIT {
                out.push_str(&format!("-- {}\n", t));
                out.push_str(&navdb_core::ddl::script(&stmts));
                out.push_str("\n\n");
            }
        }
        if out.len() >= PREVIEW_LIMIT {
            out.push_str("-- … preview truncated, use Save to write the complete script\n");
        }
        self.script = out;
        self.script_count = count;
        self.script_dirty = false;
    }

    fn page_mapping(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let mut changed = endpoint(ui, st, "Source", &format!("ds-src-{}", self.id), &mut self.src_conn, &mut self.src_scope);
        ui.add_space(4.0);
        changed |= endpoint(ui, st, "Target", &format!("ds-dst-{}", self.id), &mut self.dst_conn, &mut self.dst_scope);
        let valid = self.validate(st);
        if changed || (valid.is_ok() && self.mapping_job.is_none() && self.mapping_for != self.endpoints()) {
            if valid.is_ok() {
                self.load_mapping(st);
            } else {
                self.mapping = None;
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Table Mapping").strong());
            if ui.add_enabled(valid.is_ok() && self.mapping_job.is_none(), egui::Button::new("⟳ Refresh").small()).clicked() {
                self.load_mapping(st);
            }
            if let Some(m) = &mut self.mapping {
                if ui.button("Select All").clicked() {
                    m.tables.iter_mut().filter(|t| t.problem.is_none()).for_each(|t| t.checked = true);
                }
                if ui.button("Unselect All").clicked() {
                    m.tables.iter_mut().for_each(|t| t.checked = false);
                }
            }
        });
        if let Err(e) = &valid {
            ui.label(RichText::new(e).weak());
        }
        if self.mapping_job.is_some() {
            ui::loading(ui, "Loading tables and keys…");
        }
        if let Some(e) = &self.mapping_error {
            ui::error_text(ui, e);
        }
        if let Some(m) = &mut self.mapping {
            egui::ScrollArea::vertical().id_salt("ds-map").max_height(220.0).auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("ds-map-grid").striped(true).num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                    ui.label(RichText::new("Source / Target table").weak());
                    ui.label(RichText::new("Key columns").weak());
                    ui.label(RichText::new("Status").weak());
                    ui.end_row();
                    for t in m.tables.iter_mut() {
                        let ok = t.problem.is_none();
                        ui.add_enabled(ok, egui::Checkbox::new(&mut t.checked, format!("⊞ {}", t.name)));
                        ui.label(RichText::new(t.key.join(", ")).monospace());
                        match &t.problem {
                            None => ui.label(RichText::new("✔ ready").color(egui::Color32::from_rgb(60, 170, 90))),
                            Some(pb) => ui.label(RichText::new(format!("⚠ {}", pb)).color(egui::Color32::from_rgb(220, 150, 40))),
                        };
                        ui.end_row();
                    }
                });
                if m.tables.is_empty() {
                    ui.label(RichText::new("No tables with the same name exist in both databases.").weak());
                }
            });
            if !m.only_source.is_empty() {
                ui.label(RichText::new(format!("Only in source (not synchronized): {}", truncate_sql(&m.only_source.join(", "), 160))).weak().small());
            }
            if !m.only_target.is_empty() {
                ui.label(RichText::new(format!("Only in target: {}", truncate_sql(&m.only_target.join(", "), 160))).weak().small());
            }
        }
        ui.add_space(6.0);
        let n_checked = self.mapping.as_ref().map(|m| m.tables.iter().filter(|t| t.checked && t.problem.is_none()).count()).unwrap_or(0);
        ui.horizontal(|ui| {
            let busy = self.compare_job.is_some();
            if ui.add_enabled(n_checked > 0 && !busy, egui::Button::new(RichText::new(format!("🔁 Compare {} table(s)", n_checked)).strong())).clicked() {
                self.start_compare(st);
            }
            if busy {
                ui.spinner();
                let (cur, done, total) = self.compare_progress.lock().map(|g| (g.current.clone(), g.done, g.total)).unwrap_or_default();
                ui.add(egui::ProgressBar::new(if total > 0 { done as f32 / total as f32 } else { 0.0 }).desired_width(160.0));
                ui.label(RichText::new(cur).weak());
                if ui.button("Cancel").clicked() {
                    request_cancel(&self.compare_progress);
                }
            }
        });
    }

    fn page_result(&mut self, ui: &mut egui::Ui) {
        if self.results.is_empty() {
            ui.label("Run Compare on the first page.");
            return;
        }
        let mut changed = false;
        egui::ScrollArea::vertical().id_salt("ds-res").max_height(170.0).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("ds-res-grid").striped(true).num_columns(6).spacing([18.0, 4.0]).show(ui, |ui| {
                for h in ["Table", "Insert", "Update", "Delete", "Identical", ""] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (i, r) in self.results.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= ui.add_enabled(r.error.is_none(), egui::Checkbox::without_text(&mut r.apply)).changed();
                        if ui.selectable_label(self.sel == Some(i), &r.name).clicked() {
                            self.sel = Some(i);
                        }
                    });
                    let n = [r.diff.inserts.len(), r.diff.updates.len(), r.diff.deletes.len()];
                    let flags = [&mut r.insert, &mut r.update, &mut r.delete];
                    for (k, f) in flags.into_iter().enumerate() {
                        changed |= ui.add_enabled(r.apply && n[k] > 0, egui::Checkbox::new(f, n[k].to_string())).changed();
                    }
                    ui.label(r.diff.identical.to_string());
                    match &r.error {
                        Some(e) => ui.colored_label(ui.visuals().error_fg_color, truncate_sql(e, 60)),
                        None if r.diff.total_changes() == 0 => ui.label(RichText::new("identical").weak()),
                        None => ui.label(""),
                    };
                    ui.end_row();
                }
            });
        });
        if changed {
            self.script_dirty = true;
        }
        ui.separator();
        let Some(si) = self.sel.filter(|&i| i < self.results.len()) else {
            ui.label(RichText::new("Select a table to view the differing records.").weak());
            return;
        };
        let r = &self.results[si];
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Records of {}", r.name)).strong());
            for (k, label, n) in [(0, "To insert", r.diff.inserts.len()), (1, "To update", r.diff.updates.len()), (2, "To delete", r.diff.deletes.len())] {
                if ui.selectable_label(self.viewer == k, format!("{} ({})", label, n)).clicked() {
                    self.viewer = k;
                }
            }
        });
        let key = (si, self.viewer);
        if self.viewer_cache.as_ref().map(|(a, b, _)| (*a, *b)) != Some(key) {
            let rows = match self.viewer {
                0 => &r.diff.inserts,
                1 => &r.diff.updates,
                _ => &r.diff.deletes,
            };
            let qr = QueryResult {
                columns: r.diff.columns.iter().map(|c| ColumnMeta::new(c.clone())).collect(),
                rows: rows.clone(),
                is_query: true,
                ..Default::default()
            };
            self.viewer_cache = Some((si, self.viewer, qr));
            self.grid.reset();
        }
        if let Some((_, _, qr)) = &self.viewer_cache {
            if self.viewer == 1 && !qr.rows.is_empty() {
                ui.label(RichText::new("Showing the source values that will overwrite the target rows.").weak().small());
            }
            let h = ui.available_height().max(120.0);
            ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
                ui.push_id(("ds-viewer", si, self.viewer), |ui| {
                    let _ = grid::data_grid(ui, &format!("ds-grid-{}", self.id), qr, &mut self.grid, &GridOptions::default());
                });
            });
        }
    }

    fn page_script(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let d = self.dst_dialect(st);
        self.rebuild_script(d);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} statement(s)", self.script_count)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("💾 Save…").clicked() {
                    if let Some(p) = ui::save_file_dialog("data_sync.sql", "sql") {
                        let full: String = self
                            .statements(d)
                            .into_iter()
                            .map(|(t, s)| format!("-- {}\n{}\n", t, navdb_core::ddl::script(&s)))
                            .collect::<Vec<_>>()
                            .join("\n");
                        match std::fs::write(&p, full) {
                            Ok(()) => st.toast(format!("Saved {}", p.display())),
                            Err(e) => st.error(format!("Save failed: {}", e)),
                        }
                    }
                }
                if ui.button("📋 Copy").clicked() {
                    ui.ctx().copy_text(self.script.clone());
                }
            });
        });
        let text = if self.script.is_empty() { "-- nothing to synchronize" } else { self.script.as_str() };
        code_view(ui, "ds-script", text, 380.0);
    }

    fn page_deploy(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let Some((_, _, dc, ds)) = self.compared_for.clone() else {
            ui.label("Run Compare first.");
            return;
        };
        let d = self.dst_dialect(st);
        let stmts = self.statements(d);
        let n: usize = stmts.iter().map(|(_, s)| s.len()).sum();
        ui.label(format!("Target: {} / {}", st.conn_name(&dc), ds.display()));
        ui.label(RichText::new(format!("{} statement(s) in {} table(s) will be executed.", n, stmts.len())).weak());
        ui.checkbox(&mut self.use_tx, "Use a transaction per table");
        ui.checkbox(&mut self.continue_on_error, "Continue on error");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(n > 0 && !self.deploy.running(), egui::Button::new(RichText::new("▶ Deploy").strong())).clicked() {
                if let Some(cfg) = st.config(&dc).cloned() {
                    let (tx, coe) = (self.use_tx, self.continue_on_error);
                    let scope = ds.clone();
                    self.deploy.start(st, move |p| deploy(cfg, scope, stmts, tx, coe, p));
                }
            }
            if self.deploy.running() && ui.button("■ Cancel").clicked() {
                self.deploy.cancel();
            }
            if self.deploy.finished() && !self.deploy.running() && ui.button("🔁 Compare Again").clicked() {
                self.start_compare(st);
                self.page = Page::Mapping;
            }
        });
        ui.add_space(6.0);
        if self.deploy.started.is_some() {
            job_ui(ui, &self.deploy, "statements", 260.0);
        }
    }
}

fn deploy(cfg: ConnectionConfig, scope: Scope, tables: Vec<(String, Vec<String>)>, use_tx: bool, continue_on_error: bool, p: Shared) -> navdb_core::Result<()> {
    let d = cfg.dialect();
    plog(&p, LogLevel::Info, format!("Connecting to {}…", cfg.display_name()));
    let mut drv = driver::connect(&cfg)?;
    let total: usize = tables.iter().map(|(_, s)| s.len()).sum();
    set_counts(&p, 0, total);
    let mut done = 0;
    for (t, stmts) in &tables {
        if cancelled(&p) {
            plog(&p, LogLevel::Warn, "Cancelled by user");
            break;
        }
        set_current(&p, format!("Synchronizing {}…", t));
        let res = (|| -> navdb_core::Result<()> {
            if use_tx {
                exec(drv.as_mut(), &scope, begin_sql(d))?;
            }
            for s in stmts {
                if cancelled(&p) {
                    anyhow::bail!("cancelled");
                }
                if let Err(e) = exec(drv.as_mut(), &scope, s) {
                    if use_tx || !continue_on_error {
                        return Err(e);
                    }
                    plog(&p, LogLevel::Error, format!("{}: {}\n  {:#}", t, truncate_sql(s, 120), e));
                }
                done += 1;
                set_counts(&p, done, total);
            }
            if use_tx {
                exec(drv.as_mut(), &scope, "COMMIT")?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => plog(&p, LogLevel::Ok, format!("{}: {} statement(s) executed", t, stmts.len())),
            Err(e) => {
                if use_tx {
                    let _ = exec(drv.as_mut(), &scope, "ROLLBACK");
                }
                plog(&p, LogLevel::Error, format!("{}: {:#}{}", t, e, if use_tx { " (rolled back)" } else { "" }));
                if !continue_on_error {
                    anyhow::bail!("synchronization stopped after an error");
                }
            }
        }
    }
    set_current(&p, "Done");
    Ok(())
}

impl Dialog for DataSyncDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        if let Some(p) = &mut self.mapping_job {
            match p.poll() {
                Some(Ok(m)) => {
                    self.mapping_job = None;
                    self.mapping = Some(m);
                }
                Some(Err(e)) => {
                    self.mapping_job = None;
                    self.mapping_error = Some(format!("{:#}", e));
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(150)),
            }
        }
        if let Some(p) = &mut self.compare_job {
            match p.poll() {
                Some(Ok(r)) => {
                    self.compare_job = None;
                    self.results = r;
                    self.sel = self.results.iter().position(|r| r.diff.total_changes() > 0).or(Some(0));
                    self.viewer_cache = None;
                    self.script_dirty = true;
                    self.page = Page::Result;
                }
                Some(Err(e)) => {
                    self.compare_job = None;
                    self.mapping_error = Some(format!("Compare failed: {:#}", e));
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(150)),
            }
        }
        if self.deploy.poll(ctx) {
            if let Some((_, _, dc, ds)) = &self.compared_for {
                st.refresh_scope(dc, ds);
            }
            let errs = error_count(&self.deploy.progress);
            if errs == 0 && matches!(self.deploy.result, Some(Ok(()))) {
                st.toast("Data synchronization deployed");
            } else {
                st.error(format!("Data synchronization finished with {} error(s)", errs.max(1)));
            }
        }
        let mut open = true;
        let mut close_clicked = false;
        super::window("Data Synchronization", &self.id).open(&mut open).default_width(860.0).show(ctx, |ui| {
            let has = !self.results.is_empty();
            let busy = self.deploy.running();
            ui.add_enabled_ui(!busy, |ui| {
                step_bar(
                    ui,
                    &[(Page::Mapping, "Source & Mapping"), (Page::Result, "Comparison Result"), (Page::Script, "Deployment Script"), (Page::Deploy, "Deploy")],
                    &mut self.page,
                    |p| p == Page::Mapping || has,
                );
            });
            ui.separator();
            ui.allocate_ui(egui::vec2(ui.available_width(), 440.0), |ui| {
                ui.set_min_height(440.0);
                ui.set_max_height(440.0);
                ui.vertical(|ui| match self.page {
                    Page::Mapping => self.page_mapping(ui, st),
                    Page::Result => self.page_result(ui),
                    Page::Script => self.page_script(ui, st),
                    Page::Deploy => self.page_deploy(ui, st),
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close_clicked = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let next = match self.page {
                        Page::Result => Some(Page::Script),
                        Page::Script => Some(Page::Deploy),
                        _ => None,
                    };
                    if let Some(n) = next {
                        if ui.add_enabled(!busy, egui::Button::new("Next ›")).clicked() {
                            self.page = n;
                        }
                    }
                    let back = match self.page {
                        Page::Result => Some(Page::Mapping),
                        Page::Script => Some(Page::Result),
                        Page::Deploy => Some(Page::Script),
                        Page::Mapping => None,
                    };
                    if let Some(b) = back {
                        if ui.add_enabled(!busy, egui::Button::new("‹ Back")).clicked() {
                            self.page = b;
                        }
                    }
                });
            });
        });
        if close_clicked {
            open = false;
        }
        if !open {
            self.deploy.cancel();
            request_cancel(&self.compare_progress);
        }
        open
    }
}
