//! Import Wizard (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct ImportWizard {
    id: String,
}

impl ImportWizard {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, table: Option<String>) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for ImportWizard {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Import Wizard", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Import Wizard is not implemented yet.");
        });
        open
    }
}
