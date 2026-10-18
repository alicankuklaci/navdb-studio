//! Console (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct ConsoleTab {
    id: String,
}

impl ConsoleTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for ConsoleTab {
    fn title(&self) -> String {
        "Console".into()
    }

    fn key(&self) -> String {
        format!("console:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Console is not implemented yet.");
    }
}
