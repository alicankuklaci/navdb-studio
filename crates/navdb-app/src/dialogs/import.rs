//! Import Wizard: format → source → options/preview → target → mapping → mode → run.

use std::path::PathBuf;
use std::time::Duration;

use egui::RichText;
use navdb_core::import::{self, FieldMapping, ImportFormat, ImportMode, ImportOptions};
use navdb_core::{ddl, DbKind, Dialect, Pending, QueryResult, Scope, Session, TableDef};

use super::dump::{self as job, lock, Progress, RunOpts, SharedProgress};
use super::Dialog;
use crate::state::{AppState, Loadable};
use crate::ui;
use crate::ui::grid::{self, GridOptions, GridState};

const STEPS: [&str; 7] = ["Format", "Source File", "Options", "Target Table", "Field Mapping", "Import Mode", "Import"];
const PREVIEW_ROWS: usize = 50;

pub struct ImportWizard {
    id: String,
    conn: String,
    title: String,
    kind: DbKind,
    dialect: Dialect,
    scope: Scope,
    session: Option<Session>,
    step: usize,
    // source
    path: String,
    content: Option<std::result::Result<String, String>>,
    content_for: String,
    // options
    opts: ImportOptions,
    last_row_text: String,
    parsed: Option<std::result::Result<QueryResult, String>>,
    parse_dirty: bool,
    preview: QueryResult,
    grid: GridState,
    // target
    tables: Loadable<Vec<String>>,
    new_table: bool,
    target: String,
    new_name: String,
    new_def: Option<TableDef>,
    new_def_sig: Vec<String>,
    target_def: Loadable<TableDef>,
    target_def_for: String,
    // mapping
    mapping: Vec<FieldMapping>,
    mapping_sig: String,
    // mode
    mode: ImportMode,
    continue_on_error: bool,
    use_tx: bool,
    batch: usize,
    // run
    progress: Option<SharedProgress>,
    job: Option<Pending<()>>,
    outcome: Option<std::result::Result<(), String>>,
    _worker: Option<Session>,
}

impl ImportWizard {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, table: Option<String>) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        let session = st.session(conn);
        let tables = match &session {
            Some(s) if kind.is_sql() => {
                let sc = scope.clone();
                Loadable::Loading(s.run(move |d| Ok(d.list_objects(&sc, navdb_core::ObjectKind::Table)?.into_iter().map(|o| o.name).collect())))
            }
            _ => Loadable::NotLoaded,
        };
        let new_table = table.is_none();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            title: format!("{} › {}", st.conn_name(conn), scope.display()),
            kind,
            dialect: kind.dialect(),
            scope,
            session,
            step: 0,
            path: String::new(),
            content: None,
            content_for: String::new(),
            opts: ImportOptions::default(),
            last_row_text: String::new(),
            parsed: None,
            parse_dirty: true,
            preview: QueryResult::default(),
            grid: GridState::default(),
            tables,
            new_table,
            target: table.unwrap_or_default(),
            new_name: String::new(),
            new_def: None,
            new_def_sig: vec![],
            target_def: Loadable::NotLoaded,
            target_def_for: String::new(),
            mapping: vec![],
            mapping_sig: String::new(),
            mode: ImportMode::Append,
            continue_on_error: false,
            use_tx: true,
            batch: 100,
            progress: None,
            job: None,
            outcome: None,
            _worker: None,
        }
    }

    fn reload_tables(&mut self) {
        if let Some(s) = &self.session {
            let sc = self.scope.clone();
            self.tables = Loadable::Loading(s.run(move |d| Ok(d.list_objects(&sc, navdb_core::ObjectKind::Table)?.into_iter().map(|o| o.name).collect())));
        }
    }

    fn exts(&self) -> &'static [&'static str] {
        match self.opts.format {
            ImportFormat::Csv => &["csv", "txt"],
            ImportFormat::Tsv => &["tsv", "txt", "tab"],
            ImportFormat::Json => &["json"],
            ImportFormat::Xml => &["xml"],
        }
    }

    fn load_content(&mut self) {
        if self.content_for == self.path && self.content.is_some() {
            return;
        }
        self.content_for = self.path.clone();
        self.content = Some(job::read_text(std::path::Path::new(self.path.trim())).map_err(|e| format!("{:#}", e)));
        self.parse_dirty = true;
    }

    fn reparse(&mut self) {
        if !self.parse_dirty {
            return;
        }
        self.parse_dirty = false;
        self.parsed = match &self.content {
            Some(Ok(c)) => Some(import::parse(c, &self.opts).map_err(|e| format!("{:#}", e))),
            Some(Err(e)) => Some(Err(e.clone())),
            None => None,
        };
        self.preview = QueryResult { is_query: true, ..Default::default() };
        if let Some(Ok(r)) = &self.parsed {
            self.preview.columns = r.columns.clone();
            self.preview.rows = r.rows.iter().take(PREVIEW_ROWS).cloned().collect();
        }
        self.grid.reset();
    }

    fn source(&self) -> Option<&QueryResult> {
        match &self.parsed {
            Some(Ok(r)) => Some(r),
            _ => None,
        }
    }

    fn source_cols(&self) -> Vec<String> {
        self.source().map(|r| r.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default()
    }

    fn target_name(&self) -> String {
        if self.new_table {
            self.new_name.trim().to_string()
        } else {
            self.target.clone()
        }
    }

    /// Prepare state when entering `step`.
    fn enter(&mut self, step: usize) {
        match step {
            2 => {
                self.load_content();
                self.reparse();
            }
            3 => {
                if self.new_name.is_empty() {
                    let stem = std::path::Path::new(&self.path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    self.new_name = stem.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
                }
                self.ensure_new_def();
            }
            4 => {
                self.ensure_new_def();
                if !self.new_table {
                    self.ensure_target_def();
                }
                self.ensure_mapping();
            }
            _ => {}
        }
    }

    fn ensure_new_def(&mut self) {
        let sig = self.source_cols();
        let name = self.new_name.trim().to_string();
        if self.new_def.is_none() || self.new_def_sig != sig {
            if let Some(src) = self.source() {
                self.new_def = Some(import::infer_table(src, self.scope.path(name.clone()), self.dialect));
                self.new_def_sig = sig;
            }
        }
        if let Some(d) = &mut self.new_def {
            d.path = self.scope.path(name);
        }
    }

    fn ensure_target_def(&mut self) {
        if self.target_def_for == self.target && !self.target_def.is_not_loaded() {
            return;
        }
        let Some(s) = &self.session else { return };
        self.target_def_for = self.target.clone();
        let path = self.scope.path(self.target.clone());
        self.target_def = Loadable::Loading(s.run(move |d| d.table_def(&path)));
    }

    fn target_columns(&self) -> Option<(Vec<String>, Vec<String>)> {
        if self.new_table {
            let d = self.new_def.as_ref()?;
            Some((d.columns.iter().map(|c| c.name.clone()).collect(), d.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()))
        } else {
            let d = self.target_def.ready()?;
            Some((d.columns.iter().map(|c| c.name.clone()).collect(), d.primary_key()))
        }
    }

    fn ensure_mapping(&mut self) {
        let Some((cols, pk)) = self.target_columns() else { return };
        let sig = format!("{}|{}|{}|{}", self.new_table, self.target_name(), self.source_cols().join(","), cols.join(","));
        if sig == self.mapping_sig {
            return;
        }
        self.mapping_sig = sig;
        let Some(src) = self.source() else { return };
        self.mapping = if self.new_table {
            src.columns
                .iter()
                .enumerate()
                .map(|(i, c)| FieldMapping { source: c.name.clone(), target: cols.get(i).cloned(), is_key: cols.get(i).map(|t| pk.contains(t)).unwrap_or(false) })
                .collect()
        } else {
            let mut m = import::auto_map(src, &cols);
            for f in &mut m {
                f.is_key = f.target.as_ref().map(|t| pk.contains(t)).unwrap_or(false);
            }
            m
        };
    }

    fn mode_needs_key(&self) -> bool {
        matches!(self.mode, ImportMode::Update | ImportMode::AppendUpdate | ImportMode::Delete)
    }

    fn step_valid(&self, step: usize) -> bool {
        match step {
            1 => !self.path.trim().is_empty() && std::path::Path::new(self.path.trim()).is_file(),
            2 => self.source().map(|r| !r.columns.is_empty()).unwrap_or(false),
            3 => {
                if self.new_table {
                    !self.new_name.trim().is_empty() && self.new_def.as_ref().map(|d| !d.columns.is_empty() && d.columns.iter().all(|c| !c.name.trim().is_empty() && !c.data_type.trim().is_empty())).unwrap_or(false)
                } else {
                    !self.target.is_empty()
                }
            }
            4 => self.mapping.iter().any(|m| m.target.is_some()),
            5 => !self.mode_needs_key() || self.mapping.iter().any(|m| m.is_key && m.target.is_some()),
            _ => true,
        }
    }

    fn start(&mut self) {
        let Some(base) = &self.session else { return };
        let Some(src) = self.source().cloned() else { return };
        let worker = base.fork();
        let p = Progress::shared();
        let scope = self.scope.clone();
        let table = self.scope.path(self.target_name());
        let create = if self.new_table { self.new_def.clone() } else { None };
        let mapping = self.mapping.clone();
        let mode = self.mode;
        let batch = self.batch.max(1);
        let ro = RunOpts { continue_on_error: self.continue_on_error, transaction: self.use_tx };
        let p2 = p.clone();
        {
            let mut g = lock(&p);
            g.info(format!("Importing {} record(s) from {} into {}", src.rows.len(), self.path, table.display()));
        }
        self.job = Some(worker.run(move |d| {
            let dialect = d.dialect();
            if let Some(def) = &create {
                lock(&p2).current = format!("Creating table {}", def.path.name);
                for s in ddl::create_table(dialect, def) {
                    if let Err(e) = d.execute(Some(&scope), &s) {
                        lock(&p2).err(format!("Create table failed: {:#}\n    {}", e, job::preview_sql(&s)));
                        return Err(e);
                    }
                }
                lock(&p2).info(format!("Table {} created", def.path.name));
            }
            lock(&p2).current = "Preparing statements".into();
            let stmts = import::build_statements(dialect, &table, &src, &mapping, mode, batch);
            lock(&p2).info(format!("{} statement(s) to execute", stmts.len()));
            let (ok, failed, affected) = job::run_statements(d, Some(&scope), &stmts, ro, &p2)?;
            let mut g = lock(&p2);
            g.current.clear();
            let el = g.started.elapsed();
            g.info(format!(
                "Finished: {} statement(s) succeeded, {} failed, {} row(s) affected, {}",
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
    }

    // ----- steps ---------------------------------------------------------

    fn ui_format(&mut self, ui: &mut egui::Ui) {
        ui.label("Choose the format of the source file:");
        ui.add_space(4.0);
        for f in ImportFormat::ALL {
            if ui.radio(self.opts.format == f, f.label()).clicked() && self.opts.format != f {
                self.opts.format = f;
                self.opts.delimiter = if f == ImportFormat::Tsv { '\t' } else { ',' };
                self.parse_dirty = true;
            }
        }
    }

    fn ui_source(&mut self, ui: &mut egui::Ui) {
        ui.label("Import from file:");
        ui.horizontal(|ui| {
            if ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(400.0).min_size(egui::vec2(400.0, 0.0)).hint_text("/path/to/file")).changed() {
                self.content = None;
            }
            if ui.button("Browse…").clicked() {
                if let Some(p) = ui::open_file_dialog(self.exts()) {
                    self.path = p.display().to_string();
                    self.content = None;
                }
            }
        });
        let p = PathBuf::from(self.path.trim());
        if !self.path.trim().is_empty() {
            match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => {
                    ui.label(RichText::new(format!("Size: {}", ui::format_bytes(m.len() as i64))).weak());
                }
                _ => ui::error_text(ui, "File not found"),
            }
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Encoding: the file is read as UTF-8 (a BOM is skipped; invalid byte sequences are replaced).").weak());
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let delimited = matches!(self.opts.format, ImportFormat::Csv | ImportFormat::Tsv);
        egui::Grid::new(("imp-opts", &self.id)).num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
            if delimited {
                ui.label("Field delimiter:");
                if self.opts.format == ImportFormat::Tsv {
                    ui.label("Tab");
                } else {
                    egui::ComboBox::from_id_salt(("imp-delim", &self.id)).selected_text(match self.opts.delimiter {
                        ',' => "Comma (,)".to_string(),
                        ';' => "Semicolon (;)".to_string(),
                        '|' => "Pipe (|)".to_string(),
                        '\t' => "Tab".to_string(),
                        c => format!("'{}'", c),
                    }).show_ui(ui, |ui| {
                        for (c, l) in [(',', "Comma (,)"), (';', "Semicolon (;)"), ('|', "Pipe (|)"), ('\t', "Tab")] {
                            if ui.selectable_label(self.opts.delimiter == c, l).clicked() {
                                self.opts.delimiter = c;
                                changed = true;
                            }
                        }
                    });
                }
                ui.label("Text qualifier:");
                ui.horizontal(|ui| {
                    changed |= ui.radio_value(&mut self.opts.quote, '"', "\"").changed();
                    changed |= ui.radio_value(&mut self.opts.quote, '\'', "'").changed();
                });
                ui.end_row();
                ui.label("Field name row:");
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut self.opts.has_header, "").changed() {
                        self.opts.first_data_row = if self.opts.has_header { self.opts.header_row + 1 } else { 1 };
                        changed = true;
                    }
                    ui.add_enabled_ui(self.opts.has_header, |ui| {
                        if ui.add(egui::DragValue::new(&mut self.opts.header_row).range(1..=1_000_000)).changed() {
                            if self.opts.first_data_row <= self.opts.header_row {
                                self.opts.first_data_row = self.opts.header_row + 1;
                            }
                            changed = true;
                        }
                    });
                });
                ui.label("First data row:");
                changed |= ui.add(egui::DragValue::new(&mut self.opts.first_data_row).range(1..=100_000_000)).changed();
                ui.end_row();
                ui.label("Last data row:");
                if ui.add(egui::TextEdit::singleline(&mut self.last_row_text).hint_text("(end of file)").desired_width(90.0).min_size(egui::vec2(90.0, 0.0))).changed() {
                    self.opts.last_data_row = self.last_row_text.trim().parse().ok();
                    changed = true;
                }
            }
            if self.opts.format == ImportFormat::Xml {
                ui.label("Record tag:");
                changed |= ui.add(egui::TextEdit::singleline(&mut self.opts.xml_record_tag).desired_width(120.0).min_size(egui::vec2(120.0, 0.0))).changed();
            }
            ui.label("Empty string:");
            changed |= ui.checkbox(&mut self.opts.empty_as_null, "Import as NULL").changed();
            ui.end_row();
        });
        if changed {
            self.parse_dirty = true;
            self.reparse();
        }
        ui.add_space(4.0);
        ui.separator();
        match &self.parsed {
            None => ui::loading(ui, "Reading file..."),
            Some(Err(e)) => ui::error_text(ui, &format!("Cannot parse the file: {}", e)),
            Some(Ok(r)) => {
                ui.label(RichText::new(format!("Preview: first {} of {} record(s), {} field(s)", self.preview.rows.len(), r.rows.len(), r.columns.len())).weak());
                let opt = GridOptions::default();
                ui.allocate_ui(egui::vec2(ui.available_width(), 230.0), |ui| {
                    grid::data_grid(ui, &format!("imp-prev-{}", self.id), &self.preview, &mut self.grid, &opt);
                });
            }
        }
    }

    fn ui_target(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui.radio_value(&mut self.new_table, false, "Existing table").changed();
            changed |= ui.radio_value(&mut self.new_table, true, "New table").changed();
        });
        ui.add_space(4.0);
        if !self.new_table {
            if let Some(tables) = ui::show_loadable(ui, &self.tables, "tables") {
                let tables = tables.clone();
                ui.horizontal(|ui| {
                    ui.label("Target table:");
                    let label = if self.target.is_empty() { "Select table".to_string() } else { self.target.clone() };
                    egui::ComboBox::from_id_salt(("imp-target", &self.id)).selected_text(label).width(240.0).height(300.0).show_ui(ui, |ui| {
                        for t in &tables {
                            if ui.selectable_label(&self.target == t, t).clicked() {
                                self.target = t.clone();
                            }
                        }
                    });
                });
                if !self.target.is_empty() && !tables.contains(&self.target) {
                    ui::error_text(ui, "Table not found in this database");
                }
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.label("New table name:");
            if ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(240.0).min_size(egui::vec2(240.0, 0.0))).changed() {
                changed = true;
            }
            if ui.small_button("Re-detect types").clicked() {
                self.new_def = None;
                changed = true;
            }
        });
        if let Some(t) = self.tables.ready() {
            if t.iter().any(|n| n.eq_ignore_ascii_case(self.new_name.trim())) {
                ui::error_text(ui, "A table with this name already exists");
            }
        }
        if changed {
            self.ensure_new_def();
        }
        let types = self.dialect.data_types();
        let id = self.id.clone();
        let Some(def) = &mut self.new_def else {
            ui.label("Parse the source file first.");
            return;
        };
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("imp-newcols").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new(("imp-newcols-grid", &id)).num_columns(4).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Column").strong());
                ui.label(RichText::new("Type").strong());
                ui.label(RichText::new("Primary key").strong());
                ui.label(RichText::new("Allow NULL").strong());
                ui.end_row();
                for (i, c) in def.columns.iter_mut().enumerate() {
                    ui.add_sized([180.0, 20.0], egui::TextEdit::singleline(&mut c.name));
                    ui.horizontal(|ui| {
                        ui.add_sized([120.0, 20.0], egui::TextEdit::singleline(&mut c.data_type));
                        egui::ComboBox::from_id_salt(("imp-type", &id, i)).selected_text("").width(18.0).height(300.0).show_ui(ui, |ui| {
                            for t in types {
                                if ui.selectable_label(c.data_type.eq_ignore_ascii_case(t), *t).clicked() {
                                    c.data_type = t.to_string();
                                }
                            }
                        });
                    });
                    if ui.checkbox(&mut c.primary_key, "").changed() && c.primary_key {
                        c.nullable = false;
                    }
                    ui.add_enabled(!c.primary_key, egui::Checkbox::new(&mut c.nullable, ""));
                    ui.end_row();
                }
            });
        });
    }

    fn ui_mapping(&mut self, ui: &mut egui::Ui) {
        if !self.new_table {
            if self.target_def.poll() {
                self.ensure_mapping();
            }
            if ui::show_loadable(ui, &self.target_def, "table structure").is_none() {
                return;
            }
        }
        let Some((cols, _)) = self.target_columns() else { return };
        ui.horizontal(|ui| {
            ui.label(format!("Source fields → {}", self.target_name()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Skip All").clicked() {
                    self.mapping.iter_mut().for_each(|m| m.target = None);
                }
                if ui.small_button("Map by Name").clicked() {
                    for m in &mut self.mapping {
                        m.target = cols.iter().find(|c| c.eq_ignore_ascii_case(&m.source)).cloned();
                    }
                }
            });
        });
        let id = self.id.clone();
        egui::ScrollArea::vertical().id_salt("imp-map").max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new(("imp-map-grid", &id)).num_columns(3).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Source Field").strong());
                ui.label(RichText::new("Target Field").strong());
                ui.label(RichText::new("Primary Key").strong());
                ui.end_row();
                for (i, m) in self.mapping.iter_mut().enumerate() {
                    ui.label(&m.source);
                    let label = m.target.clone().unwrap_or_else(|| "(skip)".into());
                    let text = if m.target.is_some() { RichText::new(label) } else { RichText::new(label).weak() };
                    egui::ComboBox::from_id_salt(("imp-map-c", &id, i)).selected_text(text).width(200.0).height(300.0).show_ui(ui, |ui| {
                        if ui.selectable_label(m.target.is_none(), "(skip)").clicked() {
                            m.target = None;
                        }
                        for c in &cols {
                            if ui.selectable_label(m.target.as_ref() == Some(c), c).clicked() {
                                m.target = Some(c.clone());
                            }
                        }
                    });
                    ui.add_enabled(m.target.is_some(), egui::Checkbox::new(&mut m.is_key, ""));
                    ui.end_row();
                }
            });
        });
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = self.mapping.iter().filter_map(|m| m.target.as_ref()).find(|t| !seen.insert(t.to_string())) {
            ui::error_text(ui, &format!("Target field \"{}\" is mapped more than once", dup));
        }
    }

    fn ui_mode(&mut self, ui: &mut egui::Ui) {
        ui.label("Import mode:");
        ui.add_space(2.0);
        for m in ImportMode::ALL {
            ui.radio_value(&mut self.mode, m, m.label());
        }
        if self.mode_needs_key() && !self.step_valid(5) {
            ui::error_text(ui, "This mode needs at least one key field (mark keys in Field Mapping).");
        }
        if self.mode == ImportMode::Copy {
            ui.label(RichText::new("⚠ All existing records of the target table will be deleted first.").color(ui.visuals().warn_fg_color));
        }
        ui.add_space(8.0);
        ui.separator();
        ui.checkbox(&mut self.use_tx, "Run the import in a single transaction");
        ui.checkbox(&mut self.continue_on_error, "Continue on error");
        ui.horizontal(|ui| {
            ui.label("Records per INSERT statement:");
            ui.add(egui::DragValue::new(&mut self.batch).range(1..=10000));
        });
    }

    fn ui_run(&mut self, ui: &mut egui::Ui, running: bool) {
        match &self.progress {
            None => {
                ui.label(RichText::new("Ready to import").strong());
                ui.add_space(4.0);
                let rows = self.source().map(|r| r.rows.len()).unwrap_or(0);
                let mapped = self.mapping.iter().filter(|m| m.target.is_some()).count();
                ui::kv_grid(
                    ui,
                    &format!("imp-sum-{}", self.id),
                    &[
                        ("Source".into(), self.path.clone()),
                        ("Records".into(), rows.to_string()),
                        ("Target".into(), format!("{}{}", self.target_name(), if self.new_table { " (new)" } else { "" })),
                        ("Mapped fields".into(), format!("{} of {}", mapped, self.mapping.len())),
                        ("Mode".into(), self.mode.label().into()),
                    ],
                );
                ui.add_space(6.0);
                ui.label(RichText::new("Click Start to begin.").weak());
            }
            Some(p) => {
                job::progress_view(ui, p, running, 220.0);
                if let Some(o) = &self.outcome {
                    match o {
                        Ok(()) => {
                            let e = lock(p).errors;
                            if e == 0 {
                                ui.label(RichText::new("✔ Import finished successfully").strong().color(egui::Color32::from_rgb(60, 170, 90)));
                            } else {
                                ui::error_text(ui, &format!("Import finished with {} error(s)", e));
                            }
                        }
                        Err(e) => ui::error_text(ui, &format!("Import failed: {}", e)),
                    }
                }
            }
        }
    }
}

impl Dialog for ImportWizard {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        self.tables.poll();
        if self.target_def.poll() {
            self.ensure_mapping();
        }
        if let Some(r) = job::finish_job(&mut self.job, self.progress.as_ref()) {
            match &r {
                Ok(()) => st.toast(format!("Import into {} finished", self.target_name())),
                Err(e) => st.error(format!("Import failed: {}", e)),
            }
            st.refresh_scope(&self.conn, &self.scope);
            self.reload_tables();
            self.outcome = Some(r);
        }
        let running = self.job.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        super::window(format!("Import Wizard – {}", self.title), &self.id).open(&mut open).default_width(780.0).show(ctx, |ui| {
            if !self.kind.is_sql() {
                ui.label("The Import Wizard is only available for SQL databases.");
                if ui.button("Close").clicked() {
                    close = true;
                }
                return;
            }
            job::steps_header(ui, &STEPS, self.step);
            egui::ScrollArea::vertical().id_salt("imp-body").max_height(440.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_min_height(320.0);
                match self.step {
                    0 => self.ui_format(ui),
                    1 => self.ui_source(ui),
                    2 => self.ui_options(ui),
                    3 => self.ui_target(ui),
                    4 => self.ui_mapping(ui),
                    5 => self.ui_mode(ui),
                    _ => self.ui_run(ui, running),
                }
            });
            let last = self.step == STEPS.len() - 1;
            let next = if last {
                if running || self.outcome.is_some() {
                    None
                } else {
                    Some(("Start", true))
                }
            } else {
                Some(("Next ›", self.step_valid(self.step)))
            };
            let close_label = if running { "Stop" } else if self.outcome.is_some() { "Close" } else { "Cancel" };
            let c = job::footer(ui, self.step > 0 && !running, next, close_label);
            if c.back {
                if last {
                    self.progress = None;
                    self.outcome = None;
                }
                self.step -= 1;
            }
            if c.next {
                if last {
                    self.start();
                } else {
                    self.step += 1;
                    self.enter(self.step);
                }
            }
            if c.close {
                if running {
                    if let Some(p) = &self.progress {
                        lock(p).cancel = true;
                    }
                } else {
                    close = true;
                }
            }
        });
        open && !close
    }
}
