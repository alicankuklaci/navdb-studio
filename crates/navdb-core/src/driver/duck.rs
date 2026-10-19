//! DuckDB driver. Also serves Apache Iceberg through DuckDB's `iceberg`
//! extension (REST catalogs attached with `ATTACH ... (TYPE iceberg)`, or a
//! directory of table folders exposed as views over `iceberg_scan`).
//!
//! Iceberg options (`cfg.options`): `catalog_type` (`rest` | `path`), `uri`,
//! `warehouse`, `token`, `oauth2_server_uri`, `oauth2_scope`,
//! `access_delegation_mode`, `path`, and S3 storage settings `s3_endpoint`,
//! `s3_region`, `s3_access_key`, `s3_secret_key`, `s3_url_style`.

use anyhow::{anyhow, bail, Context, Result};
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use duckdb::types::{TimeUnit, Value as DValue, ValueRef};
use duckdb::{AccessMode, Config, Connection};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Generic;
/// Catalog name used for the attached Iceberg catalog.
pub const ICEBERG_CATALOG: &str = "iceberg";

pub struct DuckDriver {
    conn: Connection,
    kind: DbKind,
    /// Iceberg "path" mode: tables are read-only views over `iceberg_scan`.
    iceberg_path: bool,
    /// Key in [`INSTANCES`] for DuckDB connections.
    instance: Option<String>,
}

/// One DuckDB database instance per file (or in-memory connection settings)
/// in this process. Every session (navigator, query tabs, tools) gets a clone
/// of it; separate instances on one file would not see each other's writes.
static INSTANCES: std::sync::Mutex<Vec<(String, Connection, usize)>> = std::sync::Mutex::new(Vec::new());

impl Drop for DuckDriver {
    fn drop(&mut self) {
        let Some(key) = &self.instance else { return };
        if let Ok(mut v) = INSTANCES.lock() {
            if let Some(i) = v.iter().position(|(k, _, _)| k == key) {
                v[i].2 -= 1;
                if v[i].2 == 0 {
                    v.remove(i);
                }
            }
        }
    }
}

fn lit(s: &str) -> String {
    D.string_literal(s)
}

fn qi(s: &str) -> String {
    D.quote_ident(s)
}

fn sql_err(e: duckdb::Error) -> anyhow::Error {
    anyhow!("{}", e)
}

impl DuckDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        if cfg.kind == DbKind::Iceberg {
            return Self::connect_iceberg(cfg);
        }
        let memory = cfg.database.is_empty() || cfg.database == ":memory:";
        // in-memory databases are shared per saved connection, files per path
        let owner = if memory { cfg.id.as_str() } else { "" };
        let key = format!("{}|{}|{:?}|{}", cfg.database, cfg.read_only, cfg.attached, owner);
        let mut pool = INSTANCES.lock().map_err(|_| anyhow!("DuckDB instance registry poisoned"))?;
        if let Some(entry) = pool.iter_mut().find(|(k, _, _)| *k == key) {
            let conn = entry.1.try_clone().map_err(sql_err)?;
            entry.2 += 1;
            return Ok(Self { conn, kind: cfg.kind, iceberg_path: false, instance: Some(key) });
        }
        let mut config = Config::default().enable_autoload_extension(true).map_err(sql_err)?;
        if cfg.read_only {
            config = config.access_mode(AccessMode::ReadOnly).map_err(sql_err)?;
        }
        let conn = if cfg.database.is_empty() || cfg.database == ":memory:" {
            Connection::open_in_memory_with_flags(config)
        } else {
            Connection::open_with_flags(&cfg.database, config)
        }
        .map_err(sql_err)
        .with_context(|| format!("opening {}", if cfg.database.is_empty() { ":memory:" } else { &cfg.database }))?;
        for (alias, file) in &cfg.attached {
            let ro = if cfg.read_only { " (READ_ONLY)" } else { "" };
            conn.execute_batch(&format!("ATTACH {} AS {}{}", lit(file), qi(alias), ro))
                .map_err(sql_err)
                .with_context(|| format!("attaching {}", file))?;
        }
        pool.push((key.clone(), conn.try_clone().map_err(sql_err)?, 1));
        Ok(Self { conn, kind: cfg.kind, iceberg_path: false, instance: Some(key) })
    }

    fn connect_iceberg(cfg: &ConnectionConfig) -> Result<Self> {
        let conn = Connection::open_in_memory_with_flags(Config::default().enable_autoload_extension(true).map_err(sql_err)?)
            .map_err(sql_err)?;
        let run = |sql: &str| conn.execute_batch(sql).map_err(sql_err).with_context(|| sql.to_string());
        run("INSTALL iceberg; LOAD iceberg;")?;
        let path_mode = cfg.option("catalog_type").eq_ignore_ascii_case("path");
        let location = if path_mode { cfg.option("path").to_string() } else { String::new() };
        let needs_httpfs = !cfg.option("s3_endpoint").is_empty()
            || !cfg.option("s3_access_key").is_empty()
            || location.contains("://");
        if needs_httpfs {
            run("INSTALL httpfs; LOAD httpfs;")?;
        }
        if let Some(secret) = s3_secret_sql(cfg) {
            run(&secret)?;
        }
        if path_mode {
            if location.is_empty() {
                bail!("Iceberg path catalog: set the `path` option to a directory of Iceberg tables");
            }
            run("SET unsafe_enable_version_guessing = true")?;
            run(&format!("ATTACH ':memory:' AS {}", qi(ICEBERG_CATALOG)))?;
            let drv = Self { conn, kind: cfg.kind, iceberg_path: true, instance: None };
            for (name, dir) in drv.iceberg_tables(&location)? {
                drv.conn
                    .execute_batch(&format!(
                        "CREATE VIEW {}.main.{} AS SELECT * FROM iceberg_scan({}, allow_moved_paths = true)",
                        qi(ICEBERG_CATALOG),
                        qi(&name),
                        lit(&dir)
                    ))
                    .map_err(sql_err)
                    .with_context(|| format!("loading Iceberg table {}", dir))?;
            }
            return Ok(drv);
        }
        if let Some(secret) = iceberg_secret_sql(cfg) {
            run(&secret)?;
        }
        run(&iceberg_attach_sql(cfg))?;
        Ok(Self { conn, kind: cfg.kind, iceberg_path: false, instance: None })
    }

    /// Table folders (name, dir) below `location`: every `*/metadata/*.metadata.json`,
    /// or `location` itself when it is a table folder.
    fn iceberg_tables(&self, location: &str) -> Result<Vec<(String, String)>> {
        let base = location.trim_end_matches('/');
        let mut out: Vec<(String, String)> = Vec::new();
        for pattern in [format!("{}/metadata/*.metadata.json", base), format!("{}/*/metadata/*.metadata.json", base)] {
            let r = self.run_one(&format!("SELECT file FROM glob({}) ORDER BY file", lit(&pattern)))?;
            for i in 0..r.rows.len() {
                let file = r.text(i, 0).replace('\\', "/");
                let Some(dir) = file.rsplit_once("/metadata/").map(|(d, _)| d.to_string()) else { continue };
                let name = dir.rsplit('/').next().unwrap_or(&dir).to_string();
                if !name.is_empty() && !out.iter().any(|(_, d)| *d == dir) {
                    out.push((name, dir));
                }
            }
        }
        Ok(out)
    }

    fn rows(&self, sql: &str) -> Result<QueryResult> {
        self.run_one(sql)
    }

    fn run_one(&self, sql: &str) -> Result<QueryResult> {
        let start = std::time::Instant::now();
        let mut st = self.conn.prepare(sql).map_err(sql_err)?;
        let changed = match st.execute([]) {
            Ok(n) => n,
            Err(e) if e.to_string().contains("not supported") && is_query_keyword(sql) => {
                // types the client library cannot decode (e.g. VARIANT): fetch as text
                drop(st);
                let mut r = self.run_one(&format!("SELECT COLUMNS(*)::VARCHAR FROM ({})", sql.trim().trim_end_matches(';')))?;
                r.sql = sql.to_string();
                return Ok(r);
            }
            Err(e) => return Err(sql_err(e)),
        };
        let ncol = st.column_count();
        let mut columns = Vec::with_capacity(ncol);
        let mut kinds = Vec::with_capacity(ncol);
        for i in 0..ncol {
            let lt = st.column_logical_type(i);
            kinds.push(lt.id());
            columns.push(ColumnMeta { name: st.column_name(i).map(|s| s.to_string()).unwrap_or_default(), type_name: type_name(&lt) });
        }
        let mut rows = Vec::new();
        {
            let mut q = st.raw_query();
            while let Some(r) = q.next().map_err(sql_err)? {
                let mut vals = Vec::with_capacity(ncol);
                for (i, k) in kinds.iter().enumerate() {
                    vals.push(match r.get_ref(i) {
                        Ok(v) => convert(v, *k),
                        Err(e) => bail!("column {}: {}", columns[i].name, e),
                    });
                }
                rows.push(vals);
            }
        }
        let elapsed_ms = start.elapsed().as_millis();
        if ncol == 0 || !is_query_statement(sql) {
            let mut affected = changed as u64;
            if affected == 0 && ncol == 1 && columns[0].name.eq_ignore_ascii_case("Count") {
                affected = rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            }
            return Ok(QueryResult { sql: sql.to_string(), affected_rows: affected, elapsed_ms, ..Default::default() });
        }
        Ok(QueryResult { sql: sql.to_string(), columns, rows, affected_rows: 0, elapsed_ms, is_query: true })
    }

    fn use_scope(&self, scope: &Scope) -> Result<()> {
        if scope.db.is_empty() {
            return Ok(());
        }
        let schema = scope.schema.as_deref().filter(|s| !s.is_empty());
        let target = match schema {
            Some(s) => format!("{}.{}", qi(&scope.db), qi(s)),
            None => qi(&scope.db),
        };
        match self.conn.execute_batch(&format!("USE {}", target)) {
            Ok(()) => Ok(()),
            // catalogs without a "main" schema (Iceberg): switch to their first schema, if any
            Err(_) if schema.is_none() => {
                let r = self.rows(&format!(
                    "SELECT schema_name FROM duckdb_schemas() WHERE database_name = {} ORDER BY schema_name LIMIT 1",
                    lit(&scope.db)
                ))?;
                if !r.rows.is_empty() {
                    self.conn.execute_batch(&format!("USE {}.{}", qi(&scope.db), qi(&r.text(0, 0)))).map_err(sql_err)?;
                }
                Ok(())
            }
            Err(e) => Err(sql_err(e)),
        }
    }

    fn schema_of(&self, p: &ObjPath) -> String {
        p.schema.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "main".into())
    }

    fn where_obj(&self, p: &ObjPath, name_col: &str) -> String {
        format!("database_name = {} AND schema_name = {} AND {} = {}", lit(&p.db), lit(&self.schema_of(p)), name_col, lit(&p.name))
    }

    fn where_scope(scope: &Scope) -> String {
        format!(
            "database_name = {} AND schema_name = {}",
            lit(&scope.db),
            lit(scope.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("main"))
        )
    }

    fn is_path_catalog(&self, db: &str) -> bool {
        self.iceberg_path && db == ICEBERG_CATALOG
    }

    fn generated_table_ddl(&mut self, path: &ObjPath) -> Result<String> {
        let def = self.table_def(path)?;
        Ok(crate::ddl::create_table(D, &def).join(";\n") + ";")
    }
}

/// `CREATE SECRET` for S3 storage settings, when any are configured.
fn s3_secret_sql(cfg: &ConnectionConfig) -> Option<String> {
    let keys = ["s3_endpoint", "s3_region", "s3_access_key", "s3_secret_key", "s3_url_style"];
    if keys.iter().all(|k| cfg.option(k).is_empty()) {
        return None;
    }
    let mut parts = vec!["TYPE s3".to_string()];
    if !cfg.option("s3_access_key").is_empty() {
        parts.push(format!("KEY_ID {}", lit(cfg.option("s3_access_key"))));
        parts.push(format!("SECRET {}", lit(cfg.option("s3_secret_key"))));
    }
    if !cfg.option("s3_region").is_empty() {
        parts.push(format!("REGION {}", lit(cfg.option("s3_region"))));
    }
    let ep = cfg.option("s3_endpoint");
    if !ep.is_empty() {
        let (ssl, host) = match ep.split_once("://") {
            Some((scheme, rest)) => (scheme.eq_ignore_ascii_case("https"), rest),
            None => (true, ep),
        };
        parts.push(format!("ENDPOINT {}", lit(host.trim_end_matches('/'))));
        parts.push(format!("USE_SSL {}", ssl));
        if cfg.option("s3_url_style").is_empty() {
            parts.push("URL_STYLE 'path'".into());
        }
    }
    if !cfg.option("s3_url_style").is_empty() {
        parts.push(format!("URL_STYLE {}", lit(cfg.option("s3_url_style"))));
    }
    Some(format!("CREATE OR REPLACE SECRET navdb_s3 ({})", parts.join(", ")))
}

fn iceberg_endpoint(cfg: &ConnectionConfig) -> String {
    if !cfg.option("uri").is_empty() {
        return cfg.option("uri").trim_end_matches('/').to_string();
    }
    let host = if cfg.host.is_empty() { "localhost" } else { &cfg.host };
    format!("{}://{}:{}", if cfg.ssl.enabled { "https" } else { "http" }, host, cfg.port)
}

/// `CREATE SECRET` (TYPE iceberg) for bearer-token or OAuth2 client-credential auth.
fn iceberg_secret_sql(cfg: &ConnectionConfig) -> Option<String> {
    let mut parts = vec!["TYPE iceberg".to_string()];
    if !cfg.option("token").is_empty() {
        parts.push(format!("TOKEN {}", lit(cfg.option("token"))));
    } else if !cfg.user.is_empty() {
        parts.push(format!("CLIENT_ID {}", lit(&cfg.user)));
        parts.push(format!("CLIENT_SECRET {}", lit(&cfg.password)));
        let oauth = if cfg.option("oauth2_server_uri").is_empty() {
            format!("{}/v1/oauth/tokens", iceberg_endpoint(cfg))
        } else {
            cfg.option("oauth2_server_uri").to_string()
        };
        parts.push(format!("OAUTH2_SERVER_URI {}", lit(&oauth)));
        if !cfg.option("oauth2_scope").is_empty() {
            parts.push(format!("OAUTH2_SCOPE {}", lit(cfg.option("oauth2_scope"))));
        }
    } else {
        return None;
    }
    Some(format!("CREATE OR REPLACE SECRET navdb_iceberg ({})", parts.join(", ")))
}

fn iceberg_attach_sql(cfg: &ConnectionConfig) -> String {
    let warehouse = if cfg.option("warehouse").is_empty() { cfg.database.as_str() } else { cfg.option("warehouse") };
    let mut opts = vec!["TYPE iceberg".to_string(), format!("ENDPOINT {}", lit(&iceberg_endpoint(cfg)))];
    if iceberg_secret_sql(cfg).is_some() {
        opts.push("SECRET navdb_iceberg".into());
    } else {
        opts.push("AUTHORIZATION_TYPE 'none'".into());
    }
    let delegation = cfg.option("access_delegation_mode");
    if !delegation.is_empty() {
        opts.push(format!("ACCESS_DELEGATION_MODE {}", lit(delegation)));
    } else if !cfg.option("s3_access_key").is_empty() {
        opts.push("ACCESS_DELEGATION_MODE 'none'".into());
    }
    if cfg.read_only {
        opts.push("READ_ONLY".into());
    }
    format!("ATTACH {} AS {} ({})", lit(warehouse), qi(ICEBERG_CATALOG), opts.join(", "))
}

/// First keyword of a statement, skipping comments and opening parentheses.
pub(crate) fn first_keyword(sql: &str) -> String {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(rest) = s.strip_prefix('(') {
            s = rest.trim_start();
        } else {
            break;
        }
    }
    s.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

fn is_query_keyword(sql: &str) -> bool {
    matches!(
        first_keyword(sql).as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "SUMMARIZE" | "PIVOT" | "UNPIVOT"
    )
}

/// Whether a statement's result set should be shown as rows (vs. an affected-row count).
pub(crate) fn is_query_statement(sql: &str) -> bool {
    let kw = first_keyword(sql);
    match kw.as_str() {
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => sql.to_ascii_uppercase().contains("RETURNING"),
        "CREATE" | "DROP" | "ALTER" | "ATTACH" | "DETACH" | "USE" | "SET" | "RESET" | "BEGIN" | "START" | "COMMIT"
        | "END" | "ROLLBACK" | "ABORT" | "CHECKPOINT" | "FORCE" | "INSTALL" | "LOAD" | "COPY" | "VACUUM" | "TRUNCATE"
        | "COMMENT" | "IMPORT" | "EXPORT" | "PREPARE" | "DEALLOCATE" | "GRANT" | "REVOKE" | "ANALYZE" => false,
        _ => true,
    }
}

fn is_write_statement(sql: &str) -> bool {
    matches!(
        first_keyword(sql).as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE" | "ALTER" | "DROP" | "CREATE" | "COPY" | "COMMENT"
    )
}

/// DuckDB SQL type name of a result column.
fn type_name(lt: &LogicalTypeHandle) -> String {
    if let Some(alias) = lt.get_alias() {
        return alias;
    }
    let id = lt.id();
    match id {
        LogicalTypeId::Boolean => "BOOLEAN".into(),
        LogicalTypeId::Tinyint => "TINYINT".into(),
        LogicalTypeId::Smallint => "SMALLINT".into(),
        LogicalTypeId::Integer => "INTEGER".into(),
        LogicalTypeId::Bigint => "BIGINT".into(),
        LogicalTypeId::UTinyint => "UTINYINT".into(),
        LogicalTypeId::USmallint => "USMALLINT".into(),
        LogicalTypeId::UInteger => "UINTEGER".into(),
        LogicalTypeId::UBigint => "UBIGINT".into(),
        LogicalTypeId::Hugeint => "HUGEINT".into(),
        LogicalTypeId::UHugeint => "UHUGEINT".into(),
        LogicalTypeId::Float => "FLOAT".into(),
        LogicalTypeId::Double => "DOUBLE".into(),
        LogicalTypeId::Decimal => format!("DECIMAL({},{})", lt.decimal_width(), lt.decimal_scale()),
        LogicalTypeId::Varchar => "VARCHAR".into(),
        LogicalTypeId::Blob => "BLOB".into(),
        LogicalTypeId::Date => "DATE".into(),
        LogicalTypeId::Time => "TIME".into(),
        LogicalTypeId::TimeNs => "TIME_NS".into(),
        LogicalTypeId::TimeTZ => "TIME WITH TIME ZONE".into(),
        LogicalTypeId::Timestamp => "TIMESTAMP".into(),
        LogicalTypeId::TimestampS => "TIMESTAMP_S".into(),
        LogicalTypeId::TimestampMs => "TIMESTAMP_MS".into(),
        LogicalTypeId::TimestampNs => "TIMESTAMP_NS".into(),
        LogicalTypeId::TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        LogicalTypeId::Interval => "INTERVAL".into(),
        LogicalTypeId::Uuid => "UUID".into(),
        LogicalTypeId::Enum => "ENUM".into(),
        LogicalTypeId::Bit => "BIT".into(),
        LogicalTypeId::Bignum => "BIGNUM".into(),
        LogicalTypeId::Geometry => "GEOMETRY".into(),
        LogicalTypeId::Variant => "VARIANT".into(),
        LogicalTypeId::SqlNull => "NULL".into(),
        LogicalTypeId::List => format!("{}[]", type_name(&lt.child(0))),
        LogicalTypeId::Array => format!("{}[]", type_name(&lt.child(0))),
        LogicalTypeId::Map => format!("MAP({}, {})", type_name(&lt.child(0)), type_name(&lt.child(1))),
        LogicalTypeId::Struct | LogicalTypeId::Union => format!(
            "{}({})",
            if id == LogicalTypeId::Struct { "STRUCT" } else { "UNION" },
            (0..lt.num_children())
                .map(|i| format!("{} {}", qi(&lt.child_name(i)), type_name(&lt.child(i))))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => format!("{:?}", other).to_ascii_uppercase(),
    }
}

fn unit_to_micros(unit: TimeUnit, v: i64) -> (i64, u32) {
    // (seconds, nanoseconds)
    let (secs, nanos) = match unit {
        TimeUnit::Second => (v, 0),
        TimeUnit::Millisecond => (v.div_euclid(1_000), v.rem_euclid(1_000) * 1_000_000),
        TimeUnit::Microsecond => (v.div_euclid(1_000_000), v.rem_euclid(1_000_000) * 1_000),
        TimeUnit::Nanosecond => (v.div_euclid(1_000_000_000), v.rem_euclid(1_000_000_000)),
    };
    (secs, nanos as u32)
}

fn frac(nanos: u32) -> String {
    if nanos == 0 {
        return String::new();
    }
    let s = if nanos.is_multiple_of(1000) { format!("{:06}", nanos / 1000) } else { format!("{:09}", nanos) };
    format!(".{}", s.trim_end_matches('0'))
}

pub(crate) fn fmt_date(days: i32) -> String {
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|d| d.checked_add_signed(chrono::Duration::days(days as i64)))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| days.to_string())
}

pub(crate) fn fmt_timestamp(unit: TimeUnit, v: i64, tz: bool) -> String {
    let (secs, nanos) = unit_to_micros(unit, v);
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(dt) => format!("{}{}{}", dt.format("%Y-%m-%d %H:%M:%S"), frac(nanos), if tz { "+00" } else { "" }),
        None => v.to_string(),
    }
}

pub(crate) fn fmt_time(unit: TimeUnit, v: i64) -> String {
    let (secs, nanos) = unit_to_micros(unit, v);
    format!("{:02}:{:02}:{:02}{}", secs / 3600, secs / 60 % 60, secs % 60, frac(nanos))
}

pub(crate) fn fmt_interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    let (y, m) = (months / 12, months % 12);
    let plural = |n: i64, unit: &str| format!("{} {}{}", n, unit, if n.abs() == 1 { "" } else { "s" });
    if y != 0 {
        parts.push(plural(y as i64, "year"));
    }
    if m != 0 {
        parts.push(plural(m as i64, "month"));
    }
    if days != 0 {
        parts.push(plural(days as i64, "day"));
    }
    if nanos != 0 || parts.is_empty() {
        let neg = nanos < 0;
        let n = nanos.unsigned_abs();
        let secs = n / 1_000_000_000;
        parts.push(format!(
            "{}{:02}:{:02}:{:02}{}",
            if neg { "-" } else { "" },
            secs / 3600,
            secs / 60 % 60,
            secs % 60,
            frac((n % 1_000_000_000) as u32)
        ));
    }
    parts.join(" ")
}

fn fmt_uuid(b: &[u8]) -> Option<String> {
    uuid::Uuid::from_slice(b).ok().map(|u| u.to_string())
}

/// Convert one result cell. `kind` is the column's logical type.
fn convert(v: ValueRef<'_>, kind: LogicalTypeId) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::Int(i as i64),
        ValueRef::SmallInt(i) => Value::Int(i as i64),
        ValueRef::Int(i) => Value::Int(i as i64),
        ValueRef::BigInt(i) => Value::Int(i),
        ValueRef::HugeInt(i) => i64::try_from(i).map(Value::Int).unwrap_or_else(|_| Value::Text(i.to_string())),
        ValueRef::UHugeInt(i) => u64::try_from(i).map(Value::UInt).unwrap_or_else(|_| Value::Text(i.to_string())),
        ValueRef::UTinyInt(i) => Value::Int(i as i64),
        ValueRef::USmallInt(i) => Value::Int(i as i64),
        ValueRef::UInt(i) => Value::Int(i as i64),
        ValueRef::UBigInt(i) => Value::UInt(i),
        ValueRef::Float(f) => Value::Float(f as f64),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Decimal(d) => Value::Text(d.to_string()),
        ValueRef::Timestamp(u, t) => Value::Text(fmt_timestamp(u, t, kind == LogicalTypeId::TimestampTZ)),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) if kind == LogicalTypeId::Uuid => fmt_uuid(b).map(Value::Text).unwrap_or_else(|| Value::Bytes(b.to_vec())),
        ValueRef::Blob(b) | ValueRef::Geometry(b) => Value::Bytes(b.to_vec()),
        ValueRef::Date32(d) => Value::Text(fmt_date(d)),
        ValueRef::Time64(u, t) => Value::Text(fmt_time(u, t)),
        ValueRef::Interval { months, days, nanos } => Value::Text(fmt_interval(months, days, nanos)),
        ValueRef::Enum(..) => Value::Text(v.as_str().map(|s| s.to_string()).unwrap_or_default()),
        other => {
            let owned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| other.to_owned()));
            match owned {
                Ok(o) => Value::Text(json_of(&o).to_string()),
                Err(_) => Value::Text("(undecodable value)".into()),
            }
        }
    }
}

/// Compact JSON for nested values (LIST, STRUCT, MAP, ARRAY, UNION).
fn json_of(v: &DValue) -> serde_json::Value {
    use serde_json::Value as J;
    let scalar = |v: Value| match v {
        Value::Null => J::Null,
        Value::Bytes(b) => J::String(crate::value::hex(&b)),
        other => other.to_json(),
    };
    match v {
        DValue::List(items) | DValue::Array(items) => J::Array(items.iter().map(json_of).collect()),
        DValue::Struct(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), json_of(v))).collect()),
        DValue::Map(m) => {
            if m.keys().all(|k| matches!(k, DValue::Text(_) | DValue::Enum(_))) {
                J::Object(m.iter().map(|(k, v)| (json_key(k), json_of(v))).collect())
            } else {
                J::Array(m.iter().map(|(k, v)| serde_json::json!({"key": json_of(k), "value": json_of(v)})).collect())
            }
        }
        DValue::Union(inner) => json_of(inner),
        DValue::Null => J::Null,
        DValue::Boolean(b) => J::Bool(*b),
        DValue::TinyInt(i) => (*i).into(),
        DValue::SmallInt(i) => (*i).into(),
        DValue::Int(i) => (*i).into(),
        DValue::BigInt(i) => (*i).into(),
        DValue::UTinyInt(i) => (*i).into(),
        DValue::USmallInt(i) => (*i).into(),
        DValue::UInt(i) => (*i).into(),
        DValue::UBigInt(i) => (*i).into(),
        DValue::HugeInt(i) => i64::try_from(*i).map(J::from).unwrap_or_else(|_| J::String(i.to_string())),
        DValue::UHugeInt(i) => u64::try_from(*i).map(J::from).unwrap_or_else(|_| J::String(i.to_string())),
        DValue::Float(f) => scalar(Value::Float(*f as f64)),
        DValue::Double(f) => scalar(Value::Float(*f)),
        DValue::Decimal(d) => J::String(d.to_string()),
        DValue::Timestamp(u, t) => J::String(fmt_timestamp(*u, *t, false)),
        DValue::Text(s) | DValue::Enum(s) => J::String(s.clone()),
        DValue::Blob(b) => {
            if b.len() == 16 {
                // nested UUIDs arrive as 16 raw bytes
                fmt_uuid(b).map(J::String).unwrap_or_else(|| J::String(crate::value::hex(b)))
            } else {
                scalar(Value::Bytes(b.clone()))
            }
        }
        DValue::Geometry(b) => scalar(Value::Bytes(b.clone())),
        DValue::Date32(d) => J::String(fmt_date(*d)),
        DValue::Time64(u, t) => J::String(fmt_time(*u, *t)),
        DValue::Interval { months, days, nanos } => J::String(fmt_interval(*months, *days, *nanos)),
        other => J::String(format!("{:?}", other)),
    }
}

fn json_key(v: &DValue) -> String {
    match json_of(v) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Items of a list cell decoded as JSON text (e.g. `constraint_column_names`).
fn list_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Text(s)) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Array(a)) => a
                .into_iter()
                .map(|x| match x {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                })
                .collect(),
            _ => parse_bracket_list(s),
        },
        _ => vec![],
    }
}

/// `[a, "b c"]` → `["a", "b c"]` (index expressions are rendered like this).
pub(crate) fn parse_bracket_list(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|x| {
            let t = x.trim().trim_matches('\'');
            if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
                t[1..t.len() - 1].replace("\"\"", "\"")
            } else {
                t.to_string()
            }
        })
        .filter(|x| !x.is_empty())
        .collect()
}

impl Driver for DuckDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.rows("SELECT version()")?;
        let mut v = format!("DuckDB {}", r.text(0, 0));
        if self.kind == DbKind::Iceberg {
            let e = self.rows("SELECT extension_version FROM duckdb_extensions() WHERE extension_name = 'iceberg'")?;
            v.push_str(&format!(", iceberg extension {}", e.text(0, 0)));
        }
        Ok(v)
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut sql = "SELECT database_name FROM duckdb_databases() WHERE NOT internal AND database_name NOT IN ('system', 'temp')".to_string();
        if self.kind == DbKind::Iceberg {
            sql.push_str(" AND database_name <> 'memory'");
        }
        sql.push_str(" ORDER BY database_name");
        let r = self.rows(&sql)?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn list_schemas(&mut self, db: &str) -> Result<Vec<String>> {
        let r = self.rows(&format!(
            "SELECT DISTINCT schema_name FROM duckdb_schemas() WHERE database_name = {} AND schema_name NOT IN ('information_schema', 'pg_catalog') ORDER BY schema_name",
            lit(db)
        ))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        if self.kind == DbKind::Iceberg {
            vec![ObjectKind::Table, ObjectKind::View]
        } else {
            vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Function, ObjectKind::Sequence, ObjectKind::Index]
        }
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let w = Self::where_scope(scope);
        let path_cat = self.is_path_catalog(&scope.db);
        let mut out = Vec::new();
        match kind {
            ObjectKind::Table if path_cat => {
                let r = self.rows(&format!("SELECT view_name FROM duckdb_views() WHERE {} AND NOT internal ORDER BY 1", w))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.engine = "Iceberg".into();
                    out.push(info);
                }
            }
            ObjectKind::Table => {
                let r = self.rows(&format!(
                    "SELECT table_name, estimated_size, column_count, comment, has_primary_key, temporary FROM duckdb_tables() WHERE {} ORDER BY 1",
                    w
                ))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.rows = r.rows[i][1].as_f64().map(|v| v as i64);
                    info.comment = r.text(i, 3);
                    info.engine = if self.kind == DbKind::Iceberg { "Iceberg".into() } else { String::new() };
                    info.extra.push(("Columns".into(), r.text(i, 2)));
                    info.extra.push(("Primary Key".into(), r.text(i, 4)));
                    if r.text(i, 5) == "true" {
                        info.extra.push(("Temporary".into(), "true".into()));
                    }
                    out.push(info);
                }
            }
            ObjectKind::View if path_cat => {}
            ObjectKind::View => {
                let r = self.rows(&format!(
                    "SELECT view_name, comment, column_count FROM duckdb_views() WHERE {} AND NOT internal ORDER BY 1",
                    w
                ))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.comment = r.text(i, 1);
                    info.extra.push(("Columns".into(), r.text(i, 2)));
                    out.push(info);
                }
            }
            ObjectKind::Function => {
                let r = self.rows(&format!(
                    "SELECT function_name, function_type, string_agg(array_to_string(parameters, ', '), ' | '), first(comment) FROM duckdb_functions() WHERE {} AND NOT internal GROUP BY 1, 2 ORDER BY 1",
                    w
                ))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.comment = r.text(i, 3);
                    info.extra.push(("Type".into(), r.text(i, 1)));
                    info.extra.push(("Parameters".into(), r.text(i, 2)));
                    out.push(info);
                }
            }
            ObjectKind::Sequence => {
                let r = self.rows(&format!(
                    "SELECT sequence_name, start_value, increment_by, last_value, comment FROM duckdb_sequences() WHERE {} ORDER BY 1",
                    w
                ))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.comment = r.text(i, 4);
                    info.extra.push(("Start".into(), r.text(i, 1)));
                    info.extra.push(("Increment".into(), r.text(i, 2)));
                    info.extra.push(("Last Value".into(), r.text(i, 3)));
                    out.push(info);
                }
            }
            ObjectKind::Index => {
                let r = self.rows(&format!(
                    "SELECT index_name, table_name, is_unique, comment FROM duckdb_indexes() WHERE {} ORDER BY 1",
                    w
                ))?;
                for i in 0..r.rows.len() {
                    let mut info = ObjectInfo::named(r.text(i, 0));
                    info.comment = r.text(i, 3);
                    info.extra.push(("Table".into(), r.text(i, 1)));
                    info.extra.push(("Unique".into(), r.text(i, 2)));
                    out.push(info);
                }
            }
            _ => {}
        }
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let r = self.rows(&format!(
            "SELECT column_name, data_type, is_nullable, column_default, comment FROM duckdb_columns() WHERE {} ORDER BY column_index",
            self.where_obj(path, "table_name")
        ))?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let mut columns: Vec<ColumnDef> = (0..r.rows.len())
            .map(|i| {
                let default = match &r.rows[i][3] {
                    Value::Null => None,
                    v => Some(v.to_string()),
                };
                ColumnDef {
                    name: r.text(i, 0),
                    data_type: r.text(i, 1),
                    nullable: r.text(i, 2) != "false",
                    auto_increment: default.as_deref().is_some_and(|d| d.to_ascii_lowercase().starts_with("nextval(")),
                    default,
                    comment: r.text(i, 4),
                    ..Default::default()
                }
            })
            .collect();
        let mut indexes = Vec::new();
        let mut foreign_keys = Vec::new();
        let mut checks = Vec::new();
        let is_table = !self.is_path_catalog(&path.db)
            && !self.rows(&format!("SELECT 1 FROM duckdb_tables() WHERE {}", self.where_obj(path, "table_name")))?.rows.is_empty();
        if is_table {
            let c = self.rows(&format!(
                "SELECT constraint_type, constraint_column_names, constraint_name, expression, referenced_table, referenced_column_names FROM duckdb_constraints() WHERE {} ORDER BY constraint_index",
                self.where_obj(path, "table_name")
            ))?;
            for i in 0..c.rows.len() {
                let cols = list_of(c.rows[i].get(1));
                let name = c.text(i, 2);
                match c.text(i, 0).as_str() {
                    "PRIMARY KEY" => {
                        for col in columns.iter_mut().filter(|x| cols.contains(&x.name)) {
                            col.primary_key = true;
                        }
                        indexes.push(IndexDef { name: "PRIMARY".into(), columns: cols, primary: true, unique: true, ..Default::default() });
                    }
                    "UNIQUE" => indexes.push(IndexDef {
                        name: if name.is_empty() { format!("ux_{}_{}", path.name, cols.join("_")) } else { name },
                        columns: cols,
                        unique: true,
                        method: "CONSTRAINT".into(),
                        ..Default::default()
                    }),
                    "FOREIGN KEY" => foreign_keys.push(ForeignKeyDef {
                        name: if name.is_empty() { format!("fk_{}_{}", path.name, cols.join("_")) } else { name },
                        columns: cols,
                        ref_db: path.db.clone(),
                        ref_schema: Some(self.schema_of(path)),
                        ref_table: c.text(i, 4),
                        ref_columns: list_of(c.rows[i].get(5)),
                        on_update: String::new(),
                        on_delete: String::new(),
                    }),
                    "CHECK" => checks.push(CheckDef {
                        name: if name.is_empty() { format!("ck_{}_{}", path.name, checks.len() + 1) } else { name },
                        expression: c.text(i, 3),
                    }),
                    _ => {}
                }
            }
            let ix = self.rows(&format!(
                "SELECT index_name, is_unique, is_primary, expressions, comment FROM duckdb_indexes() WHERE {} ORDER BY 1",
                self.where_obj(path, "table_name")
            ))?;
            for i in 0..ix.rows.len() {
                if ix.text(i, 2) == "true" {
                    continue;
                }
                indexes.push(IndexDef {
                    name: ix.text(i, 0),
                    columns: list_of(ix.rows[i].get(3)).into_iter().map(|c| c.trim_matches('"').to_string()).collect(),
                    unique: ix.text(i, 1) == "true",
                    comment: ix.text(i, 4),
                    ..Default::default()
                });
            }
        }
        let comment = if is_table {
            self.rows(&format!("SELECT comment FROM duckdb_tables() WHERE {}", self.where_obj(path, "table_name")))?.text(0, 0)
        } else {
            String::new()
        };
        Ok(TableDef { path: path.clone(), columns, indexes, foreign_keys, checks, comment, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let w = |c: &str| self.where_obj(path, c);
        let kind = if self.is_path_catalog(&path.db) && kind == ObjectKind::Table { ObjectKind::View } else { kind };
        let r = match kind {
            ObjectKind::Table => {
                let r = self.rows(&format!("SELECT sql FROM duckdb_tables() WHERE {}", w("table_name")))?;
                if r.rows.is_empty() || r.text(0, 0).trim().is_empty() {
                    return self.generated_table_ddl(path);
                }
                let mut out = r.text(0, 0).trim().trim_end_matches(';').to_string() + ";";
                let ix = self.rows(&format!("SELECT sql FROM duckdb_indexes() WHERE {} AND sql IS NOT NULL ORDER BY index_name", w("table_name")))?;
                for i in 0..ix.rows.len() {
                    out.push_str(&format!("\n{};", ix.text(i, 0).trim().trim_end_matches(';')));
                }
                return Ok(out);
            }
            ObjectKind::View => self.rows(&format!("SELECT sql FROM duckdb_views() WHERE {}", w("view_name")))?,
            ObjectKind::Index => self.rows(&format!("SELECT sql FROM duckdb_indexes() WHERE {}", w("index_name")))?,
            ObjectKind::Sequence => self.rows(&format!("SELECT sql FROM duckdb_sequences() WHERE {}", w("sequence_name")))?,
            ObjectKind::Function => {
                let f = self.rows(&format!(
                    "SELECT function_type, array_to_string(parameters, ', '), macro_definition FROM duckdb_functions() WHERE {} AND NOT internal",
                    w("function_name")
                ))?;
                if f.rows.is_empty() {
                    bail!("function {} not found", path.display());
                }
                let defs = (0..f.rows.len())
                    .map(|i| {
                        let table = if f.text(i, 0).contains("table") { "TABLE " } else { "" };
                        format!("({}) AS {}{}", f.text(i, 1), table, f.text(i, 2))
                    })
                    .collect::<Vec<_>>();
                return Ok(format!("CREATE MACRO {}\n    {};", D.qualify(path), defs.join(",\n    ")));
            }
            _ => bail!("{} DDL is not available for DuckDB", kind.singular()),
        };
        if r.rows.is_empty() {
            bail!("object {} not found", path.display());
        }
        Ok(r.text(0, 0).trim().trim_end_matches(';').to_string() + ";")
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope {
            self.use_scope(s)?;
        }
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            if self.iceberg_path && is_write_statement(&stmt) {
                bail!("the Iceberg path catalog is read-only; use a REST catalog to modify tables");
            }
            out.push(self.run_one(&stmt)?);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let r = self.query(scope, &format!("EXPLAIN {}", sql.trim().trim_end_matches(';')))?;
        let vi = r.column_index("explain_value").unwrap_or(r.columns.len().saturating_sub(1));
        let mut out = QueryResult {
            sql: r.sql.clone(),
            columns: vec![ColumnMeta::new("QUERY PLAN")],
            is_query: true,
            elapsed_ms: r.elapsed_ms,
            ..Default::default()
        };
        for i in 0..r.rows.len() {
            for line in r.text(i, vi).lines() {
                out.rows.push(vec![Value::Text(line.to_string())]);
            }
        }
        Ok(out)
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let variables = self.rows("SELECT name AS \"Variable_name\", value AS \"Value\", description AS \"Description\" FROM duckdb_settings() ORDER BY name")?;
        let mut status = QueryResult {
            columns: vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            is_query: true,
            ..Default::default()
        };
        if let Ok(r) = self.rows("SELECT * FROM pragma_database_size()") {
            for i in 0..r.rows.len() {
                let db = r.text(i, 0);
                for (c, col) in r.columns.iter().enumerate().skip(1) {
                    status.rows.push(vec![Value::Text(format!("{}.{}", db, col.name)), r.rows[i][c].clone()]);
                }
            }
        }
        if let Ok(r) = self.rows("SELECT tag, memory_usage_bytes FROM duckdb_memory() WHERE memory_usage_bytes > 0 ORDER BY 1") {
            for i in 0..r.rows.len() {
                status.rows.push(vec![Value::Text(format!("memory.{}", r.text(i, 0))), r.rows[i][1].clone()]);
            }
        }
        Ok(ServerSnapshot { processes: QueryResult::default(), variables, status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dml;

    fn mem() -> DuckDriver {
        DuckDriver::connect(&ConnectionConfig::new(DbKind::DuckDb)).unwrap()
    }

    #[test]
    fn keywords() {
        assert_eq!(first_keyword("  -- c\n /* x */ (select 1)"), "SELECT");
        assert!(is_query_statement("with a as (select 1) select * from a"));
        assert!(!is_query_statement("insert into t values (1)"));
        assert!(is_query_statement("insert into t values (1) returning *"));
        assert!(!is_query_statement("CREATE TABLE t(x int)"));
        assert!(is_write_statement("update t set x = 1"));
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_time(TimeUnit::Microsecond, 3_723_500_000), "01:02:03.5");
        assert_eq!(fmt_timestamp(TimeUnit::Microsecond, 1_000_000, true), "1970-01-01 00:00:01+00");
        assert_eq!(fmt_interval(14, 3, 3_600_000_000_000), "1 year 2 months 3 days 01:00:00");
        assert_eq!(fmt_interval(0, 0, 0), "00:00:00");
        assert_eq!(parse_bracket_list("[a, \"b c\"]"), vec!["a", "b c"]);
    }

    #[test]
    fn types_and_values() {
        let mut d = mem();
        let r = d
            .query(
                None,
                "SELECT 1::TINYINT a, 18446744073709551615::UBIGINT b, 170141183460469231731687303715884105727::HUGEINT c, 12.50::DECIMAL(10,2) e,
                 DATE '2024-02-29' f, TIME '12:34:56.789' g, TIMESTAMP '2024-01-02 03:04:05.123456' h, INTERVAL 90 MINUTE i,
                 '5b2a1e4c-6a5f-4d9e-8f0e-2b3c4d5e6f70'::UUID j, '\\xAA\\xBB'::BLOB k, [1, 2, NULL] l, {'x': 1, 'y': 'z'} m,
                 MAP {'k': [1]} n, [1, 2]::INTEGER[2] o, 'v'::ENUM_T p, 1.5::DOUBLE q, true r, NULL::VARCHAR s",
            )
            .err();
        assert!(r.is_some(), "ENUM_T is undefined");
        d.execute(None, "CREATE TYPE ENUM_T AS ENUM ('v', 'w')").unwrap();
        let r = d
            .query(
                None,
                "SELECT 1::TINYINT a, 18446744073709551615::UBIGINT b, 170141183460469231731687303715884105727::HUGEINT c, 12.50::DECIMAL(10,2) e,
                 DATE '2024-02-29' f, TIME '12:34:56.789' g, TIMESTAMP '2024-01-02 03:04:05.123456' h, INTERVAL 90 MINUTE i,
                 '5b2a1e4c-6a5f-4d9e-8f0e-2b3c4d5e6f70'::UUID j, '\\xAA\\xBB'::BLOB k, [1, 2, NULL] l, {'x': 1, 'y': 'z'} m,
                 MAP {'k': [1]} n, [1, 2]::INTEGER[2] o, 'v'::ENUM_T p, 1.5::DOUBLE q, true r, NULL::VARCHAR s, TIMESTAMPTZ '2024-01-02 03:04:05+00' t",
            )
            .unwrap();
        let row = &r.rows[0];
        let types: Vec<&str> = r.columns.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(row[0], Value::Int(1));
        assert_eq!(row[1], Value::UInt(u64::MAX));
        assert_eq!(row[2], Value::Text("170141183460469231731687303715884105727".into()));
        assert_eq!(types[2], "HUGEINT");
        assert_eq!(row[3], Value::Text("12.50".into()));
        assert_eq!(types[3], "DECIMAL(10,2)");
        assert_eq!(row[4], Value::Text("2024-02-29".into()));
        assert_eq!(row[5], Value::Text("12:34:56.789".into()));
        assert_eq!(row[6], Value::Text("2024-01-02 03:04:05.123456".into()));
        assert_eq!(row[7], Value::Text("01:30:00".into()));
        assert_eq!(row[8], Value::Text("5b2a1e4c-6a5f-4d9e-8f0e-2b3c4d5e6f70".into()));
        assert_eq!(types[8], "UUID");
        assert_eq!(row[9], Value::Bytes(vec![0xAA, 0xBB]));
        assert_eq!(row[10], Value::Text("[1,2,null]".into()));
        assert_eq!(types[10], "INTEGER[]");
        assert_eq!(row[11], Value::Text("{\"x\":1,\"y\":\"z\"}".into()));
        assert_eq!(row[12], Value::Text("{\"k\":[1]}".into()));
        assert_eq!(row[13], Value::Text("[1,2]".into()));
        assert_eq!(row[14], Value::Text("v".into()));
        assert_eq!(row[15], Value::Float(1.5));
        assert_eq!(row[16], Value::Bool(true));
        assert_eq!(row[17], Value::Null);
        assert_eq!(row[18], Value::Text("2024-01-02 03:04:05+00".into()));
        assert_eq!(types[18], "TIMESTAMP WITH TIME ZONE");
    }

    #[test]
    fn crud_without_primary_key() {
        let mut d = mem();
        let s = Scope::new("memory", Some("main".into()));
        d.execute(Some(&s), "CREATE TABLE np (a INTEGER, b VARCHAR, c DOUBLE); INSERT INTO np VALUES (1, 'x', NULL), (1, 'x', NULL), (2, 'y', 2.5)")
            .unwrap();
        let p = s.path("np");
        let def = d.table_def(&p).unwrap();
        assert!(def.primary_key().is_empty());
        let r = d.execute(Some(&s), &dml::update_row(D, &p, &[("b".into(), Value::Text("z".into()))], &[
            ("a".into(), Value::Int(2)),
            ("b".into(), Value::Text("y".into())),
            ("c".into(), Value::Float(2.5)),
        ]))
        .unwrap();
        assert_eq!(r[0].affected_rows, 1);
        let r = d
            .execute(Some(&s), &dml::delete_row(D, &p, &[("a".into(), Value::Int(1)), ("b".into(), Value::Text("x".into())), ("c".into(), Value::Null)]))
            .unwrap();
        assert_eq!(r[0].affected_rows, 2);
        assert_eq!(d.count(&p).unwrap(), 1);
    }
}
