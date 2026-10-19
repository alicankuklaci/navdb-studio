//! Data Generation: fill tables with realistic random test data.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use egui::RichText;
use navdb_core::datagen::{self, Generator};
use navdb_core::driver::{self, Driver};
use navdb_core::{ddl, dml, ColumnDef, ColumnMeta, ConnectionConfig, Dialect, ObjPath, Pending, QueryResult, Scope, TableDef, Value};

use super::data_transfer::common::*;
use super::Dialog;
use crate::state::{AppState, Loadable};
use crate::ui;
use crate::ui::grid::{self, GridOptions, GridState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Columns,
    Preview,
    Sql,
    Run,
}

struct ColGen {
    col: ColumnDef,
    gen: Generator,
    list_text: String,
    /// Referenced table/column when the column is a (single-column) foreign key.
    fk: Option<(ObjPath, String)>,
    fk_job: Option<Pending<Vec<String>>>,
    fk_error: Option<String>,
}

struct TableGen {
    def: Loadable<TableDef>,
    cols: Vec<ColGen>,
    rows: usize,
    inited: bool,
}

#[derive(Clone)]
struct GenPlan {
    path: ObjPath,
    rows: usize,
    cols: Vec<(String, Generator)>,
    /// Lower-case target types of the generated (non-skipped) columns.
    types: Vec<String>,
}

pub struct DataGenDialog {
    id: String,
    conn: String,
    scope: Scope,
    dialect: Dialect,
    checked: BTreeSet<String>,
    tables: BTreeMap<String, TableGen>,
    current: Option<String>,
    filter: String,
    default_rows: usize,
    delete_existing: bool,
    batch: usize,
    continue_on_error: bool,
    view: View,
    preview: Option<QueryResult>,
    preview_grid: GridState,
    sql_preview: String,
    job: Job,
}

impl DataGenDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, tables: Vec<String>) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let _ = st.session(conn);
        let current = tables.first().cloned();
        let mut d = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            scope,
            dialect,
            checked: tables.into_iter().collect(),
            tables: BTreeMap::new(),
            current,
            filter: String::new(),
            default_rows: 100,
            delete_existing: false,
            batch: 500,
            continue_on_error: false,
            view: View::Columns,
            preview: None,
            preview_grid: GridState::default(),
            sql_preview: String::new(),
            job: Job::default(),
        };
        let names: Vec<String> = d.checked.iter().cloned().collect();
        for n in names {
            d.ensure_table(st, &n);
        }
        d
    }

    fn ensure_table(&mut self, st: &mut AppState, name: &str) {
        if self.tables.contains_key(name) {
            return;
        }
        let Some(s) = st.session(&self.conn) else { return };
        let path = self.scope.path(name);
        let def = Loadable::Loading(s.run(move |d| d.table_def(&path)));
        self.tables.insert(name.to_string(), TableGen { def, cols: vec![], rows: self.default_rows, inited: false });
    }

    fn poll(&mut self) {
        let scope = self.scope.clone();
        for t in self.tables.values_mut() {
            t.def.poll();
            if !t.inited {
                if let Some(def) = t.def.ready() {
                    t.cols = def.columns.iter().map(|c| new_colgen(c, def, &scope)).collect();
                    t.inited = true;
                }
            }
            for c in t.cols.iter_mut() {
                if let Some(p) = &mut c.fk_job {
                    if let Some(r) = p.poll() {
                        c.fk_job = None;
                        match r {
                            Ok(v) if v.is_empty() => c.fk_error = Some("referenced table is empty".into()),
                            Ok(v) => {
                                c.list_text = v.join("\n");
                                c.gen = Generator::List(v);
                                c.fk_error = None;
                            }
                            Err(e) => c.fk_error = Some(format!("{:#}", e)),
                        }
                    }
                }
            }
        }
    }

    fn any_loading(&self) -> bool {
        self.tables.values().any(|t| t.def.is_loading() || t.cols.iter().any(|c| c.fk_job.is_some()))
    }

    fn load_fk(&mut self, st: &mut AppState, table: &str, col: usize) {
        let Some(session) = st.session(&self.conn) else { return };
        let d = self.dialect;
        let scope = self.scope.clone();
        let Some(c) = self.tables.get_mut(table).and_then(|t| t.cols.get_mut(col)) else { return };
        let Some((path, rc)) = c.fk.clone() else { return };
        c.fk_error = None;
        c.fk_job = Some(session.run(move |drv| {
            let q = d.quote_ident(&rc);
            let sql = format!("SELECT DISTINCT {} FROM {} WHERE {} IS NOT NULL{}", q, d.qualify(&path), q, d.limit_clause(1000, 0));
            let r = drv.query(Some(&scope), &sql)?;
            Ok(r.rows.iter().filter_map(|r| r.first()).map(|v| v.edit_text()).collect())
        }));
    }

    /// Generation order: referenced (parent) tables first.
    fn ordered_tables(&self) -> Vec<String> {
        let names: Vec<String> = self.checked.iter().cloned().collect();
        let deps = |n: &str| -> Vec<String> {
            self.tables
                .get(n)
                .and_then(|t| t.def.ready())
                .map(|d| d.foreign_keys.iter().map(|f| f.ref_table.clone()).filter(|r| r != n && names.contains(r)).collect())
                .unwrap_or_default()
        };
        let mut out: Vec<String> = Vec::new();
        let mut visiting = HashSet::new();
        fn visit(n: &str, deps: &dyn Fn(&str) -> Vec<String>, out: &mut Vec<String>, visiting: &mut HashSet<String>) {
            if out.iter().any(|x| x == n) || !visiting.insert(n.to_string()) {
                return;
            }
            for d in deps(n) {
                visit(&d, deps, out, visiting);
            }
            out.push(n.to_string());
        }
        for n in &names {
            visit(n, &deps, &mut out, &mut visiting);
        }
        out
    }

    fn plans(&self) -> std::result::Result<Vec<GenPlan>, String> {
        let mut out = Vec::new();
        for name in self.ordered_tables() {
            let Some(t) = self.tables.get(&name) else { return Err(format!("{}: not loaded", name)) };
            if !t.inited {
                return Err(format!("{}: definition is still loading", name));
            }
            out.push(plan_of(&self.scope, &name, t));
        }
        if out.is_empty() {
            return Err("Select at least one table.".into());
        }
        if out.iter().all(|p| p.cols.iter().all(|(_, g)| *g == Generator::Skip)) {
            return Err("All columns are skipped.".into());
        }
        Ok(out)
    }

    fn build_sql(&self, plans: &[GenPlan], max_rows: Option<usize>) -> String {
        let d = self.dialect;
        let mut out = String::new();
        if self.delete_existing {
            for p in plans.iter().rev() {
                out.push_str(&format!("{};\n", ddl::empty_table(d, &p.path)));
            }
            out.push('\n');
        }
        for p in plans {
            let n = max_rows.map(|m| m.min(p.rows)).unwrap_or(p.rows);
            out.push_str(&format!("-- {}: {} row(s)\n", p.path.name, p.rows));
            out.push_str(&format!("{};\n", begin_sql(d)));
            let mut start = 0;
            while start < n {
                let k = (n - start).min(self.batch.max(1));
                let (names, rows) = gen_rows(p, k, start);
                if names.is_empty() {
                    break;
                }
                out.push_str(&format!("{};\n", dml::insert_rows(d, &p.path, &names, &rows)));
                start += k;
            }
            if n < p.rows {
                out.push_str(&format!("-- … {} more row(s) are generated when running\n", p.rows - n));
            }
            out.push_str("COMMIT;\n\n");
        }
        out
    }

    fn table_list(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let conn = Some(self.conn.clone());
        let scope = Some(self.scope.clone());
        let tables = match scope_tables(st, &conn, &scope) {
            Tables::Ready(v) => v,
            Tables::Loading => {
                ui::loading(ui, "Loading tables…");
                return;
            }
            Tables::Failed(e) => {
                ui::error_text(ui, &e);
                return;
            }
            Tables::NoScope => return,
        };
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("🔍 Filter").desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.small_button("All").clicked() {
                for t in &tables {
                    self.checked.insert(t.name.clone());
                }
            }
            if ui.small_button("None").clicked() {
                self.checked.clear();
            }
            ui.label(RichText::new(format!("{} selected", self.checked.len())).weak().small());
        });
        let f = self.filter.to_lowercase();
        let mut to_load = Vec::new();
        egui::ScrollArea::vertical().id_salt("dg-tables").auto_shrink([false, false]).show(ui, |ui| {
            for t in tables.iter().filter(|t| f.is_empty() || t.name.to_lowercase().contains(&f)) {
                ui.horizontal(|ui| {
                    let mut on = self.checked.contains(&t.name);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            self.checked.insert(t.name.clone());
                            to_load.push(t.name.clone());
                            self.current = Some(t.name.clone());
                        } else {
                            self.checked.remove(&t.name);
                        }
                    }
                    let rows = self.tables.get(&t.name).filter(|_| on).map(|g| format!("  {}", g.rows)).unwrap_or_default();
                    let label = RichText::new(format!("⊞ {}{}", t.name, rows));
                    if ui.selectable_label(self.current.as_deref() == Some(&t.name), label).clicked() {
                        self.current = Some(t.name.clone());
                        to_load.push(t.name.clone());
                    }
                });
            }
        });
        for n in self.checked.clone() {
            to_load.push(n);
        }
        for n in to_load {
            self.ensure_table(st, &n);
        }
    }

    fn columns_view(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let Some(name) = self.current.clone() else {
            ui.label("Select a table on the left.");
            return;
        };
        let checked = self.checked.contains(&name);
        let mut fk_requests = Vec::new();
        let id = self.id.clone();
        let Some(t) = self.tables.get_mut(&name) else { return };
        match &t.def {
            Loadable::Loading(_) | Loadable::NotLoaded => {
                ui::loading(ui, "Loading table definition…");
                return;
            }
            Loadable::Failed(e) => {
                ui::error_text(ui, e);
                return;
            }
            Loadable::Ready(_) => {}
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("⊞ {}", name)).strong());
            if !checked {
                ui.label(RichText::new("(not selected for generation)").weak());
            }
            ui.add_space(12.0);
            ui.label("Rows to generate:");
            ui.add(egui::DragValue::new(&mut t.rows).range(1..=10_000_000).speed(5.0));
            let has_fk = t.cols.iter().any(|c| c.fk.is_some());
            if has_fk && ui.button("Load all FK values").clicked() {
                for (i, c) in t.cols.iter().enumerate() {
                    if c.fk.is_some() {
                        fk_requests.push(i);
                    }
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt(("dg-cols", &name)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("dg-grid", &id, &name)).striped(true).num_columns(4).spacing([12.0, 6.0]).min_col_width(60.0).show(ui, |ui| {
                for h in ["Column", "Type", "Generator", "Parameters"] {
                    ui.label(RichText::new(h).weak());
                }
                ui.end_row();
                for (i, c) in t.cols.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let mut badges = String::new();
                        if c.col.primary_key {
                            badges.push_str("🔑");
                        }
                        if c.fk.is_some() {
                            badges.push_str("🔗");
                        }
                        ui.label(RichText::new(format!("{} {}", c.col.name, badges)).strong());
                    })
                    .response
                    .on_hover_text(c.fk.as_ref().map(|(p, col)| format!("References {}.{}", p.name, col)).unwrap_or_default());
                    let mut ty = c.col.data_type.clone();
                    if c.col.auto_increment {
                        ty.push_str(" (auto)");
                    }
                    if !c.col.nullable {
                        ty.push_str(" not null");
                    }
                    ui.label(RichText::new(ty).weak().monospace());
                    egui::ComboBox::from_id_salt(("dg-gen", &id, &name, i)).selected_text(c.gen.name()).width(130.0).show_ui(ui, |ui| {
                        for n in Generator::NAMES {
                            if ui.selectable_label(c.gen.name() == n, n).clicked() && c.gen.name() != n {
                                c.gen = Generator::from_name(n);
                                if let Generator::List(items) = &c.gen {
                                    c.list_text = items.join("\n");
                                }
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        params_ui(ui, c);
                        if c.fk.is_some() {
                            if c.fk_job.is_some() {
                                ui.spinner();
                            } else if ui.small_button("Load FK values").on_hover_text("Use up to 1000 distinct values of the referenced column").clicked() {
                                fk_requests.push(i);
                            }
                            if let Some(e) = &c.fk_error {
                                ui.colored_label(ui.visuals().error_fg_color, "⚠").on_hover_text(e);
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        });
        for i in fk_requests {
            self.load_fk(st, &name, i);
        }
    }

    fn preview_view(&mut self, ui: &mut egui::Ui) {
        let Some(name) = self.current.clone() else {
            ui.label("Select a table on the left.");
            return;
        };
        let Some(t) = self.tables.get(&name).filter(|t| t.inited) else {
            ui::loading(ui, "Loading table definition…");
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Preview of {} (10 rows)", name)).strong());
            if ui.button("⟳ Regenerate").clicked() || self.preview.as_ref().map(|p| p.sql != name).unwrap_or(true) {
                let plan = plan_of(&self.scope, &name, t);
                let (names, rows) = gen_rows(&plan, 10, 0);
                self.preview = Some(QueryResult {
                    sql: name.clone(),
                    columns: names.into_iter().map(ColumnMeta::new).collect(),
                    rows,
                    is_query: true,
                    ..Default::default()
                });
                self.preview_grid.reset();
            }
        });
        if let Some(p) = &self.preview {
            let h = ui.available_height().max(120.0);
            ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
                let _ = grid::data_grid(ui, &format!("dg-prev-{}", self.id), p, &mut self.preview_grid, &GridOptions::default());
            });
        }
    }

    fn sql_view(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let plans = self.plans();
        ui.horizontal(|ui| {
            if ui.add_enabled(plans.is_ok(), egui::Button::new("⟳ Generate SQL Preview")).clicked() || (self.sql_preview.is_empty() && plans.is_ok()) {
                if let Ok(p) = &plans {
                    self.sql_preview = self.build_sql(p, Some(50));
                }
            }
            if ui.add_enabled(!self.sql_preview.is_empty(), egui::Button::new("📋 Copy")).clicked() {
                ui.ctx().copy_text(self.sql_preview.clone());
            }
            if ui.add_enabled(plans.is_ok(), egui::Button::new("💾 Save Full Script…")).clicked() {
                if let (Ok(p), Some(path)) = (&plans, ui::save_file_dialog("generated_data.sql", "sql")) {
                    let sql = self.build_sql(p, None);
                    match std::fs::write(&path, sql) {
                        Ok(()) => st.toast(format!("Saved {}", path.display())),
                        Err(e) => st.error(format!("Save failed: {}", e)),
                    }
                }
            }
            ui.label(RichText::new("Preview shows up to 50 rows per table").weak().small());
        });
        if let Err(e) = &plans {
            ui.label(RichText::new(e).weak());
        }
        code_view(ui, "dg-sql", &self.sql_preview, ui.available_height().max(120.0) - 8.0);
    }

    fn run_view(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let plans = self.plans();
        let total: usize = plans.as_ref().map(|p| p.iter().map(|x| x.rows).sum()).unwrap_or(0);
        ui.label(RichText::new(format!("Target: {} / {}", st.conn_name(&self.conn), self.scope.display())).strong());
        if let Ok(p) = &plans {
            ui.label(RichText::new(format!("{} row(s) in {} table(s): {}", total, p.len(), truncate_sql(&p.iter().map(|x| format!("{} ({})", x.path.name, x.rows)).collect::<Vec<_>>().join(", "), 200))).weak());
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.delete_existing, "Delete existing records first");
            ui.checkbox(&mut self.continue_on_error, "Continue on error");
            ui.label("Batch size:");
            ui.add(egui::DragValue::new(&mut self.batch).range(1..=10_000).speed(10.0));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can = plans.is_ok() && !self.job.running();
            if ui.add_enabled(can, egui::Button::new(RichText::new("▶ Start").strong())).clicked() {
                if let (Ok(p), Some(cfg)) = (plans.clone(), st.config(&self.conn).cloned()) {
                    let opts = (self.delete_existing, self.batch.max(1), self.continue_on_error);
                    let scope = self.scope.clone();
                    self.job.start(st, move |prog| run_generation(cfg, scope, p, opts, prog));
                }
            }
            if self.job.running() && ui.button("■ Cancel").clicked() {
                self.job.cancel();
            }
            if let Err(e) = &plans {
                ui.label(RichText::new(e).weak());
            }
        });
        ui.add_space(6.0);
        if self.job.started.is_some() {
            job_ui(ui, &self.job, "rows", 220.0);
        }
    }
}

fn new_colgen(c: &ColumnDef, def: &TableDef, scope: &Scope) -> ColGen {
    let fk = def
        .foreign_keys
        .iter()
        .find_map(|f| f.columns.iter().position(|x| *x == c.name).and_then(|i| f.ref_columns.get(i).map(|rc| (fk_target(scope, f), rc.clone()))));
    let pk = def.primary_key();
    let mut gen = Generator::guess(c);
    let t = c.data_type.to_ascii_lowercase();
    if !c.auto_increment && pk.len() == 1 && pk[0] == c.name && (t.contains("int") || t.contains("serial")) {
        gen = Generator::Sequence { start: 1, step: 1 };
    }
    let list_text = if let Generator::List(items) = &gen { items.join("\n") } else { String::new() };
    ColGen { col: c.clone(), gen, list_text, fk, fk_job: None, fk_error: None }
}

fn params_ui(ui: &mut egui::Ui, c: &mut ColGen) {
    match &mut c.gen {
        Generator::Sequence { start, step } => {
            ui.label("start");
            ui.add(egui::DragValue::new(start));
            ui.label("step");
            ui.add(egui::DragValue::new(step));
        }
        Generator::IntRange { min, max } => {
            ui.label("min");
            ui.add(egui::DragValue::new(min));
            ui.label("max");
            ui.add(egui::DragValue::new(max));
        }
        Generator::FloatRange { min, max, decimals } => {
            ui.label("min");
            ui.add(egui::DragValue::new(min).speed(1.0));
            ui.label("max");
            ui.add(egui::DragValue::new(max).speed(1.0));
            ui.label("decimals");
            ui.add(egui::DragValue::new(decimals).range(0..=10));
        }
        Generator::Date { from_year, to_year } | Generator::DateTime { from_year, to_year } => {
            ui.label("years");
            ui.add(egui::DragValue::new(from_year).range(1900..=2100));
            ui.label("–");
            ui.add(egui::DragValue::new(to_year).range(1900..=2100));
        }
        Generator::Lorem { words } => {
            ui.label("words");
            ui.add(egui::DragValue::new(words).range(1..=1000));
        }
        Generator::RandomString { len } => {
            ui.label("length");
            ui.add(egui::DragValue::new(len).range(1..=4000));
        }
        Generator::Fixed(s) => {
            ui.add(egui::TextEdit::singleline(s).hint_text("value").desired_width(180.0).min_size(egui::vec2(180.0, 0.0)));
        }
        Generator::List(items) => {
            let r = ui.add(egui::TextEdit::multiline(&mut c.list_text).desired_rows(2).desired_width(180.0).hint_text("one value per line"));
            if r.changed() {
                *items = c.list_text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            }
            ui.label(RichText::new(format!("{} value(s)", items.len())).weak().small());
        }
        _ => {
            ui.label(RichText::new("—").weak());
        }
    }
}

fn plan_of(scope: &Scope, name: &str, t: &TableGen) -> GenPlan {
    GenPlan {
        path: scope.path(name),
        rows: t.rows,
        cols: t.cols.iter().map(|c| (c.col.name.clone(), c.gen.clone())).collect(),
        types: t.cols.iter().filter(|c| c.gen != Generator::Skip).map(|c| c.col.data_type.to_ascii_lowercase()).collect(),
    }
}

fn gen_rows(p: &GenPlan, n: usize, start: usize) -> (Vec<String>, Vec<Vec<Value>>) {
    let (names, rows) = datagen::generate_rows(&p.cols, n, start);
    let rows = rows
        .into_iter()
        .map(|r| r.into_iter().enumerate().map(|(i, v)| fix_value(v, "", p.types.get(i).map(|s| s.as_str()).unwrap_or(""))).collect())
        .collect();
    (names, rows)
}

/// Shift sequence generators past the current maximum so new rows don't hit existing keys.
fn continue_sequences(drv: &mut dyn Driver, scope: &Scope, d: Dialect, pl: &mut GenPlan, p: &Shared) {
    for (name, gen) in pl.cols.iter_mut() {
        let Generator::Sequence { start, step } = gen else { continue };
        if *step <= 0 {
            continue;
        }
        let sql = format!("SELECT MAX({}) FROM {}", d.quote_ident(name), d.qualify(&pl.path));
        let max = drv.query(Some(scope), &sql).ok().and_then(|r| r.rows.first().and_then(|row| row.first().cloned()));
        let max = max.and_then(|v| v.as_f64().or_else(|| v.display().parse().ok())).map(|f| f as i64);
        if let Some(m) = max.filter(|m| *m >= *start) {
            *start = m + 1;
            plog(p, LogLevel::Info, format!("{}.{}: sequence continues from {}", pl.path.name, name, *start));
        }
    }
}

fn run_generation(cfg: ConnectionConfig, scope: Scope, plans: Vec<GenPlan>, opts: (bool, usize, bool), p: Shared) -> navdb_core::Result<()> {
    let (delete_existing, batch, continue_on_error) = opts;
    let d = cfg.dialect();
    plog(&p, LogLevel::Info, format!("Connecting to {}…", cfg.display_name()));
    let mut drv = driver::connect(&cfg)?;
    let total: usize = plans.iter().map(|x| x.rows).sum();
    set_counts(&p, 0, total);
    if delete_existing {
        for pl in plans.iter().rev() {
            set_current(&p, format!("Deleting records of {}…", pl.path.name));
            match exec(drv.as_mut(), &scope, &ddl::empty_table(d, &pl.path)) {
                Ok(()) => plog(&p, LogLevel::Info, format!("{}: existing records deleted", pl.path.name)),
                Err(e) => {
                    plog(&p, LogLevel::Error, format!("{}: delete failed: {:#}", pl.path.name, e));
                    if !continue_on_error {
                        anyhow::bail!("stopped after an error");
                    }
                }
            }
        }
    }
    let mut plans = plans;
    if !delete_existing {
        for pl in plans.iter_mut() {
            continue_sequences(drv.as_mut(), &scope, d, pl, &p);
        }
    }
    let mut done = 0usize;
    for pl in &plans {
        if cancelled(&p) {
            plog(&p, LogLevel::Warn, "Cancelled by user");
            break;
        }
        let started = std::time::Instant::now();
        let base = done;
        let res = (|| -> navdb_core::Result<usize> {
            exec(drv.as_mut(), &scope, begin_sql(d))?;
            let mut start = 0;
            while start < pl.rows {
                if cancelled(&p) {
                    anyhow::bail!("cancelled");
                }
                let k = (pl.rows - start).min(batch);
                let (names, rows) = gen_rows(pl, k, start);
                if names.is_empty() {
                    break;
                }
                exec(drv.as_mut(), &scope, &dml::insert_rows(d, &pl.path, &names, &rows))?;
                start += k;
                set_counts(&p, base + start, total);
                set_current(&p, format!("Generating {}… {} / {}", pl.path.name, start, pl.rows));
            }
            exec(drv.as_mut(), &scope, "COMMIT")?;
            Ok(start)
        })();
        match res {
            Ok(n) => {
                plog(&p, LogLevel::Ok, format!("{}: {} row(s) inserted in {}", pl.path.name, n, format_duration(started.elapsed())));
                done = base + pl.rows;
            }
            Err(e) => {
                let _ = exec(drv.as_mut(), &scope, "ROLLBACK");
                plog(&p, LogLevel::Error, format!("{}: {:#} (rolled back)", pl.path.name, e));
                done = base + pl.rows;
                if !continue_on_error {
                    anyhow::bail!("stopped after an error");
                }
            }
        }
        set_counts(&p, done, total);
    }
    set_current(&p, "Done");
    Ok(())
}

impl Dialog for DataGenDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        self.poll();
        if self.any_loading() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
        if self.job.poll(ctx) {
            st.refresh_scope(&self.conn, &self.scope);
            let errs = error_count(&self.job.progress);
            if errs == 0 && matches!(self.job.result, Some(Ok(()))) {
                st.toast(format!("Data generated in {}", self.job.elapsed_text()));
            } else {
                st.error(format!("Data generation finished with {} error(s)", errs.max(1)));
            }
        }
        let mut open = true;
        let mut close_clicked = false;
        let title = format!("Data Generation — {} / {}", st.conn_name(&self.conn), self.scope.display());
        super::window(title, &self.id).open(&mut open).default_width(960.0).show(ctx, |ui| {
            let busy = self.job.running();
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    step_bar(
                        ui,
                        &[(View::Columns, "Tables & Columns"), (View::Preview, "Preview"), (View::Sql, "SQL Preview"), (View::Run, "Generate")],
                        &mut self.view,
                        |_| true,
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(egui::DragValue::new(&mut self.default_rows).range(1..=10_000_000).speed(5.0));
                    ui.label(RichText::new("Default rows:").weak());
                });
            });
            ui.separator();
            let h = 460.0;
            // bounded height: a vertical separator otherwise stretches to the whole screen
            ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), h), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                ui.vertical(|ui| {
                    ui.set_width(220.0);
                    ui.set_height(h);
                    ui.add_enabled_ui(!busy, |ui| self.table_list(ui, st));
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_height(h);
                    ui.set_min_width(ui.available_width());
                    match self.view {
                        View::Columns => {
                            ui.add_enabled_ui(!busy, |ui| self.columns_view(ui, st));
                        }
                        View::Preview => self.preview_view(ui),
                        View::Sql => self.sql_view(ui, st),
                        View::Run => self.run_view(ui, st),
                    }
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close_clicked = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.view != View::Run && ui.add_enabled(!busy, egui::Button::new("Next ›")).clicked() {
                        self.view = match self.view {
                            View::Columns => View::Preview,
                            View::Preview => View::Sql,
                            _ => View::Run,
                        };
                    }
                    if self.view != View::Columns && ui.add_enabled(!busy, egui::Button::new("‹ Back")).clicked() {
                        self.view = match self.view {
                            View::Run => View::Sql,
                            View::Sql => View::Preview,
                            _ => View::Columns,
                        };
                    }
                });
            });
        });
        if close_clicked {
            open = false;
        }
        if !open {
            self.job.cancel();
        }
        open
    }
}
