//! NavDB Studio – a multi-database administration GUI written in Rust.

mod app;
mod catalog;
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
            .with_min_inner_size([900.0, 560.0]),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native("NavDB Studio", options, Box::new(|cc| Ok(Box::new(app::NavApp::new(cc)))))
}
