//! Database drivers. Every driver is synchronous and lives on its own worker
//! thread (see [`crate::session`]).

use anyhow::{bail, Result};

use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::QueryResult;

pub mod cassandra;
pub mod clickhouse;
pub mod duck;
pub mod mongo;
pub mod mssql;
pub mod mysql;
#[cfg(feature = "oracle")]
pub mod oracle;
pub mod postgres;
pub mod redis;
pub mod search;
pub mod sqlite;
pub mod trino;
pub mod vertica;

/// Never constructed; lets placeholder drivers type-check.
pub enum Unsupported {}

impl Driver for Unsupported {
    fn kind(&self) -> DbKind {
        match *self {}
    }
    fn server_version(&mut self) -> Result<String> {
        match *self {}
    }
    fn list_databases(&mut self) -> Result<Vec<String>> {
        match *self {}
    }
    fn list_objects(&mut self, _: &Scope, _: ObjectKind) -> Result<Vec<ObjectInfo>> {
        match *self {}
    }
    fn object_kinds(&self) -> Vec<ObjectKind> {
        match *self {}
    }
    fn table_def(&mut self, _: &ObjPath) -> Result<TableDef> {
        match *self {}
    }
    fn object_ddl(&mut self, _: &ObjPath, _: ObjectKind) -> Result<String> {
        match *self {}
    }
    fn execute(&mut self, _: Option<&Scope>, _: &str) -> Result<Vec<QueryResult>> {
        match *self {}
    }
}

pub trait Driver: Send {
    fn kind(&self) -> DbKind;

    fn dialect(&self) -> Dialect {
        self.kind().dialect()
    }

    fn server_version(&mut self) -> Result<String>;

    fn list_databases(&mut self) -> Result<Vec<String>>;

    /// Schemas inside `db` (PostgreSQL). Other drivers return an empty list.
    fn list_schemas(&mut self, _db: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>>;

    /// Object kinds this driver exposes in the navigator.
    fn object_kinds(&self) -> Vec<ObjectKind>;

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef>;

    /// Columns of a table or view.
    fn columns(&mut self, path: &ObjPath) -> Result<Vec<ColumnDef>> {
        Ok(self.table_def(path)?.columns)
    }

    /// `CREATE ...` statement for any object.
    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String>;

    /// Execute one or more statements in `scope`, returning one result per statement.
    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>>;

    /// Execute a single statement and return its result.
    fn query(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let mut v = self.execute(scope, sql)?;
        match v.pop() {
            Some(r) => Ok(r),
            None => bail!("statement returned no result"),
        }
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        bail!("user management is not supported by {}", self.kind().label())
    }

    /// SQL to create/alter a user (the User Designer shows it in "SQL Preview").
    fn user_sql(&self, _old: Option<&UserInfo>, _spec: &UserSpec) -> Result<Vec<String>> {
        bail!("user management is not supported by {}", self.kind().label())
    }

    fn user_grants(&mut self, _user: &UserInfo) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn drop_user_sql(&self, _user: &UserInfo) -> Result<String> {
        bail!("user management is not supported by {}", self.kind().label())
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        bail!("server monitor is not supported by {}", self.kind().label())
    }

    fn kill_process(&mut self, _id: &str) -> Result<()> {
        bail!("not supported")
    }

    /// Text explain output for a statement.
    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let q = format!("{}{}", self.dialect().explain_prefix(), sql.trim().trim_end_matches(';'));
        self.query(scope, &q)
    }

    /// Row count shortcut used by tools.
    fn count(&mut self, path: &ObjPath) -> Result<i64> {
        let d = self.dialect();
        let r = self.query(Some(&path.scope()), &crate::dml::count_rows(d, path, ""))?;
        Ok(r.rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64)
    }

    /// Character sets / encodings (New Database dialog).
    fn charsets(&mut self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn collations(&mut self, _charset: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn ping(&mut self) -> Result<()> {
        self.server_version().map(|_| ())
    }
}

/// Open a driver for `cfg`. Starts an SSH tunnel first when configured.
pub fn connect(cfg: &ConnectionConfig) -> Result<Box<dyn Driver>> {
    let mut cfg = cfg.clone();
    let tunnel = if cfg.ssh.enabled && !cfg.kind.is_file_based() {
        if !crate::ssh::AVAILABLE {
            bail!("SSH tunnels are not available in this edition of NavDB Studio");
        }
        let t = crate::ssh::Tunnel::open(&cfg.ssh, &cfg.host, cfg.port)?;
        cfg.host = "127.0.0.1".into();
        cfg.port = t.local_port;
        Some(t)
    } else {
        None
    };
    let mut drv: Box<dyn Driver> = match cfg.kind.base() {
        DbKind::MySql | DbKind::MariaDb => Box::new(mysql::MySqlDriver::connect(&cfg)?),
        DbKind::PostgreSql => Box::new(postgres::PgDriver::connect(&cfg)?),
        DbKind::Sqlite => Box::new(sqlite::SqliteDriver::connect(&cfg)?),
        DbKind::Redis => Box::new(redis::RedisDriver::connect(&cfg)?),
        DbKind::SqlServer => Box::new(mssql::MsSqlDriver::connect(&cfg)?),
        #[cfg(feature = "oracle")]
        DbKind::Oracle => Box::new(oracle::OracleDriver::connect(&cfg)?),
        DbKind::MongoDb => Box::new(mongo::MongoDriver::connect(&cfg)?),
        DbKind::DuckDb | DbKind::Iceberg => Box::new(duck::DuckDriver::connect(&cfg)?),
        DbKind::Vertica => Box::new(vertica::VerticaDriver::connect(&cfg)?),
        DbKind::ClickHouse => Box::new(clickhouse::ClickHouseDriver::connect(&cfg)?),
        DbKind::Trino => Box::new(trino::TrinoDriver::connect(&cfg)?),
        DbKind::OpenSearch => Box::new(search::SearchDriver::connect(&cfg)?),
        DbKind::Cassandra => Box::new(cassandra::CassandraDriver::connect(&cfg)?),
        k => bail!("no driver for {}", k.label()),
    };
    if let Some(t) = tunnel {
        drv = Box::new(Tunneled { inner: drv, _tunnel: t });
    }
    Ok(drv)
}

/// Driver wrapper that keeps an SSH tunnel alive for the driver's lifetime.
struct Tunneled {
    inner: Box<dyn Driver>,
    _tunnel: crate::ssh::Tunnel,
}

impl Driver for Tunneled {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }
    fn server_version(&mut self) -> Result<String> {
        self.inner.server_version()
    }
    fn list_databases(&mut self) -> Result<Vec<String>> {
        self.inner.list_databases()
    }
    fn list_schemas(&mut self, db: &str) -> Result<Vec<String>> {
        self.inner.list_schemas(db)
    }
    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        self.inner.list_objects(scope, kind)
    }
    fn object_kinds(&self) -> Vec<ObjectKind> {
        self.inner.object_kinds()
    }
    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        self.inner.table_def(path)
    }
    fn columns(&mut self, path: &ObjPath) -> Result<Vec<ColumnDef>> {
        self.inner.columns(path)
    }
    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        self.inner.object_ddl(path, kind)
    }
    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        self.inner.execute(scope, sql)
    }
    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        self.inner.list_users()
    }
    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        self.inner.user_sql(old, spec)
    }
    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        self.inner.user_grants(user)
    }
    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        self.inner.drop_user_sql(user)
    }
    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        self.inner.server_snapshot()
    }
    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.inner.kill_process(id)
    }
    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        self.inner.explain(scope, sql)
    }
    fn charsets(&mut self) -> Result<Vec<String>> {
        self.inner.charsets()
    }
    fn collations(&mut self, charset: &str) -> Result<Vec<String>> {
        self.inner.collations(charset)
    }
}

/// Test a configuration (Connection form "Test Connection").
pub fn test_connection(cfg: &ConnectionConfig) -> Result<String> {
    let mut d = connect(cfg)?;
    d.server_version()
}

pub(crate) fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, u128)> {
    let start = std::time::Instant::now();
    let v = f()?;
    Ok((v, start.elapsed().as_millis()))
}

/// Parse "a, b, c" into a list.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().trim_matches('"').to_string()).filter(|x| !x.is_empty()).collect()
}
