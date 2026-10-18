//! Main-area tabs (Objects, table viewers, query editors, designers, tools).

pub mod automation;
pub mod charts;
pub mod console;
pub mod er_model;
pub mod find;
pub mod history;
pub mod objects;
pub mod query;
pub mod redis_key;
pub mod routine;
pub mod server_monitor;
pub mod table_data;
pub mod table_design;
pub mod users;
pub mod view_design;

use crate::state::AppState;

pub trait Tab {
    fn title(&self) -> String;

    /// Stable identity used to focus an existing tab instead of opening a duplicate.
    fn key(&self) -> String;

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState);

    /// Asked before closing; return false to keep the tab open (e.g. unsaved changes).
    fn can_close(&mut self, _st: &mut AppState) -> bool {
        true
    }

    /// Unsaved changes marker shown in the tab title.
    fn dirty(&self) -> bool {
        false
    }

    /// Called every frame even when the tab is not visible (for polling).
    fn tick(&mut self, _st: &mut AppState) {}
}
