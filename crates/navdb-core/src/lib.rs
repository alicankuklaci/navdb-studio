//! Core (GUI-free) layer of NavDB Studio: connection configuration, database
//! drivers, metadata model, SQL dialects, DDL/DML generation, import/export and
//! the background session worker used by the GUI.

pub mod config;
pub mod datagen;
pub mod ddl;
pub mod dialect;
pub mod dml;
pub mod dump;
pub mod driver;
pub mod export;
pub mod import;
pub mod model;
pub mod session;
pub mod sqlmini;
pub mod sqlutil;
pub mod ssh;
pub mod sync;
pub mod value;

pub use anyhow::{anyhow, bail, Result};
pub use config::{ConnectionConfig, DbKind};
pub use dialect::Dialect;
pub use model::*;
pub use session::{Pending, Session};
pub use value::{ColumnMeta, QueryResult, Value};
