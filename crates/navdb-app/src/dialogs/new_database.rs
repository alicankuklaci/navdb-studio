//! Database Properties (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct DatabaseDialog {
    id: String,
}

impl DatabaseDialog {
    pub fn new(st: &mut AppState, conn: &str, existing: Option<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for DatabaseDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Database Properties", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Database Properties is not implemented yet.");
        });
        open
    }
}
