//! NavDB Studio – a multi-database administration GUI written in Rust.

// release builds on Windows are GUI apps: no console window next to the main window
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod catalog;
mod commands;
mod dialogs;
mod state;
mod store;
mod tabs;
mod ui;

fn main() {
    store::start();
    #[cfg(windows)]
    let renderers = [eframe::Renderer::Wgpu, eframe::Renderer::Glow];
    #[cfg(not(windows))]
    let renderers = [eframe::Renderer::Glow];
    let mut errors = Vec::new();
    for renderer in renderers {
        match eframe::run_native("NavDB Studio", options(renderer), Box::new(|cc| Ok(Box::new(app::NavApp::new(cc))))) {
            Ok(()) => return,
            Err(e) => errors.push(format!("{renderer}: {e}")),
        }
    }
    // a GUI-subsystem build has no console, so say why nothing opened
    let msg = format!("NavDB Studio could not open its window.\n\n{}\n\nUpdating the graphics driver usually fixes this.", errors.join("\n"));
    eprintln!("{msg}");
    rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("NavDB Studio").set_description(msg).show();
    std::process::exit(1);
}

fn options(renderer: eframe::Renderer) -> eframe::NativeOptions {
    eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_title("NavDB Studio")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/navdb-256.png")).unwrap_or_default()),
        persist_window: true,
        ..Default::default()
    }
}
