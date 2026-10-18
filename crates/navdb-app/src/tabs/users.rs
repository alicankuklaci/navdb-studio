//! Users (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct UsersTab {
    id: String,
}

impl UsersTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Tab for UsersTab {
    fn title(&self) -> String {
        "Users".into()
    }

    fn key(&self) -> String {
        format!("users:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Users is not implemented yet.");
    }
}
