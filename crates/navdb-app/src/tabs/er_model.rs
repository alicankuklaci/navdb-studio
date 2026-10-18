//! Model (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct ErModelTab {
    id: String,
}

impl ErModelTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for ErModelTab {
    fn title(&self) -> String {
        "Model".into()
    }

    fn key(&self) -> String {
        format!("er_model:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Model is not implemented yet.");
    }
}
