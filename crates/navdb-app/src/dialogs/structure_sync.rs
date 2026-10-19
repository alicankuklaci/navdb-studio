//! Structure Synchronization: compare table definitions of two databases and
//! deploy the statements that make the target match the source.

use std::collections::HashMap;

use egui::RichText;
use navdb_core::driver;
use navdb_core::sync::{self, DiffStatus, StructDiff};
use navdb_core::{ddl, Dialect, Pending, Scope, TableDef};

use super::data_transfer::common::*;
use super::Dialog;
use crate::state::AppState;
use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Options,
    Result,
    Script,
    Deploy,
}

struct Compared {
    src_dialect: Dialect,
    dst_dialect: Dialect,
    dst_conn: String,
    dst_scope: Scope,
    /// Original (unconverted) source definitions for the DDL view.
    src_defs: HashMap<String, TableDef>,
}

type DefPair = (Vec<TableDef>, Vec<TableDef>);

pub struct StructureSyncDialog {
    id: String,
    page: Page,
    src_conn: Option<String>,
    src_scope: Option<Scope>,
    dst_conn: Option<String>,
    dst_scope: Option<Scope>,
    drop_extra: bool,
    continue_on_error: bool,
    show_identical: bool,
    compare: Option<Pending<DefPair>>,
    compare_progress: Shared,
    compare_error: Option<String>,
    diffs: Vec<StructDiff>,
    compared: Option<Compared>,
    selected: Option<usize>,
    script: String,
    script_dirty: bool,
    deploy: Job,
}

impl StructureSyncDialog {
    pub fn new(st: &mut AppState, source: Option<(String, Scope)>) -> Self {
        let (src_conn, src_scope) = match source {
            Some((c, s)) => (Some(c), Some(s)),
            None => (st.selection.conn.clone(), st.selection.scope.clone()),
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            page: Page::Options,
            src_conn,
            src_scope,
            dst_conn: None,
            dst_scope: None,
            drop_extra: false,
            continue_on_error: true,
            show_identical: true,
            compare: None,
            compare_progress: new_shared(),
            compare_error: None,
            diffs: Vec::new(),
            compared: None,
            selected: None,
            script: String::new(),
            script_dirty: true,
            deploy: Job::default(),
        }
    }

    fn can_compare(&self, st: &AppState) -> std::result::Result<(), String> {
        let (Some(sc), Some(_)) = (&self.src_conn, &self.src_scope) else { return Err("Select a source database.".into()) };
        let (Some(dc), Some(_)) = (&self.dst_conn, &self.dst_scope) else { return Err("Select a target database.".into()) };
        for c in [sc, dc] {
            if let Some(cfg) = st.config(c) {
                if !cfg.kind.is_sql() {
                    return Err(format!("{} is not supported.", cfg.kind.label()));
                }
            }
        }
        if same_endpoint(&(self.src_conn.clone(), self.src_scope.clone()), &(self.dst_conn.clone(), self.dst_scope.clone())) {
            return Err("Source and target are the same database.".into());
        }
        Ok(())
    }

    fn start_compare(&mut self, st: &mut AppState) {
        let (Some(sc), Some(ss), Some(dc), Some(ds)) = (self.src_conn.clone(), self.src_scope.clone(), self.dst_conn.clone(), self.dst_scope.clone()) else {
            return;
        };
        let (Some(scfg), Some(dcfg)) = (st.config(&sc).cloned(), st.config(&dc).cloned()) else { return };
        self.compare_progress = new_shared();
        self.compare_error = None;
        let p = self.compare_progress.clone();
        self.compare = Some(navdb_core::session::spawn_task(st.notify.clone(), move || {
            set_current(&p, "Connecting to source…");
            let mut s = driver::connect(&scfg)?;
            let src = load_defs(s.as_mut(), &ss, &p, "source")?;
            drop(s);
            set_current(&p, "Connecting to target…");
            let mut d = driver::connect(&dcfg)?;
            let dst = load_defs(d.as_mut(), &ds, &p, "target")?;
            Ok((src, dst))
        }));
    }

    fn finish_compare(&mut self, st: &AppState, src: Vec<TableDef>, dst: Vec<TableDef>) {
        let (Some(sc), Some(dc), Some(ds)) = (self.src_conn.clone(), self.dst_conn.clone(), self.dst_scope.clone()) else { return };
        let sd = st.config(&sc).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let td = st.config(&dc).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let converted: Vec<TableDef> = src.iter().map(|t| convert_table(t, sd, td, &ds).0).collect();
        self.diffs = sync::compare_structures(td, &converted, &dst, &ds, self.drop_extra);
        self.compared = Some(Compared {
            src_dialect: sd,
            dst_dialect: td,
            dst_conn: dc,
            dst_scope: ds,
            src_defs: src.into_iter().map(|t| (t.path.name.clone(), t)).collect(),
        });
        self.selected = self.diffs.iter().position(|d| d.status != DiffStatus::Identical).or(if self.diffs.is_empty() { None } else { Some(0) });
        self.script_dirty = true;
        self.page = Page::Result;
    }

    fn script_text(&mut self) -> &str {
        if self.script_dirty {
            self.script = ddl::script(&sync::structure_script(&self.diffs));
            self.script_dirty = false;
        }
        &self.script
    }

    fn page_options(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        if endpoint(ui, st, "Source", &format!("ss-src-{}", self.id), &mut self.src_conn, &mut self.src_scope) {
            self.compared = None;
        }
        ui.add_space(4.0);
        if endpoint(ui, st, "Target", &format!("ss-dst-{}", self.id), &mut self.dst_conn, &mut self.dst_scope) {
            self.compared = None;
        }
        ui.add_space(6.0);
        let sd = self.src_conn.as_ref().and_then(|c| st.config(c)).map(|c| c.dialect());
        let td = self.dst_conn.as_ref().and_then(|c| st.config(c)).map(|c| c.dialect());
        if let (Some(a), Some(b)) = (sd, td) {
            dialect_badge(ui, a, b);
        }
        ui.add_space(6.0);
        ui.label(RichText::new("Options").strong());
        if ui.checkbox(&mut self.drop_extra, "Drop objects that exist only in the target").changed() {
            for d in self.diffs.iter_mut().filter(|d| d.status == DiffStatus::OnlyInTarget) {
                d.selected = self.drop_extra;
            }
            self.script_dirty = true;
        }
        ui.checkbox(&mut self.continue_on_error, "Continue on error during deployment");
        ui.checkbox(&mut self.show_identical, "Show identical objects in the result");
        ui.add_space(10.0);
        let check = self.can_compare(st);
        ui.horizontal(|ui| {
            let busy = self.compare.is_some();
            if ui.add_enabled(check.is_ok() && !busy, egui::Button::new(RichText::new("🔁 Compare").strong())).clicked() {
                self.start_compare(st);
            }
            if busy {
                ui.spinner();
                let cur = self.compare_progress.lock().map(|g| g.current.clone()).unwrap_or_default();
                ui.label(RichText::new(cur).weak());
                if ui.button("Cancel").clicked() {
                    request_cancel(&self.compare_progress);
                }
            }
        });
        if let Err(e) = check {
            ui.label(RichText::new(e).weak());
        }
        if let Some(e) = &self.compare_error {
            ui::error_text(ui, e);
        }
        let errs: Vec<String> = self
            .compare_progress
            .lock()
            .map(|g| g.log.iter().filter(|(l, _)| *l == LogLevel::Error).map(|(_, m)| m.clone()).collect())
            .unwrap_or_default();
        for e in errs {
            ui::error_text(ui, &e);
        }
    }

    fn page_result(&mut self, ui: &mut egui::Ui) {
        let Some(cmp) = &self.compared else {
            ui.label("Run Compare on the first page to see the differences.");
            return;
        };
        let counts = |s: DiffStatus| self.diffs.iter().filter(|d| d.status == s).count();
        ui.horizontal(|ui| {
            for s in [DiffStatus::OnlyInSource, DiffStatus::Different, DiffStatus::OnlyInTarget, DiffStatus::Identical] {
                ui.label(RichText::new(format!("{} {}: {}", status_icon(s), short_label(s), counts(s))).color(status_color(s)));
                ui.add_space(8.0);
            }
        });
        ui.separator();
        let h = 380.0;
        let mut changed = false;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(250.0);
                ui.set_height(h);
                egui::ScrollArea::vertical().id_salt("ss-tree").auto_shrink([false, false]).show(ui, |ui| {
                    for s in [DiffStatus::OnlyInSource, DiffStatus::Different, DiffStatus::OnlyInTarget, DiffStatus::Identical] {
                        if s == DiffStatus::Identical && !self.show_identical {
                            continue;
                        }
                        let idx: Vec<usize> = (0..self.diffs.len()).filter(|&i| self.diffs[i].status == s).collect();
                        if idx.is_empty() {
                            continue;
                        }
                        ui.horizontal(|ui| {
                            if s != DiffStatus::Identical {
                                let n_sel = idx.iter().filter(|&&i| self.diffs[i].selected).count();
                                let mut all = n_sel == idx.len();
                                let cb = egui::Checkbox::without_text(&mut all).indeterminate(n_sel > 0 && n_sel < idx.len());
                                if ui.add(cb).changed() {
                                    for &i in &idx {
                                        self.diffs[i].selected = all;
                                    }
                                    changed = true;
                                }
                            }
                            ui.label(RichText::new(format!("{} ({})", s.label(), idx.len())).strong().color(status_color(s)));
                        });
                        ui.indent(("ss-grp", s as u8), |ui| {
                            for &i in &idx {
                                ui.horizontal(|ui| {
                                    if s != DiffStatus::Identical && ui.checkbox(&mut self.diffs[i].selected, "").changed() {
                                        changed = true;
                                    }
                                    let label = format!("{} {}", status_icon(s), self.diffs[i].name);
                                    if ui.selectable_label(self.selected == Some(i), label).clicked() {
                                        self.selected = Some(i);
                                    }
                                });
                            }
                        });
                        ui.add_space(4.0);
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_height(h);
                let Some(d) = self.selected.and_then(|i| self.diffs.get(i)) else {
                    ui.label("Select an object to view its definition.");
                    return;
                };
                let src_ddl = cmp
                    .src_defs
                    .get(&d.name)
                    .map(|t| ddl::script(&ddl::create_table(cmp.src_dialect, t)))
                    .unwrap_or_else(|| "-- does not exist in source".into());
                let dst_ddl = d.target.as_ref().map(|t| ddl::script(&ddl::create_table(cmp.dst_dialect, t))).unwrap_or_else(|| "-- does not exist in target".into());
                ui.label(RichText::new(format!("{}  —  {}", d.name, d.status.label())).strong());
                ui.columns(2, |cols| {
                    cols[0].label(RichText::new("Source DDL").weak());
                    code_view(&mut cols[0], "ss-src-ddl", &src_ddl, 190.0);
                    cols[1].label(RichText::new(format!("Target DDL ({})", cmp.dst_scope.display())).weak());
                    code_view(&mut cols[1], "ss-dst-ddl", &dst_ddl, 190.0);
                });
                ui.add_space(4.0);
                ui.label(RichText::new("Statements for this object").weak());
                let stm = if d.statements.is_empty() { "-- no changes".to_string() } else { ddl::script(&d.statements) };
                code_view(ui, "ss-obj-stmts", &stm, 110.0);
            });
        });
        if changed {
            self.script_dirty = true;
        }
    }

    fn page_script(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        if self.compared.is_none() {
            ui.label("Run Compare first.");
            return;
        }
        let n = sync::structure_script(&self.diffs).len();
        let id = self.id.clone();
        let text = self.script_text().to_string();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} statement(s) for the selected objects", n)).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("💾 Save…").clicked() {
                    if let Some(p) = ui::save_file_dialog("structure_sync.sql", "sql") {
                        match std::fs::write(&p, &text) {
                            Ok(()) => st.toast(format!("Saved {}", p.display())),
                            Err(e) => st.error(format!("Save failed: {}", e)),
                        }
                    }
                }
                if ui.button("📋 Copy").clicked() {
                    ui.ctx().copy_text(text.clone());
                }
            });
        });
        code_view(ui, &format!("ss-script-{}", id), if text.is_empty() { "-- nothing to deploy" } else { &text }, 370.0);
    }

    fn page_deploy(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let Some((dst_conn, dst_scope)) = self.compared.as_ref().map(|c| (c.dst_conn.clone(), c.dst_scope.clone())) else {
            ui.label("Run Compare first.");
            return;
        };
        let stmts = sync::structure_script(&self.diffs);
        ui.label(format!("Target: {} / {}", st.conn_name(&dst_conn), dst_scope.display()));
        ui.label(RichText::new(format!("{} statement(s) will be executed on the target.", stmts.len())).weak());
        ui.checkbox(&mut self.continue_on_error, "Continue on error");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can = !stmts.is_empty() && !self.deploy.running();
            if ui.add_enabled(can, egui::Button::new(RichText::new("▶ Deploy").strong())).clicked() {
                if let Some(cfg) = st.config(&dst_conn).cloned() {
                    let scope = dst_scope.clone();
                    let coe = self.continue_on_error;
                    self.deploy.start(st, move |p| deploy(cfg, scope, stmts, coe, p));
                }
            }
            if self.deploy.running() && ui.button("■ Cancel").clicked() {
                self.deploy.cancel();
            }
            if self.deploy.finished() && ui.button("🔁 Compare Again").clicked() {
                self.start_compare(st);
                self.page = Page::Options;
            }
        });
        ui.add_space(6.0);
        if self.deploy.started.is_some() {
            job_ui(ui, &self.deploy, "statements", 260.0);
        }
    }
}

fn deploy(cfg: navdb_core::ConnectionConfig, scope: Scope, stmts: Vec<String>, continue_on_error: bool, p: Shared) -> navdb_core::Result<()> {
    plog(&p, LogLevel::Info, format!("Connecting to {}…", cfg.display_name()));
    let mut d = driver::connect(&cfg)?;
    let total = stmts.len();
    set_counts(&p, 0, total);
    for (i, s) in stmts.iter().enumerate() {
        if cancelled(&p) {
            plog(&p, LogLevel::Warn, "Cancelled by user");
            break;
        }
        set_current(&p, format!("Executing {}/{}", i + 1, total));
        match exec(d.as_mut(), &scope, s) {
            Ok(()) => plog(&p, LogLevel::Ok, truncate_sql(s, 140)),
            Err(e) => {
                plog(&p, LogLevel::Error, format!("{}\n  {:#}", truncate_sql(s, 140), e));
                if !continue_on_error {
                    anyhow::bail!("deployment stopped after an error");
                }
            }
        }
        set_counts(&p, i + 1, total);
    }
    set_current(&p, "Done");
    Ok(())
}

fn short_label(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::OnlyInSource => "Create",
        DiffStatus::Different => "Modify",
        DiffStatus::OnlyInTarget => "Drop",
        DiffStatus::Identical => "Identical",
    }
}

fn status_icon(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::OnlyInSource => "➕",
        DiffStatus::Different => "✏",
        DiffStatus::OnlyInTarget => "-",
        DiffStatus::Identical => "=",
    }
}

fn status_color(s: DiffStatus) -> egui::Color32 {
    match s {
        DiffStatus::OnlyInSource => egui::Color32::from_rgb(60, 170, 90),
        DiffStatus::Different => egui::Color32::from_rgb(220, 150, 40),
        DiffStatus::OnlyInTarget => egui::Color32::from_rgb(210, 80, 80),
        DiffStatus::Identical => egui::Color32::GRAY,
    }
}

impl Dialog for StructureSyncDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        if let Some(p) = &mut self.compare {
            match p.poll() {
                Some(Ok((src, dst))) => {
                    self.compare = None;
                    self.finish_compare(st, src, dst);
                }
                Some(Err(e)) => {
                    self.compare = None;
                    self.compare_error = Some(format!("{:#}", e));
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(150)),
            }
        }
        if self.deploy.poll(ctx) {
            if let Some(c) = &self.compared {
                st.refresh_scope(&c.dst_conn, &c.dst_scope);
            }
            let errs = error_count(&self.deploy.progress);
            if errs == 0 && matches!(self.deploy.result, Some(Ok(()))) {
                st.toast("Structure synchronization deployed");
            } else {
                st.error(format!("Deployment finished with {} error(s)", errs.max(1)));
            }
        }
        let mut open = true;
        let mut close_clicked = false;
        super::window("Structure Synchronization", &self.id).open(&mut open).default_width(900.0).show(ctx, |ui| {
            let has = self.compared.is_some();
            let busy = self.deploy.running();
            ui.add_enabled_ui(!busy, |ui| {
                step_bar(
                    ui,
                    &[(Page::Options, "Options"), (Page::Result, "Comparison Result"), (Page::Script, "Deployment Script"), (Page::Deploy, "Deploy")],
                    &mut self.page,
                    |p| p == Page::Options || has,
                );
            });
            ui.separator();
            ui.allocate_ui(egui::vec2(ui.available_width(), 440.0), |ui| {
                ui.set_min_height(440.0);
                ui.vertical(|ui| match self.page {
                    Page::Options => self.page_options(ui, st),
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
                        Page::Options => None,
                        Page::Result => Some(Page::Script),
                        Page::Script => Some(Page::Deploy),
                        Page::Deploy => None,
                    };
                    if let Some(n) = next {
                        if ui.add_enabled(has && !busy, egui::Button::new("Next ›")).clicked() {
                            self.page = n;
                        }
                    }
                    let back = match self.page {
                        Page::Options => None,
                        Page::Result => Some(Page::Options),
                        Page::Script => Some(Page::Result),
                        Page::Deploy => Some(Page::Script),
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
