//! Data Transfer (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct DataTransferDialog {
    id: String,
}

impl DataTransferDialog {
    pub fn new(st: &mut AppState, source: Option<(String, Scope)>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for DataTransferDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Data Transfer", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Data Transfer is not implemented yet.");
        });
        open
    }
}
