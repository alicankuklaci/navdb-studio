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
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        report(&format!("NavDB Studio stopped unexpectedly.\n\n{info}"), &format!("{info}\n\n{bt}"));
    }));
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
    report(&msg, &msg);
    std::process::exit(1);
}

/// Writes `detail` to crash.log in the data folder and shows `msg`, since a
/// GUI-subsystem build on Windows has no console to print to.
fn report(msg: &str, detail: &str) {
    eprintln!("{detail}");
    let log = navdb_core::config::data_dir().join("crash.log");
    let _ = std::fs::write(&log, format!("{} {}\n{detail}\n", chrono::Local::now().to_rfc3339(), env!("CARGO_PKG_VERSION")));
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("NavDB Studio")
        .set_description(format!("{msg}\n\nDetails: {}", log.display()))
        .show();
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
