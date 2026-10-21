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
    let relaunched = std::env::var_os(SOFTWARE_GL).is_some();
    #[cfg(windows)]
    let software = relaunched && load_software_gl();
    #[cfg(windows)]
    let renderers: &[eframe::Renderer] = if software { &[eframe::Renderer::Glow] } else { &[eframe::Renderer::Wgpu, eframe::Renderer::Glow] };
    #[cfg(not(windows))]
    let renderers: &[eframe::Renderer] = &[eframe::Renderer::Glow];
    let mut errors = Vec::new();
    for &renderer in renderers {
        match eframe::run_native("NavDB Studio", options(renderer), Box::new(|cc| Ok(Box::new(app::NavApp::new(cc))))) {
            Ok(()) => return,
            Err(e) => errors.push(format!("{renderer}: {e}")),
        }
    }
    // no GPU driver (remote desktop, VMs): start again on the bundled software OpenGL
    #[cfg(windows)]
    if !relaunched && relaunch_with_software_gl() {
        return;
    }
    // a GUI-subsystem build has no console, so say why nothing opened
    let msg = format!("NavDB Studio could not open its window.\n\n{}\n\nUpdating the graphics driver usually fixes this.", errors.join("\n"));
    report(&msg, &msg);
    std::process::exit(1);
}

/// Set in the environment of a relaunch that draws with the bundled software OpenGL.
#[cfg(windows)]
const SOFTWARE_GL: &str = "NAVDB_SOFTWARE_GL";

/// Mesa's llvmpipe OpenGL, shipped in the `mesa` folder next to the exe.
#[cfg(windows)]
fn software_gl_dll() -> Option<std::path::PathBuf> {
    let dll = std::env::current_exe().ok()?.parent()?.join("mesa").join("opengl32.dll");
    dll.exists().then_some(dll)
}

#[cfg(windows)]
fn relaunch_with_software_gl() -> bool {
    software_gl_dll().is_some()
        && std::env::current_exe()
            .and_then(|exe| std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(SOFTWARE_GL, "1").spawn())
            .is_ok()
}

/// Loads the bundled opengl32.dll before the window exists, so the GL loader
/// later resolves "opengl32.dll" to it instead of the system's OpenGL 1.1.
#[cfg(windows)]
fn load_software_gl() -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut std::ffi::c_void, flags: u32) -> *mut std::ffi::c_void;
    }
    // resolve the DLL's own dependencies (libgallium_wgl.dll) from its folder
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x8;
    let Some(dll) = software_gl_dll() else { return false };
    let name: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
    !unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) }.is_null()
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
