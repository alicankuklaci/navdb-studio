//! View Designer (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct ViewDesignTab {
    id: String,
}

impl ViewDesignTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, view: Option<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for ViewDesignTab {
    fn title(&self) -> String {
        "View Designer".into()
    }

    fn key(&self) -> String {
        format!("view_design:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("View Designer is not implemented yet.");
    }
}
