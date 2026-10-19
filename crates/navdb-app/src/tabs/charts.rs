//! Charts tab: build charts from SQL queries and arrange them on a dashboard.

use std::collections::HashMap;

use egui::{pos2, vec2, Align2, Color32, FontId, RichText, Sense, Stroke, Ui};
use egui_plot::{Bar, BarChart, Corner, Legend, Line, Plot, PlotPoints, Points};
use navdb_core::export::{self, ExportFormat, ExportOptions};
use navdb_core::{ColumnMeta, ObjectInfo, ObjectKind, Pending, QueryResult, Scope, Value};
use serde::{Deserialize, Serialize};

use super::Tab;
use crate::ui::grid::{self, GridOptions, GridState};
use crate::ui::sql_editor::SqlEditor;
use crate::ui::style::icon;
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ChartKind {
    Bar,
    HBar,
    Line,
    Area,
    Scatter,
    Pie,
    Kpi,
    Table,
}

impl ChartKind {
    const ALL: [ChartKind; 8] = [ChartKind::Bar, ChartKind::HBar, ChartKind::Line, ChartKind::Area, ChartKind::Scatter, ChartKind::Pie, ChartKind::Kpi, ChartKind::Table];

    fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "📊 Bar",
            ChartKind::HBar => "☰ Horizontal Bar",
            ChartKind::Line => "📈 Line",
            ChartKind::Area => "📈 Area",
            ChartKind::Scatter => "⭐ Scatter",
            ChartKind::Pie => "◔ Pie / Donut",
            ChartKind::Kpi => "🔢 KPI Value",
            ChartKind::Table => "⊞ Table",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Agg {
    None,
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

impl Agg {
    const ALL: [Agg; 6] = [Agg::None, Agg::Sum, Agg::Avg, Agg::Count, Agg::Min, Agg::Max];

    fn label(self) -> &'static str {
        match self {
            Agg::None => "None (raw rows)",
            Agg::Sum => "Sum",
            Agg::Avg => "Average",
            Agg::Count => "Count",
            Agg::Min => "Minimum",
            Agg::Max => "Maximum",
        }
    }

    fn short(self) -> &'static str {
        match self {
            Agg::None => "",
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Count => "count",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SortMode {
    None,
    XAsc,
    XDesc,
    ValueAsc,
    ValueDesc,
}

impl SortMode {
    const ALL: [SortMode; 5] = [SortMode::None, SortMode::XAsc, SortMode::XDesc, SortMode::ValueAsc, SortMode::ValueDesc];

    fn label(self) -> &'static str {
        match self {
            SortMode::None => "Query order",
            SortMode::XAsc => "Category ⬆",
            SortMode::XDesc => "Category ⬇",
            SortMode::ValueAsc => "Value ⬆",
            SortMode::ValueDesc => "Value ⬇",
        }
    }
}

const PALETTES: [(&str, [Color32; 8]); 5] = [
    (
        "Classic",
        [
            Color32::from_rgb(78, 121, 167),
            Color32::from_rgb(242, 142, 43),
            Color32::from_rgb(225, 87, 89),
            Color32::from_rgb(118, 183, 178),
            Color32::from_rgb(89, 161, 79),
            Color32::from_rgb(237, 201, 72),
            Color32::from_rgb(176, 122, 161),
            Color32::from_rgb(156, 117, 95),
        ],
    ),
    (
        "Vivid",
        [
            Color32::from_rgb(0, 122, 255),
            Color32::from_rgb(255, 59, 48),
            Color32::from_rgb(52, 199, 89),
            Color32::from_rgb(255, 149, 0),
            Color32::from_rgb(175, 82, 222),
            Color32::from_rgb(255, 204, 0),
            Color32::from_rgb(90, 200, 250),
            Color32::from_rgb(255, 45, 85),
        ],
    ),
    (
        "Pastel",
        [
            Color32::from_rgb(141, 182, 226),
            Color32::from_rgb(246, 178, 140),
            Color32::from_rgb(160, 214, 180),
            Color32::from_rgb(240, 160, 180),
            Color32::from_rgb(200, 180, 230),
            Color32::from_rgb(250, 220, 140),
            Color32::from_rgb(150, 210, 220),
            Color32::from_rgb(210, 190, 160),
        ],
    ),
    (
        "Ocean",
        [
            Color32::from_rgb(8, 64, 129),
            Color32::from_rgb(8, 104, 172),
            Color32::from_rgb(43, 140, 190),
            Color32::from_rgb(78, 179, 211),
            Color32::from_rgb(123, 204, 196),
            Color32::from_rgb(168, 221, 181),
            Color32::from_rgb(204, 235, 197),
            Color32::from_rgb(30, 90, 110),
        ],
    ),
    (
        "Warm",
        [
            Color32::from_rgb(158, 1, 66),
            Color32::from_rgb(213, 62, 79),
            Color32::from_rgb(244, 109, 67),
            Color32::from_rgb(253, 174, 97),
            Color32::from_rgb(254, 224, 139),
            Color32::from_rgb(230, 145, 56),
            Color32::from_rgb(190, 80, 40),
            Color32::from_rgb(120, 40, 30),
        ],
    ),
];

fn color(palette: usize, i: usize) -> Color32 {
    let p = &PALETTES[palette.min(PALETTES.len() - 1)].1;
    p[i % p.len()]
}

/// Serializable chart definition (one entry of a dashboard file).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChartConfig {
    title: String,
    connection: Option<String>,
    scope: Option<Scope>,
    sql: String,
    kind: ChartKind,
    x_col: Option<String>,
    y_cols: Vec<String>,
    agg: Agg,
    sort: SortMode,
    top_n: usize,
    legend: bool,
    palette: usize,
    donut: bool,
    #[serde(default)]
    show_values: bool,
}

impl Default for ChartConfig {
    fn default() -> Self {
        Self {
            title: "Chart".into(),
            connection: None,
            scope: None,
            sql: String::new(),
            kind: ChartKind::Bar,
            x_col: None,
            y_cols: vec![],
            agg: Agg::None,
            sort: SortMode::None,
            top_n: 0,
            legend: true,
            palette: 0,
            donut: false,
            show_values: false,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct DashboardFile {
    title: String,
    columns: usize,
    charts: Vec<ChartConfig>,
}

/// Aggregated, chart-ready data.
#[derive(Clone, Default)]
struct ChartData {
    x_name: String,
    cats: Vec<String>,
    /// x positions (numeric x or category index)
    xs: Vec<f64>,
    numeric_x: bool,
    series: Vec<(String, Vec<f64>)>,
}

impl ChartData {
    fn to_result(&self) -> QueryResult {
        let mut r = QueryResult { is_query: true, ..Default::default() };
        r.columns.push(ColumnMeta::new(self.x_name.clone()));
        for (n, _) in &self.series {
            r.columns.push(ColumnMeta::new(n.clone()));
        }
        for (i, c) in self.cats.iter().enumerate() {
            let mut row = vec![Value::Text(c.clone())];
            for (_, v) in &self.series {
                row.push(Value::Float(v[i]));
            }
            r.rows.push(row);
        }
        r
    }
}

fn is_numeric_col(r: &QueryResult, c: usize) -> bool {
    let mut seen = false;
    for row in r.rows.iter().take(50) {
        match row.get(c) {
            Some(v) if v.is_null() => {}
            Some(v) => {
                if v.as_f64().is_none() {
                    return false;
                }
                seen = true;
            }
            None => {}
        }
    }
    seen
}

fn cmp_text(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.cmp(b),
    }
}

fn compute(cfg: &ChartConfig, r: &QueryResult) -> ChartData {
    let xi = cfg.x_col.as_ref().and_then(|c| r.column_index(c));
    let yis: Vec<(String, usize)> = cfg.y_cols.iter().filter_map(|c| r.column_index(c).map(|i| (c.clone(), i))).collect();
    let mut d = ChartData { x_name: cfg.x_col.clone().unwrap_or_else(|| "#".into()), ..Default::default() };
    let cat_of = |row_i: usize, row: &Vec<Value>| -> String {
        match xi {
            Some(i) => row.get(i).map(|v| v.display()).unwrap_or_default(),
            None => (row_i + 1).to_string(),
        }
    };
    let names: Vec<String> = if yis.is_empty() {
        if cfg.agg == Agg::Count {
            vec!["count".into()]
        } else {
            vec![]
        }
    } else {
        yis.iter().map(|(n, _)| if cfg.agg == Agg::None { n.clone() } else { format!("{}({})", cfg.agg.short(), n) }).collect()
    };
    if names.is_empty() {
        return d;
    }
    let nser = names.len();
    let mut cats: Vec<String> = Vec::new();
    let mut vals: Vec<Vec<f64>> = vec![Vec::new(); nser];
    if cfg.agg == Agg::None {
        for (ri, row) in r.rows.iter().enumerate() {
            cats.push(cat_of(ri, row));
            for (s, (_, ci)) in yis.iter().enumerate() {
                vals[s].push(row.get(*ci).and_then(|v| v.as_f64()).unwrap_or(0.0));
            }
        }
    } else {
        // (sum, count, min, max) per group / series
        let mut idx: HashMap<String, usize> = HashMap::new();
        let mut acc: Vec<Vec<(f64, f64, f64, f64)>> = Vec::new();
        for (ri, row) in r.rows.iter().enumerate() {
            let c = cat_of(ri, row);
            let g = *idx.entry(c.clone()).or_insert_with(|| {
                cats.push(c);
                acc.push(vec![(0.0, 0.0, f64::INFINITY, f64::NEG_INFINITY); nser]);
                acc.len() - 1
            });
            if yis.is_empty() {
                acc[g][0].1 += 1.0;
            }
            for (s, (_, ci)) in yis.iter().enumerate() {
                let v = row.get(*ci);
                if cfg.agg == Agg::Count {
                    if v.is_some_and(|v| !v.is_null()) {
                        acc[g][s].1 += 1.0;
                    }
                    continue;
                }
                if let Some(x) = v.and_then(|v| v.as_f64()) {
                    let a = &mut acc[g][s];
                    a.0 += x;
                    a.1 += 1.0;
                    a.2 = a.2.min(x);
                    a.3 = a.3.max(x);
                }
            }
        }
        for group in &acc {
            for (s, a) in group.iter().enumerate() {
                let v = match cfg.agg {
                    Agg::Sum => a.0,
                    Agg::Avg => {
                        if a.1 > 0.0 {
                            a.0 / a.1
                        } else {
                            0.0
                        }
                    }
                    Agg::Count => a.1,
                    Agg::Min => {
                        if a.2.is_finite() {
                            a.2
                        } else {
                            0.0
                        }
                    }
                    Agg::Max => {
                        if a.3.is_finite() {
                            a.3
                        } else {
                            0.0
                        }
                    }
                    Agg::None => a.0,
                };
                vals[s].push(v);
            }
        }
    }
    // sort
    let mut order: Vec<usize> = (0..cats.len()).collect();
    match cfg.sort {
        SortMode::None => {}
        SortMode::XAsc => order.sort_by(|&a, &b| cmp_text(&cats[a], &cats[b])),
        SortMode::XDesc => order.sort_by(|&a, &b| cmp_text(&cats[b], &cats[a])),
        SortMode::ValueAsc => order.sort_by(|&a, &b| vals[0][a].partial_cmp(&vals[0][b]).unwrap_or(std::cmp::Ordering::Equal)),
        SortMode::ValueDesc => order.sort_by(|&a, &b| vals[0][b].partial_cmp(&vals[0][a]).unwrap_or(std::cmp::Ordering::Equal)),
    }
    if cfg.top_n > 0 {
        order.truncate(cfg.top_n);
    }
    d.cats = order.iter().map(|&i| cats[i].clone()).collect();
    d.series = names.into_iter().enumerate().map(|(s, n)| (n, order.iter().map(|&i| vals[s][i]).collect())).collect();
    let numeric: Option<Vec<f64>> = if matches!(cfg.kind, ChartKind::Line | ChartKind::Area | ChartKind::Scatter) && xi.is_some() {
        d.cats.iter().map(|c| c.trim().parse::<f64>().ok()).collect()
    } else {
        None
    };
    match numeric {
        Some(xs) if !xs.is_empty() => {
            d.xs = xs;
            d.numeric_x = true;
        }
        _ => d.xs = (0..d.cats.len()).map(|i| i as f64).collect(),
    }
    d
}

fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "–".into();
    }
    if (v - v.round()).abs() < 1e-9 && v.abs() < 1e15 {
        let s = format!("{}", v.round() as i64);
        let (sign, digits) = if let Some(d) = s.strip_prefix('-') { ("-", d.to_string()) } else { ("", s) };
        let mut out = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        format!("{}{}", sign, out)
    } else if v.abs() >= 1000.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.2}", v)
    }
}

/// One chart of the dashboard with its own query.
struct ChartPanel {
    uid: String,
    cfg: ChartConfig,
    editor: SqlEditor,
    result: Option<QueryResult>,
    running: Option<(Pending<QueryResult>, std::time::Instant)>,
    prefill: Option<Pending<Vec<ObjectInfo>>>,
    error: Option<String>,
    raw_grid: GridState,
    data_grid: GridState,
    /// designer pane: false = chart, true = data preview
    show_data: bool,
    /// set defaults for axes after the first result
    auto_axes: bool,
}

impl ChartPanel {
    fn new(cfg: ChartConfig) -> Self {
        let editor = SqlEditor::new(cfg.sql.clone());
        let auto_axes = cfg.x_col.is_none() && cfg.y_cols.is_empty();
        Self {
            uid: uuid::Uuid::new_v4().to_string(),
            cfg,
            editor,
            result: None,
            running: None,
            prefill: None,
            error: None,
            raw_grid: GridState::default(),
            data_grid: GridState::default(),
            show_data: false,
            auto_axes,
        }
    }

    fn run(&mut self, st: &mut AppState) {
        self.cfg.sql = self.editor.text.clone();
        let Some(conn) = self.cfg.connection.clone() else {
            self.error = Some("Select a connection first".into());
            return;
        };
        if self.cfg.sql.trim().is_empty() {
            self.error = Some("Enter a SQL query".into());
            return;
        }
        let Some(session) = st.session(&conn) else {
            self.error = Some("connection not found".into());
            return;
        };
        let scope = self.cfg.scope.clone();
        let sql = self.cfg.sql.clone();
        self.error = None;
        self.running = Some((session.run(move |d| d.query(scope.as_ref(), &sql)), std::time::Instant::now()));
    }

    fn poll(&mut self, st: &mut AppState) {
        if let Some(p) = &mut self.prefill {
            if let Some(r) = p.poll() {
                self.prefill = None;
                if let Ok(list) = r {
                    if let Some(first) = list.first() {
                        self.prefill_with(st, &first.name.clone());
                    }
                }
            }
        }
        let done = self.running.as_mut().and_then(|(p, _)| p.poll());
        if let Some(r) = done {
            let started = self.running.take().map(|(_, s)| s).unwrap_or_else(std::time::Instant::now);
            let conn = self.cfg.connection.clone().unwrap_or_default();
            match r {
                Ok(res) => {
                    st.record(&conn, self.cfg.scope.as_ref(), &self.cfg.sql, true, &format!("{} rows", res.rows.len()), started.elapsed().as_millis());
                    self.raw_grid.reset();
                    self.data_grid.reset();
                    // drop configured columns that no longer exist
                    if let Some(x) = &self.cfg.x_col {
                        if res.column_index(x).is_none() {
                            self.cfg.x_col = None;
                            self.auto_axes = true;
                        }
                    }
                    self.cfg.y_cols.retain(|c| res.column_index(c).is_some());
                    if self.cfg.y_cols.is_empty() {
                        self.auto_axes = true;
                    }
                    if self.auto_axes {
                        self.auto_axes = false;
                        self.pick_axes(&res);
                    }
                    self.result = Some(res);
                    self.error = None;
                }
                Err(e) => {
                    let msg = format!("{:#}", e);
                    st.record(&conn, self.cfg.scope.as_ref(), &self.cfg.sql, false, &msg, started.elapsed().as_millis());
                    self.error = Some(msg);
                }
            }
        }
    }

    fn pick_axes(&mut self, r: &QueryResult) {
        let numeric: Vec<bool> = (0..r.columns.len()).map(|c| is_numeric_col(r, c)).collect();
        let x = (0..r.columns.len()).find(|&c| !numeric[c]).or(if r.columns.len() > 1 { Some(0) } else { None });
        self.cfg.x_col = x.map(|c| r.columns[c].name.clone());
        let y = (0..r.columns.len()).find(|&c| numeric[c] && Some(c) != x);
        self.cfg.y_cols = y.map(|c| vec![r.columns[c].name.clone()]).unwrap_or_default();
        if self.cfg.y_cols.is_empty() {
            self.cfg.agg = Agg::Count;
        } else if x.is_some_and(|c| !numeric[c]) {
            // text categories usually repeat: aggregate them
            let cats: std::collections::HashSet<String> = r.rows.iter().map(|row| row[x.unwrap_or(0)].display()).collect();
            if cats.len() < r.rows.len() {
                self.cfg.agg = Agg::Sum;
            }
        }
    }

    fn prefill_with(&mut self, st: &mut AppState, table: &str) {
        let (Some(conn), Some(scope)) = (self.cfg.connection.clone(), self.cfg.scope.clone()) else { return };
        let d = st.config(&conn).map(|c| c.dialect()).unwrap_or(navdb_core::Dialect::MySql);
        let sql = format!("SELECT * FROM {}{}", d.qualify(&scope.path(table)), d.limit_clause(100, 0));
        self.editor.text = sql.clone();
        self.cfg.sql = sql;
        self.cfg.title = table.to_string();
        self.run(st);
    }
}

pub struct ChartsTab {
    id: String,
    title: String,
    charts: Vec<ChartPanel>,
    active: usize,
    dashboard: bool,
    columns: usize,
    card_height: f32,
}

impl ChartsTab {
    pub fn new(st: &mut AppState, source: Option<(String, Scope, String)>) -> Self {
        let mut cfg = ChartConfig { title: "Chart 1".into(), ..Default::default() };
        let mut first_table: Option<String> = None;
        let mut need_list = false;
        if let Some((conn, scope, sql)) = &source {
            cfg.connection = Some(conn.clone());
            cfg.scope = if scope.db.is_empty() { None } else { Some(scope.clone()) };
            cfg.sql = sql.clone();
            if sql.trim().is_empty() && cfg.scope.is_some() {
                first_table = st.cached_tables(conn, scope).into_iter().next();
                need_list = first_table.is_none();
            }
        }
        let mut panel = ChartPanel::new(cfg);
        if let Some((conn, scope, sql)) = source {
            if !sql.trim().is_empty() {
                panel.run(st);
            } else if let Some(t) = first_table {
                panel.prefill_with(st, &t);
            } else if need_list {
                if let Some(session) = st.session(&conn) {
                    panel.prefill = Some(session.run(move |d| d.list_objects(&scope, ObjectKind::Table)));
                }
            }
        }
        Self { id: uuid::Uuid::new_v4().to_string(), title: "Dashboard".into(), charts: vec![panel], active: 0, dashboard: false, columns: 2, card_height: 280.0 }
    }

    fn add_chart(&mut self, st: &mut AppState, duplicate: bool) {
        let mut cfg = match self.charts.get(self.active) {
            Some(p) if duplicate => {
                let mut c = p.cfg.clone();
                c.sql = p.editor.text.clone();
                c
            }
            Some(p) => ChartConfig { connection: p.cfg.connection.clone(), scope: p.cfg.scope.clone(), sql: p.editor.text.clone(), ..Default::default() },
            None => ChartConfig::default(),
        };
        cfg.title = if duplicate { format!("{} copy", cfg.title) } else { format!("Chart {}", self.charts.len() + 1) };
        let mut p = ChartPanel::new(cfg);
        if !p.cfg.sql.trim().is_empty() {
            p.run(st);
        }
        self.charts.push(p);
        self.active = self.charts.len() - 1;
    }

    fn save_dashboard(&mut self, st: &mut AppState) {
        for p in &mut self.charts {
            p.cfg.sql = p.editor.text.clone();
        }
        let f = DashboardFile { title: self.title.clone(), columns: self.columns, charts: self.charts.iter().map(|p| p.cfg.clone()).collect() };
        let Some(path) = crate::ui::save_file_dialog(&format!("{}.dashboard.json", self.title), "json") else { return };
        match serde_json::to_string_pretty(&f).map_err(|e| e.to_string()).and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string())) {
            Ok(()) => st.toast(format!("Dashboard saved to {}", path.display())),
            Err(e) => st.error(format!("Save failed: {}", e)),
        }
    }

    fn load_dashboard(&mut self, st: &mut AppState) {
        let Some(path) = crate::ui::open_file_dialog(&["json"]) else { return };
        let res = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str::<DashboardFile>(&s).map_err(|e| e.to_string()));
        match res {
            Ok(f) => {
                self.title = f.title;
                self.columns = f.columns.clamp(1, 4);
                self.charts = f.charts.into_iter().map(ChartPanel::new).collect();
                for p in &mut self.charts {
                    p.auto_axes = false;
                    p.run(st);
                }
                if self.charts.is_empty() {
                    self.charts.push(ChartPanel::new(ChartConfig { title: "Chart 1".into(), ..Default::default() }));
                }
                self.active = 0;
                self.dashboard = true;
                st.toast("Dashboard loaded");
            }
            Err(e) => st.error(format!("Load failed: {}", e)),
        }
    }

    fn export_csv(&self, st: &mut AppState, raw: bool) {
        let Some(p) = self.charts.get(self.active) else { return };
        let Some(r) = &p.result else {
            st.error("Run the query first");
            return;
        };
        let data = if raw { r.clone() } else { compute(&p.cfg, r).to_result() };
        let Some(path) = crate::ui::save_file_dialog(&format!("{}.csv", p.cfg.title), "csv") else { return };
        match export::export_to_file(&data, ExportFormat::Csv, &ExportOptions::default(), &path) {
            Ok(()) => st.toast(format!("Exported {} rows", data.rows.len())),
            Err(e) => st.error(format!("Export failed: {:#}", e)),
        }
    }

    fn top_bar(&mut self, ui: &mut Ui, st: &mut AppState) {
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.dashboard, "⊞ Dashboard").clicked() {
                self.dashboard = true;
            }
            ui.separator();
            let mut pick = None;
            for (i, p) in self.charts.iter().enumerate() {
                let busy = if p.running.is_some() { " ⏳" } else { "" };
                if ui.selectable_label(!self.dashboard && self.active == i, format!("{}{}", p.cfg.title, busy)).clicked() {
                    pick = Some(i);
                }
            }
            if let Some(i) = pick {
                self.active = i;
                self.dashboard = false;
            }
            if ui.button(format!("{} New Chart", icon::ADD)).clicked() {
                self.add_chart(st, false);
                self.dashboard = false;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(format!("{} Export", icon::EXPORT), |ui| {
                    if ui.button("Chart data as CSV...").clicked() {
                        ui.close_menu();
                        self.export_csv(st, false);
                    }
                    if ui.button("Raw query result as CSV...").clicked() {
                        ui.close_menu();
                        self.export_csv(st, true);
                    }
                });
                if ui.button(format!("{} Open", icon::IMPORT)).on_hover_text("Load dashboard (.json)").clicked() {
                    self.load_dashboard(st);
                }
                if ui.button(format!("{} Save", icon::SAVE)).on_hover_text("Save dashboard (.json)").clicked() {
                    self.save_dashboard(st);
                }
            });
        });
    }

    fn dashboard_ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        ui.horizontal(|ui| {
            ui.label("Title:");
            ui.add(egui::TextEdit::singleline(&mut self.title).desired_width(200.0));
            ui.separator();
            ui.label("Columns:");
            for n in 1..=4 {
                ui.selectable_value(&mut self.columns, n, n.to_string());
            }
            ui.separator();
            ui.label("Height:");
            ui.add(egui::Slider::new(&mut self.card_height, 180.0..=600.0).show_value(false));
            ui.separator();
            if ui.button(format!("{} Refresh All", icon::REFRESH)).clicked() {
                for p in &mut self.charts {
                    p.run(st);
                }
            }
        });
        ui.separator();
        let mut edit = None;
        let mut remove = None;
        let cols = self.columns.max(1);
        let h = self.card_height;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let n = self.charts.len();
            let mut start = 0;
            while start < n {
                let end = (start + cols).min(n);
                ui.columns(cols, |uis| {
                    for (k, cu) in uis.iter_mut().enumerate() {
                        let i = start + k;
                        if i >= end {
                            continue;
                        }
                        let p = &mut self.charts[i];
                        egui::Frame::group(cu.style()).corner_radius(6.0).inner_margin(8.0).show(cu, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&p.cfg.title).strong().size(14.0));
                                if p.running.is_some() {
                                    ui.spinner();
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button(icon::DELETE).on_hover_text("Remove from dashboard").clicked() {
                                        remove = Some(i);
                                    }
                                    if ui.small_button(icon::DESIGN).on_hover_text("Edit chart").clicked() {
                                        edit = Some(i);
                                    }
                                    if ui.small_button(icon::REFRESH).on_hover_text("Re-run query").clicked() {
                                        p.run(st);
                                    }
                                });
                            });
                            let inner_h = h - 40.0;
                            match (&p.result, &p.error) {
                                (_, Some(e)) => {
                                    ui.allocate_ui(vec2(ui.available_width(), inner_h), |ui| crate::ui::error_text(ui, e));
                                }
                                (Some(r), None) => {
                                    let data = compute(&p.cfg, r);
                                    render_chart(ui, &format!("dash-{}", p.uid), &p.cfg, &data, r, &mut p.data_grid, inner_h, st);
                                }
                                (None, None) => {
                                    ui.allocate_ui(vec2(ui.available_width(), inner_h), |ui| {
                                        ui.label(RichText::new("No data yet").weak());
                                    });
                                }
                            }
                        });
                    }
                });
                ui.add_space(8.0);
                start = end;
            }
            if n == 0 {
                ui.label(RichText::new("The dashboard is empty. Use \"New Chart\" to add one.").weak());
            }
        });
        if let Some(i) = edit {
            self.active = i;
            self.dashboard = false;
        }
        if let Some(i) = remove {
            self.charts.remove(i);
            if self.active >= self.charts.len() {
                self.active = self.charts.len().saturating_sub(1);
            }
        }
    }

    fn designer_ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        let tab_id = self.id.clone();
        let n_charts = self.charts.len();
        let Some(p) = self.charts.get_mut(self.active) else {
            ui.label("No chart selected");
            return;
        };
        let mut delete = false;
        let mut duplicate = false;
        // ---- designer side panel
        egui::SidePanel::left(egui::Id::new(("chart-designer", &tab_id)))
            .resizable(true)
            .default_width(250.0)
            .width_range(210.0..=420.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.heading("Chart Designer");
                    ui.add_space(4.0);
                    ui.label(RichText::new("Title").weak());
                    ui.add(egui::TextEdit::singleline(&mut p.cfg.title).desired_width(f32::INFINITY));
                    ui.add_space(4.0);
                    ui.label(RichText::new("Chart type").weak());
                    egui::Grid::new(("chart-kinds", &p.uid)).num_columns(2).spacing([4.0, 4.0]).show(ui, |ui| {
                        for (k, kind) in ChartKind::ALL.iter().enumerate() {
                            ui.selectable_value(&mut p.cfg.kind, *kind, kind.label());
                            if k % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
                    ui.separator();
                    let cols: Vec<String> = p.result.as_ref().map(|r| r.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
                    let numeric: Vec<bool> = p.result.as_ref().map(|r| (0..r.columns.len()).map(|c| is_numeric_col(r, c)).collect()).unwrap_or_default();
                    if cols.is_empty() {
                        ui.label(RichText::new("Run the query to choose columns.").weak());
                    } else {
                        let x_label = if matches!(p.cfg.kind, ChartKind::Pie) { "Category (slices)" } else if p.cfg.kind == ChartKind::HBar { "Category (Y axis)" } else { "X axis / category" };
                        ui.label(RichText::new(x_label).weak());
                        egui::ComboBox::from_id_salt(("chart-x", &p.uid))
                            .selected_text(p.cfg.x_col.clone().unwrap_or_else(|| "(row number)".into()))
                            .width(ui.available_width() - 8.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut p.cfg.x_col, None, "(row number)");
                                for c in &cols {
                                    ui.selectable_value(&mut p.cfg.x_col, Some(c.clone()), c);
                                }
                            });
                        ui.add_space(4.0);
                        ui.label(RichText::new("Values (Y)").weak());
                        for (i, c) in cols.iter().enumerate() {
                            let mut on = p.cfg.y_cols.contains(c);
                            let label = if numeric.get(i).copied().unwrap_or(false) { RichText::new(format!("# {}", c)) } else { RichText::new(format!("Aa {}", c)).weak() };
                            if ui.checkbox(&mut on, label).changed() {
                                if on {
                                    p.cfg.y_cols.push(c.clone());
                                } else {
                                    p.cfg.y_cols.retain(|x| x != c);
                                }
                            }
                        }
                    }
                    ui.separator();
                    egui::Grid::new(("chart-opts", &p.uid)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.label("Aggregate");
                        egui::ComboBox::from_id_salt(("chart-agg", &p.uid)).selected_text(p.cfg.agg.label()).show_ui(ui, |ui| {
                            for a in Agg::ALL {
                                ui.selectable_value(&mut p.cfg.agg, a, a.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Sort");
                        egui::ComboBox::from_id_salt(("chart-sort", &p.uid)).selected_text(p.cfg.sort.label()).show_ui(ui, |ui| {
                            for s in SortMode::ALL {
                                ui.selectable_value(&mut p.cfg.sort, s, s.label());
                            }
                        });
                        ui.end_row();
                        ui.label("Top N");
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut p.cfg.top_n).range(0..=10_000));
                            ui.label(RichText::new(if p.cfg.top_n == 0 { "all" } else { "" }).weak());
                        });
                        ui.end_row();
                        ui.label("Palette");
                        egui::ComboBox::from_id_salt(("chart-pal", &p.uid)).selected_text(PALETTES[p.cfg.palette.min(PALETTES.len() - 1)].0).show_ui(ui, |ui| {
                            for (i, (name, cs)) in PALETTES.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    for c in cs.iter().take(6) {
                                        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                                        ui.painter().rect_filled(r, 2.0, *c);
                                    }
                                    ui.selectable_value(&mut p.cfg.palette, i, *name);
                                });
                            }
                        });
                        ui.end_row();
                    });
                    ui.checkbox(&mut p.cfg.legend, "Show legend");
                    ui.checkbox(&mut p.cfg.show_values, "Show value labels");
                    if p.cfg.kind == ChartKind::Pie {
                        ui.checkbox(&mut p.cfg.donut, "Donut");
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("📋 Duplicate").clicked() {
                            duplicate = true;
                        }
                        if ui.add_enabled(n_charts > 1, egui::Button::new(format!("{} Remove", icon::DELETE))).clicked() {
                            delete = true;
                        }
                    });
                });
            });

        // ---- query editor
        let mut run = false;
        egui::TopBottomPanel::top(egui::Id::new(("chart-sql", &tab_id, &p.uid)))
            .resizable(true)
            .default_height(150.0)
            .height_range(70.0..=(ui.available_height() - 120.0).max(80.0))
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let (mut c, mut s) = (p.cfg.connection.clone(), p.cfg.scope.clone());
                    crate::ui::scope_picker(ui, &format!("chart-scope-{}", p.uid), st, &mut c, &mut s);
                    p.cfg.connection = c;
                    p.cfg.scope = s;
                    let busy = p.running.is_some();
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new(format!("{} Run", icon::RUN)).strong())).on_hover_text("⌘R / Ctrl+Enter").clicked() {
                        run = true;
                    }
                    if busy {
                        ui.spinner();
                    }
                    if p.prefill.is_some() {
                        crate::ui::loading(ui, "Finding a table...");
                    }
                    if let Some(r) = &p.result {
                        ui.label(RichText::new(format!("{} rows · {} ms", r.rows.len(), r.elapsed_ms)).weak());
                    }
                });
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::R) || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
                    run = true;
                }
                p.editor.ui(ui, &format!("chart-editor-{}", p.uid), &[]);
            });
        if run && p.running.is_none() {
            p.run(st);
        }

        // ---- chart / data
        ui.horizontal(|ui| {
            ui.selectable_value(&mut p.show_data, false, "📊 Chart");
            ui.selectable_value(&mut p.show_data, true, "⊞ Query Result");
        });
        ui.separator();
        if let Some(e) = &p.error {
            crate::ui::error_text(ui, e);
        }
        match &p.result {
            None => {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Write a query and press Run to preview the chart.").weak());
                });
            }
            Some(r) if p.show_data => {
                let null_text = st.prefs().show_null_as.clone();
                let opt = GridOptions { null_text: &null_text, font_size: st.prefs().grid_font_size, ..Default::default() };
                let _ = grid::data_grid(ui, &format!("chart-raw-{}", p.uid), r, &mut p.raw_grid, &opt);
            }
            Some(r) => {
                let data = compute(&p.cfg, r);
                ui.label(RichText::new(&p.cfg.title).strong().size(16.0));
                let h = ui.available_height() - 4.0;
                render_chart(ui, &format!("designer-{}", p.uid), &p.cfg, &data, r, &mut p.data_grid, h.max(120.0), st);
            }
        }
        if duplicate {
            self.add_chart(st, true);
        }
        if delete && self.charts.len() > 1 {
            self.charts.remove(self.active);
            self.active = self.active.min(self.charts.len() - 1);
        }
    }
}

// ---------------------------------------------------------------------- rendering

#[allow(clippy::too_many_arguments)]
fn render_chart(ui: &mut Ui, id: &str, cfg: &ChartConfig, data: &ChartData, raw: &QueryResult, grid_state: &mut GridState, height: f32, st: &AppState) {
    if cfg.kind != ChartKind::Kpi && data.series.is_empty() {
        ui.allocate_ui(vec2(ui.available_width(), height), |ui| {
            ui.label(RichText::new("Select at least one value column (or use Count aggregation).").weak());
        });
        return;
    }
    match cfg.kind {
        ChartKind::Bar | ChartKind::HBar => bar_chart(ui, id, cfg, data, height),
        ChartKind::Line | ChartKind::Area | ChartKind::Scatter => xy_chart(ui, id, cfg, data, height),
        ChartKind::Pie => pie_chart(ui, cfg, data, height),
        ChartKind::Kpi => kpi(ui, cfg, raw, height),
        ChartKind::Table => {
            let r = data.to_result();
            ui.allocate_ui(vec2(ui.available_width(), height), |ui| {
                let opt = GridOptions { null_text: &st.prefs().show_null_as, font_size: st.prefs().grid_font_size, ..Default::default() };
                let _ = grid::data_grid(ui, &format!("{}-table", id), &r, grid_state, &opt);
            });
        }
    }
}

fn category_formatter(cats: Vec<String>) -> impl Fn(egui_plot::GridMark, &std::ops::RangeInclusive<f64>) -> String {
    move |m, _| {
        let v = m.value;
        if (v - v.round()).abs() < 1e-6 && v >= -0.5 {
            let i = v.round() as usize;
            cats.get(i).map(|c| if c.chars().count() > 18 { format!("{}…", c.chars().take(17).collect::<String>()) } else { c.clone() }).unwrap_or_default()
        } else {
            String::new()
        }
    }
}

fn base_plot<'a>(id: &str, cfg: &ChartConfig, height: f32) -> Plot<'a> {
    let mut p = Plot::new(id.to_string()).height(height).allow_scroll(false).allow_boxed_zoom(true).y_axis_min_width(40.0);
    if cfg.legend {
        p = p.legend(Legend::default().position(Corner::RightTop).background_alpha(0.75));
    }
    p
}

fn bar_chart(ui: &mut Ui, id: &str, cfg: &ChartConfig, data: &ChartData, height: f32) {
    let horizontal = cfg.kind == ChartKind::HBar;
    let m = data.series.len();
    let w = 0.8 / m as f64;
    let mut plot = base_plot(id, cfg, height).y_axis_formatter(|m, _| fmt_num(m.value));
    let spacer = egui_plot::uniform_grid_spacer(|_| [1.0, 5.0, 25.0]);
    if horizontal {
        plot = base_plot(id, cfg, height).y_axis_formatter(category_formatter(data.cats.clone())).x_axis_formatter(|m, _| fmt_num(m.value)).y_grid_spacer(spacer).y_axis_min_width(90.0);
    } else {
        plot = plot.x_axis_formatter(category_formatter(data.cats.clone())).x_grid_spacer(spacer);
    }
    plot.show(ui, |pu| {
        for (s, (name, vals)) in data.series.iter().enumerate() {
            let c = color(cfg.palette, s);
            let off = (s as f64 - (m as f64 - 1.0) / 2.0) * w;
            let bars: Vec<Bar> = vals
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let b = Bar::new(i as f64 + off, *v).width(w * 0.92).fill(if m == 1 && cfg.palette > 0 { color(cfg.palette, i) } else { c }).name(&data.cats[i]);
                    if horizontal {
                        b.horizontal()
                    } else {
                        b
                    }
                })
                .collect();
            let mut chart = BarChart::new(bars).color(c).name(name);
            if horizontal {
                chart = chart.horizontal();
            }
            pu.bar_chart(chart);
            if cfg.show_values && vals.len() <= 60 {
                for (i, v) in vals.iter().enumerate() {
                    let pos = if horizontal { egui_plot::PlotPoint::new(*v, i as f64 + off) } else { egui_plot::PlotPoint::new(i as f64 + off, *v) };
                    let anchor = if horizontal { Align2::LEFT_CENTER } else { Align2::CENTER_BOTTOM };
                    pu.text(egui_plot::Text::new(pos, RichText::new(fmt_num(*v)).small()).anchor(anchor));
                }
            }
        }
    });
}

fn xy_chart(ui: &mut Ui, id: &str, cfg: &ChartConfig, data: &ChartData, height: f32) {
    let mut plot = base_plot(id, cfg, height).y_axis_formatter(|m, _| fmt_num(m.value));
    if !data.numeric_x {
        plot = plot.x_axis_formatter(category_formatter(data.cats.clone()));
    }
    plot.show(ui, |pu| {
        for (s, (name, vals)) in data.series.iter().enumerate() {
            let c = color(cfg.palette, s);
            let mut pts: Vec<[f64; 2]> = data.xs.iter().zip(vals).map(|(x, y)| [*x, *y]).collect();
            if data.numeric_x && cfg.kind != ChartKind::Scatter {
                pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
            }
            match cfg.kind {
                ChartKind::Scatter => {
                    pu.points(Points::new(PlotPoints::from(pts.clone())).radius(3.5_f32).color(c).name(name));
                }
                ChartKind::Area => {
                    pu.line(Line::new(PlotPoints::from(pts.clone())).color(c).width(2.0_f32).fill(0.0_f32).fill_alpha(0.25_f32).name(name));
                }
                _ => {
                    pu.line(Line::new(PlotPoints::from(pts.clone())).color(c).width(2.0_f32).name(name));
                    if pts.len() <= 80 {
                        pu.points(Points::new(PlotPoints::from(pts.clone())).radius(2.5_f32).color(c).name(name));
                    }
                }
            }
            if cfg.show_values && pts.len() <= 60 {
                for p in &pts {
                    pu.text(egui_plot::Text::new(egui_plot::PlotPoint::new(p[0], p[1]), RichText::new(fmt_num(p[1])).small()).anchor(Align2::CENTER_BOTTOM));
                }
            }
        }
    });
}

fn pie_chart(ui: &mut Ui, cfg: &ChartConfig, data: &ChartData, height: f32) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    let vals: Vec<f64> = data.series[0].1.iter().map(|v| v.max(0.0)).collect();
    let total: f64 = vals.iter().sum();
    let vis = ui.visuals().clone();
    if total <= 0.0 {
        painter.text(rect.center(), Align2::CENTER_CENTER, "No positive values to display", FontId::proportional(13.0), vis.weak_text_color());
        return;
    }
    let legend_w = if cfg.legend { (rect.width() * 0.38).min(240.0) } else { 0.0 };
    let pie_rect = egui::Rect::from_min_max(rect.min, pos2(rect.max.x - legend_w, rect.max.y));
    let center = pie_rect.center();
    let r = (pie_rect.width().min(pie_rect.height()) / 2.0 - 12.0).max(10.0);
    let inner = if cfg.donut { r * 0.58 } else { 0.0 };
    let tau = std::f32::consts::TAU;
    let start = -std::f32::consts::FRAC_PI_2;
    // hovered slice
    let mut hovered = None;
    if let Some(p) = resp.hover_pos() {
        let d = p - center;
        let dist = d.length();
        if dist <= r + 6.0 && dist >= inner {
            let mut a = d.y.atan2(d.x) - start;
            while a < 0.0 {
                a += tau;
            }
            let mut acc = 0.0f32;
            for (i, v) in vals.iter().enumerate() {
                let sweep = (*v / total) as f32 * tau;
                if a >= acc && a < acc + sweep {
                    hovered = Some(i);
                    break;
                }
                acc += sweep;
            }
        }
    }
    let mut acc = start;
    let mut mesh = egui::Mesh::default();
    let mut labels = Vec::new();
    for (i, v) in vals.iter().enumerate() {
        let sweep = (*v / total) as f32 * tau;
        if sweep <= 0.0 {
            continue;
        }
        let c = color(cfg.palette, i);
        let rr = if hovered == Some(i) { r + 6.0 } else { r };
        let steps = ((sweep / 0.04).ceil() as usize).max(2);
        for k in 0..steps {
            let a0 = acc + sweep * k as f32 / steps as f32;
            let a1 = acc + sweep * (k + 1) as f32 / steps as f32;
            let o0 = center + vec2(a0.cos(), a0.sin()) * rr;
            let o1 = center + vec2(a1.cos(), a1.sin()) * rr;
            let base = mesh.vertices.len() as u32;
            if inner <= 0.0 {
                mesh.colored_vertex(center, c);
                mesh.colored_vertex(o0, c);
                mesh.colored_vertex(o1, c);
                mesh.add_triangle(base, base + 1, base + 2);
            } else {
                let i0 = center + vec2(a0.cos(), a0.sin()) * inner;
                let i1 = center + vec2(a1.cos(), a1.sin()) * inner;
                mesh.colored_vertex(i0, c);
                mesh.colored_vertex(o0, c);
                mesh.colored_vertex(o1, c);
                mesh.colored_vertex(i1, c);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            }
        }
        // slice separators
        let sep_a = acc;
        labels.push((i, acc + sweep / 2.0, sep_a, (*v / total) as f32));
        acc += sweep;
    }
    painter.add(egui::Shape::mesh(mesh));
    let bg = vis.extreme_bg_color;
    for (_, _, a, _) in &labels {
        if vals.len() > 1 {
            painter.line_segment([center + vec2(a.cos(), a.sin()) * inner, center + vec2(a.cos(), a.sin()) * (r + 6.0)], Stroke::new(1.5_f32, bg));
        }
    }
    for (i, mid, _, share) in &labels {
        if *share >= 0.04 && r > 50.0 {
            let lr = if inner > 0.0 { (r + inner) / 2.0 } else { r * 0.65 };
            let p = center + vec2(mid.cos(), mid.sin()) * lr;
            let text = if cfg.show_values { fmt_num(vals[*i]) } else { format!("{:.0}%", share * 100.0) };
            painter.text(p, Align2::CENTER_CENTER, text, FontId::proportional(11.0), Color32::WHITE);
        }
    }
    if cfg.donut {
        painter.text(center - vec2(0.0, 8.0), Align2::CENTER_CENTER, fmt_num(total), FontId::proportional((r * 0.22).clamp(12.0, 28.0)), vis.strong_text_color());
        painter.text(center + vec2(0.0, (r * 0.16).clamp(10.0, 18.0)), Align2::CENTER_CENTER, &data.series[0].0, FontId::proportional(11.0), vis.weak_text_color());
    }
    // legend
    if cfg.legend {
        let mut y = rect.min.y + 8.0;
        let x = rect.max.x - legend_w + 8.0;
        let max_rows = ((rect.height() - 16.0) / 18.0).floor() as usize;
        for (i, c) in data.cats.iter().enumerate() {
            if i >= max_rows.saturating_sub(1) && data.cats.len() > max_rows {
                painter.text(pos2(x, y + 7.0), Align2::LEFT_CENTER, format!("… {} more", data.cats.len() - i), FontId::proportional(11.0), vis.weak_text_color());
                break;
            }
            let sw = egui::Rect::from_min_size(pos2(x, y + 2.0), vec2(10.0, 10.0));
            painter.rect_filled(sw, 2.0, color(cfg.palette, i));
            let pct = vals[i] / total * 100.0;
            let label = if c.chars().count() > 22 { format!("{}…", c.chars().take(21).collect::<String>()) } else { c.clone() };
            let col = if hovered == Some(i) { vis.strong_text_color() } else { vis.text_color() };
            painter.text(pos2(x + 16.0, y + 7.0), Align2::LEFT_CENTER, format!("{}  {:.1}%", label, pct), FontId::proportional(11.5), col);
            y += 18.0;
        }
    }
    if let Some(i) = hovered {
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(RichText::new(&data.cats[i]).strong());
            ui.label(format!("{}: {} ({:.1}%)", data.series[0].0, fmt_num(vals[i]), vals[i] / total * 100.0));
        });
    }
}

fn kpi(ui: &mut Ui, cfg: &ChartConfig, raw: &QueryResult, height: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    let vis = ui.visuals().clone();
    let cols: Vec<(String, Option<usize>)> = if cfg.y_cols.is_empty() { vec![("rows".into(), None)] } else { cfg.y_cols.iter().map(|c| (c.clone(), raw.column_index(c))).collect() };
    let n = cols.len().max(1);
    let w = rect.width() / n as f32;
    for (k, (name, ci)) in cols.iter().enumerate() {
        let vals: Vec<f64> = match ci {
            Some(i) => raw.rows.iter().filter_map(|r| r.get(*i).and_then(|v| v.as_f64())).collect(),
            None => vec![],
        };
        let value = match (cfg.agg, ci) {
            (_, None) => raw.rows.len() as f64,
            (Agg::None, _) => vals.first().copied().unwrap_or(f64::NAN),
            (Agg::Sum, _) => vals.iter().sum(),
            (Agg::Avg, _) => {
                if vals.is_empty() {
                    f64::NAN
                } else {
                    vals.iter().sum::<f64>() / vals.len() as f64
                }
            }
            (Agg::Count, Some(i)) => raw.rows.iter().filter(|r| r.get(*i).is_some_and(|v| !v.is_null())).count() as f64,
            (Agg::Min, _) => vals.iter().copied().fold(f64::NAN, f64::min),
            (Agg::Max, _) => vals.iter().copied().fold(f64::NAN, f64::max),
        };
        let cell = egui::Rect::from_min_size(pos2(rect.min.x + w * k as f32, rect.min.y), vec2(w, rect.height())).shrink(6.0);
        let c = color(cfg.palette, k);
        painter.rect_filled(cell, 8.0, c.gamma_multiply(0.12));
        painter.rect_filled(egui::Rect::from_min_size(cell.min, vec2(4.0, cell.height())), 2.0, c);
        let big = (cell.height() * 0.28).clamp(18.0, 56.0);
        painter.text(cell.center() - vec2(0.0, big * 0.25), Align2::CENTER_CENTER, fmt_num(value), FontId::proportional(big), vis.strong_text_color());
        let label = if cfg.agg == Agg::None || ci.is_none() { name.clone() } else { format!("{} of {}", cfg.agg.label(), name) };
        painter.text(cell.center() + vec2(0.0, big * 0.55), Align2::CENTER_CENTER, label, FontId::proportional(13.0), vis.weak_text_color());
    }
}

impl Tab for ChartsTab {
    fn title(&self) -> String {
        if self.charts.len() == 1 {
            format!("{} {}", icon::CHARTS, self.charts[0].cfg.title)
        } else {
            format!("{} {}", icon::CHARTS, self.title)
        }
    }

    fn key(&self) -> String {
        format!("charts:{}", self.id)
    }

    fn tick(&mut self, st: &mut AppState) {
        for p in &mut self.charts {
            p.poll(st);
        }
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.tick(st);
        if self.active >= self.charts.len() && !self.charts.is_empty() {
            self.active = self.charts.len() - 1;
        }
        self.top_bar(ui, st);
        ui.separator();
        if self.dashboard {
            self.dashboard_ui(ui, st);
        } else {
            self.designer_ui(ui, st);
        }
    }
}
