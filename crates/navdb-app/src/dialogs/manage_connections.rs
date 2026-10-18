//! Export Connections window.

use super::Dialog;
use crate::state::AppState;

pub struct ExportConnectionsDialog {
    selected: Vec<(String, String, bool)>,
    include_passwords: bool,
}

impl ExportConnectionsDialog {
    pub fn new(st: &AppState) -> Self {
        Self {
            selected: st.ws.connections.iter().map(|c| (c.id.clone(), c.display_name(), true)).collect(),
            include_passwords: false,
        }
    }
}

impl Dialog for ExportConnectionsDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        let mut open = true;
        let mut done = false;
        super::window("Export Connections", "export-conns").open(&mut open).default_width(380.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for (_, name, on) in self.selected.iter_mut() {
                    ui.checkbox(on, name.as_str());
                }
            });
            ui.checkbox(&mut self.include_passwords, "Export passwords (stored in plain text!)");
            ui.horizontal(|ui| {
                if ui.button("Export…").clicked() {
                    let ids: Vec<&String> = self.selected.iter().filter(|(_, _, on)| *on).map(|(id, _, _)| id).collect();
                    let list: Vec<_> = st.ws.connections.iter().filter(|c| ids.contains(&&c.id)).cloned().collect();
                    let json = navdb_core::config::export_connections(&list, self.include_passwords);
                    if let Some(p) = crate::ui::save_file_dialog("connections.json", "json") {
                        match std::fs::write(&p, json) {
                            Ok(_) => st.toast(format!("Exported {} connection(s)", list.len())),
                            Err(e) => st.error(e.to_string()),
                        }
                        done = true;
                    }
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
        });
        open && !done
    }
}
