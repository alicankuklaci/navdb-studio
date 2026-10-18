//! Floating windows: connection form, wizards, tools and prompts.

pub mod about;
pub mod backup;
pub mod confirm;
pub mod connection;
pub mod data_gen;
pub mod data_sync;
pub mod data_transfer;
pub mod dump;
pub mod execute_sql;
pub mod export;
pub mod import;
pub mod manage_connections;
pub mod new_database;
pub mod preferences;
pub mod prompt;
pub mod structure_sync;

use crate::state::AppState;

pub trait Dialog {
    /// Draw the window. Return false once the dialog should be closed.
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool;
}

/// Standard modal-like window frame used by dialogs.
pub fn window<'a>(title: impl Into<egui::WidgetText>, id: impl std::hash::Hash) -> egui::Window<'a> {
    egui::Window::new(title)
        .id(egui::Id::new(id))
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(egui::pos2(640.0, 380.0))
}
