//! ClickHouse over the HTTP interface (stateless requests; `USE` and `SET`
//! inside scripts are tracked by the driver and sent as request parameters).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value as Json;
use std::time::{Duration, Instant};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::sqlutil::strip_leading_comments;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Generic;

/// ClickHouse string literal (backslash is an escape character).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub struct ClickHouseDriver {
    cfg: ConnectionConfig,
    agent: ureq::Agent,
    url: String,
    db: String,
    settings: Vec<(String, String)>,
}

struct Reply {
    body: String,
    format: Option<String>,
    written: u64,
}

impl ClickHouseDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut tls = ureq::tls::TlsConfig::builder().disable_verification(cfg.ssl.enabled && !cfg.ssl.verify_server);
        if cfg.ssl.enabled && !cfg.ssl.ca_cert.is_empty() {
            let pem = std::fs::read(&cfg.ssl.ca_cert).context("reading CA certificate")?;
            let cert = ureq::tls::Certificate::from_pem(&pem).context("parsing CA certificate")?;
            tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&[cert]));
        }
        if cfg.ssl.enabled && !cfg.ssl.client_cert.is_empty() && !cfg.ssl.client_key.is_empty() {
            let cert = ureq::tls::Certificate::from_pem(&std::fs::read(&cfg.ssl.client_cert)?)?;
            let key = ureq::tls::PrivateKey::from_pem(&std::fs::read(&cfg.ssl.client_key)?)?;
            tls = tls.client_cert(Some(ureq::tls::ClientCert::new_with_certs(&[cert], key)));
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(cfg.connect_timeout_secs.max(1))))
            .tls_config(tls.build())
            .build()
            .new_agent();
        let path = cfg.option("path").trim().trim_matches('/');
        let url = format!(
            "{}://{}:{}/{}{}",
            if cfg.ssl.enabled { "https" } else { "http" },
            cfg.host,
            cfg.port,
            path,
            if path.is_empty() { "" } else { "/" }
        );
        let d = Self { cfg: cfg.clone(), agent, url, db: cfg.default_database(), settings: vec![] };
        d.send(&d.db.clone(), "SELECT 1")
            .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
        Ok(d)
    }

    fn send(&self, db: &str, sql: &str) -> Result<Reply> {
        let mut params: Vec<(String, String)> = vec![
            ("default_format".into(), "JSONCompact".into()),
            ("output_format_json_quote_64bit_integers".into(), "1".into()),
            ("output_format_json_quote_decimals".into(), "1".into()),
            ("output_format_decimal_trailing_zeros".into(), "1".into()),
            ("output_format_json_quote_denormals".into(), "1".into()),
            ("output_format_json_validate_utf8".into(), "1".into()),
            ("mutations_sync".into(), "2".into()),
            ("lightweight_deletes_sync".into(), "2".into()),
        ];
        if self.cfg.read_only {
            params.push(("readonly".into(), "2".into()));
        }
        params.retain(|(k, _)| !self.settings.iter().any(|(s, _)| s.eq_ignore_ascii_case(k)));
        params.extend(self.settings.iter().cloned());
        if !db.is_empty() {
            params.push(("database".into(), db.into()));
        }
        let mut req = self.agent.post(&self.url).query_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if !self.cfg.user.is_empty() {
            req = req.header("X-ClickHouse-User", &self.cfg.user);
        }
        if !self.cfg.password.is_empty() {
            req = req.header("X-ClickHouse-Key", &self.cfg.password);
        }
        let mut resp = req.send(sql.as_bytes()).map_err(|e| anyhow!("ClickHouse request failed: {}", e))?;
        let status = resp.status();
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let format = header("X-ClickHouse-Format");
        let summary = header("X-ClickHouse-Summary");
        let bytes = resp.body_mut().with_config().limit(4 << 30).read_to_vec()?;
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if !status.is_success() {
            let msg = body.trim();
            bail!("{}", if msg.is_empty() { status.to_string() } else { msg.to_string() });
        }
        let written = summary
            .and_then(|s| serde_json::from_str::<Json>(&s).ok())
            .and_then(|j| j.get("written_rows").and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64())))
            .unwrap_or(0);
        Ok(Reply { body, format, written })
    }

    fn run(&mut self, db: &str, stmt: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut out = QueryResult { sql: stmt.to_string(), ..Default::default() };
        let head = strip_leading_comments(stmt);
        let upper = head.to_ascii_uppercase();
        let mut sql = head.to_string();
        let mut counted = None;
        if let Some((target, set, cond)) = split_update(head) {
            counted = self.count_where(db, &target, &cond);
            sql = format!("ALTER TABLE {} UPDATE {} WHERE {}", target, set, cond);
        } else if let Some((target, cond)) = split_delete(head) {
            counted = self.count_where(db, &target, &cond);
        }
        let dropped_db = upper
            .strip_prefix("DROP DATABASE ")
            .map(|r| r.trim_start().strip_prefix("IF EXISTS ").unwrap_or(r))
            .and_then(|r| r.split_whitespace().next())
            .map(|n| unquote(n.trim_end_matches(';')));
        let reply = self.send(if dropped_db.is_some() { "" } else { db }, &sql)?;
        if dropped_db.is_some_and(|n| n.eq_ignore_ascii_case(&self.db)) {
            self.db.clear();
        }
        if upper.starts_with("USE ") || upper.starts_with("USE\t") || upper.starts_with("USE\n") {
            self.db = unquote(head[3..].trim().trim_end_matches(';').trim());
        } else if upper.starts_with("SET ") {
            for (k, v) in parse_set(&head[4..]) {
                self.settings.retain(|(s, _)| !s.eq_ignore_ascii_case(&k));
                self.settings.push((k, v));
            }
        }
        if reply.format.is_some() && !reply.body.trim().is_empty() {
            out.is_query = true;
            if reply.format.as_deref() == Some("JSONCompact") {
                parse_json_compact(&reply.body, &mut out)?;
            } else {
                out.columns = vec![ColumnMeta { name: reply.format.unwrap_or_default(), type_name: "String".into() }];
                out.rows = reply.body.lines().map(|l| vec![Value::Text(l.to_string())]).collect();
            }
        } else {
            out.affected_rows = counted.unwrap_or(reply.written);
        }
        out.elapsed_ms = start.elapsed().as_millis();
        Ok(out)
    }

    fn count_where(&self, db: &str, target: &str, cond: &str) -> Option<u64> {
        let r = self.send(db, &format!("SELECT count() FROM {} WHERE {}", target, cond)).ok()?;
        let mut q = QueryResult::default();
        parse_json_compact(&r.body, &mut q).ok()?;
        q.rows.first()?.first()?.as_f64().map(|n| n as u64)
    }

    /// Catalog query (fully qualified, so it runs without a database).
    fn rows(&mut self, sql: &str) -> Result<QueryResult> {
        self.run("", sql)
    }
}

fn parse_json_compact(body: &str, out: &mut QueryResult) -> Result<()> {
    let j: Json = match serde_json::from_str(body) {
        Ok(j) => j,
        Err(e) => match body.find("Code: ") {
            Some(p) => bail!("{}", body[p..].trim()),
            None => bail!("invalid ClickHouse response: {}", e),
        },
    };
    let types: Vec<String> = j
        .get("meta")
        .and_then(|m| m.as_array())
        .map(|m| m.iter().map(|c| c.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string()).collect())
        .unwrap_or_default();
    out.columns = j
        .get("meta")
        .and_then(|m| m.as_array())
        .map(|m| {
            m.iter()
                .zip(&types)
                .map(|(c, t)| ColumnMeta {
                    name: c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                    type_name: t.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(data) = j.get("data").and_then(|d| d.as_array()) {
        for row in data {
            let cells = row.as_array().map(|r| r.as_slice()).unwrap_or(&[]);
            out.rows.push(
                cells.iter().enumerate().map(|(i, v)| to_value(types.get(i).map(|s| s.as_str()).unwrap_or(""), v)).collect(),
            );
        }
    }
    Ok(())
}

/// Type with `Nullable(..)` / `LowCardinality(..)` wrappers removed.
fn base_type(mut t: &str) -> &str {
    loop {
        let inner = t
            .strip_prefix("Nullable(")
            .or_else(|| t.strip_prefix("LowCardinality("))
            .and_then(|r| r.strip_suffix(')'));
        match inner {
            Some(i) => t = i,
            None => return t,
        }
    }
}

fn to_value(ty: &str, v: &Json) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let t = base_type(ty);
    let name = t.split('(').next().unwrap_or(t);
    let text = || match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    match name {
        "Int8" | "Int16" | "Int32" | "Int64" => match v {
            Json::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Text(n.to_string())),
            Json::String(s) => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Text(text()),
        },
        "UInt8" | "UInt16" | "UInt32" | "UInt64" => match v {
            Json::Number(n) => n.as_u64().map(Value::UInt).unwrap_or_else(|| Value::Text(n.to_string())),
            Json::String(s) => s.parse().map(Value::UInt).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Text(text()),
        },
        "Float32" | "Float64" | "BFloat16" => match v {
            Json::Number(n) => n.as_f64().map(Value::Float).unwrap_or(Value::Null),
            Json::String(s) => s.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Text(text()),
        },
        "Bool" => match v {
            Json::Bool(b) => Value::Bool(*b),
            _ => Value::Text(text()),
        },
        _ => match v {
            Json::Bool(b) if !matches!(name, "Array" | "Map" | "Tuple" | "JSON" | "Object" | "Nested") => Value::Bool(*b),
            _ => Value::Text(text()),
        },
    }
}

/// Byte offsets of top-level occurrences of `kw` (a whole word, case
/// insensitive) outside strings, quoted identifiers, comments and parentheses.
fn find_keyword(s: &str, kw: &str) -> Option<usize> {
    let b = s.as_bytes();
    let k = kw.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = s[i + 2..].find("*/").map(|p| i + 2 + p + 1).unwrap_or(b.len());
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if depth == 0
                && c.is_ascii_alphabetic()
                && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
                && b.len() >= i + k.len()
                && b[i..i + k.len()].eq_ignore_ascii_case(k)
                && b.get(i + k.len()).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_')) =>
            {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `UPDATE <target> SET <set> WHERE <cond>` -> parts (standard SQL UPDATE, as
/// generated by the data grid) so it can be sent as `ALTER TABLE .. UPDATE`.
fn split_update(stmt: &str) -> Option<(String, String, String)> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    if !s.get(..7)?.eq_ignore_ascii_case("UPDATE ") {
        return None;
    }
    let set = find_keyword(s, "SET")?;
    let wh = find_keyword(s, "WHERE")?;
    if wh < set {
        return None;
    }
    let target = s[7..set].trim();
    let assignments = s[set + 3..wh].trim();
    let cond = s[wh + 5..].trim();
    if target.is_empty() || assignments.is_empty() || cond.is_empty() {
        return None;
    }
    Some((target.to_string(), assignments.to_string(), cond.to_string()))
}

/// `DELETE FROM <target> WHERE <cond>` -> parts.
fn split_delete(stmt: &str) -> Option<(String, String)> {
    let s = stmt.trim().trim_end_matches(';').trim_end();
    if !s.get(..12)?.eq_ignore_ascii_case("DELETE FROM ") {
        return None;
    }
    let wh = find_keyword(s, "WHERE")?;
    let target = s[12..wh].trim();
    let cond = s[wh + 5..].trim();
    if target.is_empty() || cond.is_empty() || find_keyword(cond, "SETTINGS").is_some() {
        return None;
    }
    Some((target.to_string(), cond.to_string()))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    s.to_string()
}

/// `a = 1, b = 'x'` -> [(a, 1), (b, x)].
fn parse_set(s: &str) -> Vec<(String, String)> {
    let s = s.trim().trim_end_matches(';');
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        match find_comma(rest) {
            Some(p) => {
                parts.push(&rest[..p]);
                rest = &rest[p + 1..];
            }
            None => {
                parts.push(rest);
                break;
            }
        }
    }
    parts
        .into_iter()
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let v = v.trim();
            let v = if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
                v[1..v.len() - 1].replace("\\'", "'").replace("''", "'")
            } else {
                v.to_string()
            };
            Some((k.trim().to_string(), v))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn find_comma(s: &str) -> Option<usize> {
    let mut in_str = false;
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'\'' => in_str = !in_str,
            b',' if !in_str => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn info_rows(r: &QueryResult) -> Vec<ObjectInfo> {
    (0..r.rows.len())
        .map(|i| {
            let n = |c: usize| r.rows[i].get(c).and_then(|v| v.as_f64()).map(|f| f as i64);
            let mut o = ObjectInfo::named(r.text(i, 0));
            o.engine = r.text(i, 1);
            o.rows = n(2);
            o.data_length = n(3);
            o.modified = r.text(i, 4);
            o.comment = r.text(i, 5);
            for (c, label) in [(6, "Sorting Key"), (7, "Primary Key"), (8, "Partition Key"), (9, "UUID")] {
                let v = r.text(i, c);
                if !v.is_empty() {
                    o.extra.push((label.into(), v));
                }
            }
            o
        })
        .collect()
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
            ObjectKind::Table => "engine NOT IN ('View', 'MaterializedView', 'LiveView', 'WindowView') AND name NOT LIKE '.inner%'",
            ObjectKind::View => "engine IN ('View', 'LiveView', 'WindowView')",
            ObjectKind::MaterializedView => "engine = 'MaterializedView'",
            ObjectKind::Function => {
                let r = self.rows(
                    "SELECT name, 'SQLUserDefined', NULL, NULL, '', description FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name",
                )?;
                return Ok(info_rows(&r));
            }
            _ => return Ok(vec![]),
        };
        let r = self.rows(&format!(
            "SELECT name, engine, total_rows, total_bytes, toString(metadata_modification_time), comment, sorting_key, primary_key, partition_key, toString(uuid) \
             FROM system.tables WHERE database = {} AND NOT is_temporary AND {} ORDER BY name",
            lit(&scope.db),
            filter
        ))?;
        Ok(info_rows(&r))
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let r = self.rows(&format!(
            "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key, is_in_sorting_key, compression_codec \
             FROM system.columns WHERE database = {} AND table = {} ORDER BY position",
            lit(&path.db),
            lit(&path.name)
        ))?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let flag = |i: usize, c: usize| r.rows[i].get(c).and_then(|v| v.as_f64()).unwrap_or(0.0) != 0.0;
        let any_pk = (0..r.rows.len()).any(|i| flag(i, 5));
        let columns: Vec<ColumnDef> = (0..r.rows.len())
            .map(|i| {
                let ty = r.text(i, 1);
                let kind = r.text(i, 2);
                let expr = r.text(i, 3);
                let mut c = ColumnDef::new(r.text(i, 0), ty.clone());
                c.nullable = ty.starts_with("Nullable(") || ty.starts_with("LowCardinality(Nullable(");
                c.primary_key = if any_pk { flag(i, 5) } else { flag(i, 6) };
                c.comment = r.text(i, 4);
                match kind.as_str() {
                    "" => {}
                    "DEFAULT" => c.default = Some(expr),
                    k => c.extra = format!("{} {}", k, expr).trim().to_string(),
                }
                let codec = r.text(i, 7);
                if !codec.is_empty() {
                    c.extra = format!("{} {}", c.extra, codec).trim().to_string();
                }
                c
            })
            .collect();
        let t = self.rows(&format!(
            "SELECT engine, comment, sorting_key, primary_key FROM system.tables WHERE database = {} AND name = {}",
            lit(&path.db),
            lit(&path.name)
        ))?;
        let mut indexes = Vec::new();
        let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        if !pk.is_empty() {
            indexes.push(IndexDef { name: "PRIMARY".into(), columns: pk, primary: true, method: "primary".into(), ..Default::default() });
        }
        if let Ok(ix) = self.rows(&format!(
            "SELECT name, expr, type_full, granularity FROM system.data_skipping_indices WHERE database = {} AND table = {} ORDER BY name",
            lit(&path.db),
            lit(&path.name)
        )) {
            for i in 0..ix.rows.len() {
                indexes.push(IndexDef {
                    name: ix.text(i, 0),
                    columns: vec![ix.text(i, 1)],
                    method: ix.text(i, 2),
                    comment: format!("GRANULARITY {}", ix.text(i, 3)),
                    ..Default::default()
                });
            }
        }
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
            ObjectKind::Function => {
                format!("SELECT create_query FROM system.functions WHERE name = {} AND origin = 'SQLUserDefined'", lit(&path.name))
            }
            ObjectKind::View => format!("SHOW CREATE VIEW {}", D.qualify(path)),
            ObjectKind::Table | ObjectKind::MaterializedView => format!("SHOW CREATE TABLE {}", D.qualify(path)),
            _ => bail!("DDL not available for {:?}", kind),
        };
        let r = self.rows(&sql)?;
        if r.rows.is_empty() {
            bail!("{} {} not found", kind.singular(), path.display());
        }
        Ok(format!("{};", r.text(0, 0).trim_end_matches(';')))
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope.filter(|s| !s.db.is_empty()) {
            self.db = s.db.clone();
        }
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            let db = self.db.clone();
            out.push(self.run(&db, &stmt)?);
        }
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.rows(
            "SELECT name, storage, toString(auth_type), arrayStringConcat(host_ip, ', '), arrayStringConcat(default_roles_list, ', ') FROM system.users ORDER BY name",
        )?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: vec![
                    ("Storage".into(), r.text(i, 1)),
                    ("Auth Type".into(), r.text(i, 2)),
                    ("Host IP".into(), r.text(i, 3)),
                    ("Default Roles".into(), r.text(i, 4)),
                ],
            })
            .collect())
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.rows(
                "SELECT query_id, user, toString(address) AS address, round(elapsed, 3) AS elapsed, read_rows, written_rows, formatReadableSize(memory_usage) AS memory, query_kind, query \
                 FROM system.processes ORDER BY elapsed DESC",
            )?,
            variables: self.rows("SELECT name, value, changed, type, description FROM system.settings ORDER BY changed DESC, name")?,
            status: self.rows(
                "SELECT metric AS name, toString(value) AS value, description FROM system.metrics \
                 UNION ALL SELECT event, toString(value), description FROM system.events ORDER BY name",
            )?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.rows(&format!("KILL QUERY WHERE query_id = {}", lit(id.trim())))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_update() {
        let (t, s, w) = split_update("UPDATE \"db\".\"t\" SET \"v\" = 'a WHERE b', \"x\" = 1 WHERE \"id\" = 2").unwrap();
        assert_eq!(t, "\"db\".\"t\"");
        assert_eq!(s, "\"v\" = 'a WHERE b', \"x\" = 1");
        assert_eq!(w, "\"id\" = 2");
        assert!(split_update("UPDATE t SET a = 1").is_none());
        assert!(split_update("SELECT 1").is_none());
        let (_, s, _) = split_update("update t set \"set\" = 'it''s', w = (select 1 where 1) where id = 1;").unwrap();
        assert_eq!(s, "\"set\" = 'it''s', w = (select 1 where 1)");
    }

    #[test]
    fn splits_delete() {
        let (t, w) = split_delete("DELETE FROM \"db\".\"t\" WHERE \"id\" = 1 AND \"v\" = 'x\\'y'").unwrap();
        assert_eq!(t, "\"db\".\"t\"");
        assert_eq!(w, "\"id\" = 1 AND \"v\" = 'x\\'y'");
        assert!(split_delete("DELETE FROM t").is_none());
    }

    #[test]
    fn parses_set() {
        assert_eq!(
            parse_set(" max_threads = 2, format_csv_delimiter = ';' "),
            vec![("max_threads".to_string(), "2".to_string()), ("format_csv_delimiter".to_string(), ";".to_string())]
        );
        assert_eq!(unquote("\"my db\""), "my db");
        assert_eq!(unquote("`x`"), "x");
    }

    #[test]
    fn maps_types() {
        use serde_json::json;
        assert_eq!(base_type("LowCardinality(Nullable(String))"), "String");
        assert_eq!(to_value("UInt64", &json!("18446744073709551615")), Value::UInt(u64::MAX));
        assert_eq!(to_value("Nullable(Int64)", &json!("-5")), Value::Int(-5));
        assert_eq!(to_value("Int32", &json!(7)), Value::Int(7));
        assert_eq!(to_value("Int128", &json!("170141183460469231731687303715884105727")), Value::Text("170141183460469231731687303715884105727".into()));
        assert_eq!(to_value("Decimal(18, 2)", &json!("1.50")), Value::Text("1.50".into()));
        assert_eq!(to_value("Float64", &json!("nan")).as_f64().map(|f| f.is_nan()), Some(true));
        assert_eq!(to_value("Bool", &json!(true)), Value::Bool(true));
        assert_eq!(to_value("Array(UInt8)", &json!([1, 2])), Value::Text("[1,2]".into()));
        assert_eq!(to_value("Map(String, UInt8)", &json!({"b": 1, "a": 2})), Value::Text("{\"b\":1,\"a\":2}".into()));
        assert_eq!(to_value("Nullable(String)", &Json::Null), Value::Null);
        assert_eq!(to_value("DateTime", &json!("2024-01-01 00:00:00")), Value::Text("2024-01-01 00:00:00".into()));
    }

    #[test]
    fn parses_compact_json() {
        let body = r#"{"meta":[{"name":"a","type":"UInt32"},{"name":"b","type":"String"}],"data":[[1,"x"],[2,"y"]],"rows":2}"#;
        let mut q = QueryResult::default();
        parse_json_compact(body, &mut q).unwrap();
        assert_eq!(q.columns.len(), 2);
        assert_eq!(q.columns[0].type_name, "UInt32");
        assert_eq!(q.rows[1], vec![Value::UInt(2), Value::Text("y".into())]);
        let mut q = QueryResult::default();
        let err = parse_json_compact("{\"meta\":[ Code: 241. DB::Exception: Memory limit", &mut q).unwrap_err();
        assert!(err.to_string().starts_with("Code: 241"));
    }
}
