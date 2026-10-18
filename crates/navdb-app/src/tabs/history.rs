//! History Log (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct HistoryTab {
    id: String,
}

impl HistoryTab {
    pub fn new() -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for HistoryTab {
    fn title(&self) -> String {
        "History Log".into()
    }

    fn key(&self) -> String {
        format!("history:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("History Log is not implemented yet.");
    }
}
