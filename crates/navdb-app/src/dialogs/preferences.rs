//! Preferences (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{Dialect, QueryResult, Scope};

use super::Dialog;
use crate::state::AppState;

pub struct PreferencesDialog {
    id: String,
}

impl PreferencesDialog {
    pub fn new(st: &mut AppState) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }
}

impl Dialog for PreferencesDialog {
    fn show(&mut self, ctx: &egui::Context, _st: &mut AppState) -> bool {
        let mut open = true;
        super::window("Preferences", &self.id).open(&mut open).show(ctx, |ui| {
            ui.label("Preferences is not implemented yet.");
        });
        open
    }
}
