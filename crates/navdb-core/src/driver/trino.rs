//! Trino / Presto over the client REST protocol (`POST /v1/statement`, then
//! follow `nextUri`). Catalog = database, schema = schema. Session state that
//! the server returns in response headers (`USE`, `SET SESSION`, transactions)
//! is kept by the driver and sent with every request.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::Value as Json;
use std::time::{Duration, Instant};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::sqlutil::strip_leading_comments;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Generic;

fn lit(s: &str) -> String {
    D.string_literal(s)
}

pub struct TrinoDriver {
    cfg: ConnectionConfig,
    agent: ureq::Agent,
    url: String,
    /// Header prefix: `X-Trino-` or `X-Presto-`.
    prefix: &'static str,
    auth: Option<String>,
    catalog: String,
    schema: String,
    session: Vec<(String, String)>,
    txn: Option<String>,
}

/// Status, body and (lower-cased) headers of one HTTP exchange.
type HttpReply = (u16, String, Vec<(String, String)>);

/// Accumulated reply of one statement.
#[derive(Default)]
struct Reply {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    update_type: Option<String>,
    update_count: Option<u64>,
}

impl TrinoDriver {
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
        let auth = if cfg.password.is_empty() {
            None
        } else if !cfg.ssl.enabled {
            bail!("{} requires SSL/TLS (https) for password authentication", cfg.kind.label());
        } else {
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", cfg.user, cfg.password));
            Some(format!("Basic {}", token))
        };
        let url = format!("{}://{}:{}", if cfg.ssl.enabled { "https" } else { "http" }, cfg.host, cfg.port);
        let mut d = Self {
            cfg: cfg.clone(),
            agent,
            url,
            prefix: if cfg.kind == DbKind::Presto { "X-Presto-" } else { "X-Trino-" },
            auth,
            catalog: cfg.database.trim().to_string(),
            schema: cfg.option("schema").trim().to_string(),
            session: parse_properties(cfg.option("session_properties")),
            txn: None,
        };
        // Presto rejects OFFSET (used by the data grid) unless enabled
        if cfg.kind == DbKind::Presto && !d.session.iter().any(|(k, _)| k == "offset_clause_enabled") {
            d.session.push(("offset_clause_enabled".into(), "true".into()));
        }
        d.run("SELECT 1").with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
        Ok(d)
    }

    fn user(&self) -> String {
        if self.cfg.user.is_empty() { "navdb".into() } else { self.cfg.user.clone() }
    }

    fn get(&self, method: &str, uri: &str, body: Option<&str>) -> Result<HttpReply> {
        let h = |n: &str| format!("{}{}", self.prefix, n);
        let user = self.user();
        let session = self.session.iter().map(|(k, v)| format!("{}={}", k, encode_value(v))).collect::<Vec<_>>().join(",");
        macro_rules! headers {
            ($req:expr) => {{
                let mut r = $req
                    .header(h("User"), &user)
                    .header(h("Source"), "navdb-studio")
                    .header(h("Client-Capabilities"), "PARAMETRIC_DATETIME");
                if !self.catalog.is_empty() {
                    r = r.header(h("Catalog"), &self.catalog);
                }
                if !self.schema.is_empty() {
                    r = r.header(h("Schema"), &self.schema);
                }
                if !session.is_empty() {
                    r = r.header(h("Session"), &session);
                }
                if let Some(t) = &self.txn {
                    r = r.header(h("Transaction-Id"), t);
                }
                if let Some(a) = &self.auth {
                    r = r.header("Authorization", a);
                }
                r
            }};
        }
        let resp = match (method, body) {
            ("POST", Some(b)) => headers!(self.agent.post(uri)).header("Content-Type", "text/plain").send(b.as_bytes()),
            ("DELETE", _) => headers!(self.agent.delete(uri)).call(),
            _ => headers!(self.agent.get(uri)).call(),
        };
        let mut resp = resp.map_err(|e| anyhow!("{} request failed: {}", self.cfg.kind.label(), e))?;
        let status = resp.status().as_u16();
        let hdrs: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
            .collect();
        let bytes = resp.body_mut().with_config().limit(4 << 30).read_to_vec()?;
        Ok((status, String::from_utf8_lossy(&bytes).into_owned(), hdrs))
    }

    /// Send one statement and follow `nextUri` until the result is complete.
    fn request(&mut self, sql: &str) -> Result<Reply> {
        let mut out = Reply::default();
        let mut types: Vec<String> = vec![];
        let mut uri = format!("{}/v1/statement", self.url);
        let mut post = true;
        let mut retries = 0;
        loop {
            let (status, body, hdrs) = self.get(if post { "POST" } else { "GET" }, &uri, if post { Some(sql) } else { None })?;
            if matches!(status, 502..=504) && retries < 100 {
                retries += 1;
                std::thread::sleep(Duration::from_millis(50 * retries.min(10)));
                continue;
            }
            retries = 0;
            if status != 200 {
                let msg = body.trim();
                if !post {
                    let _ = self.get("DELETE", &uri, None);
                }
                bail!("{}", if msg.is_empty() { format!("HTTP {}", status) } else { format!("HTTP {}: {}", status, msg) });
            }
            post = false;
            self.apply_headers(&hdrs);
            let j: Json = match serde_json::from_str(&body) {
                Ok(j) => j,
                Err(e) => {
                    let _ = self.get("DELETE", &uri, None);
                    bail!("invalid {} response: {}", self.cfg.kind.label(), e);
                }
            };
            if let Some(err) = j.get("error").filter(|e| !e.is_null()) {
                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("query failed");
                return Err(match err.get("errorName").and_then(|n| n.as_str()) {
                    Some(n) => anyhow!("{} ({})", msg, n),
                    None => anyhow!("{}", msg),
                });
            }
            if out.columns.is_empty() {
                if let Some(cols) = j.get("columns").and_then(|c| c.as_array()) {
                    for c in cols {
                        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let ty = c.get("type").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        types.push(ty.clone());
                        out.columns.push(ColumnMeta { name, type_name: ty });
                    }
                }
            }
            if let Some(data) = j.get("data").and_then(|d| d.as_array()) {
                for row in data {
                    let cells = row.as_array().map(|r| r.as_slice()).unwrap_or(&[]);
                    out.rows.push(
                        cells.iter().enumerate().map(|(i, v)| to_value(types.get(i).map(|s| s.as_str()).unwrap_or(""), v)).collect(),
                    );
                }
            }
            if let Some(t) = j.get("updateType").and_then(|t| t.as_str()) {
                out.update_type = Some(t.to_string());
            }
            if let Some(n) = j.get("updateCount").and_then(|n| n.as_u64()) {
                out.update_count = Some(n);
            }
            match j.get("nextUri").and_then(|u| u.as_str()) {
                Some(u) => uri = u.to_string(),
                None => return Ok(out),
            }
        }
    }

    fn apply_headers(&mut self, hdrs: &[(String, String)]) {
        let p = self.prefix.to_ascii_lowercase();
        for (k, v) in hdrs {
            let Some(name) = k.strip_prefix(&p) else { continue };
            match name {
                "set-catalog" => self.catalog = v.clone(),
                "set-schema" => self.schema = v.clone(),
                "set-session" => {
                    if let Some((k, v)) = v.split_once('=') {
                        let k = k.trim().to_string();
                        self.session.retain(|(s, _)| *s != k);
                        self.session.push((k, decode_value(v.trim())));
                    }
                }
                "clear-session" => self.session.retain(|(s, _)| s != v.trim()),
                "started-transaction-id" => self.txn = Some(v.clone()),
                "clear-transaction-id" => self.txn = None,
                _ => {}
            }
        }
    }

    fn run(&mut self, stmt: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let sql = rewrite(stmt);
        if self.cfg.read_only && !is_read_statement(&sql) {
            bail!("connection is read-only");
        }
        let r = match self.request(&sql) {
            Err(e) if is_type_mismatch(&e.to_string()) => match self.cast_insert(&sql) {
                Some(cast) => self.request(&cast)?,
                None => return Err(e),
            },
            r => r?,
        };
        let mut out = QueryResult { sql: stmt.to_string(), ..Default::default() };
        match r.update_type {
            Some(_) => out.affected_rows = r.update_count.unwrap_or(0),
            None => {
                out.is_query = true;
                out.columns = r.columns;
                out.rows = r.rows;
            }
        }
        out.elapsed_ms = start.elapsed().as_millis();
        Ok(out)
    }

    /// Statement run in a temporary catalog/schema (restored afterwards).
    fn rows_in(&mut self, catalog: &str, sql: &str) -> Result<QueryResult> {
        let saved = (self.catalog.clone(), self.schema.clone());
        if !catalog.is_empty() {
            self.catalog = catalog.into();
            self.schema.clear();
        }
        let r = self.run(sql);
        (self.catalog, self.schema) = saved;
        r
    }

    fn rows(&mut self, sql: &str) -> Result<QueryResult> {
        self.rows_in("", sql)
    }

    /// `INSERT INTO t (cols) VALUES ..` with every value cast to its column
    /// type (Trino does not coerce e.g. varchar literals to decimal or date).
    fn cast_insert(&mut self, sql: &str) -> Option<String> {
        let (target, cols, values) = split_insert(sql)?;
        let probe = format!("SELECT {} FROM {} WHERE false", cols.join(", "), target);
        let types: Vec<String> = self.request(&probe).ok()?.columns.into_iter().map(|c| c.type_name).collect();
        (types.len() == cols.len()).then(|| cast_insert_sql(&target, &cols, &types, &values))
    }

    fn materialized_views(&mut self, scope: &Scope) -> Vec<String> {
        let Some(schema) = scope.schema.as_deref() else { return vec![] };
        let r = self.rows(&format!(
            "SELECT name FROM system.metadata.materialized_views WHERE catalog_name = {} AND schema_name = {} ORDER BY name",
            lit(&scope.db),
            lit(schema)
        ));
        r.map(|r| (0..r.rows.len()).map(|i| r.text(i, 0)).collect()).unwrap_or_default()
    }
}

/// Session property values are URL-encoded in the header.
fn encode_value(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn decode_value(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => match v.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(x) => {
                    out.push(x);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `k=v, k2=v2` -> pairs.
fn parse_properties(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().trim_matches('\'').to_string()))
        })
        .collect()
}

/// Server-ready text: comments before the statement and trailing `;` removed,
/// and a trailing `LIMIT n OFFSET m` (as generated by the data grid) turned
/// into `OFFSET m LIMIT n`.
fn rewrite(stmt: &str) -> String {
    let s = strip_leading_comments(stmt).trim().trim_end_matches(';').trim_end();
    let re = regex::Regex::new(r"(?is)\s+LIMIT\s+(\d+)\s+OFFSET\s+(\d+)$").expect("valid regex");
    match re.captures(s) {
        Some(c) => {
            let m = c.get(0).map(|m| m.start()).unwrap_or(s.len());
            format!("{} OFFSET {} LIMIT {}", &s[..m], &c[2], &c[1])
        }
        None => s.to_string(),
    }
}

/// `INSERT INTO <target> (<cols>) VALUES <rows>` (as generated by the data
/// grid) -> parts. Quoted strings and identifiers are skipped while scanning.
fn split_insert(sql: &str) -> Option<(String, Vec<String>, String)> {
    let s = sql.trim();
    if !s.get(..12)?.eq_ignore_ascii_case("INSERT INTO ") {
        return None;
    }
    let b = s.as_bytes();
    let (mut i, mut depth, mut open) = (12, 0, None);
    let mut cols = vec![];
    let mut start = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            b'(' => {
                if depth == 0 {
                    open = Some(i);
                    start = i + 1;
                }
                depth += 1;
            }
            b',' if depth == 1 => {
                cols.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    cols.push(s[start..i].trim().to_string());
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let target = s[12..open?].trim().to_string();
    let rest = s.get(i + 1..)?.trim_start();
    if !rest.get(..6)?.eq_ignore_ascii_case("VALUES") || target.is_empty() || cols.iter().any(|c| c.is_empty()) {
        return None;
    }
    Some((target, cols, rest[6..].trim().to_string()))
}

/// INSERT rejected because value types do not match the columns (Trino / Presto wording).
fn is_type_mismatch(msg: &str) -> bool {
    msg.contains("mismatched column types") || (msg.contains("Mismatch at column") && msg.contains("but expression is of type"))
}

fn cast_insert_sql(target: &str, cols: &[String], types: &[String], values: &str) -> String {
    let alias: Vec<String> = (1..=cols.len()).map(|i| format!("\"_c{}\"", i)).collect();
    let casts: Vec<String> = alias.iter().zip(types).map(|(a, t)| format!("CAST({} AS {})", a, t)).collect();
    format!(
        "INSERT INTO {} ({}) SELECT {} FROM (VALUES {}) AS v({})",
        target,
        cols.join(", "),
        casts.join(", "),
        values,
        alias.join(", ")
    )
}

fn is_read_statement(sql: &str) -> bool {
    let u = sql.trim_start().to_ascii_uppercase();
    let first = u.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("");
    match first {
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "VALUES" | "TABLE" | "USE" => {
            !u.starts_with("EXPLAIN ANALYZE")
        }
        "SET" | "RESET" => u.split_whitespace().nth(1) == Some("SESSION"),
        _ => false,
    }
}

/// Type name without parameters: `timestamp(3) with time zone` -> `timestamp`.
fn base_type(t: &str) -> String {
    t.split(|c: char| c == '(' || c.is_whitespace()).next().unwrap_or(t).to_ascii_lowercase()
}

fn to_value(ty: &str, v: &Json) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let text = || match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    match base_type(ty).as_str() {
        "boolean" => match v {
            Json::Bool(b) => Value::Bool(*b),
            _ => Value::Text(text()),
        },
        "tinyint" | "smallint" | "integer" | "int" | "bigint" => match v {
            Json::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Text(n.to_string())),
            Json::String(s) => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Text(text()),
        },
        "real" | "double" => match v {
            Json::Number(n) => n.as_f64().map(Value::Float).unwrap_or(Value::Null),
            Json::String(s) => s.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Text(text()),
        },
        "varbinary" => match v {
            Json::String(s) => {
                base64::engine::general_purpose::STANDARD.decode(s).map(Value::Bytes).unwrap_or_else(|_| Value::Text(s.clone()))
            }
            _ => Value::Text(text()),
        },
        // Presto may send nested values as pre-formatted JSON text
        "array" | "map" | "row" => match v {
            Json::String(s) => {
                Value::Text(serde_json::from_str::<Json>(s).map(|j| j.to_string()).unwrap_or_else(|_| s.clone()))
            }
            _ => Value::Text(text()),
        },
        _ => Value::Text(text()),
    }
}

impl Driver for TrinoDriver {
    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self
            .rows("SELECT version()")
            .or_else(|_| self.rows("SELECT node_version FROM system.runtime.nodes WHERE coordinator LIMIT 1"))?;
        Ok(format!("{} {}", self.cfg.kind.label(), r.text(0, 0)))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.rows("SHOW CATALOGS")?;
        let mut v: Vec<String> = (0..r.rows.len()).map(|i| r.text(i, 0)).collect();
        v.sort_by_key(|c| (c == "system", c.clone()));
        Ok(v)
    }

    fn list_schemas(&mut self, db: &str) -> Result<Vec<String>> {
        let r = self.rows(&format!("SHOW SCHEMAS FROM {}", D.quote_ident(db)))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).filter(|s| s != "information_schema").collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let Some(schema) = scope.schema.clone().filter(|s| !s.is_empty()) else { return Ok(vec![]) };
        let mvs = self.materialized_views(scope);
        if kind == ObjectKind::MaterializedView {
            return Ok(mvs.into_iter().map(ObjectInfo::named).collect());
        }
        let ty = match kind {
            ObjectKind::Table => "BASE TABLE",
            ObjectKind::View => "VIEW",
            _ => return Ok(vec![]),
        };
        let r = self.rows(&format!(
            "SELECT table_name FROM {}.information_schema.tables WHERE table_schema = {} AND table_type = {} ORDER BY table_name",
            D.quote_ident(&scope.db),
            lit(&schema),
            lit(ty)
        ))?;
        let comments: Vec<(String, String)> = self
            .rows(&format!(
                "SELECT table_name, comment FROM system.metadata.table_comments WHERE catalog_name = {} AND schema_name = {} AND comment IS NOT NULL",
                lit(&scope.db),
                lit(&schema)
            ))
            .map(|c| (0..c.rows.len()).map(|i| (c.text(i, 0), c.text(i, 1))).collect())
            .unwrap_or_default();
        Ok((0..r.rows.len())
            .map(|i| r.text(i, 0))
            .filter(|n| !mvs.contains(n))
            .map(|n| {
                let mut o = ObjectInfo::named(n.clone());
                o.engine = scope.db.clone();
                o.comment = comments.iter().find(|(t, _)| *t == n).map(|(_, c)| c.clone()).unwrap_or_default();
                o
            })
            .collect())
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let schema = path.schema.clone().unwrap_or_default();
        let r = self.rows(&format!(
            "SELECT column_name, data_type, is_nullable, column_default FROM {}.information_schema.columns \
             WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            D.quote_ident(&path.db),
            lit(&schema),
            lit(&path.name)
        ))?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let desc = self.rows(&format!("DESCRIBE {}", D.qualify(path))).ok();
        let columns = (0..r.rows.len())
            .map(|i| {
                let mut c = ColumnDef::new(r.text(i, 0), r.text(i, 1));
                c.nullable = !r.text(i, 2).eq_ignore_ascii_case("NO");
                c.default = r.rows[i].get(3).filter(|v| !v.is_null()).map(|v| v.to_string());
                if let Some(d) = &desc {
                    if let Some(j) = (0..d.rows.len()).find(|&j| d.text(j, 0) == c.name) {
                        c.extra = d.text(j, 2);
                        c.comment = d.text(j, 3);
                    }
                }
                c
            })
            .collect();
        let comment = self
            .rows(&format!(
                "SELECT comment FROM system.metadata.table_comments WHERE catalog_name = {} AND schema_name = {} AND table_name = {}",
                lit(&path.db),
                lit(&schema),
                lit(&path.name)
            ))
            .map(|c| c.text(0, 0))
            .unwrap_or_default();
        Ok(TableDef { path: path.clone(), columns, comment, engine: path.db.clone(), ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let what = match kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED VIEW",
            _ => bail!("DDL not available for {:?}", kind),
        };
        let r = self.rows(&format!("SHOW CREATE {} {}", what, D.qualify(path)))?;
        if r.rows.is_empty() {
            bail!("{} {} not found", kind.singular(), path.display());
        }
        Ok(format!("{};", r.text(0, 0).trim_end_matches(';')))
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope.filter(|s| !s.db.is_empty()) {
            if s.db != self.catalog {
                self.schema.clear();
            }
            self.catalog = s.db.clone();
            if let Some(sc) = s.schema.as_deref().filter(|x| !x.is_empty()) {
                self.schema = sc.to_string();
            }
        }
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            if rewrite(&stmt).is_empty() {
                continue;
            }
            out.push(self.run(&stmt)?);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        self.query(scope, &format!("EXPLAIN {}", rewrite(sql)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let processes = self
            .rows("SELECT query_id, state, \"user\", source, created, query FROM system.runtime.queries ORDER BY created DESC")
            .or_else(|_| self.rows("SELECT * FROM system.runtime.queries"))?;
        Ok(ServerSnapshot {
            processes,
            variables: self.rows("SHOW SESSION")?,
            status: self.rows("SELECT * FROM system.runtime.nodes")?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.rows(&format!(
            "CALL system.runtime.kill_query(query_id => {}, message => 'killed by NavDB Studio')",
            lit(id.trim())
        ))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_paging() {
        assert_eq!(rewrite("SELECT * FROM \"a\".\"b\".\"t\" ORDER BY \"id\" LIMIT 5 OFFSET 10;"), "SELECT * FROM \"a\".\"b\".\"t\" ORDER BY \"id\" OFFSET 10 LIMIT 5");
        assert_eq!(rewrite("select 1 limit 5"), "select 1 limit 5");
        assert_eq!(rewrite("-- c\nSELECT 1;  "), "SELECT 1");
        assert_eq!(rewrite("select x from t\nlimit 3\noffset 4"), "select x from t OFFSET 4 LIMIT 3");
    }

    #[test]
    fn maps_types() {
        assert_eq!(to_value("bigint", &json!(9223372036854775807i64)), Value::Int(i64::MAX));
        assert_eq!(to_value("integer", &json!(-3)), Value::Int(-3));
        assert_eq!(to_value("double", &json!(1.5)), Value::Float(1.5));
        assert!(to_value("double", &json!("NaN")).as_f64().is_some_and(|f| f.is_nan()));
        assert_eq!(to_value("real", &json!("Infinity")), Value::Float(f64::INFINITY));
        assert_eq!(to_value("boolean", &json!(true)), Value::Bool(true));
        assert_eq!(to_value("decimal(3, 2)", &json!("1.50")), Value::Text("1.50".into()));
        assert_eq!(to_value("varbinary", &json!("AQI=")), Value::Bytes(vec![1, 2]));
        assert_eq!(to_value("array(integer)", &json!([1, 2])), Value::Text("[1,2]".into()));
        assert_eq!(to_value("map(varchar, integer)", &json!({"k": 1})), Value::Text("{\"k\":1}".into()));
        assert_eq!(to_value("row(a integer, b varchar)", &json!([1, "x"])), Value::Text("[1,\"x\"]".into()));
        assert_eq!(to_value("array(varchar)", &json!("[ \"t1\" ]")), Value::Text("[\"t1\"]".into()));
        assert_eq!(to_value("timestamp(3) with time zone", &json!("2024-01-01 00:00:00.000 UTC")), Value::Text("2024-01-01 00:00:00.000 UTC".into()));
        assert_eq!(to_value("varchar", &Json::Null), Value::Null);
    }

    #[test]
    fn session_properties() {
        assert_eq!(
            parse_properties("query_max_run_time=1h, join_distribution_type = 'BROADCAST',bad"),
            vec![("query_max_run_time".into(), "1h".into()), ("join_distribution_type".into(), "BROADCAST".into())]
        );
        assert_eq!(encode_value("a b,c"), "a%20b%2Cc");
        assert_eq!(decode_value("a%20b%2Cc"), "a b,c");
        assert_eq!(decode_value("x%"), "x%");
    }

    #[test]
    fn casts_insert() {
        let (t, c, v) = split_insert("INSERT INTO \"m\".\"s\".\"t(x\" (\"id\", \"a,b\") VALUES\n(1, 'x)'), (2, NULL)").unwrap();
        assert_eq!(t, "\"m\".\"s\".\"t(x\"");
        assert_eq!(c, vec!["\"id\"".to_string(), "\"a,b\"".to_string()]);
        assert_eq!(v, "(1, 'x)'), (2, NULL)");
        assert_eq!(
            cast_insert_sql(&t, &c, &["integer".into(), "decimal(10,2)".into()], &v),
            "INSERT INTO \"m\".\"s\".\"t(x\" (\"id\", \"a,b\") SELECT CAST(\"_c1\" AS integer), CAST(\"_c2\" AS decimal(10,2)) \
             FROM (VALUES (1, 'x)'), (2, NULL)) AS v(\"_c1\", \"_c2\")"
        );
        assert!(split_insert("INSERT INTO t SELECT 1").is_none());
        assert!(split_insert("SELECT 1").is_none());
    }

    #[test]
    fn read_only_check() {
        assert!(is_read_statement("SELECT 1"));
        assert!(is_read_statement("with x as (select 1) select * from x"));
        assert!(is_read_statement("SET SESSION a = 1"));
        assert!(!is_read_statement("INSERT INTO t VALUES (1)"));
        assert!(!is_read_statement("EXPLAIN ANALYZE INSERT INTO t VALUES (1)"));
        assert!(!is_read_statement("SET ROLE admin"));
    }
}
