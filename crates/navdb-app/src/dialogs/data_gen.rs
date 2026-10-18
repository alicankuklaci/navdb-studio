//! Data Generation (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct DataGenDialog {
    id: String,
}

impl DataGenDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, tables: Vec<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for DataGenDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Data Generation", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Data Generation is not implemented yet.");
        });
        open
    }
}
