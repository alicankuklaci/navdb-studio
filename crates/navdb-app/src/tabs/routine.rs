//! Object Designer (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct RoutineTab {
    id: String,
}

impl RoutineTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, kind: ObjectKind, name: Option<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for RoutineTab {
    fn title(&self) -> String {
        "Object Designer".into()
    }

    fn key(&self) -> String {
        format!("routine:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Object Designer is not implemented yet.");
    }
}
