//! Backup (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct BackupDialog {
    id: String,
}

impl BackupDialog {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for BackupDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Backup", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Backup is not implemented yet.");
        });
        open
    }
}
