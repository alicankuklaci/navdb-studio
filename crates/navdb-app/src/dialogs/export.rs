//! Export Wizard: format → objects/file → columns → options → run.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::RichText;
use navdb_core::driver::Driver;
use navdb_core::export::{self, ExportFormat, ExportOptions};
use navdb_core::{anyhow, Dialect, ObjPath, ObjectKind, Pending, QueryResult, Result, Scope, Session};

use super::dump::{self as job, lock, Progress, SharedProgress};
use super::Dialog;
use crate::state::{AppState, Loadable};
use crate::ui;

const PAGE: usize = 2000;

const DELIMS: [(char, &str); 5] = [(',', "Comma (,)"), (';', "Semicolon (;)"), ('\t', "Tab"), ('|', "Pipe (|)"), (' ', "Space")];

struct Item {
    name: String,
    kind: ObjectKind,
    selected: bool,
    file: String,
}

enum Source {
    Tables { scope: Scope, session: Option<Session> },
    Result { result: Arc<QueryResult> },
}

pub struct ExportWizard {
    id: String,
    title_name: String,
    source: Source,
    dialect: Dialect,
    step: usize,
    format: ExportFormat,
    // objects (table mode)
    preselect: Vec<String>,
    objects: Loadable<Vec<(String, ObjectKind)>>,
    items: Vec<Item>,
    item_filter: String,
    folder: String,
    // single file (result mode)
    file: String,
    // columns
    cols_for: Option<String>,
    cols: Loadable<Vec<String>>,
    col_sel: Vec<(String, bool)>,
    // options
    opts: ExportOptions,
    delim_other: String,
    table_name: String,
    // run
    progress: Option<SharedProgress>,
    job: Option<Pending<()>>,
    outcome: Option<std::result::Result<(), String>>,
    _worker: Option<Session>,
}

fn stem_with_ext(name: &str, fmt: ExportFormat) -> String {
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string());
    format!("{}.{}", stem, fmt.extension())
}

impl ExportWizard {
    fn base(source: Source, title_name: String, dialect: Dialect) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title_name,
            source,
            dialect,
            step: 0,
            format: ExportFormat::Csv,
            preselect: vec![],
            objects: Loadable::NotLoaded,
            items: vec![],
            item_filter: String::new(),
            folder: job::home_dir().display().to_string(),
            file: String::new(),
            cols_for: None,
            cols: Loadable::NotLoaded,
            col_sel: vec![],
            opts: ExportOptions { dialect, ..Default::default() },
            delim_other: String::new(),
            table_name: String::new(),
            progress: None,
            job: None,
            outcome: None,
            _worker: None,
        }
    }

    pub fn new(st: &mut AppState, conn: &str, scope: Scope, tables: Vec<String>) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let session = st.session(conn);
        let objects = match &session {
            Some(s) => {
                let sc = scope.clone();
                Loadable::Loading(s.run(move |d| {
                    let kinds = d.object_kinds();
                    let mut v: Vec<(String, ObjectKind)> = Vec::new();
                    for k in [ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView] {
                        if kinds.contains(&k) {
                            v.extend(d.list_objects(&sc, k)?.into_iter().map(|o| (o.name, k)));
                        }
                    }
                    Ok(v)
                }))
            }
            None => Loadable::Failed("connection not found".into()),
        };
        let title = format!("{} › {}", st.conn_name(conn), scope.display());
        let mut w = Self::base(Source::Tables { scope, session }, title, dialect);
        w.preselect = tables;
        w.objects = objects;
        w
    }

    pub fn from_result(result: QueryResult, name: &str, dialect: Dialect) -> Self {
        let name = if name.trim().is_empty() { "result".to_string() } else { name.to_string() };
        let col_sel = result.columns.iter().map(|c| (c.name.clone(), true)).collect();
        let mut w = Self::base(Source::Result { result: Arc::new(result) }, name.clone(), dialect);
        w.file = job::home_dir().join(stem_with_ext(&job::safe_file_name(&name), ExportFormat::Csv)).display().to_string();
        w.table_name = name;
        w.col_sel = col_sel;
        w
    }

    fn is_result(&self) -> bool {
        matches!(self.source, Source::Result { .. })
    }

    fn steps(&self) -> [&'static str; 5] {
        if self.is_result() {
            ["Format", "Target File", "Columns", "Options", "Export"]
        } else {
            ["Format", "Objects", "Columns", "Options", "Export"]
        }
    }

    fn selected_items(&self) -> Vec<&Item> {
        self.items.iter().filter(|i| i.selected).collect()
    }

    fn set_format(&mut self, f: ExportFormat) {
        self.format = f;
        for it in &mut self.items {
            it.file = stem_with_ext(&it.file, f);
        }
        if !self.file.is_empty() {
            let p = PathBuf::from(&self.file);
            let name = stem_with_ext(&p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(), f);
            self.file = p.with_file_name(name).display().to_string();
        }
        if f == ExportFormat::Tsv {
            self.opts.delimiter = '\t';
        } else if self.opts.delimiter == '\t' && f == ExportFormat::Csv {
            self.opts.delimiter = ',';
        }
    }

    fn step_valid(&self, step: usize) -> bool {
        match step {
            1 => {
                if self.is_result() {
                    !self.file.trim().is_empty()
                } else {
                    !self.folder.trim().is_empty() && self.items.iter().any(|i| i.selected && !i.file.trim().is_empty())
                }
            }
            2 => self.col_sel.is_empty() || self.col_sel.iter().any(|c| c.1),
            _ => true,
        }
    }

    /// Load column list when exactly one table is selected.
    fn ensure_columns(&mut self) {
        let (session, scope) = match &self.source {
            Source::Tables { session: Some(s), scope, .. } => (s.clone(), scope.clone()),
            _ => return,
        };
        let sel = self.selected_items();
        if sel.len() != 1 {
            self.cols_for = None;
            self.cols = Loadable::NotLoaded;
            self.col_sel.clear();
            return;
        }
        let name = sel[0].name.clone();
        if self.cols_for.as_deref() == Some(name.as_str()) {
            return;
        }
        self.cols_for = Some(name.clone());
        self.col_sel.clear();
        let path = scope.path(name);
        self.cols = Loadable::Loading(session.run(move |d| Ok(d.columns(&path)?.into_iter().map(|c| c.name).collect())));
    }

    fn effective_options(&self) -> ExportOptions {
        let mut o = self.opts.clone();
        o.dialect = self.dialect;
        o.columns = if self.col_sel.iter().all(|c| c.1) { vec![] } else { self.col_sel.iter().filter(|c| c.1).map(|c| c.0.clone()).collect() };
        if self.format == ExportFormat::Tsv {
            o.delimiter = '\t';
        }
        o
    }

    fn start(&mut self, st: &mut AppState) {
        let p = Progress::shared();
        let fmt = self.format;
        let o = self.effective_options();
        self.outcome = None;
        match &self.source {
            Source::Result { result } => {
                let result = result.clone();
                let path = PathBuf::from(&self.file);
                let mut o = o;
                o.table = ObjPath::new("", None, self.table_name.clone());
                let p2 = p.clone();
                lock(&p).total = 1;
                self.job = Some(navdb_core::session::spawn_task(st.notify.clone(), move || {
                    let t = Instant::now();
                    lock(&p2).current = format!("Writing {}", path.display());
                    export::export_to_file(&result, fmt, &o, &path)?;
                    let mut g = lock(&p2);
                    g.done = 1;
                    g.current.clear();
                    g.info(format!("{} row(s) exported to {} ({})", result.rows.len(), path.display(), job::fmt_duration(t.elapsed())));
                    Ok(())
                }));
            }
            Source::Tables { scope, session, .. } => {
                let Some(base) = session else { return };
                let folder = PathBuf::from(&self.folder);
                let jobs: Vec<(ObjPath, PathBuf)> = self.selected_items().iter().map(|i| (scope.path(i.name.clone()), folder.join(&i.file))).collect();
                let worker = base.fork();
                let scope = scope.clone();
                let p2 = p.clone();
                lock(&p).total = jobs.len();
                self.job = Some(worker.run(move |d| export_tables(d, &scope, &jobs, fmt, &o, &p2)));
                self._worker = Some(worker);
            }
        }
        self.progress = Some(p);
        self.step = 4;
    }

    fn output_location(&self) -> PathBuf {
        if self.is_result() {
            PathBuf::from(&self.file)
        } else {
            PathBuf::from(&self.folder)
        }
    }

    // ----- steps ---------------------------------------------------------

    fn ui_format(&mut self, ui: &mut egui::Ui) {
        ui.label("Choose the export format:");
        ui.add_space(4.0);
        let mut chosen = None;
        egui::Grid::new(("exp-fmt", &self.id)).num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
            for (i, f) in ExportFormat::ALL.iter().enumerate() {
                if ui.radio(self.format == *f, f.label()).clicked() {
                    chosen = Some(*f);
                }
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });
        if let Some(f) = chosen {
            self.set_format(f);
        }
    }

    fn ui_objects(&mut self, ui: &mut egui::Ui) {
        if self.is_result() {
            ui.label("Save the result to:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.file).desired_width(400.0).min_size(egui::vec2(400.0, 0.0)));
                if ui.button("Save As…").clicked() {
                    let name = Path::new(&self.file).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    if let Some(p) = ui::save_file_dialog(&name, self.format.extension()) {
                        self.file = p.display().to_string();
                    }
                }
            });
            if let Source::Result { result } = &self.source {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{} row(s), {} column(s) will be exported.", result.rows.len(), result.columns.len())).weak());
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Output folder:");
            ui.add(egui::TextEdit::singleline(&mut self.folder).desired_width(340.0).min_size(egui::vec2(340.0, 0.0)));
            if ui.button("Browse…").clicked() {
                if let Some(p) = ui::pick_folder_dialog() {
                    self.folder = p.display().to_string();
                }
            }
        });
        ui.add_space(4.0);
        if ui::show_loadable(ui, &self.objects, "objects").is_none() {
            return;
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.item_filter).hint_text("Filter").desired_width(150.0).min_size(egui::vec2(150.0, 0.0)));
            if ui.small_button("Select All").clicked() {
                self.items.iter_mut().for_each(|i| i.selected = true);
            }
            if ui.small_button("Deselect All").clicked() {
                self.items.iter_mut().for_each(|i| i.selected = false);
            }
            let n = self.items.iter().filter(|i| i.selected).count();
            ui.label(RichText::new(format!("{} of {} selected", n, self.items.len())).weak());
        });
        let f = self.item_filter.to_lowercase();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("exp-items").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new(("exp-items-grid", &self.id)).num_columns(2).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                    ui.label(RichText::new("Source object").strong());
                    ui.label(RichText::new("Output file name").strong());
                    ui.end_row();
                    for it in self.items.iter_mut() {
                        if !f.is_empty() && !it.name.to_lowercase().contains(&f) {
                            continue;
                        }
                        ui.checkbox(&mut it.selected, format!("{} {}", crate::ui::style::object_icon(it.kind), it.name));
                        ui.add_enabled(it.selected, egui::TextEdit::singleline(&mut it.file).desired_width(260.0).min_size(egui::vec2(260.0, 0.0)));
                        ui.end_row();
                    }
                });
                if self.items.is_empty() {
                    ui.label(RichText::new("No tables or views in this database.").weak());
                }
            });
        });
    }

    fn ui_columns(&mut self, ui: &mut egui::Ui) {
        if !self.is_result() {
            let n = self.selected_items().len();
            if n != 1 {
                ui.label(format!("{} objects selected: all columns of every object will be exported.", n));
                ui.label(RichText::new("Select a single table to choose individual columns.").weak());
                return;
            }
            if self.cols.poll() {
                if let Some(c) = self.cols.ready() {
                    self.col_sel = c.iter().map(|n| (n.clone(), true)).collect();
                }
            }
            if ui::show_loadable(ui, &self.cols, "columns").is_none() {
                return;
            }
            ui.label(format!("Columns of {}:", self.cols_for.clone().unwrap_or_default()));
        } else {
            ui.label("Columns to export:");
        }
        ui.horizontal(|ui| {
            if ui.small_button("Select All").clicked() {
                self.col_sel.iter_mut().for_each(|c| c.1 = true);
            }
            if ui.small_button("Deselect All").clicked() {
                self.col_sel.iter_mut().for_each(|c| c.1 = false);
            }
        });
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("exp-cols").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                for (name, sel) in self.col_sel.iter_mut() {
                    ui.checkbox(sel, name.as_str());
                }
            });
        });
        if !self.step_valid(2) {
            ui::error_text(ui, "Select at least one column.");
        }
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        let f = self.format;
        let delimited = matches!(f, ExportFormat::Csv | ExportFormat::Txt);
        let has_header = matches!(f, ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Txt | ExportFormat::Html | ExportFormat::Excel);
        let has_null = !matches!(f, ExportFormat::Json | ExportFormat::Xml | ExportFormat::SqlInsert);
        let can_append = matches!(f, ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Txt | ExportFormat::SqlInsert | ExportFormat::Markdown);
        egui::Grid::new(("exp-opts", &self.id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            if has_header {
                ui.label("Header:");
                ui.checkbox(&mut self.opts.include_headers, "Include column titles");
                ui.end_row();
            }
            if delimited {
                ui.label("Field delimiter:");
                ui.horizontal(|ui| {
                    let known = DELIMS.iter().find(|d| d.0 == self.opts.delimiter).map(|d| d.1);
                    egui::ComboBox::from_id_salt(("exp-delim", &self.id)).selected_text(known.unwrap_or("Other")).show_ui(ui, |ui| {
                        for (c, label) in DELIMS {
                            if ui.selectable_label(self.opts.delimiter == c, label).clicked() {
                                self.opts.delimiter = c;
                            }
                        }
                        if ui.selectable_label(known.is_none(), "Other").clicked() && known.is_some() {
                            self.opts.delimiter = self.delim_other.chars().next().unwrap_or('#');
                        }
                    });
                    if known.is_none() {
                        if self.delim_other.is_empty() {
                            self.delim_other = self.opts.delimiter.to_string();
                        }
                        if ui.add(egui::TextEdit::singleline(&mut self.delim_other).desired_width(30.0).min_size(egui::vec2(30.0, 0.0)).char_limit(1)).changed() {
                            if let Some(c) = self.delim_other.chars().next().filter(|c| c.is_ascii()) {
                                self.opts.delimiter = c;
                            }
                        }
                    }
                });
                ui.end_row();
            }
            if matches!(f, ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Txt) {
                ui.label("Text qualifier:");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.opts.quote, '"', "Double quote (\")");
                    ui.radio_value(&mut self.opts.quote, '\'', "Single quote (')");
                });
                ui.end_row();
            }
            if has_null {
                ui.label("NULL as:");
                ui.add(egui::TextEdit::singleline(&mut self.opts.null_text).hint_text("(empty)").desired_width(120.0).min_size(egui::vec2(120.0, 0.0)));
                ui.end_row();
            }
            ui.label("Line ending:");
            ui.horizontal(|ui| {
                let mut crlf = self.opts.line_ending == "\r\n";
                ui.radio_value(&mut crlf, false, "LF (Unix / macOS)");
                ui.radio_value(&mut crlf, true, "CRLF (Windows)");
                self.opts.line_ending = if crlf { "\r\n".into() } else { "\n".into() };
            });
            ui.end_row();
            if f == ExportFormat::SqlInsert {
                ui.label("Records per INSERT:");
                ui.add(egui::DragValue::new(&mut self.opts.rows_per_insert).range(1..=10000));
                ui.end_row();
                if self.is_result() {
                    ui.label("Target table name:");
                    ui.add(egui::TextEdit::singleline(&mut self.table_name).desired_width(200.0).min_size(egui::vec2(200.0, 0.0)));
                    ui.end_row();
                }
            }
            if can_append {
                ui.label("Existing file:");
                ui.checkbox(&mut self.opts.append, "Append to existing file instead of overwriting");
                ui.end_row();
            }
        });
    }

    fn ui_run(&mut self, ui: &mut egui::Ui, running: bool) {
        match &self.progress {
            None => {
                ui.label(RichText::new("Ready to export").strong());
                ui.add_space(4.0);
                let what = if self.is_result() {
                    format!("Query result → {}", self.file)
                } else {
                    format!("{} object(s) → {}", self.selected_items().len(), self.folder)
                };
                ui::kv_grid(
                    ui,
                    &format!("exp-sum-{}", self.id),
                    &[("Format".into(), self.format.label().into()), ("Export".into(), what)],
                );
                ui.add_space(6.0);
                ui.label(RichText::new("Click Start to begin.").weak());
            }
            Some(p) => {
                job::progress_view(ui, p, running, 220.0);
                if let Some(o) = &self.outcome {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        match o {
                            Ok(()) => {
                                let errs = lock(p).errors;
                                if errs == 0 {
                                    ui.label(RichText::new("✔ Export finished").strong().color(egui::Color32::from_rgb(60, 170, 90)));
                                } else {
                                    ui::error_text(ui, &format!("Export finished with {} error(s)", errs));
                                }
                            }
                            Err(e) => ui::error_text(ui, e),
                        }
                        let loc = self.output_location();
                        if ui.button("📂 Open Folder").clicked() {
                            if loc.is_dir() {
                                job::open_path(&loc);
                            } else {
                                job::reveal_path(&loc);
                            }
                        }
                        if self.is_result() && loc.is_file() && ui.button("Open File").clicked() {
                            job::open_path(&loc);
                        }
                    });
                }
            }
        }
    }
}

/// Worker: export every (table, file) pair page by page.
fn export_tables(d: &mut dyn Driver, scope: &Scope, jobs: &[(ObjPath, PathBuf)], fmt: ExportFormat, base: &ExportOptions, p: &SharedProgress) -> Result<()> {
    let streaming = matches!(fmt, ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Txt | ExportFormat::SqlInsert);
    let dialect = d.dialect();
    let mut failed = 0;
    for (i, (path, file)) in jobs.iter().enumerate() {
        if lock(p).cancel {
            lock(p).err("Cancelled by user");
            return Err(anyhow!("cancelled"));
        }
        let t = Instant::now();
        lock(p).current = format!("Exporting {}", path.name);
        let mut o = base.clone();
        o.table = path.clone();
        o.dialect = dialect;
        let r = (|| -> Result<usize> {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new().create(true).write(true).append(o.append).truncate(!o.append).open(file)?;
            let mut w = std::io::BufWriter::new(f);
            let mut offset = 0usize;
            let mut rows = 0usize;
            let mut all = QueryResult { is_query: true, ..Default::default() };
            loop {
                if lock(p).cancel {
                    return Err(anyhow!("cancelled"));
                }
                let sql = navdb_core::dml::select_page(dialect, path, "", &[], PAGE, offset);
                let page = d.query(Some(scope), &sql)?;
                let n = page.rows.len();
                rows += n;
                if streaming {
                    let mut po = o.clone();
                    if offset > 0 {
                        po.include_headers = false;
                    }
                    if offset == 0 || n > 0 {
                        export::export(&page, fmt, &po, &mut w)?;
                    }
                } else {
                    if all.columns.is_empty() {
                        all.columns = page.columns;
                    }
                    all.rows.extend(page.rows);
                }
                lock(p).current = format!("Exporting {}: {} rows", path.name, rows);
                if n < PAGE {
                    break;
                }
                offset += PAGE;
            }
            if !streaming {
                export::export(&all, fmt, &o, &mut w)?;
            }
            w.flush()?;
            Ok(rows)
        })();
        let mut g = lock(p);
        match r {
            Ok(rows) => g.info(format!("{}: {} row(s) → {} ({})", path.name, rows, file.display(), job::fmt_duration(t.elapsed()))),
            Err(e) => {
                failed += 1;
                g.err(format!("{}: {:#}", path.name, e));
            }
        }
        g.done = i + 1;
    }
    let mut g = lock(p);
    g.current.clear();
    let el = g.started.elapsed();
    g.info(format!("Finished: {} object(s) exported, {} failed, total time {}", jobs.len() - failed, failed, job::fmt_duration(el)));
    Ok(())
}

impl Dialog for ExportWizard {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut close = false;
        if self.objects.poll() {
            if let Some(objs) = self.objects.ready() {
                let fmt = self.format;
                self.items = objs
                    .iter()
                    .map(|(n, k)| Item {
                        name: n.clone(),
                        kind: *k,
                        selected: self.preselect.contains(n),
                        file: format!("{}.{}", job::safe_file_name(n), fmt.extension()),
                    })
                    .collect();
            }
        }
        if let Some(r) = job::finish_job(&mut self.job, self.progress.as_ref()) {
            match &r {
                Ok(()) => st.toast("Export finished"),
                Err(e) => st.error(format!("Export failed: {}", e)),
            }
            self.outcome = Some(r);
        }
        let running = self.job.is_some();
        if running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let title = format!("Export Wizard – {}", self.title_name);
        super::window(title, &self.id).open(&mut open).default_width(620.0).show(ctx, |ui| {
            let steps = self.steps();
            job::steps_header(ui, &steps, self.step);
            egui::ScrollArea::vertical().id_salt("exp-body").max_height(420.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_min_height(300.0);
                match self.step {
                    0 => self.ui_format(ui),
                    1 => self.ui_objects(ui),
                    2 => self.ui_columns(ui),
                    3 => self.ui_options(ui),
                    _ => self.ui_run(ui, running),
                }
            });
            let last = self.step == 4;
            let next = if last {
                if self.progress.is_some() && self.outcome.is_some() {
                    Some(("Export Again", true))
                } else if running {
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
                if self.step == 4 {
                    self.progress = None;
                    self.outcome = None;
                }
                self.step -= 1;
            }
            if c.next {
                if last {
                    self.start(st);
                } else {
                    self.step += 1;
                    if self.step == 2 {
                        self.ensure_columns();
                    }
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
