//! Charts (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct ChartsTab {
    id: String,
}

impl ChartsTab {
    pub fn new(st: &mut AppState, source: Option<(String, Scope, String)>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for ChartsTab {
    fn title(&self) -> String {
        "Charts".into()
    }

    fn key(&self) -> String {
        format!("charts:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Charts is not implemented yet.");
    }
}
