//! Redis Key (stub – to be implemented).
#![allow(unused_variables, dead_code, unused_imports)]

use navdb_core::{ObjectKind, Scope};

use super::Tab;
use crate::state::AppState;

pub struct RedisKeyTab {
    id: String,
}

impl RedisKeyTab {
    pub fn new(st: &mut AppState, conn: &str, db: String, key: String) -> Self {
        Self { id: uuid::Uuid::new_v4().to_string() }
    }

    pub fn new_key(st: &mut AppState, conn: &str, db: String) -> Self {
        Self::new(st, conn, db, String::new())
    }
}

impl Tab for RedisKeyTab {
    fn title(&self) -> String {
        "Redis Key".into()
    }

    fn key(&self) -> String {
        format!("redis_key:{}", self.id)
    }

    fn ui(&mut self, ui: &mut egui::Ui, _st: &mut AppState) {
        ui.label("Redis Key is not implemented yet.");
    }
}
