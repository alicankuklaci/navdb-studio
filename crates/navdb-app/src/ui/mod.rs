//! Shared widgets.

pub mod grid;
pub mod info_pane;
pub mod sql_editor;
pub mod style;
pub mod tree;

use egui::{RichText, Ui};
use navdb_core::Scope;

use crate::state::{AppState, Loadable};

/// Large toolbar button: icon above label (main window toolbar).
pub fn toolbar_button(ui: &mut Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let w = 62.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 50.0), egui::Sense::click());
    let visuals = ui.style().interact_selectable(&resp, selected);
    if selected || resp.hovered() {
        ui.painter().rect_filled(rect.shrink(1.0), 6.0, visuals.bg_fill);
    }
    let color = if selected { ui.visuals().selection.stroke.color } else { ui.visuals().text_color() };
    ui.painter().text(
        rect.center_top() + egui::vec2(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(20.0),
        color,
    );
    ui.painter().text(
        rect.center_bottom() - egui::vec2(0.0, 10.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        color,
    );
    resp
}

/// Small icon+text button for tab toolbars.
pub fn tool_button(ui: &mut Ui, icon: &str, label: &str) -> egui::Response {
    ui.add(egui::Button::new(format!("{} {}", icon, label)).small())
}

pub fn error_text(ui: &mut Ui, msg: &str) {
    ui.colored_label(ui.visuals().error_fg_color, msg);
}

pub fn loading(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(RichText::new(text).weak());
    });
}

pub fn format_bytes(b: i64) -> String {
    let b = b as f64;
    if b < 1024.0 {
        format!("{} B", b)
    } else if b < 1024.0 * 1024.0 {
        format!("{:.2} KB", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MB", b / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", b / 1024.0 / 1024.0 / 1024.0)
    }
}

/// Two-column key/value grid (information pane, summaries).
pub fn kv_grid(ui: &mut Ui, id: &str, rows: &[(String, String)]) {
    // keep long values (file paths) from widening a side panel
    let value_w = (ui.available_width() - 110.0).max(80.0);
    // keyed on content: the grid caches row heights, which go stale when another object is shown
    egui::Grid::new((id, rows)).num_columns(2).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
        for (k, v) in rows {
            ui.label(RichText::new(k).weak());
            ui.scope(|ui| {
                ui.set_max_width(value_w);
                ui.add(egui::Label::new(v).wrap());
            });
            ui.end_row();
        }
    });
}

/// Connection / database / schema selector used by query tabs and tools.
/// Returns true when the selection changed.
pub fn scope_picker(ui: &mut Ui, id: &str, st: &mut AppState, conn: &mut Option<String>, scope: &mut Option<Scope>) -> bool {
    let mut changed = false;
    let choices = st.connection_choices();
    let conn_label = conn.as_ref().map(|c| st.conn_name(c)).unwrap_or_else(|| "Select connection".into());
    egui::ComboBox::from_id_salt(format!("{}-conn", id)).selected_text(conn_label).width(170.0).show_ui(ui, |ui| {
        for (cid, name) in &choices {
            if ui.selectable_label(conn.as_deref() == Some(cid), name).clicked() {
                *conn = Some(cid.clone());
                *scope = None;
                changed = true;
            }
        }
    });
    let Some(cid) = conn.clone() else { return changed };
    if st.session(&cid).is_none() {
        return changed;
    }
    let Some(oc) = st.conn_mut(&cid) else { return changed };
    let dbs = oc.visible_databases();
    if scope.is_none() {
        let def = oc.cfg.default_database();
        if dbs.contains(&def) {
            *scope = Some(Scope::new(def, None));
            changed = true;
        } else if let Some(first) = dbs.first() {
            *scope = Some(Scope::new(first.clone(), None));
            changed = true;
        }
    }
    let db_label = scope.as_ref().map(|s| s.db.clone()).unwrap_or_else(|| "Database".into());
    egui::ComboBox::from_id_salt(format!("{}-db", id)).selected_text(db_label).width(150.0).show_ui(ui, |ui| {
        if oc.databases.is_loading() {
            ui.spinner();
        }
        for d in &dbs {
            if ui.selectable_label(scope.as_ref().map(|s| &s.db) == Some(d), d).clicked() {
                *scope = Some(Scope::new(d.clone(), None));
                changed = true;
            }
        }
    });
    if oc.cfg.kind.has_schemas() {
        if let Some(sc) = scope.as_mut() {
            oc.load_schemas(&sc.db);
            let schemas = oc.schemas.get(&sc.db).and_then(|l| l.ready()).cloned().unwrap_or_default();
            if sc.schema.is_none() {
                if schemas.iter().any(|s| s == "public") {
                    sc.schema = Some("public".into());
                    changed = true;
                } else if let Some(f) = schemas.first() {
                    sc.schema = Some(f.clone());
                    changed = true;
                }
            }
            let label = sc.schema.clone().unwrap_or_else(|| "Schema".into());
            egui::ComboBox::from_id_salt(format!("{}-schema", id)).selected_text(label).width(120.0).show_ui(ui, |ui| {
                for s in &schemas {
                    if ui.selectable_label(sc.schema.as_ref() == Some(s), s).clicked() {
                        sc.schema = Some(s.clone());
                        changed = true;
                    }
                }
            });
        }
    }
    changed
}

/// Show a Loadable's state; returns the value when ready.
pub fn show_loadable<'a, T>(ui: &mut Ui, l: &'a Loadable<T>, what: &str) -> Option<&'a T> {
    match l {
        Loadable::NotLoaded => None,
        Loadable::Loading(_) => {
            loading(ui, &format!("Loading {}...", what));
            None
        }
        Loadable::Failed(e) => {
            error_text(ui, e);
            None
        }
        Loadable::Ready(v) => Some(v),
    }
}

/// File-save helper that never panics when the native dialog is unavailable.
pub fn save_file_dialog(default_name: &str, ext: &str) -> Option<std::path::PathBuf> {
    rfd::FileDialog::new().set_file_name(default_name).add_filter(ext.to_ascii_uppercase(), &[ext]).save_file()
}

pub fn open_file_dialog(exts: &[&str]) -> Option<std::path::PathBuf> {
    let mut d = rfd::FileDialog::new();
    if !exts.is_empty() {
        d = d.add_filter("Supported", exts);
    }
    d.pick_file()
}

pub fn pick_folder_dialog() -> Option<std::path::PathBuf> {
    rfd::FileDialog::new().pick_folder()
}
