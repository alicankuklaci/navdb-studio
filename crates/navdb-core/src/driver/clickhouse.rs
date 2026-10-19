//! ClickHouse over the HTTP interface (stateless requests via ureq).
//!
//! Options: `path` – URL prefix when the server sits behind a proxy (e.g. `/clickhouse`).

use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Generic;
const BODY_LIMIT: u64 = 1 << 30;

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub struct ClickHouseDriver {
    cfg: ConnectionConfig,
    agent: ureq::Agent,
    url: String,
    db: String,
}

impl ClickHouseDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut tls = ureq::tls::TlsConfig::builder().disable_verification(cfg.ssl.enabled && !cfg.ssl.verify_server);
        if cfg.ssl.enabled && !cfg.ssl.ca_cert.is_empty() {
            let pem = std::fs::read(&cfg.ssl.ca_cert).context("reading CA certificate")?;
            let certs = ureq::tls::parse_pem(&pem)
                .filter_map(|p| match p {
                    Ok(ureq::tls::PemItem::Certificate(c)) => Some(c.to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&certs));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(cfg.connect_timeout_secs.max(1))))
            .tls_config(tls.build())
            .build()
            .into();
        let path = cfg.option("path").trim().trim_matches('/');
        let url = format!(
            "{}://{}:{}/{}",
            if cfg.ssl.enabled { "https" } else { "http" },
            cfg.host,
            cfg.port,
            if path.is_empty() { String::new() } else { format!("{}/", path) }
        );
        let db = if cfg.database.is_empty() { "default".to_string() } else { cfg.database.clone() };
        let mut d = Self { cfg: cfg.clone(), agent, url, db };
        d.run("SELECT 1", &[]).with_context(|| format!("connecting to {}", d.url))?;
        Ok(d)
    }

    /// Run one statement; returns the parsed result.
    fn run(&mut self, sql: &str, settings: &[(&str, &str)]) -> Result<QueryResult> {
        let db = self.db.clone();
        self.request(Some(&db), sql, settings)
    }

    /// Catalog query, independent of the current database.
    fn rows(&mut self, sql: &str) -> Result<QueryResult> {
        self.request(None, sql, &[])
    }

    fn request(&mut self, db: Option<&str>, sql: &str, settings: &[(&str, &str)]) -> Result<QueryResult> {
        let start = std::time::Instant::now();
        let mut req = self.agent.post(&self.url);
        if let Some(db) = db {
            req = req.query("database", db);
        }
        let mut req = req
            .query("default_format", "JSONCompact")
            .query("wait_end_of_query", "1")
            .query("output_format_json_quote_64bit_integers", "1")
            .query("output_format_json_quote_decimals", "1")
            .query("output_format_json_quote_denormals", "1")
            .query("output_format_decimal_trailing_zeros", "1")
            .header("X-ClickHouse-User", &self.cfg.user)
            .header("X-ClickHouse-Key", &self.cfg.password);
        if self.cfg.read_only {
            req = req.query("readonly", "2");
        }
        for (k, v) in settings {
            req = req.query(*k, *v);
        }
        let mut resp = req.send(sql).map_err(|e| anyhow!("ClickHouse request failed: {}", e))?;
        let status = resp.status();
        let header = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let format = header("X-ClickHouse-Format");
        let summary = header("X-ClickHouse-Summary");
        let body = resp.body_mut().with_config().limit(BODY_LIMIT).read_to_string().map_err(|e| anyhow!("{}", e))?;
        if !status.is_success() {
            let msg = body.trim();
            bail!("{}", if msg.is_empty() { status.to_string() } else { msg.to_string() });
        }
        let mut r = match format.as_deref() {
            Some("JSONCompact") => parse_json_compact(&body)?,
            Some(_) => QueryResult {
                columns: vec![ColumnMeta { name: "result".into(), type_name: "String".into() }],
                rows: body.lines().map(|l| vec![Value::Text(l.to_string())]).collect(),
                is_query: true,
                ..Default::default()
            },
            None => QueryResult { affected_rows: summary.as_deref().map(written_rows).unwrap_or(0), ..Default::default() },
        };
        r.sql = sql.to_string();
        r.elapsed_ms = start.elapsed().as_millis();
        Ok(r)
    }
}

fn written_rows(summary: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(summary)
        .ok()
        .and_then(|v| v.get("written_rows").cloned())
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()))
        .unwrap_or(0)
}

fn parse_json_compact(body: &str) -> Result<QueryResult> {
    let v: serde_json::Value = serde_json::from_str(body).context("parsing ClickHouse response")?;
    let columns: Vec<ColumnMeta> = v
        .get("meta")
        .and_then(|m| m.as_array())
        .map(|m| {
            m.iter()
                .map(|c| ColumnMeta {
                    name: c.get("name").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                    type_name: c.get("type").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|d| {
            d.iter()
                .map(|row| {
                    let cells = row.as_array().map(|a| a.as_slice()).unwrap_or_default();
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| cells.get(i).map(|x| convert(&c.type_name, x)).unwrap_or(Value::Null))
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(QueryResult { columns, rows, is_query: true, ..Default::default() })
}

/// `Nullable(LowCardinality(String))` -> `String`.
fn base_type(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("]
            .iter()
            .find_map(|p| t.strip_prefix(p).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

fn convert(type_name: &str, v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    if v.is_null() {
        return Value::Null;
    }
    let t = base_type(type_name);
    let name = t.split('(').next().unwrap_or(t);
    match name {
        "Int8" | "Int16" | "Int32" | "Int64" => match v {
            J::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Text(n.to_string())),
            J::String(s) => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.clone())),
            other => Value::Text(other.to_string()),
        },
        "UInt8" | "UInt16" | "UInt32" | "UInt64" => match v {
            J::Number(n) => n.as_u64().map(Value::UInt).unwrap_or_else(|| Value::Text(n.to_string())),
            J::String(s) => s.parse().map(Value::UInt).unwrap_or_else(|_| Value::Text(s.clone())),
            other => Value::Text(other.to_string()),
        },
        "Float32" | "Float64" | "BFloat16" => match v {
            J::Number(n) => Value::Float(n.as_f64().unwrap_or_default()),
            J::String(s) => s.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(s.clone())),
            other => Value::Text(other.to_string()),
        },
        "Bool" => match v {
            J::Bool(b) => Value::Bool(*b),
            other => Value::Text(other.to_string()),
        },
        _ => match v {
            J::String(s) => Value::Text(s.clone()),
            J::Number(n) => Value::Text(n.to_string()),
            J::Bool(b) => Value::Bool(*b),
            other => Value::Text(other.to_string()),
        },
    }
}

/// Parse a (possibly quoted, dot-separated) identifier at the start of `s`;
/// returns (identifier text, rest).
fn take_ident(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        match b.get(i)? {
            q @ (b'"' | b'`') => {
                i += 1;
                loop {
                    let c = *b.get(i)?;
                    i += 1;
                    if c == b'\\' {
                        i += 1;
                    } else if c == *q {
                        if b.get(i) == Some(q) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            c if c.is_ascii_alphanumeric() || *c == b'_' => {
                while b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                    i += 1;
                }
            }
            _ => return None,
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
        } else {
            return Some((&s[..i], &s[i..]));
        }
    }
}

fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let head = s.get(..kw.len())?;
    let rest = &s[kw.len()..];
    (head.eq_ignore_ascii_case(kw) && rest.starts_with(|c: char| c.is_whitespace())).then_some(rest)
}

/// `UPDATE t SET ... WHERE ...` -> `ALTER TABLE t UPDATE ... WHERE ...` (mutation).
fn rewrite_update(stmt: &str) -> Option<String> {
    let rest = strip_keyword(crate::sqlutil::strip_leading_comments(stmt), "UPDATE")?;
    let (table, rest) = take_ident(rest.trim_start())?;
    let assignments = strip_keyword(rest, "SET")?;
    Some(format!("ALTER TABLE {} UPDATE {}", table, assignments.trim()))
}

/// Target database of `USE db`.
fn use_target(stmt: &str) -> Option<String> {
    let rest = strip_keyword(crate::sqlutil::strip_leading_comments(stmt), "USE")?.trim();
    let (ident, tail) = take_ident(rest)?;
    if !tail.trim().trim_end_matches(';').is_empty() {
        return None;
    }
    Some(unquote(ident))
}

fn unquote(s: &str) -> String {
    if s.len() >= 2 && (s.starts_with('"') && s.ends_with('"') || s.starts_with('`') && s.ends_with('`')) {
        let q = &s[..1];
        s[1..s.len() - 1].replace(&format!("{q}{q}"), q)
    } else {
        s.to_string()
    }
}

/// Database dropped by `DROP DATABASE [IF EXISTS] db`.
fn dropped_db(stmt: &str) -> Option<String> {
    let rest = strip_keyword(crate::sqlutil::strip_leading_comments(stmt), "DROP")?;
    let mut rest = strip_keyword(rest, "DATABASE")?.trim_start();
    if let Some(r) = strip_keyword(rest, "IF").and_then(|r| strip_keyword(r, "EXISTS")) {
        rest = r.trim_start();
    }
    take_ident(rest).map(|(i, _)| unquote(i))
}

fn is_delete(stmt: &str) -> bool {
    strip_keyword(crate::sqlutil::strip_leading_comments(stmt), "DELETE").is_some()
}

impl Driver for ClickHouseDriver {
    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.rows("SELECT version()")?;
        Ok(format!("ClickHouse {}", r.text(0, 0)))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.rows(
            "SELECT name FROM system.databases ORDER BY name IN ('INFORMATION_SCHEMA', 'information_schema', 'system'), name",
        )?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView, ObjectKind::Function]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let filter = match kind {
            ObjectKind::Table => "engine NOT IN ('View', 'MaterializedView', 'LiveView', 'WindowView')",
            ObjectKind::View => "engine IN ('View', 'LiveView', 'WindowView')",
            ObjectKind::MaterializedView => "engine = 'MaterializedView'",
            ObjectKind::Function => {
                let r = self.rows(
                    "SELECT name, create_query FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name",
                )?;
                return Ok((0..r.rows.len())
                    .map(|i| ObjectInfo { extra: vec![("Definition".into(), r.text(i, 1))], ..ObjectInfo::named(r.text(i, 0)) })
                    .collect());
            }
            _ => return Ok(vec![]),
        };
        let r = self.rows(&format!(
            "SELECT name, total_rows, total_bytes, engine, comment, toString(metadata_modification_time), engine_full, sorting_key, primary_key, partition_key \
             FROM system.tables WHERE database = {} AND NOT is_temporary AND {} ORDER BY name",
            lit(&scope.db),
            filter
        ))?;
        Ok((0..r.rows.len())
            .map(|i| {
                let mut extra = vec![("Engine".into(), r.text(i, 6))];
                for (label, col) in [("Sorting Key", 7), ("Primary Key", 8), ("Partition Key", 9)] {
                    let v = r.text(i, col);
                    if !v.is_empty() {
                        extra.push((label.into(), v));
                    }
                }
                ObjectInfo {
                    name: r.text(i, 0),
                    rows: r.rows[i][1].as_f64().map(|v| v as i64),
                    data_length: r.rows[i][2].as_f64().map(|v| v as i64),
                    engine: r.text(i, 3),
                    comment: r.text(i, 4),
                    modified: r.text(i, 5),
                    extra,
                    ..Default::default()
                }
            })
            .collect())
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let (db, name) = (lit(&path.db), lit(&path.name));
        let c = self.rows(&format!(
            "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key, is_in_sorting_key, compression_codec \
             FROM system.columns WHERE database = {} AND table = {} ORDER BY position",
            db, name
        ))?;
        if c.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let t = self.rows(&format!(
            "SELECT engine, comment, sorting_key, primary_key FROM system.tables WHERE database = {} AND name = {}",
            db, name
        ))?;
        let has_pk = (0..c.rows.len()).any(|i| c.rows[i][5].as_f64().unwrap_or(0.0) != 0.0);
        let columns = (0..c.rows.len())
            .map(|i| {
                let data_type = c.text(i, 1);
                let kind = c.text(i, 2);
                let expr = c.text(i, 3);
                let mut extra = String::new();
                if !kind.is_empty() && kind != "DEFAULT" {
                    extra = format!("{} {}", kind, expr);
                }
                let codec = c.text(i, 7);
                if !codec.is_empty() {
                    extra = format!("{} {}", extra, codec).trim().to_string();
                }
                ColumnDef {
                    name: c.text(i, 0),
                    nullable: data_type.starts_with("Nullable("),
                    default: (kind == "DEFAULT" && !expr.is_empty()).then_some(expr),
                    primary_key: c.rows[i][if has_pk { 5 } else { 6 }].as_f64().unwrap_or(0.0) != 0.0,
                    comment: c.text(i, 4),
                    extra,
                    data_type,
                    ..Default::default()
                }
            })
            .collect();
        let ix = self.rows(&format!(
            "SELECT name, expr, type, granularity FROM system.data_skipping_indices WHERE database = {} AND table = {} ORDER BY name",
            db, name
        ))?;
        let indexes = (0..ix.rows.len())
            .map(|i| IndexDef {
                name: ix.text(i, 0),
                columns: vec![ix.text(i, 1)],
                method: ix.text(i, 2),
                comment: format!("GRANULARITY {}", ix.text(i, 3)),
                ..Default::default()
            })
            .collect();
        Ok(TableDef {
            path: path.clone(),
            columns,
            indexes,
            engine: t.text(0, 0),
            comment: t.text(0, 1),
            ..Default::default()
        })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let sql = match kind {
            ObjectKind::Function => format!(
                "SELECT create_query FROM system.functions WHERE origin = 'SQLUserDefined' AND name = {}",
                lit(&path.name)
            ),
            ObjectKind::View => format!("SHOW CREATE VIEW {}", D.qualify(path)),
            ObjectKind::Table | ObjectKind::MaterializedView => format!("SHOW CREATE TABLE {}", D.qualify(path)),
            _ => bail!("DDL not available for {:?}", kind),
        };
        let r = self.rows(&sql)?;
        if r.rows.is_empty() {
            bail!("{} {} not found", kind.singular(), path.display());
        }
        Ok(format!("{};", r.text(0, 0).trim_end().trim_end_matches(';')))
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope.filter(|s| !s.db.is_empty()) {
            self.db = s.db.clone();
        }
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            if let Some(db) = use_target(&stmt) {
                let prev = std::mem::replace(&mut self.db, db);
                let mut r = self.run("SELECT 1", &[]).inspect_err(|_| self.db = prev)?;
                r.is_query = false;
                r.columns.clear();
                r.rows.clear();
                r.sql = stmt;
                out.push(r);
                continue;
            }
            let mut r = if let Some(alter) = rewrite_update(&stmt) {
                self.run(&alter, &[("mutations_sync", "2")])?
            } else if is_delete(&stmt) {
                self.run(&stmt, &[("mutations_sync", "2"), ("lightweight_deletes_sync", "2")])?
            } else {
                self.run(&stmt, &[("mutations_sync", "2")])?
            };
            if dropped_db(&stmt).is_some_and(|db| db == self.db) {
                self.db = "default".into();
            }
            r.sql = stmt;
            out.push(r);
        }
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.rows(
            "SELECT name, storage, toString(auth_type), toString(host_ip), toString(host_names), toString(default_roles_list), \
             toString(default_database) FROM system.users ORDER BY name",
        )?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: vec![
                    ("Storage".into(), r.text(i, 1)),
                    ("Auth Type".into(), r.text(i, 2)),
                    ("Host IP".into(), r.text(i, 3)),
                    ("Host Names".into(), r.text(i, 4)),
                    ("Default Roles".into(), r.text(i, 5)),
                    ("Default Database".into(), r.text(i, 6)),
                ],
            })
            .collect())
    }

    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        let r = self.rows(&format!("SHOW GRANTS FOR {}", D.quote_ident(&user.name)))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.rows(
                "SELECT query_id, user, toString(address) AS address, current_database AS database, round(elapsed, 3) AS elapsed, \
                 read_rows, formatReadableSize(memory_usage) AS memory, query FROM system.processes ORDER BY elapsed DESC",
            )?,
            variables: self.rows("SELECT name, value, changed, type, description FROM system.settings ORDER BY changed DESC, name")?,
            status: self.rows(
                "SELECT * FROM (SELECT metric AS name, toString(value) AS value, 'metric' AS kind, description FROM system.metrics \
                 UNION ALL SELECT event AS name, toString(value) AS value, 'event' AS kind, description FROM system.events) ORDER BY kind DESC, name",
            )?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.run(&format!("KILL QUERY WHERE query_id = {}", lit(id.trim())), &[])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_rewrite() {
        assert_eq!(
            rewrite_update("UPDATE \"db\".\"t\" SET \"a\" = 'x', \"b\" = 2 WHERE \"id\" = 3").as_deref(),
            Some("ALTER TABLE \"db\".\"t\" UPDATE \"a\" = 'x', \"b\" = 2 WHERE \"id\" = 3")
        );
        assert_eq!(
            rewrite_update("update `we\"ird SET`.t\n set a=1 where 1").as_deref(),
            Some("ALTER TABLE `we\"ird SET`.t UPDATE a=1 where 1")
        );
        assert_eq!(rewrite_update("UPDATE x"), None);
        assert_eq!(rewrite_update("ALTER TABLE t UPDATE a=1 WHERE 1"), None);
    }

    #[test]
    fn use_and_delete() {
        assert_eq!(use_target("USE \"my db\"").as_deref(), Some("my db"));
        assert_eq!(use_target("use system;").as_deref(), Some("system"));
        assert_eq!(use_target("USER x"), None);
        assert!(is_delete("-- c\nDELETE FROM t WHERE 1"));
        assert!(!is_delete("DELETED"));
        assert_eq!(dropped_db("DROP DATABASE IF EXISTS \"x\"").as_deref(), Some("x"));
        assert_eq!(dropped_db("drop database y SYNC").as_deref(), Some("y"));
        assert_eq!(dropped_db("DROP TABLE y"), None);
    }

    #[test]
    fn types() {
        use serde_json::json;
        assert_eq!(base_type("Nullable(LowCardinality(String))"), "String");
        assert_eq!(convert("UInt64", &json!("18446744073709551615")), Value::UInt(u64::MAX));
        assert_eq!(convert("Nullable(Int32)", &json!(-5)), Value::Int(-5));
        assert_eq!(convert("Nullable(Int32)", &json!(null)), Value::Null);
        assert_eq!(convert("Int128", &json!("-170141183460469231731687303715884105728")), Value::Text("-170141183460469231731687303715884105728".into()));
        assert_eq!(convert("Float64", &json!("inf")), Value::Float(f64::INFINITY));
        assert_eq!(convert("Decimal(18, 2)", &json!("1.50")), Value::Text("1.50".into()));
        assert_eq!(convert("Bool", &json!(true)), Value::Bool(true));
        assert_eq!(convert("Array(UInt8)", &json!([1, 2])), Value::Text("[1,2]".into()));
        assert_eq!(convert("Map(String, UInt8)", &json!({"a": 1})), Value::Text("{\"a\":1}".into()));
        assert_eq!(written_rows(r#"{"read_rows":"0","written_rows":"2"}"#), 2);
        let r = parse_json_compact(r#"{"meta":[{"name":"a","type":"UInt8"},{"name":"s","type":"String"}],"data":[[1,"x"]],"rows":1}"#).unwrap();
        assert_eq!(r.rows, vec![vec![Value::UInt(1), Value::Text("x".into())]]);
        assert_eq!(r.columns[1].type_name, "String");
    }

    #[test]
    fn literals() {
        assert_eq!(lit("a'b\\c"), "'a\\'b\\\\c'");
    }
}
