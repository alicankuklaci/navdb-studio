//! Data Synchronization (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct DataSyncDialog {
    id: String,
}

impl DataSyncDialog {
    pub fn new(st: &mut AppState, source: Option<(String, Scope)>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for DataSyncDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Data Synchronization", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Data Synchronization is not implemented yet.");
        });
        open
    }
}
