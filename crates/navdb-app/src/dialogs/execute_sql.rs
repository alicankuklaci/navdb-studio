//! Execute SQL File (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct ExecuteSqlDialog {
    id: String,
}

impl ExecuteSqlDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Option<Scope>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for ExecuteSqlDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Execute SQL File", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Execute SQL File is not implemented yet.");
        });
        open
    }
}
