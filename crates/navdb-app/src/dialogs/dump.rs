//! Dump SQL File (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct DumpDialog {
    id: String,
}

impl DumpDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, tables: Vec<String>, with_data: bool) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for DumpDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Dump SQL File", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Dump SQL File is not implemented yet.");
        });
        open
    }
}
