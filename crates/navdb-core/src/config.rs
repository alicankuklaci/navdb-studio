use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::dialect::Dialect;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum DbKind {
    #[default]
    MySql,
    MariaDb,
    PostgreSql,
    Sqlite,
    Redis,
    SqlServer,
    Oracle,
    MongoDb,
    DuckDb,
    Iceberg,
    Vertica,
    ClickHouse,
    Trino,
    Presto,
    OpenSearch,
    Elasticsearch,
    Cassandra,
    ScyllaDb,
    // wire-compatible variants served by the MySQL driver
    TiDb,
    SingleStore,
    OceanBase,
    StarRocks,
    Doris,
    // wire-compatible variants served by the PostgreSQL driver
    CockroachDb,
    YugabyteDb,
    TimescaleDb,
    Redshift,
    Greenplum,
    // wire-compatible variants served by the Redis driver
    Valkey,
    KeyDb,
    Dragonfly,
}

impl DbKind {
    pub const ALL: [DbKind; 31] = [
        DbKind::MySql,
        DbKind::MariaDb,
        DbKind::PostgreSql,
        DbKind::Sqlite,
        DbKind::Redis,
        DbKind::SqlServer,
        DbKind::Oracle,
        DbKind::MongoDb,
        DbKind::DuckDb,
        DbKind::Iceberg,
        DbKind::Vertica,
        DbKind::ClickHouse,
        DbKind::Trino,
        DbKind::Presto,
        DbKind::OpenSearch,
        DbKind::Elasticsearch,
        DbKind::Cassandra,
        DbKind::ScyllaDb,
        DbKind::TiDb,
        DbKind::SingleStore,
        DbKind::OceanBase,
        DbKind::StarRocks,
        DbKind::Doris,
        DbKind::CockroachDb,
        DbKind::YugabyteDb,
        DbKind::TimescaleDb,
        DbKind::Redshift,
        DbKind::Greenplum,
        DbKind::Valkey,
        DbKind::KeyDb,
        DbKind::Dragonfly,
    ];

    /// Connection types grouped the way the "New Connection" menu shows them.
    pub const GROUPS: [(&'static str, &'static [DbKind]); 5] = [
        ("Relational", &[DbKind::MySql, DbKind::MariaDb, DbKind::PostgreSql, DbKind::SqlServer, DbKind::Oracle, DbKind::Sqlite]),
        (
            "MySQL Compatible",
            &[DbKind::TiDb, DbKind::SingleStore, DbKind::OceanBase, DbKind::StarRocks, DbKind::Doris],
        ),
        (
            "PostgreSQL Compatible",
            &[DbKind::CockroachDb, DbKind::YugabyteDb, DbKind::TimescaleDb, DbKind::Redshift, DbKind::Greenplum],
        ),
        (
            "Analytics",
            &[DbKind::DuckDb, DbKind::ClickHouse, DbKind::Vertica, DbKind::Trino, DbKind::Presto, DbKind::Iceberg],
        ),
        (
            "NoSQL",
            &[
                DbKind::MongoDb,
                DbKind::Redis,
                DbKind::Valkey,
                DbKind::KeyDb,
                DbKind::Dragonfly,
                DbKind::OpenSearch,
                DbKind::Elasticsearch,
                DbKind::Cassandra,
                DbKind::ScyllaDb,
            ],
        ),
    ];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::MySql => "MySQL",
            DbKind::MariaDb => "MariaDB",
            DbKind::PostgreSql => "PostgreSQL",
            DbKind::Sqlite => "SQLite",
            DbKind::Redis => "Redis",
            DbKind::SqlServer => "SQL Server",
            DbKind::Oracle => "Oracle",
            DbKind::MongoDb => "MongoDB",
            DbKind::DuckDb => "DuckDB",
            DbKind::Iceberg => "Apache Iceberg",
            DbKind::Vertica => "Vertica",
            DbKind::ClickHouse => "ClickHouse",
            DbKind::Trino => "Trino",
            DbKind::Presto => "Presto",
            DbKind::OpenSearch => "OpenSearch",
            DbKind::Elasticsearch => "Elasticsearch",
            DbKind::Cassandra => "Cassandra",
            DbKind::ScyllaDb => "ScyllaDB",
            DbKind::TiDb => "TiDB",
            DbKind::SingleStore => "SingleStore",
            DbKind::OceanBase => "OceanBase",
            DbKind::StarRocks => "StarRocks",
            DbKind::Doris => "Apache Doris",
            DbKind::CockroachDb => "CockroachDB",
            DbKind::YugabyteDb => "YugabyteDB",
            DbKind::TimescaleDb => "TimescaleDB",
            DbKind::Redshift => "Amazon Redshift",
            DbKind::Greenplum => "Greenplum",
            DbKind::Valkey => "Valkey",
            DbKind::KeyDb => "KeyDB",
            DbKind::Dragonfly => "Dragonfly",
        }
    }

    /// The driver family a kind is served by. Wire-compatible products map to
    /// the driver they speak (TiDB -> MySQL, CockroachDB -> PostgreSQL, ...).
    pub fn base(self) -> DbKind {
        match self {
            DbKind::TiDb | DbKind::SingleStore | DbKind::OceanBase | DbKind::StarRocks | DbKind::Doris => DbKind::MySql,
            DbKind::CockroachDb | DbKind::YugabyteDb | DbKind::TimescaleDb | DbKind::Redshift | DbKind::Greenplum => {
                DbKind::PostgreSql
            }
            DbKind::Valkey | DbKind::KeyDb | DbKind::Dragonfly => DbKind::Redis,
            DbKind::Presto => DbKind::Trino,
            DbKind::Elasticsearch => DbKind::OpenSearch,
            DbKind::ScyllaDb => DbKind::Cassandra,
            k => k,
        }
    }

    pub fn is_mysql_family(self) -> bool {
        matches!(self.base(), DbKind::MySql | DbKind::MariaDb)
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::MySql | DbKind::MariaDb | DbKind::SingleStore => 3306,
            DbKind::TiDb => 4000,
            DbKind::OceanBase => 2881,
            DbKind::StarRocks | DbKind::Doris => 9030,
            DbKind::PostgreSql | DbKind::TimescaleDb | DbKind::Greenplum => 5432,
            DbKind::CockroachDb => 26257,
            DbKind::YugabyteDb => 5433,
            DbKind::Redshift => 5439,
            DbKind::Sqlite | DbKind::DuckDb => 0,
            DbKind::Redis | DbKind::Valkey | DbKind::KeyDb | DbKind::Dragonfly => 6379,
            DbKind::SqlServer => 1433,
            DbKind::Oracle => 1521,
            DbKind::MongoDb => 27017,
            // REST catalog endpoint
            DbKind::Iceberg => 8181,
            DbKind::Vertica => 5433,
            DbKind::ClickHouse => 8123,
            DbKind::Trino | DbKind::Presto => 8080,
            DbKind::OpenSearch | DbKind::Elasticsearch => 9200,
            DbKind::Cassandra | DbKind::ScyllaDb => 9042,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DbKind::MySql | DbKind::MariaDb | DbKind::TiDb | DbKind::SingleStore | DbKind::OceanBase => "root",
            DbKind::StarRocks | DbKind::Doris => "root",
            DbKind::PostgreSql | DbKind::TimescaleDb | DbKind::Greenplum => "postgres",
            DbKind::CockroachDb => "root",
            DbKind::YugabyteDb => "yugabyte",
            DbKind::Redshift => "awsuser",
            DbKind::SqlServer => "sa",
            DbKind::Oracle => "system",
            DbKind::Vertica => "dbadmin",
            DbKind::ClickHouse => "default",
            DbKind::Trino | DbKind::Presto => "navdb",
            DbKind::OpenSearch | DbKind::Elasticsearch => "admin",
            _ => "",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self.base() {
            DbKind::MySql | DbKind::MariaDb => Dialect::MySql,
            DbKind::PostgreSql => Dialect::Postgres,
            DbKind::Sqlite | DbKind::Redis => Dialect::Sqlite,
            DbKind::SqlServer => Dialect::SqlServer,
            DbKind::Oracle => Dialect::Oracle,
            _ => Dialect::Generic,
        }
    }

    /// Whether the query editor speaks SQL (or an SQL-like language such as
    /// CQL). MongoDB, OpenSearch and Redis are document/key stores but the
    /// first two still accept the SQL subset the data grid generates.
    pub fn is_sql(self) -> bool {
        !matches!(self.base(), DbKind::Redis)
    }

    /// Whether the server is a relational engine with tables, views and
    /// routines (as opposed to a document, search or key-value store).
    pub fn is_relational(self) -> bool {
        !matches!(self.base(), DbKind::Redis | DbKind::MongoDb | DbKind::OpenSearch | DbKind::Cassandra)
    }

    /// Whether objects live in schemas inside a database.
    pub fn has_schemas(self) -> bool {
        matches!(
            self.base(),
            DbKind::PostgreSql | DbKind::SqlServer | DbKind::DuckDb | DbKind::Vertica | DbKind::Trino | DbKind::Iceberg
        )
    }

    /// Schema selected when a database is opened.
    pub fn default_schema(self) -> Option<&'static str> {
        match self.base() {
            DbKind::PostgreSql | DbKind::Vertica => Some("public"),
            DbKind::SqlServer => Some("dbo"),
            DbKind::DuckDb => Some("main"),
            _ => None,
        }
    }

    pub fn is_file_based(self) -> bool {
        matches!(self, DbKind::Sqlite | DbKind::DuckDb)
    }

    /// Whether the kind supports CREATE USER / GRANT management in the app.
    pub fn has_users(self) -> bool {
        matches!(self.base(), DbKind::MySql | DbKind::MariaDb | DbKind::PostgreSql)
    }

    /// Cheap statement used to check a connection / switched database.
    pub fn ping_query(self) -> &'static str {
        match self.base() {
            DbKind::Redis => "PING",
            DbKind::MongoDb => "db.runCommand({ping: 1})",
            DbKind::OpenSearch => "GET /",
            DbKind::Oracle => "SELECT 1 FROM DUAL",
            DbKind::Cassandra => "SELECT release_version FROM system.local",
            _ => "SELECT 1",
        }
    }

    /// Language of the query editor: `sql`, `js` (MongoDB shell) or `txt`.
    pub fn query_language(self) -> &'static str {
        match self.base() {
            DbKind::Redis => "txt",
            DbKind::MongoDb => "js",
            _ => "sql",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SslConfig {
    pub enabled: bool,
    pub ca_cert: String,
    pub client_cert: String,
    pub client_key: String,
    pub verify_server: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Path to a private key. Password authentication is delegated to the
    /// user's ssh-agent / ssh config since the system `ssh` client is used.
    pub private_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub save_password: bool,
    /// Initial database (MySQL/PG) or database file path (SQLite).
    pub database: String,
    /// Restrict the navigator to these databases (empty = show all).
    pub custom_databases: Vec<String>,
    pub ssl: SslConfig,
    pub ssh: SshConfig,
    pub color: Option<[u8; 3]>,
    pub group: String,
    pub connect_timeout_secs: u64,
    pub keepalive_secs: u64,
    pub encoding: String,
    pub read_only: bool,
    /// SQLite databases to ATTACH after connecting: (alias, path).
    pub attached: Vec<(String, String)>,
    /// Driver specific settings (MongoDB URI, Iceberg catalog, Oracle
    /// service name, HTTP scheme, ...). Keys are documented per driver.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self::new(DbKind::MySql)
    }
}

impl ConnectionConfig {
    pub fn new(kind: DbKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            kind,
            host: if kind.is_file_based() { String::new() } else { "localhost".into() },
            port: kind.default_port(),
            user: kind.default_user().into(),
            password: String::new(),
            save_password: true,
            database: String::new(),
            custom_databases: vec![],
            ssl: SslConfig::default(),
            ssh: SshConfig { port: 22, ..Default::default() },
            color: None,
            group: String::new(),
            connect_timeout_secs: 10,
            keepalive_secs: 240,
            encoding: "utf8mb4".into(),
            read_only: false,
            attached: vec![],
            options: Default::default(),
        }
    }

    pub fn option(&self, key: &str) -> &str {
        self.options.get(key).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn dialect(&self) -> Dialect {
        self.kind.dialect()
    }

    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if self.kind.is_file_based() {
            PathBuf::from(&self.database)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| self.kind.label().into())
        } else if self.user.is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// Database the session starts in when nothing else is selected.
    pub fn default_database(&self) -> String {
        match self.kind.base() {
            DbKind::PostgreSql => {
                if !self.database.is_empty() {
                    self.database.clone()
                } else {
                    match self.kind {
                        DbKind::CockroachDb => "defaultdb".into(),
                        DbKind::YugabyteDb => "yugabyte".into(),
                        DbKind::Redshift => "dev".into(),
                        _ => "postgres".into(),
                    }
                }
            }
            DbKind::Sqlite => "main".into(),
            // Oracle "databases" are schemas; cfg.database holds the service name
            DbKind::Oracle => {
                let s = self.option("schema");
                if s.is_empty() { self.user.to_uppercase() } else { s.to_uppercase() }
            }
            DbKind::DuckDb => {
                if self.database.is_empty() || self.database == ":memory:" {
                    "memory".into()
                } else {
                    std::path::Path::new(&self.database)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "memory".into())
                }
            }
            // the driver attaches the Iceberg catalog under this name
            DbKind::Iceberg => "iceberg".into(),
            DbKind::Redis => {
                if self.database.is_empty() { "db0".into() } else { self.database.clone() }
            }
            _ => self.database.clone(),
        }
    }
}

/// User preferences (Preferences window).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preferences {
    pub dark_mode: bool,
    pub editor_font_size: f32,
    pub grid_font_size: f32,
    pub show_line_numbers: bool,
    pub word_wrap: bool,
    pub auto_complete: bool,
    pub uppercase_keywords: bool,
    pub tab_width: usize,
    pub records_per_page: usize,
    pub show_null_as: String,
    pub confirm_delete: bool,
    pub stop_on_error: bool,
    pub auto_begin_transaction: bool,
    pub limit_query_results: bool,
    pub query_result_limit: usize,
    pub max_history: usize,
    pub open_tabs_in_new_window: bool,
    pub auto_save_queries: bool,
    pub show_object_info_pane: bool,
    pub date_format: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark_mode: false,
            editor_font_size: 13.0,
            grid_font_size: 12.5,
            show_line_numbers: true,
            word_wrap: false,
            auto_complete: true,
            uppercase_keywords: true,
            tab_width: 4,
            records_per_page: 1000,
            show_null_as: "(Null)".into(),
            confirm_delete: true,
            stop_on_error: true,
            auto_begin_transaction: false,
            limit_query_results: true,
            query_result_limit: 5000,
            max_history: 500,
            open_tabs_in_new_window: false,
            auto_save_queries: true,
            show_object_info_pane: true,
            date_format: "%Y-%m-%d %H:%M:%S".into(),
        }
    }
}

/// A saved query (the "Queries" object type in the navigator).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedQuery {
    pub id: String,
    pub name: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub modified: chrono::DateTime<chrono::Local>,
}

/// One executed statement, shown in the History Log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub time: chrono::DateTime<chrono::Local>,
    pub connection: String,
    pub database: String,
    pub sql: String,
    pub ok: bool,
    pub message: String,
    pub elapsed_ms: u128,
}

/// A reusable code snippet (Snippet library in the SQL editor).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snippet {
    pub name: String,
    pub body: String,
}

/// An automation (batch) job: a list of steps that can be run on demand or on
/// a simple interval schedule.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AutomationJob {
    pub id: String,
    pub name: String,
    pub steps: Vec<AutomationStep>,
    /// 0 = manual only.
    pub interval_minutes: u64,
    pub last_run: Option<chrono::DateTime<chrono::Local>>,
    pub last_status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum AutomationStep {
    RunQuery { connection_id: String, database: String, sql: String },
    Backup { connection_id: String, database: String, output_dir: String },
    ExportTable { connection_id: String, database: String, table: String, format: String, path: String },
}

/// Everything persisted to disk.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub connections: Vec<ConnectionConfig>,
    pub preferences: Preferences,
    pub queries: Vec<SavedQuery>,
    pub snippets: Vec<Snippet>,
    pub automation: Vec<AutomationJob>,
    pub groups: Vec<String>,
}

pub fn data_dir() -> PathBuf {
    // NAVDB_DATA_DIR isolates test runs from the user's real settings
    let dir = match std::env::var_os("NAVDB_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("navdb-studio"),
    };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn workspace_path() -> PathBuf {
    data_dir().join("workspace.json")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

pub fn backups_dir() -> PathBuf {
    let d = data_dir().join("backups");
    let _ = std::fs::create_dir_all(&d);
    d
}

impl Workspace {
    pub fn load() -> Self {
        Self::load_from(&workspace_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&workspace_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let mut copy = self.clone();
        for c in &mut copy.connections {
            if !c.save_password {
                c.password.clear();
            }
        }
        let json = serde_json::to_string_pretty(&copy)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

pub fn append_history(entry: &HistoryEntry) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(history_path()) {
        if let Ok(line) = serde_json::to_string(entry) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

pub fn load_history(limit: usize) -> Vec<HistoryEntry> {
    let Ok(s) = std::fs::read_to_string(history_path()) else { return vec![] };
    let mut v: Vec<HistoryEntry> = s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    if v.len() > limit {
        v.drain(0..v.len() - limit);
    }
    v
}

/// Export connections to a portable `.ncx`-like JSON file (Export Connections).
pub fn export_connections(conns: &[ConnectionConfig], include_passwords: bool) -> String {
    let list: Vec<ConnectionConfig> = conns
        .iter()
        .cloned()
        .map(|mut c| {
            if !include_passwords {
                c.password.clear();
            }
            c
        })
        .collect();
    serde_json::to_string_pretty(&list).unwrap_or_default()
}

pub fn import_connections(json: &str) -> anyhow::Result<Vec<ConnectionConfig>> {
    Ok(serde_json::from_str(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_roundtrip_drops_unsaved_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ws.json");
        let mut ws = Workspace::default();
        let mut a = ConnectionConfig::new(DbKind::MySql);
        a.password = "secret".into();
        a.save_password = false;
        let mut b = ConnectionConfig::new(DbKind::PostgreSql);
        b.password = "keep".into();
        ws.connections = vec![a, b];
        ws.save_to(&p).unwrap();
        let back = Workspace::load_from(&p);
        assert_eq!(back.connections.len(), 2);
        assert_eq!(back.connections[0].password, "");
        assert_eq!(back.connections[1].password, "keep");
    }
}
