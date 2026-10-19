//! NavDB Studio – a multi-database administration GUI written in Rust.

// release builds on Windows are GUI apps: no console window next to the main window
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod commands;
mod dialogs;
mod state;
mod tabs;
mod ui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NavDB Studio")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/navdb-256.png")).unwrap_or_default()),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native("NavDB Studio", options, Box::new(|cc| Ok(Box::new(app::NavApp::new(cc)))))
}
