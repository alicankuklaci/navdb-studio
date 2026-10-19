//! Server Monitor: process list (with kill), server variables (with edit) and
//! status counters with a live chart.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui::RichText;
use egui_plot::{Line, Plot, PlotPoints};
use navdb_core::{DbKind, Pending, QueryResult, ServerSnapshot, Session};

use super::Tab;
use crate::state::AppState;
use crate::ui::grid::{self, GridEvent, GridOptions, GridState};
use crate::ui::style::icon;

const INTERVALS: [u64; 5] = [1, 2, 5, 10, 30];
const MAX_POINTS: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    Processes,
    Variables,
    Status,
}

/// One filtered view of a snapshot list.
#[derive(Default)]
struct List {
    filter: String,
    grid: GridState,
    view: QueryResult,
    built_for: Option<(u64, String)>,
    /// Value of the key column of the selected row (kept across refreshes).
    selected_key: Option<String>,
    key_col: usize,
}

impl List {
    fn rebuild(&mut self, src: &QueryResult, version: u64) {
        let key = (version, self.filter.clone());
        if self.built_for.as_ref() == Some(&key) {
            return;
        }
        let f = self.filter.to_lowercase();
        let rows: Vec<Vec<navdb_core::Value>> = src
            .rows
            .iter()
            .filter(|r| f.is_empty() || r.iter().any(|v| v.display().to_lowercase().contains(&f)))
            .cloned()
            .collect();
        self.view = QueryResult { columns: src.columns.clone(), rows, is_query: true, ..Default::default() };
        self.built_for = Some(key);
        self.grid.order.clear();
        self.grid.selected_rows.clear();
        self.grid.cursor = None;
        if let Some(k) = &self.selected_key {
            if let Some(r) = (0..self.view.rows.len()).find(|&r| &self.view.text(r, self.key_col) == k) {
                self.grid.cursor = Some((r, 0));
            }
        }
        if self.grid.sort.is_some() {
            self.grid.apply_client_sort(&self.view);
        }
    }

    fn selected_row(&self) -> Option<usize> {
        self.grid.cursor.map(|(r, _)| r).filter(|&r| r < self.view.rows.len())
    }
}

struct SeriesDef {
    label: &'static str,
    key: &'static str,
    /// Plot the per-second rate of a monotonically increasing counter.
    rate: bool,
    scale: f64,
}

struct Series {
    def: SeriesDef,
    points: VecDeque<[f64; 2]>,
    prev: Option<(f64, f64)>,
}

fn series_defs(kind: DbKind) -> Vec<SeriesDef> {
    let s = |label, key, rate, scale| SeriesDef { label, key, rate, scale };
    match kind {
        DbKind::MySql | DbKind::MariaDb => vec![
            s("Threads connected", "Threads_connected", false, 1.0),
            s("Threads running", "Threads_running", false, 1.0),
            s("Queries / s", "Questions", true, 1.0),
            s("Received KB / s", "Bytes_received", true, 1.0 / 1024.0),
            s("Sent KB / s", "Bytes_sent", true, 1.0 / 1024.0),
        ],
        DbKind::PostgreSql => vec![
            s("Backends", "numbackends", false, 1.0),
            s("Commits / s", "xact_commit", true, 1.0),
            s("Rollbacks / s", "xact_rollback", true, 1.0),
            s("Rows fetched / s", "tup_fetched", true, 1.0),
            s("Rows inserted / s", "tup_inserted", true, 1.0),
        ],
        DbKind::Redis => vec![
            s("Connected clients", "connected_clients", false, 1.0),
            s("Used memory (MB)", "used_memory", false, 1.0 / 1_048_576.0),
            s("Ops / s", "instantaneous_ops_per_sec", false, 1.0),
            s("Commands / s", "total_commands_processed", true, 1.0),
        ],
        DbKind::Sqlite => vec![],
    }
}

/// Numeric counters of a snapshot keyed by their server name.
fn counters(kind: DbKind, snap: &ServerSnapshot) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let st = &snap.status;
    match kind {
        DbKind::PostgreSql => {
            // per-database rows: sum every numeric column
            for (ci, c) in st.columns.iter().enumerate() {
                let mut sum = 0.0;
                let mut any = false;
                for r in &st.rows {
                    if let Some(v) = r.get(ci).and_then(|v| v.as_f64()) {
                        sum += v;
                        any = true;
                    }
                }
                if any {
                    out.insert(c.name.clone(), sum);
                }
            }
        }
        _ => {
            // name/value rows (Redis has a leading Section column)
            let (k, v) = if st.columns.len() >= 3 { (1, 2) } else { (0, 1) };
            for r in &st.rows {
                if let (Some(name), Some(val)) = (r.get(k), r.get(v).and_then(|x| x.as_f64())) {
                    out.insert(name.display(), val);
                }
            }
        }
    }
    out
}

fn is_simple_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Statement(s) that change a server variable.
fn set_variable_sql(kind: DbKind, name: &str, value: &str) -> Result<String, String> {
    if !is_simple_name(name) {
        return Err(format!("Invalid variable name: {}", name));
    }
    let v = value.trim();
    let numeric = !v.is_empty() && v.parse::<f64>().is_ok();
    let keyword = ["ON", "OFF", "DEFAULT", "TRUE", "FALSE"].iter().any(|k| k.eq_ignore_ascii_case(v));
    Ok(match kind {
        DbKind::MySql | DbKind::MariaDb => {
            let lit = if numeric || keyword { v.to_string() } else { navdb_core::Dialect::MySql.string_literal(v) };
            format!("SET GLOBAL {} = {}", name, lit)
        }
        DbKind::PostgreSql => {
            let lit = if v.eq_ignore_ascii_case("DEFAULT") { "DEFAULT".to_string() } else { navdb_core::Dialect::Postgres.string_literal(v) };
            format!("ALTER SYSTEM SET {} = {};\nSELECT pg_reload_conf()", name, lit)
        }
        DbKind::Redis => format!("CONFIG SET {} {}", name, crate::commands::quote_redis(v)),
        DbKind::Sqlite => {
            let lit = if numeric || keyword || is_simple_name(v) { v.to_string() } else { navdb_core::Dialect::Sqlite.string_literal(v) };
            format!("PRAGMA {} = {}", name, lit)
        }
    })
}

pub struct ServerMonitorTab {
    conn: String,
    conn_name: String,
    kind: DbKind,
    session: Option<Session>,
    sub: Sub,
    snapshot: Option<ServerSnapshot>,
    version: u64,
    loading: Option<Pending<ServerSnapshot>>,
    error: Option<String>,
    last_refresh: Option<Instant>,
    last_refresh_wall: Option<chrono::DateTime<chrono::Local>>,
    auto: bool,
    interval: u64,
    refresh_at: Option<Instant>,
    lists: [List; 3],
    series: Vec<Series>,
    started: Instant,
    kill_request: Rc<RefCell<Option<String>>>,
    killing: Option<(String, Pending<()>)>,
    edit: Option<(String, String)>,
    show_info: bool,
}

impl ServerMonitorTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        // dedicated connection so periodic refreshes never queue behind the navigator
        let session = st.session(conn).map(|s| s.fork());
        let mut lists: [List; 3] = Default::default();
        lists[2].key_col = if kind == DbKind::Redis { 1 } else { 0 };
        let series = series_defs(kind).into_iter().map(|def| Series { def, points: VecDeque::new(), prev: None }).collect();
        let mut me = Self {
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            kind,
            session,
            sub: Sub::Processes,
            snapshot: None,
            version: 0,
            loading: None,
            error: None,
            last_refresh: None,
            last_refresh_wall: None,
            auto: true,
            interval: 5,
            refresh_at: None,
            lists,
            series,
            started: Instant::now(),
            kill_request: Rc::new(RefCell::new(None)),
            killing: None,
            edit: None,
            show_info: true,
        };
        me.refresh();
        me
    }

    fn refresh(&mut self) {
        if self.loading.is_some() {
            return;
        }
        if let Some(s) = &self.session {
            self.loading = Some(s.run(|d| d.server_snapshot()));
            self.last_refresh = Some(Instant::now());
        } else {
            self.error = Some("Connection not found".into());
        }
    }

    fn process_id_col(&self) -> usize {
        let cols = self.snapshot.as_ref().map(|s| &s.processes).map(|p| &p.columns);
        cols.and_then(|c| c.iter().position(|m| ["id", "pid"].contains(&m.name.to_ascii_lowercase().as_str())))
            .unwrap_or(0)
    }

    fn poll(&mut self, st: &mut AppState) {
        if let Some(p) = &mut self.loading {
            if let Some(r) = p.poll() {
                self.loading = None;
                match r {
                    Ok(s) => {
                        self.collect(&s);
                        self.snapshot = Some(s);
                        self.version += 1;
                        self.error = None;
                        self.last_refresh_wall = Some(chrono::Local::now());
                    }
                    Err(e) => self.error = Some(format!("{:#}", e)),
                }
            }
        }
        if let Some((id, p)) = &mut self.killing {
            if let Some(r) = p.poll() {
                let sql = format!("KILL {}", id);
                match r {
                    Ok(()) => {
                        st.toast(format!("Process {} killed", id));
                        st.record(&self.conn, None, &sql, true, "OK", 0);
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.error(format!("Kill failed: {}", m));
                        st.record(&self.conn, None, &sql, false, &m, 0);
                    }
                }
                self.killing = None;
                self.refresh_at = Some(Instant::now() + Duration::from_millis(300));
            }
        }
        let req = self.kill_request.borrow_mut().take();
        if let Some(id) = req {
            if let Some(s) = &self.session {
                let pid = id.clone();
                self.killing = Some((id, s.run(move |d| d.kill_process(&pid))));
            }
        }
        if let Some(t) = self.refresh_at {
            if Instant::now() >= t && self.loading.is_none() {
                self.refresh_at = None;
                self.refresh();
            }
        }
        if self.auto && self.loading.is_none() {
            let due = self.last_refresh.map(|t| t.elapsed() >= Duration::from_secs(self.interval)).unwrap_or(true);
            if due {
                self.refresh();
            }
        }
    }

    fn collect(&mut self, snap: &ServerSnapshot) {
        let c = counters(self.kind, snap);
        let t = self.started.elapsed().as_secs_f64();
        for s in &mut self.series {
            let Some(&raw) = c.get(s.def.key) else { continue };
            let y = if s.def.rate {
                let y = match s.prev {
                    Some((pt, pv)) if t > pt && raw >= pv => Some((raw - pv) / (t - pt)),
                    _ => None,
                };
                s.prev = Some((t, raw));
                y
            } else {
                Some(raw)
            };
            if let Some(y) = y {
                s.points.push_back([t, y * s.def.scale]);
                while s.points.len() > MAX_POINTS {
                    s.points.pop_front();
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::MONITOR, self.conn_name)).strong());
            ui.separator();
            if ui.add_enabled(self.loading.is_none(), egui::Button::new(format!("{} Refresh", icon::REFRESH))).clicked() {
                self.refresh();
            }
            ui.checkbox(&mut self.auto, "Auto refresh");
            egui::ComboBox::from_id_salt(format!("mon-int-{}", self.conn))
                .selected_text(format!("every {} s", self.interval))
                .width(90.0)
                .show_ui(ui, |ui| {
                    for i in INTERVALS {
                        ui.selectable_value(&mut self.interval, i, format!("{} s", i));
                    }
                });
            if self.loading.is_some() {
                ui.spinner();
            }
            if let Some(t) = self.last_refresh_wall {
                ui.label(RichText::new(format!("Last refresh {}", t.format("%H:%M:%S"))).weak().small());
            }
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let counts = self
                .snapshot
                .as_ref()
                .map(|s| [s.processes.rows.len(), s.variables.rows.len(), s.status.rows.len()])
                .unwrap_or([0; 3]);
            for (sub, label, n) in [
                (Sub::Processes, "Process List", counts[0]),
                (Sub::Variables, "Variables", counts[1]),
                (Sub::Status, "Status", counts[2]),
            ] {
                let text = if n > 0 { format!("{} ({})", label, n) } else { label.to_string() };
                ui.selectable_value(&mut self.sub, sub, RichText::new(text).size(13.5));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let l = &mut self.lists[self.sub as usize];
                if !l.filter.is_empty() && ui.small_button("✖").clicked() {
                    l.filter.clear();
                }
                ui.add(egui::TextEdit::singleline(&mut l.filter).hint_text("Search").desired_width(200.0));
                ui.label("🔍");
            });
        });
        ui.separator();
    }

    fn grid(&mut self, ui: &mut egui::Ui, st: &AppState, which: usize) -> Vec<GridEvent> {
        let opt = GridOptions { null_text: &st.prefs().show_null_as, font_size: st.prefs().grid_font_size, ..Default::default() };
        let l = &mut self.lists[which];
        let ev = grid::data_grid(ui, &format!("mon-{}-{}", self.conn, which), &l.view, &mut l.grid, &opt);
        if let Some(r) = l.selected_row() {
            l.selected_key = Some(l.view.text(r, l.key_col));
        }
        ev
    }

    fn processes_ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let sel = self.lists[0].selected_row();
        let id_col = self.process_id_col();
        ui.horizontal(|ui| {
            let can_kill = sel.is_some() && self.killing.is_none() && self.kind != DbKind::Sqlite;
            if ui.add_enabled(can_kill, egui::Button::new(format!("{} Kill Process", icon::STOP))).clicked() {
                if let Some(r) = sel {
                    let id = self.lists[0].view.text(r, id_col);
                    let req = self.kill_request.clone();
                    let pid = id.clone();
                    st.open_dialog(crate::dialogs::confirm::ConfirmDialog::new(
                        "Kill Process",
                        format!("Are you sure you want to kill process {}?", id),
                        Box::new(move |_st: &mut AppState| {
                            *req.borrow_mut() = Some(pid);
                        }),
                    ));
                }
            }
            if self.killing.is_some() {
                ui.spinner();
            }
            ui.toggle_value(&mut self.show_info, "ℹ Process Info");
        });
        ui.add_space(2.0);
        if self.show_info {
            egui::SidePanel::right(format!("mon-info-{}", self.conn)).resizable(true).default_width(320.0).width_range(200.0..=700.0).show_inside(ui, |ui| {
                ui.label(RichText::new("Process Info").strong());
                ui.separator();
                let Some(r) = sel else {
                    ui.label(RichText::new("Select a process.").weak());
                    return;
                };
                let v = &self.lists[0].view;
                let query_col = v.columns.iter().position(|c| ["info", "query", "cmd"].contains(&c.name.to_ascii_lowercase().as_str()));
                let rows: Vec<(String, String)> = v
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| Some(*i) != query_col)
                    .map(|(i, c)| (c.name.clone(), v.text(r, i)))
                    .collect();
                egui::ScrollArea::vertical().id_salt("mon-info-scroll").show(ui, |ui| {
                    crate::ui::kv_grid(ui, &format!("mon-kv-{}", self.conn), &rows);
                    if let Some(qc) = query_col {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Query").strong());
                            if ui.small_button("📋").on_hover_text("Copy").clicked() {
                                ui.ctx().copy_text(v.text(r, qc));
                            }
                        });
                        let mut q = v.text(r, qc);
                        ui.add(egui::TextEdit::multiline(&mut q).code_editor().desired_width(f32::INFINITY).desired_rows(6));
                    }
                });
            });
        }
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.grid(ui, st, 0);
        });
    }

    fn variables_ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let sel = self.lists[1].selected_row();
        ui.horizontal(|ui| {
            if ui.add_enabled(sel.is_some() && self.edit.is_none(), egui::Button::new(format!("{} Edit", icon::DESIGN))).clicked() {
                if let Some(r) = sel {
                    let v = &self.lists[1].view;
                    self.edit = Some((v.text(r, 0), v.text(r, 1)));
                }
            }
            ui.label(RichText::new("Double-click a variable to edit its global value").weak().small());
        });
        if let Some((name, value)) = &mut self.edit {
            let mut apply = false;
            let mut cancel = false;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name.as_str()).monospace().strong());
                    ui.label("=");
                    let r = ui.add(egui::TextEdit::singleline(value).desired_width(260.0).font(egui::TextStyle::Monospace));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    if ui.button(RichText::new("Apply").strong()).clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                match set_variable_sql(self.kind, name, value) {
                    Ok(sql) => {
                        ui.label(RichText::new(sql).monospace().weak());
                    }
                    Err(e) => crate::ui::error_text(ui, &e),
                }
                if self.kind == DbKind::PostgreSql {
                    ui.label(RichText::new("ALTER SYSTEM needs superuser; some settings only take effect after a server restart.").weak().small());
                }
            });
            if apply {
                match set_variable_sql(self.kind, name, value) {
                    Ok(sql) => {
                        st.run_task(&self.conn, None, sql, format!("Variable {} updated", name), false);
                        self.refresh_at = Some(Instant::now() + Duration::from_millis(800));
                        cancel = true;
                    }
                    Err(e) => st.error(e),
                }
            }
            if cancel {
                self.edit = None;
            }
        }
        ui.add_space(2.0);
        let ev = self.grid(ui, st, 1);
        for e in ev {
            if let GridEvent::OpenCellEditor { row, .. } = e {
                let v = &self.lists[1].view;
                self.edit = Some((v.text(row, 0), v.text(row, 1)));
            }
        }
        // the grid only reports cell editor requests for editable grids; treat double-clicks too
        if self.edit.is_none() && ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary)) && ui.ui_contains_pointer() {
            if let Some(r) = self.lists[1].selected_row() {
                let v = &self.lists[1].view;
                self.edit = Some((v.text(r, 0), v.text(r, 1)));
            }
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        if !self.series.is_empty() {
            let avail = ui.available_width();
            let per_row = ((avail / 260.0).floor() as usize).clamp(1, self.series.len());
            let w = (avail - 10.0 * (per_row as f32 - 1.0)) / per_row as f32 - 2.0;
            let line_color = ui.visuals().selection.stroke.color;
            for chunk in self.series.chunks(per_row) {
                ui.horizontal(|ui| {
                    for s in chunk {
                        ui.vertical(|ui| {
                            ui.set_width(w);
                            let cur = s.points.back().map(|p| p[1]);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(s.def.label).strong().small());
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let txt = match cur {
                                        Some(v) if v.abs() >= 100.0 => format!("{:.0}", v),
                                        Some(v) => format!("{:.2}", v),
                                        None => "–".into(),
                                    };
                                    ui.label(RichText::new(txt).monospace());
                                });
                            });
                            let pts: Vec<[f64; 2]> = s.points.iter().copied().collect();
                            Plot::new(format!("mon-plot-{}-{}", self.conn, s.def.key))
                                .height(96.0)
                                .width(w)
                                .allow_drag(false)
                                .allow_zoom(false)
                                .allow_scroll(false)
                                .allow_boxed_zoom(false)
                                .show_x(false)
                                .include_y(0.0)
                                .x_axis_formatter(|m, _| format!("{:.0}s", m.value))
                                .show(ui, |p| {
                                    p.line(Line::new(PlotPoints::new(pts)).color(line_color).fill(0.0_f32).name(s.def.label));
                                });
                        });
                    }
                });
            }
            if self.series.iter().all(|s| s.points.is_empty()) {
                ui.label(RichText::new("Collecting samples… rates need at least two refreshes.").weak().small());
            }
            ui.separator();
        }
        self.grid(ui, st, 2);
    }
}

impl Tab for ServerMonitorTab {
    fn title(&self) -> String {
        format!("{} Monitor - {}", icon::MONITOR, self.conn_name)
    }

    fn key(&self) -> String {
        format!("monitor:{}", self.conn)
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        self.poll(st);
        if self.auto {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        self.toolbar(ui);
        if let Some(e) = &self.error {
            crate::ui::error_text(ui, e);
        }
        let Some(snap) = &self.snapshot else {
            if self.loading.is_some() {
                crate::ui::loading(ui, "Loading server information...");
            }
            return;
        };
        // refresh filtered views
        let v = self.version;
        self.lists[0].key_col = snap
            .processes
            .columns
            .iter()
            .position(|m| ["id", "pid"].contains(&m.name.to_ascii_lowercase().as_str()))
            .unwrap_or(0);
        self.lists[0].rebuild(&snap.processes, v);
        self.lists[1].rebuild(&snap.variables, v);
        self.lists[2].rebuild(&snap.status, v);
        match self.sub {
            Sub::Processes => self.processes_ui(ui, st),
            Sub::Variables => self.variables_ui(ui, st),
            Sub::Status => self.status_ui(ui, st),
        }
    }
}
