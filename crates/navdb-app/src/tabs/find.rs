//! Find in Database (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct FindInDbTab {
    id: String,
}

impl FindInDbTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for FindInDbTab {
    fn title(&self) -> String {
        "Find in Database".into()
    }

    fn key(&self) -> String {
        format!("find:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Find in Database is not implemented yet.");
    }
}
