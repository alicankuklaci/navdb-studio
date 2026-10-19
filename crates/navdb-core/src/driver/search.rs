//! OpenSearch / Elasticsearch over the REST API.
//!
//! The cluster is the single database, indices are tables and aliases are
//! views. `execute` accepts Dev Tools console requests (`GET /idx/_search` +
//! JSON body), the SQL subset the data grid generates (translated to the
//! query DSL) and any other SQL (sent to the server's SQL endpoint).
//!
//! Options: `scheme` (http/https), `path` (URL prefix), `api_key`
//! (`Authorization: ApiKey ...`), `show_hidden` ("true" lists dot-prefixed
//! system indices).

mod console;
mod tabulate;
mod translate;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Map, Value as Json};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::sqlmini;
use crate::value::{ColumnMeta, QueryResult, Value};
use tabulate::{cell, Fields, Table};
use translate::{enc, pretty, Plan, MAX_WINDOW, ROW_CAP};

const BODY_LIMIT: u64 = 1 << 30;

pub struct SearchDriver {
    kind: DbKind,
    agent: ureq::Agent,
    base: String,
    auth: Option<String>,
    cluster: String,
    version: String,
    opensearch: bool,
    show_hidden: bool,
    fields: HashMap<String, Fields>,
}

/// Readable text of an `error` object (`type: reason`, plus the cause).
pub(crate) fn error_text(e: &Json) -> String {
    let Some(o) = e.as_object() else {
        return e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
    };
    let mut s = match (o.get("type").and_then(|t| t.as_str()), o.get("reason").and_then(|r| r.as_str())) {
        (Some(t), Some(r)) => format!("{}: {}", t, r),
        (None, Some(r)) => r.to_string(),
        (Some(t), None) => t.to_string(),
        (None, None) => e.to_string(),
    };
    if let Some(d) = o.get("details").and_then(|d| d.as_str()).filter(|d| !d.is_empty() && !s.contains(*d)) {
        s.push_str(&format!(" ({})", d));
    }
    let root = o.get("root_cause").and_then(|r| r.get(0)).and_then(|r| r.get("reason")).and_then(|r| r.as_str());
    let cause = o.get("caused_by").map(error_text).or(root.map(str::to_string));
    if let Some(c) = cause.filter(|c| !s.contains(c.as_str())) {
        s.push_str(&format!("; caused by {}", c));
    }
    if let Some(f) = o.get("failed_shards").and_then(|f| f.get(0)).and_then(|f| f.get("reason")) {
        let r = error_text(f);
        if !s.contains(&r) {
            s.push_str(&format!("; {}", r));
        }
    }
    s
}

fn http_error(status: u16, body: &Json) -> anyhow::Error {
    match body.get("error") {
        Some(e) => anyhow!("{}", error_text(e)),
        None => match body {
            Json::Null => anyhow!("HTTP {}", status),
            Json::String(s) => anyhow!("HTTP {}: {}", status, s.trim()),
            other => anyhow!("HTTP {}: {}", status, other),
        },
    }
}

fn text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

fn num(v: &Json) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn result(sql: &str, columns: Vec<ColumnMeta>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult { sql: sql.to_string(), columns, rows, is_query: true, ..Default::default() }
}

fn affected(sql: &str, n: u64) -> QueryResult {
    QueryResult { sql: sql.to_string(), affected_rows: n, ..Default::default() }
}

/// Split a (possibly quoted) qualified name: `"a"."b.c"` -> [a, b.c]; a bare
/// name is kept whole since index names may contain dots.
fn name_parts(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.contains('"') && !s.contains('`') {
        return vec![s.to_string()];
    }
    let mut out = vec![];
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                if chars.peek() == Some(&q) {
                    cur.push(q);
                    chars.next();
                } else {
                    quote = None;
                }
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '`' => quote = Some(c),
            None if c == '.' => out.push(std::mem::take(&mut cur)),
            None if !c.is_whitespace() => cur.push(c),
            None => {}
        }
    }
    out.push(cur);
    out
}

impl SearchDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let scheme = match cfg.option("scheme") {
            "" => if cfg.ssl.enabled { "https" } else { "http" },
            s => s,
        };
        let prefix = cfg.option("path").trim().trim_matches('/');
        let prefix = if prefix.is_empty() { String::new() } else { format!("/{}", prefix) };
        let base = format!("{}://{}:{}{}", scheme, cfg.host, cfg.port, prefix);
        let auth = if !cfg.option("api_key").is_empty() {
            Some(format!("ApiKey {}", cfg.option("api_key")))
        } else if !cfg.password.is_empty() {
            let raw = format!("{}:{}", cfg.user, cfg.password);
            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)))
        } else {
            None
        };
        let mut tls = ureq::tls::TlsConfig::builder().disable_verification(!cfg.ssl.verify_server);
        if !cfg.ssl.ca_cert.is_empty() {
            let pem = std::fs::read(&cfg.ssl.ca_cert).with_context(|| format!("reading CA certificate {}", cfg.ssl.ca_cert))?;
            let certs: Vec<_> = ureq::tls::parse_pem(&pem)
                .filter_map(|i| match i {
                    Ok(ureq::tls::PemItem::Certificate(c)) => Some(c),
                    _ => None,
                })
                .collect();
            tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&certs));
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(cfg.connect_timeout_secs.max(1))))
                .tls_config(tls.build())
                .build(),
        );
        let mut d = SearchDriver {
            kind: cfg.kind,
            agent,
            base,
            auth,
            cluster: String::new(),
            version: String::new(),
            opensearch: cfg.kind == DbKind::OpenSearch,
            show_hidden: cfg.option("show_hidden").eq_ignore_ascii_case("true"),
            fields: HashMap::new(),
        };
        d.refresh_info().with_context(|| format!("connecting to {}", d.base))?;
        Ok(d)
    }

    fn refresh_info(&mut self) -> Result<()> {
        let r = self.call("GET", "/", None)?;
        let dist = r["version"]["distribution"].as_str().unwrap_or("");
        self.opensearch = dist == "opensearch" || (dist.is_empty() && r["tagline"].as_str().is_some_and(|t| t.contains("OpenSearch")));
        self.cluster = r["cluster_name"].as_str().unwrap_or("cluster").to_string();
        self.version = format!(
            "{} {} (cluster {})",
            if self.opensearch { "OpenSearch" } else { "Elasticsearch" },
            r["version"]["number"].as_str().unwrap_or("?"),
            self.cluster
        );
        Ok(())
    }

    /// Send a request; returns the status and the body (JSON, text as a JSON string, or null).
    fn send(&self, method: &str, path: &str, body: Option<&str>, ndjson: bool) -> Result<(u16, Json)> {
        let url = format!("{}{}", self.base, path);
        let mut b = ureq::http::Request::builder().method(method).uri(&url).header("Accept", "application/json");
        if let Some(a) = &self.auth {
            b = b.header("Authorization", a);
        }
        let resp = match body {
            Some(s) => {
                let ct = if ndjson { "application/x-ndjson" } else { "application/json" };
                self.agent.run(b.header("Content-Type", ct).body(s.to_string())?)
            }
            None => self.agent.run(b.body(())?),
        }
        .with_context(|| format!("{} {}", method, url))?;
        let status = resp.status().as_u16();
        let text = resp.into_body().with_config().limit(BODY_LIMIT).read_to_string()?;
        let json = if text.trim().is_empty() {
            Json::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Json::String(text))
        };
        Ok((status, json))
    }

    /// Request that fails on HTTP errors.
    fn call(&self, method: &str, path: &str, body: Option<&Json>) -> Result<Json> {
        let b = body.map(|b| b.to_string());
        let (status, j) = self.send(method, path, b.as_deref(), false)?;
        if status >= 400 {
            return Err(http_error(status, &j));
        }
        Ok(j)
    }

    fn call_text(&self, method: &str, path: &str, body: Option<&str>, ndjson: bool) -> Result<Json> {
        let (status, j) = self.send(method, path, body, ndjson)?;
        if status >= 400 {
            return Err(http_error(status, &j));
        }
        Ok(j)
    }

    fn fields(&mut self, index: &str) -> Result<Fields> {
        if let Some(f) = self.fields.get(index) {
            return Ok(f.clone());
        }
        let m = self.call("GET", &format!("/{}/_mapping", enc(index)), None)?;
        let f = Fields::from_mapping(&m);
        self.fields.insert(index.to_string(), f.clone());
        Ok(f)
    }

    fn label(&self) -> &'static str {
        if self.opensearch { "OpenSearch" } else { "Elasticsearch" }
    }

    // ---- console ----

    fn run_console(&mut self, text: &str) -> Result<Vec<QueryResult>> {
        let mut out = vec![];
        for req in console::parse(text)? {
            self.fields.clear();
            let start = Instant::now();
            let mut path = req.path.clone();
            if path.starts_with("/_cat") {
                path = force_json(&path);
            }
            let (status, resp) = self.send(&req.method, &path, req.body.as_deref(), req.ndjson)?;
            if req.method == "HEAD" {
                out.push(result(&req.text, vec![ColumnMeta::new("status")], vec![vec![Value::Int(status as i64)]]));
                continue;
            }
            if status >= 400 {
                return Err(http_error(status, &resp)).with_context(|| format!("{} {}", req.method, req.path));
            }
            let elapsed = start.elapsed().as_millis();
            for mut r in tabulate::response_results(&resp, &req.text) {
                r.elapsed_ms = elapsed;
                out.push(r);
            }
        }
        Ok(out)
    }

    // ---- SQL ----

    fn run_sql(&mut self, sql: &str) -> Result<QueryResult> {
        let start = Instant::now();
        let mut r = if let Some(r) = self.run_ddl(sql)? {
            r
        } else {
            match sqlmini::parse(sql) {
                Ok(st) => self.run_plan(sql, &st)?,
                Err(_) => {
                    self.fields.clear();
                    self.server_sql(sql)?
                }
            }
        };
        r.elapsed_ms = start.elapsed().as_millis();
        Ok(r)
    }

    /// DROP TABLE / DROP VIEW / TRUNCATE TABLE.
    fn run_ddl(&mut self, sql: &str) -> Result<Option<QueryResult>> {
        let re = regex::Regex::new(r"(?is)^\s*(DROP\s+(TABLE|VIEW|INDEX)|TRUNCATE(\s+TABLE)?)\s+(IF\s+EXISTS\s+)?(.+?)\s*;?\s*$")?;
        let Some(c) = re.captures(sql) else {
            return Ok(None);
        };
        let mut parts = name_parts(&c[5]);
        if parts.len() > 1 && parts[0] == self.cluster {
            parts.remove(0);
        }
        let name = parts.join(".");
        let if_exists = c.get(4).is_some();
        self.fields.clear();
        let (method, path, body) = if c[1].to_ascii_uppercase().starts_with("TRUNCATE") {
            ("POST", format!("/{}/_delete_by_query?refresh=true&conflicts=proceed", enc(&name)), Some(json!({"query": {"match_all": {}}}).to_string()))
        } else if c[2].eq_ignore_ascii_case("VIEW") {
            ("DELETE", format!("/_all/_alias/{}", enc(&name)), None)
        } else {
            ("DELETE", format!("/{}", enc(&name)), None)
        };
        let (status, resp) = self.send(method, &path, body.as_deref(), false)?;
        if status == 404 && if_exists {
            return Ok(Some(affected(sql, 0)));
        }
        if status >= 400 {
            return Err(http_error(status, &resp));
        }
        Ok(Some(affected(sql, resp["deleted"].as_u64().unwrap_or(0))))
    }

    fn plan_for(&mut self, st: &sqlmini::Statement) -> Result<(Plan, Fields)> {
        let index = translate::target(st).to_string();
        let fields = match self.fields(&index) {
            Ok(f) => f,
            // INSERT creates the index
            Err(_) if matches!(st, sqlmini::Statement::Insert { .. }) => Fields::default(),
            Err(e) => return Err(e),
        };
        Ok((translate::plan(st, &fields)?, fields))
    }

    fn run_plan(&mut self, sql: &str, st: &sqlmini::Statement) -> Result<QueryResult> {
        let (plan, fields) = self.plan_for(st)?;
        if !matches!(plan, Plan::Search { .. } | Plan::Count { .. }) {
            self.fields.remove(plan.index());
        }
        let (method, path, body) = plan.request();
        match &plan {
            Plan::Search { .. } => self.search(sql, &plan, &fields),
            Plan::Count { .. } => {
                let r = self.call_text(method, &path, body.as_deref(), false)?;
                let n = r["count"].as_i64().ok_or_else(|| anyhow!("unexpected _count response: {}", r))?;
                Ok(result(sql, vec![ColumnMeta { name: "COUNT(*)".into(), type_name: "long".into() }], vec![vec![Value::Int(n)]]))
            }
            Plan::Bulk { .. } => {
                let r = self.call_text(method, &path, body.as_deref(), true)?;
                let items = r["items"].as_array().cloned().unwrap_or_default();
                let ok = items.iter().filter(|i| i.as_object().and_then(|o| o.values().next()).and_then(|v| v["status"].as_u64()).is_some_and(|s| s < 300)).count();
                if r["errors"].as_bool() == Some(true) {
                    let err = items
                        .iter()
                        .filter_map(|i| i.as_object().and_then(|o| o.values().next()))
                        .find(|v| !v["error"].is_null())
                        .map(|v| error_text(&v["error"]))
                        .unwrap_or_default();
                    bail!("{} of {} documents failed: {}", items.len() - ok, items.len(), err);
                }
                Ok(affected(sql, ok as u64))
            }
            Plan::UpdateDoc { .. } => {
                self.call_text(method, &path, body.as_deref(), false)?;
                Ok(affected(sql, 1))
            }
            Plan::UpdateByQuery { .. } | Plan::DeleteByQuery { .. } => {
                let r = self.call_text(method, &path, body.as_deref(), false)?;
                if let Some(f) = r["failures"].as_array().and_then(|f| f.first()) {
                    bail!("{}", error_text(f.get("cause").unwrap_or(f)));
                }
                let key = if matches!(plan, Plan::DeleteByQuery { .. }) { "deleted" } else { "updated" };
                Ok(affected(sql, r[key].as_u64().unwrap_or(0)))
            }
            Plan::DeleteDoc { .. } => {
                let (status, r) = self.send(method, &path, None, false)?;
                if status == 404 && r["result"] == "not_found" {
                    return Ok(affected(sql, 0));
                }
                if status >= 400 {
                    return Err(http_error(status, &r));
                }
                Ok(affected(sql, 1))
            }
        }
    }

    fn search(&mut self, sql: &str, plan: &Plan, fields: &Fields) -> Result<QueryResult> {
        let Plan::Search { columns, offset, limit, .. } = plan else {
            bail!("not a search");
        };
        let (offset, limit) = (*offset, *limit);
        let (method, path, body) = plan.request();
        let with_score = columns.iter().any(|c| c == "_score");
        let mut table = if columns.is_empty() {
            Table::with_columns(std::iter::once(("_id".to_string(), "keyword".to_string())).chain(fields.columns.clone()))
        } else {
            Table::with_columns(columns.iter().map(|c| {
                let ty = match c.as_str() {
                    "_id" => "keyword",
                    "_score" => "float",
                    c => fields.type_of(c).unwrap_or(""),
                };
                (c.clone(), ty.to_string())
            }))
        };
        let add = |table: &mut Table, h: &Json| {
            let row = tabulate::hit_row(h, Some(fields), false, with_score);
            let row = if columns.is_empty() { row } else { row.into_iter().filter(|(k, _)| columns.contains(k)).collect() };
            table.push(row);
        };
        let mut resp = self.call_text(method, &path, body.as_deref(), false)?;
        if offset + limit <= MAX_WINDOW {
            for h in resp["hits"]["hits"].as_array().into_iter().flatten() {
                add(&mut table, h);
            }
        } else {
            let (mut skipped, mut taken) = (0u64, 0u64);
            let mut scroll_id = resp["_scroll_id"].as_str().map(str::to_string);
            loop {
                let hits = resp["hits"]["hits"].as_array().cloned().unwrap_or_default();
                if hits.is_empty() {
                    break;
                }
                for h in &hits {
                    if skipped < offset {
                        skipped += 1;
                    } else if taken < limit {
                        add(&mut table, h);
                        taken += 1;
                    }
                }
                let Some(id) = scroll_id.clone().filter(|_| taken < limit) else { break };
                resp = self.call("POST", "/_search/scroll", Some(&json!({"scroll": "1m", "scroll_id": id})))?;
                scroll_id = resp["_scroll_id"].as_str().map(str::to_string).or(scroll_id);
            }
            if let Some(id) = scroll_id {
                let _ = self.call("DELETE", "/_search/scroll", Some(&json!({"scroll_id": [id]})));
            }
        }
        Ok(table.finish(sql))
    }

    /// The server's SQL endpoint (OpenSearch SQL plugin / Elasticsearch SQL).
    fn server_sql(&mut self, sql: &str) -> Result<QueryResult> {
        let q = sql.trim().trim_end_matches(';');
        let (endpoint, close) = if self.opensearch {
            ("/_plugins/_sql?format=jdbc", "/_plugins/_sql/close")
        } else {
            ("/_sql?format=json", "/_sql/close")
        };
        let mut resp = match self.call("POST", endpoint, Some(&json!({"query": q, "fetch_size": 1000}))) {
            Ok(r) => r,
            // statements that do not support paging
            Err(_) => self.call("POST", endpoint, Some(&json!({"query": q})))?,
        };
        let cols = if self.opensearch { &resp["schema"] } else { &resp["columns"] };
        let columns: Vec<ColumnMeta> = cols
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| ColumnMeta {
                name: c["alias"].as_str().or(c["name"].as_str()).unwrap_or("").to_string(),
                type_name: text(&c["type"]),
            })
            .collect();
        let rows_key = if self.opensearch { "datarows" } else { "rows" };
        if columns.is_empty() && resp.get(rows_key).is_none() {
            // DML / other statements answer with plain JSON
            return Ok(tabulate::response_results(&resp, sql).pop().unwrap_or_default());
        }
        let mut rows = vec![];
        loop {
            for r in resp[rows_key].as_array().into_iter().flatten() {
                if rows.len() as u64 >= ROW_CAP {
                    break;
                }
                rows.push(r.as_array().map(|a| a.iter().map(cell).collect()).unwrap_or_default());
            }
            let Some(cursor) = resp["cursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string) else { break };
            if rows.len() as u64 >= ROW_CAP {
                let _ = self.call("POST", close, Some(&json!({"cursor": cursor})));
                break;
            }
            resp = self.call("POST", endpoint, Some(&json!({"cursor": cursor})))?;
        }
        Ok(result(sql, columns, rows))
    }

    fn list_indices(&mut self) -> Result<Vec<ObjectInfo>> {
        let mut path = "/_cat/indices?format=json&bytes=b&s=index&h=health,status,index,uuid,pri,rep,docs.count,store.size,creation.date.string".to_string();
        if self.show_hidden {
            path.push_str("&expand_wildcards=all");
        }
        let r = self.call("GET", &path, None)?;
        let mut out = vec![];
        for i in r.as_array().into_iter().flatten() {
            let name = text(&i["index"]);
            if name.is_empty() || (name.starts_with('.') && !self.show_hidden) {
                continue;
            }
            out.push(ObjectInfo {
                name,
                rows: num(&i["docs.count"]),
                data_length: num(&i["store.size"]),
                engine: self.label().into(),
                created: text(&i["creation.date.string"]),
                extra: ["health", "status", "pri", "rep", "uuid"]
                    .iter()
                    .map(|k| (k.to_string(), text(&i[*k])))
                    .collect(),
                ..Default::default()
            });
        }
        Ok(out)
    }

    fn list_aliases(&mut self) -> Result<Vec<ObjectInfo>> {
        let r = self.call("GET", "/_cat/aliases?format=json&s=alias", None)?;
        let mut by_alias: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for a in r.as_array().into_iter().flatten() {
            let alias = text(&a["alias"]);
            let index = text(&a["index"]);
            if alias.is_empty() || ((alias.starts_with('.') || index.starts_with('.')) && !self.show_hidden) {
                continue;
            }
            by_alias.entry(alias).or_default().push((index, text(&a["filter"])));
        }
        Ok(by_alias
            .into_iter()
            .map(|(alias, idx)| ObjectInfo {
                comment: idx.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(", "),
                extra: vec![
                    ("indices".into(), idx.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>().join(", ")),
                    ("filter".into(), idx.iter().map(|(_, f)| f.as_str()).find(|f| *f != "-" && !f.is_empty()).unwrap_or("-").into()),
                ],
                ..ObjectInfo::named(alias)
            })
            .collect())
    }

    fn snapshot_processes(&self) -> Result<QueryResult> {
        let r = self.call("GET", "/_tasks?detailed=true", None)?;
        let cols = ["Id", "Node", "Action", "Type", "Description", "Start_time", "Running_ms", "Cancellable", "Parent"];
        let mut rows = vec![];
        for (_, node) in r["nodes"].as_object().into_iter().flatten() {
            for (id, t) in node["tasks"].as_object().into_iter().flatten() {
                let start = t["start_time_in_millis"]
                    .as_i64()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                rows.push(vec![
                    Value::Text(id.clone()),
                    Value::Text(text(&node["name"])),
                    cell(&t["action"]),
                    cell(&t["type"]),
                    cell(&t["description"]),
                    Value::Text(start),
                    t["running_time_in_nanos"].as_i64().map(|n| Value::Int(n / 1_000_000)).unwrap_or_default(),
                    cell(&t["cancellable"]),
                    cell(&t["parent_task_id"]),
                ]);
            }
        }
        Ok(result("GET /_tasks", cols.iter().map(|c| ColumnMeta::new(*c)).collect(), rows))
    }

    fn snapshot_variables(&self) -> Result<QueryResult> {
        let r = self.call("GET", "/_cluster/settings?include_defaults=true&flat_settings=true", None)?;
        let mut vars: BTreeMap<String, (Value, &str)> = BTreeMap::new();
        for scope in ["defaults", "persistent", "transient"] {
            for (k, v) in r[scope].as_object().into_iter().flatten() {
                vars.insert(k.clone(), (cell(v), scope));
            }
        }
        Ok(result(
            "GET /_cluster/settings",
            vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value"), ColumnMeta::new("Scope")],
            vars.into_iter().map(|(k, (v, s))| vec![Value::Text(k), v, Value::Text(s.into())]).collect(),
        ))
    }

    fn snapshot_status(&self) -> Result<QueryResult> {
        let mut rows = vec![];
        let h = self.call("GET", "/_cluster/health", None)?;
        for (k, v) in h.as_object().into_iter().flatten() {
            rows.push(vec![Value::Text("cluster".into()), Value::Text(k.clone()), cell(v)]);
        }
        let s = self.call("GET", "/_nodes/stats/jvm,os,process,fs,indices", None)?;
        let keys: [(&str, &[&str]); 14] = [
            ("host", &["host"]),
            ("roles", &["roles"]),
            ("jvm.uptime_ms", &["jvm", "uptime_in_millis"]),
            ("jvm.heap_used_percent", &["jvm", "mem", "heap_used_percent"]),
            ("jvm.heap_used_bytes", &["jvm", "mem", "heap_used_in_bytes"]),
            ("jvm.heap_max_bytes", &["jvm", "mem", "heap_max_in_bytes"]),
            ("os.cpu_percent", &["os", "cpu", "percent"]),
            ("os.mem_used_percent", &["os", "mem", "used_percent"]),
            ("process.open_file_descriptors", &["process", "open_file_descriptors"]),
            ("fs.available_bytes", &["fs", "total", "available_in_bytes"]),
            ("indices.docs", &["indices", "docs", "count"]),
            ("indices.store_bytes", &["indices", "store", "size_in_bytes"]),
            ("indices.search.query_total", &["indices", "search", "query_total"]),
            ("indices.indexing.index_total", &["indices", "indexing", "index_total"]),
        ];
        for (_, n) in s["nodes"].as_object().into_iter().flatten() {
            let section = format!("node {}", text(&n["name"]));
            for (label, path) in keys {
                let v = path.iter().fold(n, |acc, p| &acc[*p]);
                if !v.is_null() {
                    rows.push(vec![Value::Text(section.clone()), Value::Text(label.into()), cell(v)]);
                }
            }
        }
        Ok(result(
            "GET /_cluster/health",
            vec![ColumnMeta::new("Section"), ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            rows,
        ))
    }
}

/// `_cat` APIs: always ask for JSON.
fn force_json(path: &str) -> String {
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let mut params: Vec<&str> = q.split('&').filter(|kv| !kv.is_empty() && !kv.starts_with("format=")).collect();
    params.push("format=json");
    format!("{}?{}", p, params.join("&"))
}

const SETTINGS_KEEP: [&str; 8] =
    ["number_of_shards", "number_of_replicas", "analysis", "refresh_interval", "max_result_window", "sort", "knn", "codec"];

impl Driver for SearchDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn dialect(&self) -> Dialect {
        Dialect::Generic
    }

    fn server_version(&mut self) -> Result<String> {
        self.refresh_info()?;
        Ok(self.version.clone())
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        self.refresh_info()?;
        Ok(vec![self.cluster.clone()])
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View]
    }

    fn list_objects(&mut self, _scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        match kind {
            ObjectKind::Table => self.list_indices(),
            ObjectKind::View => self.list_aliases(),
            _ => Ok(vec![]),
        }
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        self.fields.remove(&path.name);
        let f = self.fields(&path.name)?;
        let mut columns = vec![ColumnDef { primary_key: true, nullable: false, ..ColumnDef::new("_id", "keyword") }];
        for (name, ty) in &f.columns {
            let extra = f
                .subfields
                .get(name)
                .map(|s| s.iter().map(|(n, t)| format!("{} ({})", n, t)).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            columns.push(ColumnDef { extra, ..ColumnDef::new(name.clone(), ty.clone()) });
        }
        Ok(TableDef {
            path: path.clone(),
            columns,
            indexes: vec![IndexDef { name: "_id".into(), columns: vec!["_id".into()], unique: true, primary: true, ..Default::default() }],
            engine: self.label().into(),
            ..Default::default()
        })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        if kind == ObjectKind::View {
            let r = self.call("GET", &format!("/_alias/{}", enc(&path.name)), None)?;
            let mut actions = vec![];
            for (index, v) in r.as_object().into_iter().flatten() {
                let mut add = Map::new();
                add.insert("index".into(), json!(index));
                add.insert("alias".into(), json!(path.name));
                if let Some(o) = v["aliases"][&path.name].as_object() {
                    add.extend(o.clone());
                }
                actions.push(json!({ "add": add }));
            }
            return Ok(format!("POST /_aliases\n{}", pretty(&json!({ "actions": actions }))));
        }
        let r = self.call("GET", &format!("/{}", enc(&path.name)), None)?;
        let obj = r.as_object().ok_or_else(|| anyhow!("unexpected response for {}", path.name))?;
        let (name, def) = obj
            .get_key_value(&path.name)
            .or_else(|| obj.iter().next())
            .ok_or_else(|| anyhow!("index {} not found", path.name))?;
        let mut settings = Map::new();
        if let Some(ix) = def["settings"]["index"].as_object() {
            for k in SETTINGS_KEEP {
                if let Some(v) = ix.get(k) {
                    settings.insert(k.to_string(), v.clone());
                }
            }
        }
        let mut body = Map::new();
        body.insert("settings".into(), json!({ "index": settings }));
        body.insert("mappings".into(), def["mappings"].clone());
        if def["aliases"].as_object().is_some_and(|a| !a.is_empty()) {
            body.insert("aliases".into(), def["aliases"].clone());
        }
        Ok(format!("PUT /{}\n{}", name, pretty(&Json::Object(body))))
    }

    fn execute(&mut self, _scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if console::is_console(sql) {
            return self.run_console(sql);
        }
        let mut out = vec![];
        for stmt in crate::sqlutil::split_statements(sql, Dialect::Generic) {
            let s = crate::sqlutil::strip_leading_comments(&stmt).trim().to_string();
            if s.is_empty() {
                continue;
            }
            out.push(self.run_sql(&s)?);
        }
        Ok(out)
    }

    fn explain(&mut self, _scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        if console::is_console(sql) {
            bail!("EXPLAIN is available for SQL statements; add \"explain\": true or \"profile\": true to a _search body instead");
        }
        let q = sql.trim().trim_end_matches(';');
        let text = match sqlmini::parse(q) {
            Ok(st) => self.plan_for(&st)?.0.describe(),
            Err(_) => {
                let r = if self.opensearch {
                    self.call("POST", "/_plugins/_sql/_explain", Some(&json!({ "query": q })))?
                } else {
                    self.call("POST", "/_sql/translate", Some(&json!({ "query": q })))?
                };
                format!("{}\n{}", if self.opensearch { "POST /_plugins/_sql/_explain" } else { "POST /_sql/translate" }, pretty(&r))
            }
        };
        Ok(result(sql, vec![ColumnMeta::new("Query DSL")], text.lines().map(|l| vec![Value::Text(l.to_string())]).collect()))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.snapshot_processes()?,
            variables: self.snapshot_variables()?,
            status: self.snapshot_status()?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let r = self.call("POST", &format!("/_tasks/{}/_cancel", enc(id).replace("%3A", ":")), None)?;
        for k in ["node_failures", "task_failures"] {
            if let Some(f) = r[k].as_array().and_then(|f| f.first()) {
                bail!("{}", error_text(f.get("caused_by").unwrap_or(f)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors() {
        let e = json!({"root_cause": [{"type": "index_not_found_exception", "reason": "no such index [x]"}], "type": "index_not_found_exception", "reason": "no such index [x]"});
        assert_eq!(error_text(&e), "index_not_found_exception: no such index [x]");
        let e = json!({"type": "search_phase_execution_exception", "reason": "all shards failed", "caused_by": {"type": "illegal_argument_exception", "reason": "bad field"}});
        assert_eq!(error_text(&e), "search_phase_execution_exception: all shards failed; caused by illegal_argument_exception: bad field");
        assert_eq!(error_text(&json!("plain")), "plain");
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(name_parts("\"c\".\"logs.2024\""), vec!["c", "logs.2024"]);
        assert_eq!(name_parts("logs.2024"), vec!["logs.2024"]);
        assert_eq!(force_json("/_cat/indices?v&format=txt"), "/_cat/indices?v&format=json");
        assert_eq!(force_json("/_cat/nodes"), "/_cat/nodes?format=json");
    }
}
