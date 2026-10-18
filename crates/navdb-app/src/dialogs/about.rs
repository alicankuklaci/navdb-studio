//! About window.

use super::Dialog;
use crate::state::AppState;

pub struct AboutDialog;

impl AboutDialog {
    pub fn new() -> Self {
        Self
    }
}

impl Dialog for AboutDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("About NavDB Studio", "about").open(&mut open).resizable(false).default_width(360.0).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("NavDB Studio");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(6.0);
                ui.label("A multi-database GUI written in Rust (egui).");
                ui.label("MySQL · MariaDB · PostgreSQL · SQLite · Redis");
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("Data folder: {}", navdb_core::config::data_dir().display())).small().weak());
            });
        });
        open
    }
}
