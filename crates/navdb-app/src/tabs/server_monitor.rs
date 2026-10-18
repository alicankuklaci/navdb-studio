//! Server Monitor (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct ServerMonitorTab {
    id: String,
}

impl ServerMonitorTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for ServerMonitorTab {
    fn title(&self) -> String {
        "Server Monitor".into()
    }

    fn key(&self) -> String {
        format!("server_monitor:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Server Monitor is not implemented yet.");
    }
}
