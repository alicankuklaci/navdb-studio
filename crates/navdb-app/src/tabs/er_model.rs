//! Model tab: reverse-engineered ER diagram of a database / schema.

use std::collections::{HashMap, HashSet};

use egui::{pos2, vec2, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use navdb_core::{ddl, Dialect, ObjectKind, Scope, TableDef};
use serde::{Deserialize, Serialize};

use super::Tab;
use crate::state::{AppState, Loadable};
use crate::ui::style::icon;

const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 18.0;
const PAD_X: f32 = 10.0;
const FONT: f32 = 12.0;
const MIN_W: f32 = 140.0;
const GAP: f32 = 70.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Notation {
    CrowsFoot,
    Arrow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutKind {
    Grid,
    Layered,
    Force,
}

struct Node {
    pos: Pos2,
    size: Vec2,
}

/// A relationship between a FK column row and a referenced column row.
#[derive(Clone)]
struct Edge {
    from: usize,
    from_row: usize,
    to: usize,
    to_row: usize,
    nullable: bool,
    name: String,
}

/// Saved model layout (`*.ndm.json`).
#[derive(Serialize, Deserialize, Default)]
struct LayoutFile {
    connection: String,
    scope: String,
    zoom: f32,
    pan: [f32; 2],
    show_types: bool,
    notation: Option<Notation>,
    positions: HashMap<String, [f32; 2]>,
}

/// World-space geometry of an edge.
struct EdgeGeom {
    path: Vec<Pos2>,
    /// marker strokes (line segments)
    segs: Vec<[Pos2; 2]>,
    /// marker circles (center, radius)
    circles: Vec<(Pos2, f32)>,
    /// filled arrow head triangle
    arrow: Option<[Pos2; 3]>,
}

pub struct ErModelTab {
    id: String,
    conn: String,
    scope: Scope,
    dialect: Dialect,
    load: Loadable<Vec<TableDef>>,
    tables: Vec<TableDef>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    /// draw order (last = top)
    order: Vec<usize>,
    pan: Vec2,
    zoom: f32,
    selected: Option<usize>,
    dragging: Option<usize>,
    search: String,
    show_types: bool,
    notation: Notation,
    layout: LayoutKind,
    fit_request: bool,
    focus_request: Option<usize>,
    canvas: Rect,
    sizes_dirty: bool,
    show_panel: bool,
    pending_layout: bool,
    /// tables added by a reload that need a position
    unplaced: Vec<usize>,
}

impl ErModelTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let mut t = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            scope,
            dialect,
            load: Loadable::NotLoaded,
            tables: vec![],
            nodes: vec![],
            edges: vec![],
            order: vec![],
            pan: Vec2::ZERO,
            zoom: 1.0,
            selected: None,
            dragging: None,
            search: String::new(),
            show_types: true,
            notation: Notation::CrowsFoot,
            layout: LayoutKind::Layered,
            fit_request: false,
            focus_request: None,
            canvas: Rect::NOTHING,
            sizes_dirty: true,
            show_panel: true,
            pending_layout: false,
            unplaced: vec![],
        };
        t.reload(st);
        t
    }

    /// "Reverse database to model": list tables and load every definition on the worker.
    fn reload(&mut self, st: &mut AppState) {
        let Some(session) = st.session(&self.conn) else {
            self.load = Loadable::Failed("connection not found".into());
            return;
        };
        let scope = self.scope.clone();
        self.load = Loadable::Loading(session.run(move |d| {
            let objs = d.list_objects(&scope, ObjectKind::Table)?;
            let mut defs = Vec::with_capacity(objs.len());
            for o in objs {
                if let Ok(def) = d.table_def(&scope.path(o.name.clone())) {
                    defs.push(def);
                }
            }
            Ok(defs)
        }));
    }

    fn on_loaded(&mut self, defs: Vec<TableDef>) {
        // keep positions of tables that survive a reload
        let old: HashMap<String, Pos2> = self.tables.iter().zip(&self.nodes).map(|(t, n)| (t.path.name.clone(), n.pos)).collect();
        let had_layout = !old.is_empty();
        self.tables = defs;
        self.nodes = self.tables.iter().map(|t| Node { pos: old.get(&t.path.name).copied().unwrap_or(Pos2::ZERO), size: vec2(MIN_W, HEADER_H) }).collect();
        self.unplaced = self.tables.iter().enumerate().filter(|(_, t)| !old.contains_key(&t.path.name)).map(|(i, _)| i).collect();
        self.order = (0..self.tables.len()).collect();
        self.selected = None;
        self.build_edges();
        self.sizes_dirty = true;
        if !had_layout {
            self.pending_layout = true;
        }
    }

    fn build_edges(&mut self) {
        let idx: HashMap<&str, usize> = self.tables.iter().enumerate().map(|(i, t)| (t.path.name.as_str(), i)).collect();
        let mut edges = Vec::new();
        for (i, t) in self.tables.iter().enumerate() {
            for fk in &t.foreign_keys {
                let Some(&j) = idx.get(fk.ref_table.as_str()) else { continue };
                let target = &self.tables[j];
                for (k, col) in fk.columns.iter().enumerate() {
                    let from_row = t.columns.iter().position(|c| &c.name == col).unwrap_or(0);
                    let ref_col = fk.ref_columns.get(k).cloned().or_else(|| target.primary_key().first().cloned()).unwrap_or_default();
                    let to_row = target.columns.iter().position(|c| c.name == ref_col).unwrap_or(0);
                    let nullable = t.column(col).map(|c| c.nullable).unwrap_or(true);
                    edges.push(Edge { from: i, from_row, to: j, to_row, nullable, name: fk.name.clone() });
                    // composite keys: draw one line per FK
                    break;
                }
            }
        }
        self.edges = edges;
    }

    fn compute_sizes(&mut self, ui: &Ui) {
        let font = FontId::proportional(FONT);
        let bold = FontId::proportional(FONT + 1.0);
        let show_types = self.show_types;
        ui.fonts(|f| {
            for (t, n) in self.tables.iter().zip(self.nodes.iter_mut()) {
                let mut w = f.layout_no_wrap(t.path.name.clone(), bold.clone(), Color32::WHITE).size().x + 40.0;
                for c in &t.columns {
                    let mut cw = f.layout_no_wrap(c.name.clone(), font.clone(), Color32::WHITE).size().x + 34.0;
                    if show_types {
                        cw += f.layout_no_wrap(c.data_type.clone(), font.clone(), Color32::WHITE).size().x + 18.0;
                    }
                    w = w.max(cw);
                }
                n.size = vec2((w + PAD_X * 2.0).max(MIN_W), HEADER_H + ROW_H * t.columns.len().max(1) as f32 + 6.0);
            }
        });
        self.sizes_dirty = false;
    }

    // ------------------------------------------------------------------ layouts

    fn apply_layout(&mut self, kind: LayoutKind) {
        self.layout = kind;
        match kind {
            LayoutKind::Grid => self.layout_grid(),
            LayoutKind::Layered => self.layout_layered(),
            LayoutKind::Force => self.layout_force(),
        }
        self.fit_request = true;
    }

    fn layout_grid(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }
        let mut ids: Vec<usize> = (0..n).collect();
        ids.sort_by(|&a, &b| self.tables[a].path.name.cmp(&self.tables[b].path.name));
        let cols = (n as f32).sqrt().ceil() as usize;
        let mut col_w = vec![0.0f32; cols];
        for (k, &i) in ids.iter().enumerate() {
            col_w[k % cols] = col_w[k % cols].max(self.nodes[i].size.x);
        }
        let mut y = 0.0;
        for row in ids.chunks(cols) {
            let mut x = 0.0;
            let mut h: f32 = 0.0;
            for (c, &i) in row.iter().enumerate() {
                self.nodes[i].pos = pos2(x, y);
                x += col_w[c] + GAP;
                h = h.max(self.nodes[i].size.y);
            }
            y += h + GAP * 0.7;
        }
    }

    /// Parents (referenced tables) on the left, children to the right.
    fn layout_layered(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }
        let mut level = vec![0usize; n];
        for _ in 0..n.min(64) {
            let mut changed = false;
            for e in &self.edges {
                if e.from != e.to && level[e.from] < level[e.to] + 1 && level[e.to] + 1 < n {
                    level[e.from] = level[e.to] + 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // isolated tables (no relations) go to a separate grid at the bottom
        let mut connected = vec![false; n];
        for e in &self.edges {
            connected[e.from] = true;
            connected[e.to] = true;
        }
        let max_level = level.iter().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<usize>> = vec![vec![]; max_level + 1];
        for i in 0..n {
            if connected[i] {
                layers[level[i]].push(i);
            }
        }
        // order: names in first layer, barycenter of parents afterwards
        let mut rank = vec![0.0f32; n];
        for (li, layer) in layers.iter_mut().enumerate() {
            if li == 0 {
                layer.sort_by(|&a, &b| self.tables[a].path.name.cmp(&self.tables[b].path.name));
            } else {
                let bary = |i: usize| {
                    let ps: Vec<f32> = self.edges.iter().filter(|e| e.from == i && e.to != i).map(|e| rank[e.to]).collect();
                    if ps.is_empty() {
                        f32::MAX
                    } else {
                        ps.iter().sum::<f32>() / ps.len() as f32
                    }
                };
                let mut keyed: Vec<(f32, usize)> = layer.iter().map(|&i| (bary(i), i)).collect();
                keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                *layer = keyed.into_iter().map(|(_, i)| i).collect();
            }
            for (k, &i) in layer.iter().enumerate() {
                rank[i] = k as f32;
            }
        }
        let mut x = 0.0;
        let mut bottom: f32 = 0.0;
        for layer in &layers {
            if layer.is_empty() {
                continue;
            }
            let w = layer.iter().map(|&i| self.nodes[i].size.x).fold(0.0, f32::max);
            let mut y = 0.0;
            for &i in layer {
                self.nodes[i].pos = pos2(x, y);
                y += self.nodes[i].size.y + GAP * 0.5;
            }
            bottom = bottom.max(y);
            x += w + GAP * 1.6;
        }
        let iso: Vec<usize> = {
            let mut v: Vec<usize> = (0..n).filter(|&i| !connected[i]).collect();
            v.sort_by(|&a, &b| self.tables[a].path.name.cmp(&self.tables[b].path.name));
            v
        };
        if !iso.is_empty() {
            let width = x.max(1200.0);
            let mut cx = 0.0;
            let mut cy = if bottom > 0.0 { bottom + GAP } else { 0.0 };
            let mut row_h: f32 = 0.0;
            for i in iso {
                if cx > 0.0 && cx + self.nodes[i].size.x > width {
                    cx = 0.0;
                    cy += row_h + GAP * 0.5;
                    row_h = 0.0;
                }
                self.nodes[i].pos = pos2(cx, cy);
                cx += self.nodes[i].size.x + GAP * 0.6;
                row_h = row_h.max(self.nodes[i].size.y);
            }
        }
    }

    /// Simple Fruchterman–Reingold on box centers followed by overlap removal.
    fn layout_force(&mut self) {
        let n = self.nodes.len();
        if n == 0 {
            return;
        }
        if n == 1 {
            self.nodes[0].pos = Pos2::ZERO;
            return;
        }
        let avg = self.nodes.iter().map(|nd| nd.size.length()).sum::<f32>() / n as f32;
        let k = avg * 0.9;
        let radius = k * (n as f32).sqrt();
        let mut c: Vec<Vec2> = (0..n)
            .map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                vec2(a.cos(), a.sin()) * radius * 0.5
            })
            .collect();
        let mut temp = radius * 0.3;
        for _ in 0..300 {
            let mut disp = vec![Vec2::ZERO; n];
            for i in 0..n {
                for j in (i + 1)..n {
                    let mut d = c[i] - c[j];
                    let dist = d.length().max(1.0);
                    if dist < 1.5 {
                        d = vec2(1.0, 0.5);
                    }
                    let f = k * k / dist;
                    let u = d / dist * f;
                    disp[i] += u;
                    disp[j] -= u;
                }
            }
            for e in &self.edges {
                if e.from == e.to {
                    continue;
                }
                let d = c[e.from] - c[e.to];
                let dist = d.length().max(1.0);
                let f = dist * dist / k;
                let u = d / dist * f;
                disp[e.from] -= u;
                disp[e.to] += u;
            }
            for i in 0..n {
                // mild gravity keeps disconnected tables close
                disp[i] -= c[i] * 0.02;
                let l = disp[i].length();
                if l > 0.0 {
                    c[i] += disp[i] / l * l.min(temp);
                }
            }
            temp *= 0.985;
        }
        for i in 0..n {
            self.nodes[i].pos = (c[i] - self.nodes[i].size / 2.0).to_pos2();
        }
        // remove overlaps
        for _ in 0..80 {
            let mut moved = false;
            for i in 0..n {
                for j in (i + 1)..n {
                    let a = Rect::from_min_size(self.nodes[i].pos, self.nodes[i].size).expand(GAP * 0.25);
                    let b = Rect::from_min_size(self.nodes[j].pos, self.nodes[j].size).expand(GAP * 0.25);
                    if a.intersects(b) {
                        let ox = (a.max.x.min(b.max.x) - a.min.x.max(b.min.x)).max(0.0);
                        let oy = (a.max.y.min(b.max.y) - a.min.y.max(b.min.y)).max(0.0);
                        let dir = b.center() - a.center();
                        if ox < oy {
                            let s = if dir.x >= 0.0 { 1.0 } else { -1.0 };
                            self.nodes[i].pos.x -= s * ox / 2.0;
                            self.nodes[j].pos.x += s * ox / 2.0;
                        } else {
                            let s = if dir.y >= 0.0 { 1.0 } else { -1.0 };
                            self.nodes[i].pos.y -= s * oy / 2.0;
                            self.nodes[j].pos.y += s * oy / 2.0;
                        }
                        moved = true;
                    }
                }
            }
            if !moved {
                break;
            }
        }
    }

    fn bounds(&self) -> Option<Rect> {
        let mut r: Option<Rect> = None;
        for n in &self.nodes {
            let b = Rect::from_min_size(n.pos, n.size);
            r = Some(match r {
                Some(x) => x.union(b),
                None => b,
            });
        }
        r
    }

    fn fit(&mut self) {
        let Some(b) = self.bounds() else { return };
        if !self.canvas.is_positive() {
            return;
        }
        let b = b.expand(30.0);
        let z = (self.canvas.width() / b.width()).min(self.canvas.height() / b.height()).clamp(0.1, 1.5);
        self.zoom = z;
        self.pan = self.canvas.size() / 2.0 - b.center().to_vec2() * z;
    }

    fn focus(&mut self, i: usize) {
        let n = &self.nodes[i];
        let c = n.pos + n.size / 2.0;
        if self.zoom < 0.6 {
            self.zoom = 0.9;
        }
        self.pan = self.canvas.size() / 2.0 - c.to_vec2() * self.zoom;
        self.selected = Some(i);
        self.raise(i);
    }

    fn raise(&mut self, i: usize) {
        self.order.retain(|&x| x != i);
        self.order.push(i);
    }

    fn to_screen(&self, p: Pos2) -> Pos2 {
        self.canvas.min + self.pan + p.to_vec2() * self.zoom
    }

    fn to_world(&self, p: Pos2) -> Pos2 {
        ((p - self.canvas.min - self.pan) / self.zoom).to_pos2()
    }

    fn hit(&self, world: Pos2) -> Option<usize> {
        self.order.iter().rev().copied().find(|&i| Rect::from_min_size(self.nodes[i].pos, self.nodes[i].size).contains(world))
    }

    fn matches(&self, i: usize) -> bool {
        !self.search.is_empty() && self.tables[i].path.name.to_lowercase().contains(&self.search.to_lowercase())
    }

    fn row_y(&self, i: usize, row: usize) -> f32 {
        self.nodes[i].pos.y + HEADER_H + 3.0 + ROW_H * (row as f32 + 0.5)
    }

    fn edge_geom(&self, e: &Edge) -> EdgeGeom {
        let a = Rect::from_min_size(self.nodes[e.from].pos, self.nodes[e.from].size);
        let b = Rect::from_min_size(self.nodes[e.to].pos, self.nodes[e.to].size);
        let y1 = self.row_y(e.from, e.from_row);
        let y2 = self.row_y(e.to, e.to_row);
        let stub = 18.0;
        // side selection: +1 = right side, -1 = left side
        let (s1, s2, mid_x) = if e.from == e.to {
            (1.0, 1.0, a.max.x + 40.0)
        } else if b.min.x > a.max.x + stub * 2.0 {
            (1.0, -1.0, (a.max.x + b.min.x) / 2.0)
        } else if a.min.x > b.max.x + stub * 2.0 {
            (-1.0, 1.0, (b.max.x + a.min.x) / 2.0)
        } else if (a.center().x) <= b.center().x {
            (1.0, 1.0, a.max.x.max(b.max.x) + 40.0)
        } else {
            (-1.0, -1.0, a.min.x.min(b.min.x) - 40.0)
        };
        let p1 = pos2(if s1 > 0.0 { a.max.x } else { a.min.x }, y1);
        let p2 = pos2(if s2 > 0.0 { b.max.x } else { b.min.x }, y2);
        let path = vec![p1, pos2(mid_x, y1), pos2(mid_x, y2), p2];
        let mut g = EdgeGeom { path, segs: vec![], circles: vec![], arrow: None };
        let d1 = s1; // direction pointing away from box a
        let d2 = s2;
        match self.notation {
            Notation::CrowsFoot => {
                // many side (FK table): crow's foot + optional circle
                let tip = p1;
                let base = pos2(p1.x + d1 * 12.0, y1);
                g.segs.push([base, pos2(tip.x, y1 - 6.0)]);
                g.segs.push([base, pos2(tip.x, y1 + 6.0)]);
                g.segs.push([base, tip]);
                g.circles.push((pos2(p1.x + d1 * 17.0, y1), 3.5));
                // one side (referenced table): || or o|
                let bar1 = p2.x + d2 * 6.0;
                g.segs.push([pos2(bar1, y2 - 6.0), pos2(bar1, y2 + 6.0)]);
                if e.nullable {
                    g.circles.push((pos2(p2.x + d2 * 13.0, y2), 3.5));
                } else {
                    let bar2 = p2.x + d2 * 11.0;
                    g.segs.push([pos2(bar2, y2 - 6.0), pos2(bar2, y2 + 6.0)]);
                }
            }
            Notation::Arrow => {
                let tip = p2;
                let back = pos2(p2.x + d2 * 10.0, y2);
                g.arrow = Some([tip, pos2(back.x, y2 - 5.0), pos2(back.x, y2 + 5.0)]);
                g.circles.push((p1, 2.5));
            }
        }
        g
    }

    // ------------------------------------------------------------------ exports

    fn export_sql(&self, st: &mut AppState) {
        let mut out = format!("-- Model of {} ({})\n-- Generated by NavDB Studio\n\n", self.scope.display(), st.conn_name(&self.conn));
        // referenced tables first
        let mut done = HashSet::new();
        let mut ordered = Vec::new();
        fn visit(i: usize, t: &[TableDef], idx: &HashMap<&str, usize>, done: &mut HashSet<usize>, out: &mut Vec<usize>, depth: usize) {
            if done.contains(&i) || depth > 64 {
                return;
            }
            done.insert(i);
            for fk in &t[i].foreign_keys {
                if let Some(&j) = idx.get(fk.ref_table.as_str()) {
                    visit(j, t, idx, done, out, depth + 1);
                }
            }
            out.push(i);
        }
        let idx: HashMap<&str, usize> = self.tables.iter().enumerate().map(|(i, t)| (t.path.name.as_str(), i)).collect();
        for i in 0..self.tables.len() {
            visit(i, &self.tables, &idx, &mut done, &mut ordered, 0);
        }
        for i in ordered {
            out.push_str(&ddl::script(&ddl::create_table(self.dialect, &self.tables[i])));
            out.push_str("\n\n");
        }
        let Some(path) = crate::ui::save_file_dialog(&format!("{}.sql", self.scope.db), "sql") else { return };
        match std::fs::write(&path, out) {
            Ok(()) => st.toast(format!("Exported {}", path.display())),
            Err(e) => st.error(format!("Export failed: {}", e)),
        }
    }

    fn svg(&self) -> String {
        let b = self.bounds().unwrap_or(Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0))).expand(40.0);
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
        let mut s = String::new();
        s.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.0}\" height=\"{:.0}\" viewBox=\"{:.1} {:.1} {:.1} {:.1}\" font-family=\"Helvetica, Arial, sans-serif\" font-size=\"{}\">\n",
            b.width(),
            b.height(),
            b.min.x,
            b.min.y,
            b.width(),
            b.height(),
            FONT
        ));
        s.push_str(&format!("<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#ffffff\"/>\n", b.min.x, b.min.y, b.width(), b.height()));
        s.push_str("<g stroke=\"#6b7a90\" stroke-width=\"1.3\" fill=\"none\">\n");
        for e in &self.edges {
            let g = self.edge_geom(e);
            let pts: Vec<String> = g.path.iter().map(|p| format!("{:.1},{:.1}", p.x, p.y)).collect();
            s.push_str(&format!("<polyline points=\"{}\"/>\n", pts.join(" ")));
            for [a, c] in &g.segs {
                s.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\"/>\n", a.x, a.y, c.x, c.y));
            }
            for (c, r) in &g.circles {
                s.push_str(&format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\" fill=\"#ffffff\"/>\n", c.x, c.y, r));
            }
            if let Some([p, q, r]) = g.arrow {
                s.push_str(&format!("<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"#6b7a90\"/>\n", p.x, p.y, q.x, q.y, r.x, r.y));
            }
        }
        s.push_str("</g>\n");
        for (i, t) in self.tables.iter().enumerate() {
            let n = &self.nodes[i];
            let (x, y, w, h) = (n.pos.x, n.pos.y, n.size.x, n.size.y);
            s.push_str("<g>\n");
            s.push_str(&format!("<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"5\" fill=\"#ffffff\" stroke=\"#4a6fa5\" stroke-width=\"1.2\"/>\n", x, y, w, h));
            s.push_str(&format!("<path d=\"M{:.1},{:.1} h{:.1} v{:.1} h{:.1} Z\" fill=\"#4a6fa5\"/>\n", x, y + 5.0, w, HEADER_H - 5.0, -w));
            s.push_str(&format!("<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"5\" fill=\"#4a6fa5\"/>\n", x, y, w, HEADER_H - 4.0));
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#ffffff\" font-weight=\"bold\">{}</text>\n", x + PAD_X, y + HEADER_H / 2.0 + 4.5, esc(&t.path.name)));
            let pk = t.primary_key();
            let fks: HashSet<&String> = t.foreign_keys.iter().flat_map(|f| f.columns.iter()).collect();
            for (r, c) in t.columns.iter().enumerate() {
                let ry = y + HEADER_H + 3.0 + ROW_H * (r as f32 + 0.5) + 4.0;
                let mark = if pk.contains(&c.name) { "PK" } else if fks.contains(&c.name) { "FK" } else { "" };
                if !mark.is_empty() {
                    let col = if mark == "PK" { "#c99a06" } else { "#2f7d5b" };
                    s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{}\" font-size=\"9\" font-weight=\"bold\">{}</text>\n", x + 6.0, ry, col, mark));
                }
                let weight = if pk.contains(&c.name) { " font-weight=\"bold\"" } else { "" };
                s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#1f2933\"{}>{}</text>\n", x + 26.0, ry, weight, esc(&c.name)));
                if self.show_types {
                    s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#7b8794\" text-anchor=\"end\">{}</text>\n", x + w - PAD_X, ry, esc(&c.data_type)));
                }
            }
            s.push_str("</g>\n");
        }
        s.push_str("</svg>\n");
        s
    }

    fn export_svg(&self, st: &mut AppState) {
        let Some(path) = crate::ui::save_file_dialog(&format!("{}.svg", self.scope.db), "svg") else { return };
        match std::fs::write(&path, self.svg()) {
            Ok(()) => st.toast(format!("Exported {}", path.display())),
            Err(e) => st.error(format!("Export failed: {}", e)),
        }
    }

    fn save_layout(&self, st: &mut AppState) {
        let f = LayoutFile {
            connection: st.conn_name(&self.conn),
            scope: self.scope.display(),
            zoom: self.zoom,
            pan: [self.pan.x, self.pan.y],
            show_types: self.show_types,
            notation: Some(self.notation),
            positions: self.tables.iter().zip(&self.nodes).map(|(t, n)| (t.path.name.clone(), [n.pos.x, n.pos.y])).collect(),
        };
        let Some(path) = crate::ui::save_file_dialog(&format!("{}.ndm.json", self.scope.db), "json") else { return };
        let res = serde_json::to_string_pretty(&f).map_err(anyhow_like).and_then(|s| std::fs::write(&path, s).map_err(anyhow_like));
        match res {
            Ok(()) => st.toast("Model layout saved"),
            Err(e) => st.error(format!("Save failed: {}", e)),
        }
    }

    fn load_layout(&mut self, st: &mut AppState) {
        let Some(path) = crate::ui::open_file_dialog(&["json"]) else { return };
        let res = std::fs::read_to_string(&path).map_err(anyhow_like).and_then(|s| serde_json::from_str::<LayoutFile>(&s).map_err(anyhow_like));
        match res {
            Ok(f) => {
                let mut n = 0;
                for (t, node) in self.tables.iter().zip(self.nodes.iter_mut()) {
                    if let Some(p) = f.positions.get(&t.path.name) {
                        node.pos = pos2(p[0], p[1]);
                        n += 1;
                    }
                }
                if f.zoom > 0.0 {
                    self.zoom = f.zoom.clamp(0.1, 3.0);
                    self.pan = vec2(f.pan[0], f.pan[1]);
                }
                if self.show_types != f.show_types {
                    self.show_types = f.show_types;
                    self.sizes_dirty = true;
                }
                if let Some(nt) = f.notation {
                    self.notation = nt;
                }
                st.toast(format!("Layout loaded ({} tables positioned)", n));
            }
            Err(e) => st.error(format!("Load failed: {}", e)),
        }
    }

    // ------------------------------------------------------------------ UI

    fn toolbar(&mut self, ui: &mut Ui, st: &mut AppState) {
        ui.horizontal_wrapped(|ui| {
            if ui.button(format!("{} Reverse", icon::REFRESH)).on_hover_text("Reload all table definitions (reverse database to model)").clicked() {
                self.reload(st);
            }
            ui.separator();
            ui.label("Layout:");
            if ui.selectable_label(self.layout == LayoutKind::Layered, "⇶ Layered").on_hover_text("Arrange by foreign key dependencies").clicked() {
                self.apply_layout(LayoutKind::Layered);
            }
            if ui.selectable_label(self.layout == LayoutKind::Force, "✳ Force").on_hover_text("Force-directed layout").clicked() {
                self.apply_layout(LayoutKind::Force);
            }
            if ui.selectable_label(self.layout == LayoutKind::Grid, "▦ Grid").clicked() {
                self.apply_layout(LayoutKind::Grid);
            }
            ui.separator();
            if ui.button("➖").on_hover_text("Zoom out").clicked() {
                self.zoom_at(self.canvas.center(), 1.0 / 1.2);
            }
            if ui.button(format!("{:.0}%", self.zoom * 100.0)).on_hover_text("Reset zoom to 100%").clicked() {
                self.zoom_at(self.canvas.center(), 1.0 / self.zoom);
            }
            if ui.button("➕").on_hover_text("Zoom in").clicked() {
                self.zoom_at(self.canvas.center(), 1.2);
            }
            if ui.button("⛶ Fit").on_hover_text("Fit diagram to screen").clicked() {
                self.fit_request = true;
            }
            ui.separator();
            if ui.checkbox(&mut self.show_types, "Types").changed() {
                self.sizes_dirty = true;
            }
            egui::ComboBox::from_id_salt(("er-notation", &self.id))
                .selected_text(match self.notation {
                    Notation::CrowsFoot => "Crow's foot",
                    Notation::Arrow => "Simple arrows",
                })
                .width(110.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.notation, Notation::CrowsFoot, "Crow's foot");
                    ui.selectable_value(&mut self.notation, Notation::Arrow, "Simple arrows");
                });
            ui.separator();
            let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("🔍 Find table").desired_width(140.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(i) = (0..self.tables.len()).find(|&i| self.matches(i)) {
                    self.focus_request = Some(i);
                }
            }
            if !self.search.is_empty() {
                let n = (0..self.tables.len()).filter(|&i| self.matches(i)).count();
                ui.label(RichText::new(format!("{} match", n)).weak());
            }
            ui.separator();
            ui.menu_button(format!("{} Export", icon::EXPORT), |ui| {
                if ui.button("Export SQL (CREATE TABLE)...").clicked() {
                    ui.close_menu();
                    self.export_sql(st);
                }
                if ui.button("Export SVG image...").clicked() {
                    ui.close_menu();
                    self.export_svg(st);
                }
            });
            if ui.button(format!("{} Save Layout", icon::SAVE)).clicked() {
                self.save_layout(st);
            }
            if ui.button(format!("{} Load Layout", icon::IMPORT)).clicked() {
                self.load_layout(st);
            }
            ui.toggle_value(&mut self.show_panel, "☰ Details");
        });
    }

    fn zoom_at(&mut self, screen: Pos2, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(0.1, 3.0);
        let world = self.to_world(screen);
        self.zoom = new_zoom;
        self.pan = screen - self.canvas.min - world.to_vec2() * self.zoom;
    }

    fn side_panel(&mut self, ui: &mut Ui, st: &mut AppState) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let Some(i) = self.selected.filter(|&i| i < self.tables.len()) else {
                ui.heading(format!("{} {}", icon::MODEL, self.scope.display()));
                ui.label(RichText::new(format!("{} tables · {} relationships", self.tables.len(), self.edges.len())).weak());
                ui.add_space(6.0);
                ui.label(RichText::new("Click a table to see its details. Drag tables to move them, drag the background to pan, scroll to pan, ⌘/Ctrl + scroll to zoom.").weak().small());
                ui.separator();
                let mut focus = None;
                for (k, t) in self.tables.iter().enumerate() {
                    if ui.selectable_label(false, format!("{} {}", icon::TABLE, t.path.name)).clicked() {
                        focus = Some(k);
                    }
                }
                if let Some(k) = focus {
                    self.focus_request = Some(k);
                }
                return;
            };
            let t = self.tables[i].clone();
            ui.horizontal(|ui| {
                ui.heading(format!("{} {}", icon::TABLE, t.path.name));
            });
            if !t.comment.is_empty() {
                ui.label(RichText::new(&t.comment).italics().weak());
            }
            ui.horizontal(|ui| {
                if ui.button(format!("{} Open Table", icon::TABLE)).clicked() {
                    crate::commands::open_object(st, &self.conn, &self.scope, ObjectKind::Table, &t.path.name);
                }
                if ui.button(format!("{} Design Table", icon::DESIGN)).clicked() {
                    crate::commands::design_object(st, &self.conn, &self.scope, ObjectKind::Table, Some(t.path.name.clone()));
                }
            });
            ui.add_space(4.0);
            let pk = t.primary_key();
            let fk_cols: HashSet<&String> = t.foreign_keys.iter().flat_map(|f| f.columns.iter()).collect();
            egui::CollapsingHeader::new(format!("Columns ({})", t.columns.len())).default_open(true).show(ui, |ui| {
                egui::Grid::new(("er-cols", &self.id)).striped(true).num_columns(4).spacing([8.0, 3.0]).show(ui, |ui| {
                    ui.label(RichText::new("").weak());
                    ui.label(RichText::new("Name").weak());
                    ui.label(RichText::new("Type").weak());
                    ui.label(RichText::new("Null").weak());
                    ui.end_row();
                    for c in &t.columns {
                        let m = if pk.contains(&c.name) { icon::KEY } else if fk_cols.contains(&c.name) { "🔗" } else { "" };
                        ui.label(m);
                        let name = RichText::new(&c.name);
                        ui.label(if pk.contains(&c.name) { name.strong() } else { name });
                        ui.label(RichText::new(&c.data_type).monospace().small());
                        ui.label(if c.nullable { "✔" } else { "" });
                        ui.end_row();
                    }
                });
            });
            egui::CollapsingHeader::new(format!("Indexes ({})", t.indexes.len())).default_open(true).show(ui, |ui| {
                if t.indexes.is_empty() {
                    ui.label(RichText::new("No indexes").weak());
                }
                for ix in &t.indexes {
                    let kind = if ix.primary {
                        "PRIMARY"
                    } else if ix.unique {
                        "UNIQUE"
                    } else {
                        "INDEX"
                    };
                    ui.label(format!("{} {} ({})", kind, ix.name, ix.columns.join(", ")));
                }
            });
            egui::CollapsingHeader::new(format!("Foreign Keys ({})", t.foreign_keys.len())).default_open(true).show(ui, |ui| {
                if t.foreign_keys.is_empty() {
                    ui.label(RichText::new("No foreign keys").weak());
                }
                let mut go = None;
                for fk in &t.foreign_keys {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&fk.name).strong());
                        ui.label(format!("({}) →", fk.columns.join(", ")));
                        if ui.link(&fk.ref_table).clicked() {
                            go = self.tables.iter().position(|x| x.path.name == fk.ref_table);
                        }
                        ui.label(format!("({})", fk.ref_columns.join(", ")));
                    });
                    let mut extra = vec![];
                    if !fk.on_delete.is_empty() {
                        extra.push(format!("ON DELETE {}", fk.on_delete));
                    }
                    if !fk.on_update.is_empty() {
                        extra.push(format!("ON UPDATE {}", fk.on_update));
                    }
                    if !extra.is_empty() {
                        ui.label(RichText::new(extra.join("  ")).weak().small());
                    }
                }
                if let Some(j) = go {
                    self.focus_request = Some(j);
                }
            });
            let referenced_by: Vec<String> = self.edges.iter().filter(|e| e.to == i && e.from != i).map(|e| self.tables[e.from].path.name.clone()).collect::<HashSet<_>>().into_iter().collect();
            egui::CollapsingHeader::new(format!("Referenced by ({})", referenced_by.len())).default_open(true).show(ui, |ui| {
                let mut go = None;
                for n in &referenced_by {
                    if ui.link(n).clicked() {
                        go = self.tables.iter().position(|x| &x.path.name == n);
                    }
                }
                if let Some(j) = go {
                    self.focus_request = Some(j);
                }
            });
            ui.add_space(8.0);
            if ui.button("Deselect").clicked() {
                self.selected = None;
            }
        });
    }

    fn canvas_ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        self.canvas = rect;
        if self.sizes_dirty {
            self.compute_sizes(ui);
        }
        if !self.pending_layout && !self.unplaced.is_empty() {
            let placed: Vec<usize> = (0..self.nodes.len()).filter(|i| !self.unplaced.contains(i)).collect();
            let right = placed.iter().map(|&i| self.nodes[i].pos.x + self.nodes[i].size.x).fold(0.0, f32::max) + GAP;
            let mut y = 0.0;
            for &i in &self.unplaced {
                self.nodes[i].pos = pos2(right, y);
                y += self.nodes[i].size.y + GAP * 0.5;
            }
        }
        self.unplaced.clear();
        if self.pending_layout {
            self.pending_layout = false;
            let l = self.layout;
            self.apply_layout(l);
        }
        if self.fit_request {
            self.fit_request = false;
            self.fit();
        }
        if let Some(i) = self.focus_request.take() {
            if i < self.nodes.len() {
                self.focus(i);
            }
        }

        // ---- input
        let pointer = ui.input(|i| i.pointer.interact_pos());
        if resp.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
            self.dragging = origin.and_then(|p| self.hit(self.to_world(p)));
            if let Some(i) = self.dragging {
                self.selected = Some(i);
                self.raise(i);
            }
        }
        if resp.dragged() {
            let d = resp.drag_delta();
            match self.dragging {
                Some(i) => self.nodes[i].pos += d / self.zoom,
                None => self.pan += d,
            }
        }
        if resp.drag_stopped() {
            self.dragging = None;
        }
        if resp.clicked() {
            self.selected = pointer.and_then(|p| self.hit(self.to_world(p)));
            if let Some(i) = self.selected {
                self.raise(i);
            }
        }
        if resp.double_clicked() {
            if let Some(i) = pointer.and_then(|p| self.hit(self.to_world(p))) {
                let name = self.tables[i].path.name.clone();
                crate::commands::open_object(st, &self.conn, &self.scope, ObjectKind::Table, &name);
            }
        }
        if resp.hovered() {
            let (scroll, zoom_delta, hover) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
            if zoom_delta != 1.0 {
                self.zoom_at(hover.unwrap_or(rect.center()), zoom_delta);
            } else if scroll != Vec2::ZERO {
                self.pan += scroll;
            }
        }
        if resp.secondary_clicked() {
            self.selected = pointer.and_then(|p| self.hit(self.to_world(p)));
        }
        resp.context_menu(|ui| {
            if let Some(i) = self.selected {
                let name = self.tables[i].path.name.clone();
                ui.label(RichText::new(&name).strong());
                ui.separator();
                if ui.button(format!("{} Open Table", icon::TABLE)).clicked() {
                    ui.close_menu();
                    crate::commands::open_object(st, &self.conn, &self.scope, ObjectKind::Table, &name);
                }
                if ui.button(format!("{} Design Table", icon::DESIGN)).clicked() {
                    ui.close_menu();
                    crate::commands::design_object(st, &self.conn, &self.scope, ObjectKind::Table, Some(name.clone()));
                }
                if ui.button("Copy Name").clicked() {
                    ui.close_menu();
                    ui.ctx().copy_text(name.clone());
                }
                if ui.button("Copy CREATE Statement").clicked() {
                    ui.close_menu();
                    ui.ctx().copy_text(ddl::script(&ddl::create_table(self.dialect, &self.tables[i])));
                }
            } else {
                if ui.button("Fit to Screen").clicked() {
                    ui.close_menu();
                    self.fit_request = true;
                }
                if ui.button("Auto Layout").clicked() {
                    ui.close_menu();
                    let l = self.layout;
                    self.apply_layout(l);
                }
            }
        });

        // ---- drawing
        let painter = ui.painter_at(rect);
        let vis = ui.visuals().clone();
        let dark = vis.dark_mode;
        let bg = if dark { Color32::from_rgb(30, 32, 36) } else { Color32::from_rgb(246, 247, 249) };
        painter.rect_filled(rect, 0.0, bg);
        // dot grid
        let step = 24.0 * self.zoom;
        if step > 6.0 {
            let dot = if dark { Color32::from_gray(55) } else { Color32::from_gray(210) };
            let ox = (self.pan.x).rem_euclid(step);
            let oy = (self.pan.y).rem_euclid(step);
            let mut x = rect.min.x + ox;
            while x < rect.max.x {
                let mut y = rect.min.y + oy;
                while y < rect.max.y {
                    painter.circle_filled(pos2(x, y), 1.0, dot);
                    y += step;
                }
                x += step;
            }
        }
        if self.tables.is_empty() {
            let msg = match &self.load {
                Loadable::Loading(_) => "Reverse engineering database...".to_string(),
                Loadable::Failed(e) => format!("Failed: {}", e),
                _ => "No tables in this database.".to_string(),
            };
            painter.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(14.0), vis.weak_text_color());
            return;
        }

        let z = self.zoom;
        let accent = vis.selection.stroke.color;
        let line_col = if dark { Color32::from_rgb(130, 145, 165) } else { Color32::from_rgb(107, 122, 144) };
        // edges
        for e in &self.edges {
            let g = self.edge_geom(e);
            let hl = self.selected.is_some_and(|s| s == e.from || s == e.to);
            let col = if hl { accent } else { line_col };
            let stroke = Stroke::new(if hl { 2.0 } else { 1.3 } * z.max(0.5), col);
            let pts: Vec<Pos2> = g.path.iter().map(|p| self.to_screen(*p)).collect();
            painter.add(egui::Shape::line(pts, stroke));
            for [a, b] in &g.segs {
                painter.line_segment([self.to_screen(*a), self.to_screen(*b)], stroke);
            }
            for (c, r) in &g.circles {
                painter.circle(self.to_screen(*c), r * z, bg, stroke);
            }
            if let Some(tri) = g.arrow {
                painter.add(egui::Shape::convex_polygon(tri.iter().map(|p| self.to_screen(*p)).collect(), col, Stroke::NONE));
            }
            if hl && z > 0.6 && !e.name.is_empty() {
                let mid = self.to_screen(pos2(g.path[1].x, (g.path[1].y + g.path[2].y) / 2.0));
                painter.text(mid + vec2(4.0, 0.0), Align2::LEFT_CENTER, &e.name, FontId::proportional(10.0 * z), col);
            }
        }
        // tables
        let card = if dark { Color32::from_rgb(44, 47, 53) } else { Color32::WHITE };
        let header = if dark { Color32::from_rgb(58, 88, 135) } else { Color32::from_rgb(74, 111, 165) };
        let text_col = vis.text_color();
        let weak = vis.weak_text_color();
        let pk_col = Color32::from_rgb(201, 154, 6);
        let fk_col = Color32::from_rgb(47, 160, 110);
        let search_col = Color32::from_rgb(235, 150, 30);
        let hover_world = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p)).map(|p| self.to_world(p));
        let hovered = hover_world.and_then(|p| self.hit(p));
        for &i in &self.order {
            let n = &self.nodes[i];
            let r = Rect::from_min_size(self.to_screen(n.pos), n.size * z);
            if !r.intersects(rect) {
                continue;
            }
            let t = &self.tables[i];
            let sel = self.selected == Some(i);
            let rad = 5.0 * z;
            // shadow
            painter.rect_filled(r.translate(vec2(2.0, 3.0) * z), rad, Color32::from_black_alpha(if dark { 60 } else { 25 }));
            painter.rect_filled(r, rad, card);
            let hr = Rect::from_min_size(r.min, vec2(r.width(), HEADER_H * z));
            painter.rect_filled(hr, egui::CornerRadius { nw: rad as u8, ne: rad as u8, sw: 0, se: 0 }, if sel { accent } else { header });
            let border = if sel {
                Stroke::new(2.0_f32, accent)
            } else if self.matches(i) {
                Stroke::new(2.5_f32, search_col)
            } else if hovered == Some(i) {
                Stroke::new(1.5_f32, accent.gamma_multiply(0.7))
            } else {
                Stroke::new(1.0_f32, if dark { Color32::from_gray(70) } else { Color32::from_gray(190) })
            };
            painter.rect_stroke(r, rad, border, StrokeKind::Outside);
            if z < 0.25 {
                continue;
            }
            painter.text(hr.left_center() + vec2(PAD_X * z, 0.0), Align2::LEFT_CENTER, format!("{} {}", icon::TABLE, t.path.name), FontId::proportional((FONT + 1.0) * z), Color32::WHITE);
            if z < 0.45 {
                continue;
            }
            let pk = t.primary_key();
            let font = FontId::proportional(FONT * z);
            for (row, c) in t.columns.iter().enumerate() {
                let cy = self.to_screen(pos2(0.0, self.row_y(i, row))).y;
                if cy < rect.min.y - 20.0 || cy > rect.max.y + 20.0 {
                    continue;
                }
                let is_pk = pk.contains(&c.name);
                let is_fk = t.foreign_keys.iter().any(|f| f.columns.contains(&c.name));
                let x0 = r.min.x + 6.0 * z;
                if is_pk {
                    painter.text(pos2(x0, cy), Align2::LEFT_CENTER, icon::KEY, FontId::proportional(10.0 * z), pk_col);
                } else if is_fk {
                    painter.text(pos2(x0, cy), Align2::LEFT_CENTER, "🔗", FontId::proportional(10.0 * z), fk_col);
                }
                if is_pk && is_fk {
                    painter.circle_filled(pos2(x0 + 14.0 * z, cy + 4.0 * z), 2.0 * z, fk_col);
                }
                let name_col = if is_fk { fk_col } else { text_col };
                let name_font = if is_pk { FontId::new(FONT * z, egui::FontFamily::Proportional) } else { font.clone() };
                painter.text(pos2(r.min.x + 26.0 * z, cy), Align2::LEFT_CENTER, &c.name, name_font, name_col);
                if self.show_types {
                    painter.text(pos2(r.max.x - PAD_X * z, cy), Align2::RIGHT_CENTER, &c.data_type, font.clone(), weak);
                }
            }
        }
        // hover tooltip with comment / column count
        if self.dragging.is_none() {
            if let Some(i) = hovered {
                let t = &self.tables[i];
                if resp.hovered() && z < 0.45 {
                    resp.clone().on_hover_text(format!("{} ({} columns)", t.path.name, t.columns.len()));
                }
            }
        }
        // minimap-like status
        painter.text(
            rect.right_bottom() - vec2(10.0, 8.0),
            Align2::RIGHT_BOTTOM,
            format!("{} tables · {} relationships · {:.0}%", self.tables.len(), self.edges.len(), z * 100.0),
            FontId::proportional(11.0),
            weak,
        );
    }
}

fn anyhow_like<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl Tab for ErModelTab {
    fn title(&self) -> String {
        format!("{} Model: {}", icon::MODEL, self.scope.display())
    }

    fn key(&self) -> String {
        format!("model:{}:{}", self.conn, self.scope.display())
    }

    fn tick(&mut self, _st: &mut AppState) {
        if self.load.poll() {
            if let Loadable::Ready(defs) = std::mem::take(&mut self.load) {
                self.on_loaded(defs);
                self.load = Loadable::Ready(vec![]);
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.tick(st);
        self.toolbar(ui, st);
        if let Loadable::Failed(e) = &self.load {
            let e = e.clone();
            crate::ui::error_text(ui, &e);
        }
        if self.load.is_loading() {
            crate::ui::loading(ui, "Loading table definitions...");
        }
        ui.separator();
        if self.show_panel {
            egui::SidePanel::right(egui::Id::new(("er-side", &self.id)))
                .resizable(true)
                .default_width(300.0)
                .width_range(220.0..=520.0)
                .show_inside(ui, |ui| self.side_panel(ui, st));
        }
        egui::CentralPanel::default().frame(egui::Frame::new()).show_inside(ui, |ui| self.canvas_ui(ui, st));
    }
}
