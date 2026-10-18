use serde::{Deserialize, Serialize};

/// Location of an object: database, optional schema (PostgreSQL) and name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct ObjPath {
    pub db: String,
    pub schema: Option<String>,
    pub name: String,
}

impl ObjPath {
    pub fn new(db: impl Into<String>, schema: Option<String>, name: impl Into<String>) -> Self {
        Self { db: db.into(), schema, name: name.into() }
    }

    pub fn scope(&self) -> Scope {
        Scope { db: self.db.clone(), schema: self.schema.clone() }
    }

    pub fn display(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}.{}", self.db, s, self.name),
            None => format!("{}.{}", self.db, self.name),
        }
    }
}

/// A database (+ schema) the navigator has selected.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Scope {
    pub db: String,
    pub schema: Option<String>,
}

impl Scope {
    pub fn new(db: impl Into<String>, schema: Option<String>) -> Self {
        Self { db: db.into(), schema }
    }

    pub fn path(&self, name: impl Into<String>) -> ObjPath {
        ObjPath::new(self.db.clone(), self.schema.clone(), name)
    }

    pub fn display(&self) -> String {
        match &self.schema {
            Some(s) => format!("{}.{}", self.db, s),
            None => self.db.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
    Trigger,
    Event,
    Sequence,
    Index,
    /// Redis key.
    Key,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized Views",
            ObjectKind::Function => "Functions",
            ObjectKind::Procedure => "Procedures",
            ObjectKind::Trigger => "Triggers",
            ObjectKind::Event => "Events",
            ObjectKind::Sequence => "Sequences",
            ObjectKind::Index => "Indexes",
            ObjectKind::Key => "Keys",
        }
    }

    pub fn singular(self) -> &'static str {
        match self {
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
            ObjectKind::MaterializedView => "Materialized View",
            ObjectKind::Function => "Function",
            ObjectKind::Procedure => "Procedure",
            ObjectKind::Trigger => "Trigger",
            ObjectKind::Event => "Event",
            ObjectKind::Sequence => "Sequence",
            ObjectKind::Index => "Index",
            ObjectKind::Key => "Key",
        }
    }

    pub fn sql_keyword(self) -> &'static str {
        match self {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED VIEW",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Event => "EVENT",
            ObjectKind::Sequence => "SEQUENCE",
            ObjectKind::Index => "INDEX",
            ObjectKind::Key => "KEY",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            ObjectKind::Table => "▦",
            ObjectKind::View => "◫",
            ObjectKind::MaterializedView => "◩",
            ObjectKind::Function | ObjectKind::Procedure => "ƒ",
            ObjectKind::Trigger => "⚡",
            ObjectKind::Event => "⏱",
            ObjectKind::Sequence => "#",
            ObjectKind::Index => "⚷",
            ObjectKind::Key => "🔑",
        }
    }
}

/// One row of the object list (Objects tab).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ObjectInfo {
    pub name: String,
    pub rows: Option<i64>,
    pub data_length: Option<i64>,
    pub engine: String,
    pub comment: String,
    pub created: String,
    pub modified: String,
    /// Extra properties shown in the information pane.
    pub extra: Vec<(String, String)>,
}

impl ObjectInfo {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// Full type text, e.g. `varchar(255)` or `numeric(10,2)`.
    pub data_type: String,
    pub nullable: bool,
    /// Raw default expression (already SQL, e.g. `'abc'`, `0`, `CURRENT_TIMESTAMP`).
    pub default: Option<String>,
    pub auto_increment: bool,
    pub primary_key: bool,
    pub comment: String,
    pub collation: String,
    /// Driver-specific extra (e.g. `on update CURRENT_TIMESTAMP`).
    pub extra: String,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self { name: name.into(), data_type: data_type.into(), nullable: true, ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// BTREE, HASH, FULLTEXT, SPATIAL, GIN...
    pub method: String,
    pub comment: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ForeignKeyDef {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_db: String,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TriggerDef {
    pub name: String,
    /// BEFORE / AFTER / INSTEAD OF
    pub timing: String,
    /// INSERT / UPDATE / DELETE
    pub event: String,
    pub body: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckDef {
    pub name: String,
    pub expression: String,
}

/// Complete table definition used by the Table Designer and Structure Sync.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TableDef {
    pub path: ObjPath,
    pub columns: Vec<ColumnDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub checks: Vec<CheckDef>,
    pub triggers: Vec<TriggerDef>,
    pub comment: String,
    pub engine: String,
    pub charset: String,
    pub collation: String,
    pub auto_increment: Option<u64>,
}

impl TableDef {
    pub fn primary_key(&self) -> Vec<String> {
        if let Some(pk) = self.indexes.iter().find(|i| i.primary) {
            return pk.columns.clone();
        }
        self.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
    }

    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UserInfo {
    pub name: String,
    pub host: String,
    pub attributes: Vec<(String, String)>,
}

impl UserInfo {
    pub fn display(&self) -> String {
        if self.host.is_empty() {
            self.name.clone()
        } else {
            format!("{}@{}", self.name, self.host)
        }
    }
}

/// Options for user creation/alteration in the User Designer.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct UserSpec {
    pub name: String,
    pub host: String,
    pub password: String,
    pub superuser: bool,
    pub can_login: bool,
    pub create_db: bool,
    pub create_role: bool,
    /// (privilege, object e.g. `db.*` / `ALL TABLES IN SCHEMA public`)
    pub grants: Vec<(String, String)>,
}

/// Data shown on the Server Monitor.
#[derive(Clone, Debug, Default)]
pub struct ServerSnapshot {
    pub processes: crate::value::QueryResult,
    pub variables: crate::value::QueryResult,
    pub status: crate::value::QueryResult,
}
