//! MongoDB driver. Collections are exposed as tables (columns inferred from a
//! sample of documents). `execute` accepts both the generic SQL the data grid
//! generates (translated through [`crate::sqlmini`]) and mongo shell syntax.
//!
//! Options (`cfg.options`): `uri` (full connection string, overrides host /
//! port / user), `auth_source`, `replica_set`, `direct_connection`.

mod shell;

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use mongodb::bson::{self, doc, oid::ObjectId, Bson, Document};
use mongodb::sync::Client;

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::model::*;
use crate::sqlmini::{self, CmpOp, Expr, Statement};
use crate::value::{ColumnMeta, QueryResult, Value};
use shell::{Call, Stmt};

/// Rows kept from a shell `find` / `aggregate` without an explicit limit.
const MAX_ROWS: usize = 10_000;
/// Documents sampled to infer a collection's columns.
const SAMPLE: i64 = 200;

pub struct MongoDriver {
    client: Client,
    current_db: String,
    kind: DbKind,
}

enum Op {
    Find { cmd: Document, cols: Option<Vec<String>> },
    Agg { cmd: Document },
    /// `aggregate` = pipeline ending in `$count: "n"`, otherwise a `count` command.
    Count { cmd: Document, label: String, aggregate: bool },
    Distinct { key: String, cmd: Document },
    Write { cmd: Document },
    FindModify { cmd: Document },
    Cmd { admin: bool, cmd: Document },
    CmdField { admin: bool, cmd: Document, field: String },
    Cursor { cmd: Document },
    Names { cmd: Document },
    Scalar { label: String, value: Value },
    Use(String),
    Explain { inner: Box<Op>, verbosity: String },
}

fn merr(e: mongodb::error::Error) -> anyhow::Error {
    match e.kind.as_ref() {
        mongodb::error::ErrorKind::Command(c) => anyhow!("{} ({} {})", c.message, c.code_name, c.code),
        _ => anyhow!("{}", e),
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn build_uri(cfg: &ConnectionConfig) -> String {
    let ms = cfg.connect_timeout_secs.max(1) * 1000;
    let custom = cfg.option("uri").trim();
    let mut params: Vec<(String, String)> = vec![];
    let mut uri = if !custom.is_empty() {
        custom.to_string()
    } else {
        let auth = match (cfg.user.is_empty(), cfg.password.is_empty()) {
            (true, _) => String::new(),
            (false, true) => format!("{}@", enc(&cfg.user)),
            (false, false) => format!("{}:{}@", enc(&cfg.user), enc(&cfg.password)),
        };
        let host = if cfg.host.is_empty() { "localhost" } else { cfg.host.as_str() };
        let multi = host.contains(',');
        let hosts = if multi || host.contains(':') && !host.starts_with('[') { host.to_string() } else { format!("{}:{}", host, cfg.port) };
        if !cfg.user.is_empty() {
            let src = cfg.option("auth_source");
            params.push(("authSource".into(), if src.is_empty() { "admin".into() } else { enc(src) }));
        }
        let rs = cfg.option("replica_set");
        if !rs.is_empty() {
            params.push(("replicaSet".into(), enc(rs)));
        }
        let direct = match cfg.option("direct_connection").to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => !multi && rs.is_empty(),
        };
        params.push(("directConnection".into(), direct.to_string()));
        if cfg.ssl.enabled {
            params.push(("tls".into(), "true".into()));
            if !cfg.ssl.verify_server {
                params.push(("tlsAllowInvalidCertificates".into(), "true".into()));
            }
            if !cfg.ssl.ca_cert.is_empty() {
                params.push(("tlsCAFile".into(), enc(&cfg.ssl.ca_cert)));
            }
            if !cfg.ssl.client_cert.is_empty() {
                params.push(("tlsCertificateKeyFile".into(), enc(&cfg.ssl.client_cert)));
            }
        }
        format!("mongodb://{}{}/{}", auth, hosts, enc(&cfg.database))
    };
    let lower = uri.to_ascii_lowercase();
    for k in ["connectTimeoutMS", "serverSelectionTimeoutMS"] {
        if !lower.contains(&k.to_ascii_lowercase()) {
            params.push((k.into(), ms.to_string()));
        }
    }
    if !lower.contains("appname") {
        params.push(("appName".into(), "NavDB".into()));
    }
    if !params.is_empty() {
        let q = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
        if uri.contains('?') {
            uri.push('&');
        } else if uri.split_once("://").is_some_and(|(_, rest)| rest.contains('/')) {
            uri.push('?');
        } else {
            uri.push_str("/?");
        }
        uri.push_str(&q);
    }
    uri
}

// ---------------------------------------------------------------- values

fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// Relaxed extended JSON (Decimal128-safe: the bson crate panics on it).
fn to_json(b: &Bson) -> serde_json::Value {
    match b {
        Bson::Document(d) => serde_json::Value::Object(d.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        Bson::Array(a) => serde_json::Value::Array(a.iter().map(to_json).collect()),
        Bson::Decimal128(d) => serde_json::json!({ "$numberDecimal": d.to_string() }),
        other => other.clone().into_relaxed_extjson(),
    }
}

fn json_text(b: &Bson) -> String {
    to_json(b).to_string()
}

fn json_pretty(b: &Bson) -> String {
    serde_json::to_string_pretty(&to_json(b)).unwrap_or_default()
}

fn date_text(d: &bson::DateTime) -> String {
    d.try_to_rfc3339_string().unwrap_or_else(|_| d.timestamp_millis().to_string())
}

fn to_value(b: &Bson) -> Value {
    match b {
        Bson::Double(f) => Value::Float(*f),
        Bson::String(s) | Bson::Symbol(s) | Bson::JavaScriptCode(s) => Value::Text(s.clone()),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(i) => Value::Int(*i as i64),
        Bson::Int64(i) => Value::Int(*i),
        Bson::ObjectId(o) => Value::Text(o.to_hex()),
        Bson::DateTime(d) => Value::Text(date_text(d)),
        Bson::Decimal128(d) => Value::Text(d.to_string()),
        Bson::Binary(b) => Value::Bytes(b.bytes.clone()),
        Bson::RegularExpression(r) => Value::Text(format!("/{}/{}", r.pattern, r.options)),
        Bson::Timestamp(t) => Value::Text(format!("Timestamp({}, {})", t.time, t.increment)),
        Bson::MinKey => Value::Text("MinKey".into()),
        Bson::MaxKey => Value::Text("MaxKey".into()),
        other => Value::Text(json_text(other)),
    }
}

fn is_hex24(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn iso_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})$").expect("valid regex"))
}

fn int_bson(v: i64) -> Bson {
    i32::try_from(v).map(Bson::Int32).unwrap_or(Bson::Int64(v))
}

/// Grid value -> BSON: 24-hex `_id` becomes an ObjectId, JSON objects/arrays
/// become embedded documents, RFC 3339 timestamps become dates.
fn to_bson(col: &str, v: &Value) -> Bson {
    match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(i) => int_bson(*i),
        Value::UInt(u) => i64::try_from(*u).map(int_bson).unwrap_or(Bson::Double(*u as f64)),
        Value::Float(f) => Bson::Double(*f),
        Value::Bytes(b) => Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Generic, bytes: b.clone() }),
        Value::Text(s) => {
            let t = s.trim();
            if (col == "_id" || col.ends_with("._id")) && is_hex24(t) {
                if let Ok(o) = ObjectId::parse_str(t) {
                    return Bson::ObjectId(o);
                }
            }
            if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
                if let Ok(b @ (Bson::Document(_) | Bson::Array(_))) = shell::parse_value(t) {
                    return b;
                }
            }
            if iso_re().is_match(t) {
                if let Ok(d) = bson::DateTime::parse_rfc3339_str(t) {
                    return Bson::DateTime(d);
                }
            }
            Bson::String(s.clone())
        }
    }
}

/// Candidate BSON values for an equality match: the converted value plus its
/// text form, so `_id = '<hex>'` matches ObjectIds and strings alike.
fn alts(col: &str, v: &Value) -> Vec<Bson> {
    let b = to_bson(col, v);
    match v {
        Value::Text(s) if !matches!(b, Bson::String(_)) => vec![b, Bson::String(s.clone())],
        Value::Int(_) | Value::UInt(_) | Value::Float(_) => vec![b, Bson::String(v.to_string())],
        _ => vec![b],
    }
}

fn filter_doc(e: &Expr) -> Document {
    let docs = |v: &[Expr]| v.iter().map(|x| Bson::Document(filter_doc(x))).collect::<Vec<_>>();
    match e {
        Expr::And(v) => doc! { "$and": docs(v) },
        Expr::Or(v) => doc! { "$or": docs(v) },
        Expr::Not(x) => doc! { "$nor": [filter_doc(x)] },
        Expr::Cmp { column, op, value } => {
            let mut a = alts(column, value);
            let cond = match op {
                CmpOp::Eq if a.len() == 1 => doc! { "$eq": a.remove(0) },
                CmpOp::Eq => doc! { "$in": a },
                CmpOp::Ne if a.len() == 1 => doc! { "$ne": a.remove(0) },
                CmpOp::Ne => doc! { "$nin": a },
                CmpOp::Lt => doc! { "$lt": a.remove(0) },
                CmpOp::Le => doc! { "$lte": a.remove(0) },
                CmpOp::Gt => doc! { "$gt": a.remove(0) },
                CmpOp::Ge => doc! { "$gte": a.remove(0) },
            };
            doc! { column: cond }
        }
        Expr::Like { column, pattern, negated } => {
            let re = shell::regex(sqlmini::like_to_regex(pattern), "i");
            if *negated {
                doc! { column: { "$not": re } }
            } else {
                doc! { column: re }
            }
        }
        Expr::In { column, values, negated } => {
            let all: Vec<Bson> = values.iter().flat_map(|v| alts(column, v)).collect();
            doc! { column: { if *negated { "$nin" } else { "$in" }: all } }
        }
        Expr::Between { column, low, high } => {
            doc! { column: { "$gte": to_bson(column, low), "$lte": to_bson(column, high) } }
        }
        Expr::IsNull { column, negated: false } => doc! { column: Bson::Null },
        Expr::IsNull { column, negated: true } => doc! { column: { "$ne": Bson::Null } },
    }
}

fn get_path<'a>(d: &'a Document, path: &str) -> Option<&'a Bson> {
    if let Some(v) = d.get(path) {
        return Some(v);
    }
    let mut cur = d;
    let mut parts = path.split('.').peekable();
    while let Some(p) = parts.next() {
        let v = cur.get(p)?;
        if parts.peek().is_none() {
            return Some(v);
        }
        cur = v.as_document()?;
    }
    None
}

/// Top-level keys in first-seen order, `_id` first.
fn union_keys(docs: &[Document]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    if docs.iter().any(|d| d.contains_key("_id")) {
        seen.insert("_id".to_string());
        out.push("_id".to_string());
    }
    for d in docs {
        for k in d.keys() {
            if seen.insert(k.clone()) {
                out.push(k.clone());
            }
        }
    }
    out
}

/// Type of a column over many values (ignoring nulls); "mixed" for several.
fn merged_type<'a>(vals: impl Iterator<Item = &'a Bson>) -> String {
    let mut t: Option<&str> = None;
    for v in vals {
        let n = type_name(v);
        if n == "null" {
            continue;
        }
        match t {
            None => t = Some(n),
            Some(x) if x != n => return "mixed".into(),
            _ => {}
        }
    }
    t.unwrap_or("null").to_string()
}

fn docs_result(docs: &[Document], fixed: Option<&[String]>) -> QueryResult {
    let cols = match fixed {
        Some(f) => f.to_vec(),
        None => union_keys(docs),
    };
    let columns = cols
        .iter()
        .map(|c| ColumnMeta { name: c.clone(), type_name: merged_type(docs.iter().filter_map(|d| get_path(d, c))) })
        .collect();
    let rows = docs
        .iter()
        .map(|d| cols.iter().map(|c| get_path(d, c).map(to_value).unwrap_or(Value::Null)).collect())
        .collect();
    QueryResult { columns, rows, is_query: true, ..Default::default() }
}

fn fields_result(d: &Document) -> QueryResult {
    let mut r = QueryResult {
        columns: vec![ColumnMeta::new("Field"), ColumnMeta::new("Value")],
        is_query: true,
        ..Default::default()
    };
    for (k, v) in d.iter().filter(|(k, _)| !matches!(k.as_str(), "$clusterTime" | "operationTime" | "signature")) {
        r.rows.push(vec![Value::Text(k.clone()), to_value(v)]);
    }
    r
}

fn single(label: &str, type_name: &str, v: Value) -> QueryResult {
    QueryResult {
        columns: vec![ColumnMeta { name: label.into(), type_name: type_name.into() }],
        rows: vec![vec![v]],
        is_query: true,
        ..Default::default()
    }
}

fn num(b: Option<&Bson>) -> Option<f64> {
    match b? {
        Bson::Int32(v) => Some(*v as f64),
        Bson::Int64(v) => Some(*v as f64),
        Bson::Double(v) => Some(*v),
        Bson::Boolean(v) => Some(*v as i64 as f64),
        Bson::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn write_count(reply: &Document) -> Result<u64> {
    if let Ok(errs) = reply.get_array("writeErrors") {
        if let Some(Bson::Document(e)) = errs.first() {
            bail!("{} (code {})", e.get_str("errmsg").unwrap_or("write error"), e.get("code").map(|c| c.to_string()).unwrap_or_default());
        }
    }
    if let Ok(e) = reply.get_document("writeConcernError") {
        bail!("{}", e.get_str("errmsg").unwrap_or("write concern error"));
    }
    Ok(num(reply.get("n")).unwrap_or(0.0) as u64)
}

// ---------------------------------------------------------------- compiling

fn doc_arg(args: &[Bson], i: usize) -> Result<Document> {
    match args.get(i) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(o) => bail!("argument {} must be a document, got {}", i + 1, o),
    }
}

fn req_doc(args: &[Bson], i: usize, what: &str) -> Result<Document> {
    match args.get(i) {
        Some(Bson::Document(d)) => Ok(d.clone()),
        _ => bail!("{} expects a document as argument {}", what, i + 1),
    }
}

fn int_arg(args: &[Bson], i: usize) -> Result<i64> {
    num(args.get(i)).map(|v| v as i64).ok_or_else(|| anyhow!("argument {} must be a number", i + 1))
}

fn docs_arg(b: Option<&Bson>, what: &str) -> Result<Vec<Bson>> {
    match b {
        Some(Bson::Array(a)) if a.iter().all(|x| matches!(x, Bson::Document(_))) => Ok(a.clone()),
        _ => bail!("{} expects an array of documents", what),
    }
}

fn with_id(mut d: Document) -> Bson {
    if !d.contains_key("_id") {
        let mut n = doc! { "_id": ObjectId::new() };
        n.extend(d);
        d = n;
    }
    Bson::Document(d)
}

fn copy_opts(dst: &mut Document, opts: &Document, keys: &[&str]) {
    for k in keys {
        if let Some(v) = opts.get(*k) {
            dst.insert(*k, v.clone());
        }
    }
}

fn count_pipeline(filter: Document, skip: Option<i64>, limit: Option<i64>) -> Vec<Document> {
    let mut p = vec![doc! { "$match": filter }];
    if let Some(s) = skip.filter(|s| *s > 0) {
        p.push(doc! { "$skip": s });
    }
    if let Some(l) = limit.filter(|l| *l > 0) {
        p.push(doc! { "$limit": l });
    }
    p.push(doc! { "$count": "n" });
    p
}

fn compile_sql(cur_db: &str, sql: &str) -> Result<(String, Op)> {
    let target = |t: &[String]| -> (String, String) {
        let db = if t.len() >= 2 { t[0].clone() } else { cur_db.to_string() };
        (db, sqlmini::last(t).to_string())
    };
    Ok(match sqlmini::parse(sql)? {
        Statement::Select(s) => {
            let (db, coll) = target(&s.table);
            let filter = s.filter.as_ref().map(filter_doc).unwrap_or_default();
            if s.count {
                let pipeline = count_pipeline(filter, s.offset.map(|v| v as i64), s.limit.map(|v| v as i64));
                (db, Op::Count { cmd: doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} }, label: "COUNT(*)".into(), aggregate: true })
            } else {
                let mut cmd = doc! { "find": coll, "filter": filter };
                if !s.columns.is_empty() {
                    let mut p: Document = s.columns.iter().map(|c| (c.clone(), Bson::Int32(1))).collect();
                    if !s.columns.iter().any(|c| c == "_id") {
                        p.insert("_id", 0);
                    }
                    cmd.insert("projection", p);
                }
                if !s.order.is_empty() {
                    let sort: Document = s.order.iter().map(|(c, desc)| (c.clone(), Bson::Int32(if *desc { -1 } else { 1 }))).collect();
                    cmd.insert("sort", sort);
                }
                if let Some(o) = s.offset.filter(|o| *o > 0) {
                    cmd.insert("skip", o as i64);
                }
                if let Some(l) = s.limit.filter(|l| *l > 0) {
                    cmd.insert("limit", l as i64);
                }
                let cols = if s.columns.is_empty() { None } else { Some(s.columns.clone()) };
                (db, Op::Find { cmd, cols })
            }
        }
        Statement::Insert { table, columns, rows } => {
            let (db, coll) = target(&table);
            let docs: Vec<Bson> = rows
                .iter()
                .map(|r| {
                    let d: Document = columns.iter().zip(r).filter(|(_, v)| !v.is_null()).map(|(c, v)| (c.clone(), to_bson(c, v))).collect();
                    with_id(d)
                })
                .collect();
            (db, Op::Write { cmd: doc! { "insert": coll, "documents": docs } })
        }
        Statement::Update { table, set, filter } => {
            let (db, coll) = target(&table);
            let q = filter.as_ref().map(filter_doc).unwrap_or_default();
            let s: Document = set.iter().map(|(c, v)| (c.clone(), to_bson(c, v))).collect();
            (db, Op::Write { cmd: doc! { "update": coll, "updates": [{ "q": q, "u": { "$set": s }, "multi": true }] } })
        }
        Statement::Delete { table, filter } => {
            let (db, coll) = target(&table);
            let q = filter.as_ref().map(filter_doc).unwrap_or_default();
            (db, Op::Write { cmd: doc! { "delete": coll, "deletes": [{ "q": q, "limit": 0 }] } })
        }
    })
}

fn verbosity(args: &[Bson]) -> String {
    match args.first() {
        Some(Bson::String(s)) => s.clone(),
        Some(Bson::Boolean(true)) => "allPlansExecution".into(),
        _ => "queryPlanner".into(),
    }
}

fn no_more(rest: &[Call], after: &str) -> Result<()> {
    match rest.iter().find(|c| !matches!(c.name.as_str(), "toArray" | "pretty")) {
        Some(c) => bail!("unsupported .{}() after {}()", c.name, after),
        None => Ok(()),
    }
}

fn coll_op(coll: &str, first: Call, rest: Vec<Call>) -> Result<Op> {
    let a = &first.args;
    let name = first.name.as_str();
    if name == "explain" {
        let mut it = rest.into_iter();
        let next = it.next().ok_or_else(|| anyhow!("explain() needs a method, e.g. db.c.explain().find()"))?;
        let inner = coll_op(coll, next, it.collect())?;
        return Ok(Op::Explain { inner: Box::new(inner), verbosity: verbosity(a) });
    }
    let mut explain: Option<String> = None;
    let op = match name {
        "find" | "findOne" => {
            let mut cmd = doc! { "find": coll, "filter": doc_arg(a, 0)? };
            let proj = doc_arg(a, 1)?;
            if !proj.is_empty() {
                cmd.insert("projection", proj);
            }
            if name == "findOne" {
                cmd.insert("limit", 1i64);
                cmd.insert("singleBatch", true);
            }
            let mut count = false;
            for m in &rest {
                let ma = &m.args;
                match m.name.as_str() {
                    "sort" => {
                        cmd.insert("sort", req_doc(ma, 0, "sort()")?);
                    }
                    "skip" => {
                        cmd.insert("skip", int_arg(ma, 0)?);
                    }
                    "limit" => {
                        let n = int_arg(ma, 0)?.abs();
                        if n > 0 {
                            cmd.insert("limit", n);
                        }
                    }
                    "projection" | "project" => {
                        cmd.insert("projection", req_doc(ma, 0, "projection()")?);
                    }
                    "hint" | "collation" | "comment" | "maxTimeMS" | "min" | "max" | "batchSize" => {
                        let v = ma.first().cloned().ok_or_else(|| anyhow!("{}() needs an argument", m.name))?;
                        cmd.insert(m.name.clone(), v);
                    }
                    "allowDiskUse" => {
                        cmd.insert("allowDiskUse", true);
                    }
                    "toArray" | "pretty" | "noCursorTimeout" => {}
                    "count" | "itcount" | "size" | "length" => count = true,
                    "explain" => explain = Some(verbosity(ma)),
                    other => bail!("unsupported cursor method {}()", other),
                }
            }
            if count {
                let filter = cmd.get_document("filter").cloned().unwrap_or_default();
                let pipeline = count_pipeline(filter, cmd.get("skip").and_then(|v| num(Some(v))).map(|v| v as i64), cmd.get("limit").and_then(|v| num(Some(v))).map(|v| v as i64));
                Op::Count { cmd: doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} }, label: "count".into(), aggregate: true }
            } else {
                Op::Find { cmd, cols: None }
            }
        }
        "countDocuments" | "count" => {
            let opts = doc_arg(a, 1)?;
            let pipeline = count_pipeline(doc_arg(a, 0)?, num(opts.get("skip")).map(|v| v as i64), num(opts.get("limit")).map(|v| v as i64));
            let mut cmd = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} };
            copy_opts(&mut cmd, &opts, &["hint", "collation", "maxTimeMS", "comment"]);
            no_more(&rest, name)?;
            Op::Count { cmd, label: "count".into(), aggregate: true }
        }
        "estimatedDocumentCount" => {
            no_more(&rest, name)?;
            Op::Count { cmd: doc! { "count": coll }, label: "count".into(), aggregate: false }
        }
        "distinct" => {
            let key = shell::str_arg(a, 0)?;
            let mut cmd = doc! { "distinct": coll, "key": key.clone(), "query": doc_arg(a, 1)? };
            copy_opts(&mut cmd, &doc_arg(a, 2)?, &["collation", "maxTimeMS", "comment", "hint"]);
            for m in &rest {
                match m.name.as_str() {
                    "explain" => explain = Some(verbosity(&m.args)),
                    "toArray" | "pretty" => {}
                    other => bail!("unsupported .{}() after distinct()", other),
                }
            }
            Op::Distinct { key, cmd }
        }
        "aggregate" => {
            let (pipeline, opts) = match a.first() {
                Some(Bson::Array(_)) | None => (if a.is_empty() { vec![] } else { docs_arg(a.first(), "aggregate()")? }, doc_arg(a, 1)?),
                _ => (docs_arg(Some(&Bson::Array(a.clone())), "aggregate()")?, Document::new()),
            };
            let mut cursor = Document::new();
            if let Some(b) = opts.get("batchSize") {
                cursor.insert("batchSize", b.clone());
            }
            let mut cmd = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": cursor };
            copy_opts(&mut cmd, &opts, &["allowDiskUse", "collation", "hint", "maxTimeMS", "comment", "let", "bypassDocumentValidation"]);
            for m in &rest {
                match m.name.as_str() {
                    "explain" => explain = Some(verbosity(&m.args)),
                    "toArray" | "pretty" => {}
                    other => bail!("unsupported .{}() after aggregate()", other),
                }
            }
            if opts.get_bool("explain").unwrap_or(false) {
                explain = Some("queryPlanner".into());
            }
            Op::Agg { cmd }
        }
        "insertOne" | "insertMany" | "insert" => {
            let docs = match (name, a.first()) {
                ("insertOne", Some(Bson::Document(d))) | ("insert", Some(Bson::Document(d))) => vec![with_id(d.clone())],
                ("insertMany", b) | ("insert", b) => docs_arg(b, name)?.into_iter().filter_map(|d| d.as_document().cloned()).map(with_id).collect(),
                _ => bail!("{}() expects a document", name),
            };
            let mut cmd = doc! { "insert": coll, "documents": docs };
            copy_opts(&mut cmd, &doc_arg(a, 1)?, &["ordered", "bypassDocumentValidation", "comment"]);
            no_more(&rest, name)?;
            Op::Write { cmd }
        }
        "updateOne" | "updateMany" | "replaceOne" | "update" => {
            let q = doc_arg(a, 0)?;
            let u = match a.get(1) {
                Some(b @ Bson::Document(_)) => b.clone(),
                Some(b @ Bson::Array(_)) if name != "replaceOne" => b.clone(),
                _ => bail!("{}() expects a filter and an update document", name),
            };
            let opts = doc_arg(a, 2)?;
            let multi = name == "updateMany" || (name == "update" && opts.get_bool("multi").unwrap_or(false));
            let mut entry = doc! { "q": q, "u": u, "multi": multi };
            copy_opts(&mut entry, &opts, &["upsert", "arrayFilters", "hint", "collation"]);
            no_more(&rest, name)?;
            Op::Write { cmd: doc! { "update": coll, "updates": [entry] } }
        }
        "deleteOne" | "deleteMany" | "remove" => {
            let opts = doc_arg(a, 1)?;
            let limit = if name == "deleteOne" || (name == "remove" && opts.get_bool("justOne").unwrap_or(false)) { 1 } else { 0 };
            let mut entry = doc! { "q": doc_arg(a, 0)?, "limit": limit };
            copy_opts(&mut entry, &opts, &["hint", "collation"]);
            no_more(&rest, name)?;
            Op::Write { cmd: doc! { "delete": coll, "deletes": [entry] } }
        }
        "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" => {
            let mut cmd = doc! { "findAndModify": coll, "query": doc_arg(a, 0)? };
            let opts = if name == "findOneAndDelete" {
                cmd.insert("remove", true);
                doc_arg(a, 1)?
            } else {
                let u = a.get(1).cloned().ok_or_else(|| anyhow!("{}() expects an update", name))?;
                cmd.insert("update", u);
                doc_arg(a, 2)?
            };
            if opts.get_str("returnDocument").is_ok_and(|s| s == "after") || opts.get_bool("returnNewDocument").unwrap_or(false) {
                cmd.insert("new", true);
            }
            if let Some(p) = opts.get("projection") {
                cmd.insert("fields", p.clone());
            }
            copy_opts(&mut cmd, &opts, &["sort", "upsert", "arrayFilters", "hint", "collation", "maxTimeMS"]);
            no_more(&rest, name)?;
            Op::FindModify { cmd }
        }
        "createIndex" | "ensureIndex" | "createIndexes" => {
            let keys: Vec<Document> = if name == "createIndexes" {
                docs_arg(a.first(), name)?.into_iter().filter_map(|d| d.as_document().cloned()).collect()
            } else {
                vec![req_doc(a, 0, name)?]
            };
            let opts = doc_arg(a, 1)?;
            let indexes: Vec<Document> = keys
                .into_iter()
                .map(|k| {
                    let mut ix = doc! { "key": k.clone() };
                    if !opts.contains_key("name") || name == "createIndexes" {
                        ix.insert("name", shell::index_name(&k));
                    }
                    for (ok, ov) in &opts {
                        if !(ok == "name" && name == "createIndexes") {
                            ix.insert(ok.clone(), ov.clone());
                        }
                    }
                    ix
                })
                .collect();
            no_more(&rest, name)?;
            Op::Cmd { admin: false, cmd: doc! { "createIndexes": coll, "indexes": indexes } }
        }
        "dropIndex" | "dropIndexes" => {
            let index = match a.first() {
                Some(b) => b.clone(),
                None if name == "dropIndexes" => Bson::String("*".into()),
                None => bail!("dropIndex() expects an index name or key document"),
            };
            no_more(&rest, name)?;
            Op::Cmd { admin: false, cmd: doc! { "dropIndexes": coll, "index": index } }
        }
        "getIndexes" | "getIndices" | "getIndexSpecs" => {
            no_more(&rest, name)?;
            Op::Cursor { cmd: doc! { "listIndexes": coll } }
        }
        "drop" => {
            no_more(&rest, name)?;
            Op::Cmd { admin: false, cmd: doc! { "drop": coll } }
        }
        "stats" => Op::Cmd { admin: false, cmd: doc! { "collStats": coll } },
        "validate" => Op::Cmd { admin: false, cmd: doc! { "validate": coll } },
        "renameCollection" => {
            // the database is filled in by compile()
            let to = shell::str_arg(a, 0)?;
            let drop = a.get(1).and_then(|b| b.as_bool()).unwrap_or(false);
            Op::Cmd { admin: true, cmd: doc! { "renameCollection": coll, "to": to, "dropTarget": drop } }
        }
        other => bail!("unsupported collection method {}()", other),
    };
    Ok(match explain {
        Some(v) => Op::Explain { inner: Box::new(op), verbosity: v },
        None => op,
    })
}

fn db_op(db: &str, first: Call, rest: Vec<Call>) -> Result<Op> {
    let a = &first.args;
    let name = first.name.as_str();
    no_more(&rest, name)?;
    Ok(match name {
        "getCollectionNames" => Op::Names { cmd: doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true } },
        "getCollectionInfos" => Op::Cursor { cmd: doc! { "listCollections": 1, "filter": doc_arg(a, 0)? } },
        "createCollection" => {
            let mut cmd = doc! { "create": shell::str_arg(a, 0)? };
            cmd.extend(doc_arg(a, 1)?);
            Op::Cmd { admin: false, cmd }
        }
        "createView" => {
            let pipeline = match a.get(2) {
                None => vec![],
                b => docs_arg(b, name)?,
            };
            let mut cmd = doc! { "create": shell::str_arg(a, 0)?, "viewOn": shell::str_arg(a, 1)?, "pipeline": pipeline };
            cmd.extend(doc_arg(a, 3)?);
            Op::Cmd { admin: false, cmd }
        }
        "runCommand" | "adminCommand" => {
            let cmd = match a.first() {
                Some(Bson::Document(d)) => d.clone(),
                Some(Bson::String(s)) => doc! { s.clone(): 1 },
                _ => bail!("{}() expects a command document", name),
            };
            Op::Cmd { admin: name == "adminCommand", cmd }
        }
        "dropDatabase" => Op::Cmd { admin: false, cmd: doc! { "dropDatabase": 1 } },
        "stats" => Op::Cmd { admin: false, cmd: doc! { "dbStats": 1 } },
        "getName" => Op::Scalar { label: "name".into(), value: Value::Text(db.to_string()) },
        "version" => Op::CmdField { admin: true, cmd: doc! { "buildInfo": 1 }, field: "version".into() },
        "serverStatus" => Op::Cmd { admin: true, cmd: doc! { "serverStatus": 1 } },
        "hostInfo" => Op::Cmd { admin: true, cmd: doc! { "hostInfo": 1 } },
        "serverBuildInfo" => Op::Cmd { admin: true, cmd: doc! { "buildInfo": 1 } },
        "currentOp" => {
            let mut cmd = doc! { "currentOp": 1 };
            cmd.extend(doc_arg(a, 0)?);
            Op::Cmd { admin: true, cmd }
        }
        "killOp" => Op::Cmd { admin: true, cmd: doc! { "killOp": 1, "op": a.first().cloned().ok_or_else(|| anyhow!("killOp() needs an op id"))? } },
        "getUsers" => Op::Cmd { admin: false, cmd: doc! { "usersInfo": 1 } },
        "getRoles" => Op::Cmd { admin: false, cmd: doc! { "rolesInfo": 1 } },
        "listCommands" => Op::Cmd { admin: false, cmd: doc! { "listCommands": 1 } },
        other => bail!("unsupported database method db.{}()", other),
    })
}

fn compile(cur_db: &str, stmt: Stmt) -> Result<(String, Op)> {
    Ok(match stmt {
        Stmt::Use(db) => (db.clone(), Op::Use(db)),
        Stmt::Sql(sql) => compile_sql(cur_db, &sql)?,
        Stmt::Show(what) => match what.as_str() {
            "dbs" | "databases" => ("admin".into(), Op::Cmd { admin: true, cmd: doc! { "listDatabases": 1 } }),
            "collections" | "tables" => (cur_db.into(), Op::Names { cmd: doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true } }),
            "users" => (cur_db.into(), Op::Cmd { admin: false, cmd: doc! { "usersInfo": 1 } }),
            "roles" => (cur_db.into(), Op::Cmd { admin: false, cmd: doc! { "rolesInfo": 1 } }),
            other => bail!("unsupported: show {}", other),
        },
        Stmt::Chain { db, coll, calls } => {
            let db = db.unwrap_or_else(|| cur_db.to_string());
            let mut it = calls.into_iter();
            let first = it.next().ok_or_else(|| anyhow!("missing method call"))?;
            let rest: Vec<Call> = it.collect();
            let mut op = match &coll {
                Some(c) => coll_op(c, first, rest)?,
                None => db_op(&db, first, rest)?,
            };
            if let Op::Cmd { cmd, .. } = &mut op {
                if let (Some(c), Ok(to)) = (&coll, cmd.get_str("to").map(|s| s.to_string())) {
                    if cmd.contains_key("renameCollection") {
                        cmd.insert("renameCollection", format!("{}.{}", db, c));
                        cmd.insert("to", if to.contains('.') { to } else { format!("{}.{}", db, to) });
                    }
                }
            }
            (db, op)
        }
    })
}

// ---------------------------------------------------------------- driver

impl MongoDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let uri = build_uri(cfg);
        let client = Client::with_uri_str(&uri).map_err(merr)?;
        let current_db = if !cfg.database.is_empty() {
            cfg.database.clone()
        } else {
            client.default_database().map(|d| d.name().to_string()).unwrap_or_else(|| "test".into())
        };
        let d = Self { client, current_db, kind: cfg.kind };
        d.cmd("admin", doc! { "ping": 1 }).map_err(|e| anyhow!("connecting to MongoDB: {:#}", e))?;
        Ok(d)
    }

    fn cmd(&self, db: &str, c: Document) -> Result<Document> {
        self.client.database(db).run_command(c).run().map_err(merr)
    }

    fn cursor(&self, db: &str, c: Document, cap: usize) -> Result<Vec<Document>> {
        let cur = self.client.database(db).run_cursor_command(c).run().map_err(merr)?;
        let mut out = vec![];
        for d in cur {
            out.push(d.map_err(merr)?);
            if out.len() >= cap {
                break;
            }
        }
        Ok(out)
    }

    fn run(&mut self, db: &str, op: Op) -> Result<QueryResult> {
        Ok(match op {
            Op::Find { cmd, cols } => docs_result(&self.cursor(db, cmd, MAX_ROWS)?, cols.as_deref()),
            Op::Agg { cmd } | Op::Cursor { cmd } => docs_result(&self.cursor(db, cmd, MAX_ROWS)?, None),
            Op::Count { cmd, label, aggregate } => {
                let n = if aggregate {
                    self.cursor(db, cmd, 1)?.first().and_then(|d| num(d.get("n"))).unwrap_or(0.0)
                } else {
                    num(self.cmd(db, cmd)?.get("n")).unwrap_or(0.0)
                };
                single(&label, "long", Value::Int(n as i64))
            }
            Op::Distinct { key, cmd } => {
                let reply = self.cmd(db, cmd)?;
                let vals = reply.get_array("values").cloned().unwrap_or_default();
                QueryResult {
                    columns: vec![ColumnMeta { name: key, type_name: merged_type(vals.iter()) }],
                    rows: vals.iter().map(|v| vec![to_value(v)]).collect(),
                    is_query: true,
                    ..Default::default()
                }
            }
            Op::Write { cmd } => {
                let n = write_count(&self.cmd(db, cmd)?)?;
                QueryResult { affected_rows: n, ..Default::default() }
            }
            Op::FindModify { cmd } => {
                let reply = self.cmd(db, cmd)?;
                match reply.get("value") {
                    Some(Bson::Document(d)) => docs_result(std::slice::from_ref(d), None),
                    _ => single("value", "null", Value::Null),
                }
            }
            Op::Cmd { admin, cmd } => {
                let reply = self.cmd(if admin { "admin" } else { db }, cmd)?;
                if let Ok(batch) = reply.get_document("cursor").and_then(|c| c.get_array("firstBatch")) {
                    let docs: Vec<Document> = batch.iter().filter_map(|b| b.as_document().cloned()).collect();
                    return Ok(docs_result(&docs, None));
                }
                for k in ["inprog", "users", "roles", "databases"] {
                    if let Ok(list) = reply.get_array(k) {
                        let docs: Vec<Document> = list.iter().filter_map(|b| b.as_document().cloned()).collect();
                        return Ok(docs_result(&docs, None));
                    }
                }
                fields_result(&reply)
            }
            Op::CmdField { admin, cmd, field } => {
                let reply = self.cmd(if admin { "admin" } else { db }, cmd)?;
                single(&field, "", reply.get(&field).map(to_value).unwrap_or(Value::Null))
            }
            Op::Names { cmd } => {
                let mut names: Vec<String> = self.cursor(db, cmd, usize::MAX)?.iter().filter_map(|d| d.get_str("name").ok().map(String::from)).collect();
                names.sort();
                QueryResult {
                    columns: vec![ColumnMeta { name: "name".into(), type_name: "string".into() }],
                    rows: names.into_iter().map(|n| vec![Value::Text(n)]).collect(),
                    is_query: true,
                    ..Default::default()
                }
            }
            Op::Scalar { label, value } => single(&label, "string", value),
            Op::Use(name) => {
                self.current_db = name.clone();
                single("result", "string", Value::Text(format!("switched to db {}", name)))
            }
            Op::Explain { inner, verbosity } => {
                let cmd = match *inner {
                    Op::Find { cmd, .. } | Op::Agg { cmd } | Op::Count { cmd, .. } | Op::Distinct { cmd, .. } | Op::Write { cmd } | Op::FindModify { cmd } => cmd,
                    _ => bail!("this statement cannot be explained"),
                };
                let reply = self.cmd(db, doc! { "explain": cmd, "verbosity": verbosity })?;
                let mut r = QueryResult { columns: vec![ColumnMeta::new("QUERY PLAN")], is_query: true, ..Default::default() };
                let mut clean = reply.clone();
                for k in ["$clusterTime", "operationTime", "ok"] {
                    clean.remove(k);
                }
                for l in json_pretty(&Bson::Document(clean)).lines() {
                    r.rows.push(vec![Value::Text(l.to_string())]);
                }
                r
            }
        })
    }

    fn collection_info(&self, db: &str, name: &str) -> Result<Document> {
        self.cursor(db, doc! { "listCollections": 1, "filter": { "name": name } }, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("collection {}.{} not found", db, name))
    }
}

impl Driver for MongoDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.cmd("admin", doc! { "buildInfo": 1 })?;
        Ok(format!("MongoDB {}", r.get_str("version").unwrap_or("unknown")))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let mut names: Vec<String> = match self.cmd("admin", doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": true }) {
            Ok(r) => r
                .get_array("databases")
                .map(|a| a.iter().filter_map(|d| d.as_document()?.get_str("name").ok().map(String::from)).collect())
                .unwrap_or_default(),
            Err(e) if self.current_db.is_empty() => return Err(e),
            Err(_) => vec![self.current_db.clone()],
        };
        let rank = |n: &str| match n {
            "admin" => 1,
            "config" => 2,
            "local" => 3,
            _ => 0,
        };
        names.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
        Ok(names)
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let filter = match kind {
            ObjectKind::Table => doc! { "type": { "$in": ["collection", "timeseries"] } },
            ObjectKind::View => doc! { "type": "view" },
            _ => return Ok(vec![]),
        };
        let infos = self.cursor(&scope.db, doc! { "listCollections": 1, "filter": filter, "authorizedCollections": true }, usize::MAX)?;
        let with_stats = infos.len() <= 300;
        let mut out = vec![];
        for info in infos {
            let Ok(name) = info.get_str("name") else { continue };
            // internal collections (system.views, system.profile, ...) stay reachable from the shell
            if name.starts_with("system.") && scope.db != "admin" && scope.db != "config" && scope.db != "local" {
                continue;
            }
            let mut o = ObjectInfo::named(name);
            let opts = info.get_document("options").cloned().unwrap_or_default();
            if kind == ObjectKind::View {
                o.extra.push(("View on".into(), opts.get_str("viewOn").unwrap_or("").into()));
                if let Some(p) = opts.get("pipeline") {
                    o.extra.push(("Pipeline".into(), json_text(p)));
                }
            } else {
                o.engine = info.get_str("type").unwrap_or("collection").into();
                if opts.get_bool("capped").unwrap_or(false) {
                    o.extra.push(("Capped".into(), "true".into()));
                }
                if let Some(v) = opts.get("validator") {
                    o.extra.push(("Validator".into(), json_text(v)));
                }
                if with_stats {
                    match self.cmd(&scope.db, doc! { "collStats": name }) {
                        Ok(s) => {
                            o.rows = num(s.get("count")).map(|v| v as i64);
                            o.data_length = num(s.get("size")).map(|v| v as i64);
                            for (k, label) in [("storageSize", "Storage size"), ("totalIndexSize", "Index size"), ("nindexes", "Indexes"), ("avgObjSize", "Avg document size")] {
                                if let Some(v) = num(s.get(k)) {
                                    o.extra.push((label.into(), (v as i64).to_string()));
                                }
                            }
                        }
                        Err(_) => {
                            o.rows = self.cmd(&scope.db, doc! { "count": name }).ok().and_then(|r| num(r.get("n"))).map(|v| v as i64);
                        }
                    }
                }
            }
            out.push(o);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let docs = self.cursor(&path.db, doc! { "find": &path.name, "filter": {}, "limit": SAMPLE }, SAMPLE as usize)?;
        let mut names = union_keys(&docs);
        if docs.is_empty() {
            names.push("_id".into());
        }
        let columns = names
            .iter()
            .map(|n| {
                let ty = if docs.is_empty() { "objectId".to_string() } else { merged_type(docs.iter().filter_map(|d| d.get(n))) };
                let pk = n == "_id";
                ColumnDef { nullable: !pk, primary_key: pk, ..ColumnDef::new(n.clone(), ty) }
            })
            .collect();
        let indexes = self
            .cursor(&path.db, doc! { "listIndexes": &path.name }, usize::MAX)
            .unwrap_or_default()
            .iter()
            .map(|ix| {
                let key = ix.get_document("key").cloned().unwrap_or_default();
                let name = ix.get_str("name").unwrap_or("").to_string();
                let method = key.values().find_map(|v| v.as_str().map(String::from)).unwrap_or_default();
                IndexDef {
                    primary: name == "_id_",
                    unique: ix.get_bool("unique").unwrap_or(false) || name == "_id_",
                    columns: key.keys().cloned().collect(),
                    method,
                    comment: json_text(&Bson::Document(key)),
                    name,
                }
            })
            .collect();
        Ok(TableDef { path: path.clone(), columns, indexes, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let info = self.collection_info(&path.db, &path.name)?;
        let mut opts = info.get_document("options").cloned().unwrap_or_default();
        let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
        if kind == ObjectKind::View || info.get_str("type") == Ok("view") {
            let on = opts.get_str("viewOn").unwrap_or("").to_string();
            let pipeline = opts.remove("pipeline").unwrap_or(Bson::Array(vec![]));
            opts.remove("viewOn");
            let mut s = format!("db.createView({}, {}, {}", q(&path.name), q(&on), json_pretty(&pipeline));
            if !opts.is_empty() {
                s.push_str(&format!(", {}", json_text(&Bson::Document(opts))));
            }
            s.push_str(");");
            return Ok(s);
        }
        let mut s = if opts.is_empty() {
            format!("db.createCollection({});", q(&path.name))
        } else {
            format!("db.createCollection({}, {});", q(&path.name), json_pretty(&Bson::Document(opts)))
        };
        for ix in self.cursor(&path.db, doc! { "listIndexes": &path.name }, usize::MAX)? {
            if ix.get_str("name") == Ok("_id_") {
                continue;
            }
            let key = ix.get_document("key").cloned().unwrap_or_default();
            let mut o = ix.clone();
            for k in ["v", "key", "ns"] {
                o.remove(k);
            }
            s.push_str(&format!("\ndb.getCollection({}).createIndex({}, {});", q(&path.name), json_text(&Bson::Document(key)), json_text(&Bson::Document(o))));
        }
        Ok(s)
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        let mut cur = scope.map(|s| s.db.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| self.current_db.clone());
        let mut out = vec![];
        for (src, stmt) in shell::parse_script(sql)? {
            let start = Instant::now();
            let (db, op) = compile(&cur, stmt)?;
            if let Op::Use(d) = &op {
                cur = d.clone();
            }
            let mut r = self.run(&db, op)?;
            r.sql = src;
            r.elapsed_ms = start.elapsed().as_millis();
            out.push(r);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let cur = scope.map(|s| s.db.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| self.current_db.clone());
        let (src, stmt) = shell::parse_script(sql)?.into_iter().next().ok_or_else(|| anyhow!("nothing to explain"))?;
        let start = Instant::now();
        let (db, op) = compile(&cur, stmt)?;
        let op = match op {
            e @ Op::Explain { .. } => e,
            other => Op::Explain { inner: Box::new(other), verbosity: "queryPlanner".into() },
        };
        let mut r = self.run(&db, op)?;
        r.sql = src;
        r.elapsed_ms = start.elapsed().as_millis();
        Ok(r)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.cmd("admin", doc! { "usersInfo": { "forAllDBs": true } })?;
        let users = r.get_array("users").cloned().unwrap_or_default();
        Ok(users
            .iter()
            .filter_map(|u| u.as_document())
            .map(|u| {
                let roles = u
                    .get_array("roles")
                    .map(|a| {
                        a.iter()
                            .filter_map(|r| r.as_document())
                            .map(|r| format!("{}@{}", r.get_str("role").unwrap_or(""), r.get_str("db").unwrap_or("")))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let mech = u.get_array("mechanisms").map(|a| a.iter().filter_map(|m| m.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                UserInfo {
                    name: u.get_str("user").unwrap_or("").into(),
                    host: u.get_str("db").unwrap_or("").into(),
                    attributes: vec![("Roles".into(), roles), ("Mechanisms".into(), mech)],
                }
            })
            .collect())
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let ops = self.cmd("admin", doc! { "currentOp": 1 })?;
        let cols = [
            ("Id", "opid"),
            ("Type", "type"),
            ("Client", "client"),
            ("AppName", "appName"),
            ("Active", "active"),
            ("Op", "op"),
            ("Namespace", "ns"),
            ("Secs", "secs_running"),
            ("Description", "desc"),
            ("Command", "command"),
        ];
        let mut processes = QueryResult { columns: cols.iter().map(|(n, _)| ColumnMeta::new(*n)).collect(), is_query: true, ..Default::default() };
        for op in ops.get_array("inprog").cloned().unwrap_or_default().iter().filter_map(|b| b.as_document()) {
            processes.rows.push(cols.iter().map(|(_, k)| op.get(*k).map(to_value).unwrap_or(Value::Null)).collect());
        }

        let kv = || QueryResult { columns: vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")], is_query: true, ..Default::default() };
        let mut variables = kv();
        if let Ok(p) = self.cmd("admin", doc! { "getParameter": "*" }) {
            for (k, v) in p.iter().filter(|(k, _)| !matches!(k.as_str(), "ok" | "$clusterTime" | "operationTime")) {
                variables.rows.push(vec![Value::Text(k.clone()), to_value(v)]);
            }
        }
        if let Ok(c) = self.cmd("admin", doc! { "getCmdLineOpts": 1 }) {
            if let Ok(parsed) = c.get_document("parsed") {
                let mut flat = vec![];
                flatten("cmdline", parsed, &mut flat, true);
                for (k, v) in flat {
                    variables.rows.push(vec![Value::Text(k), v]);
                }
            }
        }
        variables.rows.sort_by(|a, b| a[0].display().cmp(&b[0].display()));

        let st = self.cmd("admin", doc! { "serverStatus": 1 })?;
        let mut flat = vec![];
        flatten("", &st, &mut flat, false);
        let mut status = kv();
        status.rows = flat.into_iter().map(|(k, v)| vec![Value::Text(k), v]).collect();
        Ok(ServerSnapshot { processes, variables, status })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let op = match id.trim().parse::<i64>() {
            Ok(n) => int_bson(n),
            Err(_) => Bson::String(id.trim().to_string()),
        };
        self.cmd("admin", doc! { "killOp": 1, "op": op })?;
        Ok(())
    }
}

/// Flatten nested documents into dotted keys. Status keeps numeric leaves only.
fn flatten(prefix: &str, d: &Document, out: &mut Vec<(String, Value)>, all_scalars: bool) {
    for (k, v) in d {
        if matches!(k.as_str(), "$clusterTime" | "operationTime" | "ok") && prefix.is_empty() {
            continue;
        }
        let key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
        match v {
            Bson::Document(sub) => flatten(&key, sub, out, all_scalars),
            Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => out.push((key, to_value(v))),
            Bson::Array(_) if all_scalars => out.push((key, to_value(v))),
            Bson::Array(_) => {}
            other if all_scalars || matches!(other, Bson::String(_) | Bson::Boolean(_) | Bson::DateTime(_)) => out.push((key, to_value(other))),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::dml;

    fn sql_op(sql: &str) -> (String, Op) {
        compile_sql("cur", sql).unwrap()
    }

    #[test]
    fn grid_select_translation() {
        let p = ObjPath::new("shop", None, "users");
        let mut r = dml::FilterRule::new("name");
        r.op = dml::FilterOp::Contains;
        r.value = "ab".into();
        let mut r2 = dml::FilterRule::new("age");
        r2.op = dml::FilterOp::Between;
        r2.value = "1, 5".into();
        let w = dml::filters_to_where(Dialect::Generic, &[r, r2]);
        let sql = dml::select_page(Dialect::Generic, &p, &w, &[dml::SortSpec { column: "age".into(), descending: true }], 100, 200);
        let (db, Op::Find { cmd, cols }) = sql_op(&sql) else { panic!() };
        assert_eq!(db, "shop");
        assert!(cols.is_none());
        assert_eq!(
            cmd,
            doc! {
                "find": "users",
                "filter": { "$and": [ { "name": shell::regex("^.*ab.*$".into(), "i") }, { "age": { "$gte": 1, "$lte": 5 } } ] },
                "sort": { "age": -1 },
                "skip": 200i64,
                "limit": 100i64,
            }
        );
        let (db, Op::Count { cmd, .. }) = sql_op("SELECT COUNT(*) FROM \"c\" WHERE x IS NOT NULL") else { panic!() };
        assert_eq!(db, "cur");
        assert_eq!(cmd.get_array("pipeline").unwrap()[0], Bson::Document(doc! { "$match": { "x": { "$ne": Bson::Null } } }));
        let (_, Op::Find { cmd, cols }) = sql_op("SELECT a, b.c FROM t") else { panic!() };
        assert_eq!(cols.unwrap(), vec!["a", "b.c"]);
        assert_eq!(cmd.get_document("projection").unwrap(), &doc! { "a": 1, "b.c": 1, "_id": 0 });
    }

    #[test]
    fn id_and_value_conversion() {
        let hex = "64b7f0c2a1b2c3d4e5f60718";
        let oid = ObjectId::parse_str(hex).unwrap();
        let p = ObjPath::new("d", None, "t");
        let u = dml::update_row(Dialect::Generic, &p, &[("tags".into(), Value::Text("[1, \"x\"]".into())), ("n".into(), Value::Null)], &[("_id".into(), Value::Text(hex.into()))]);
        let (_, Op::Write { cmd }) = sql_op(&u) else { panic!() };
        assert_eq!(
            cmd,
            doc! { "update": "t", "updates": [{ "q": { "_id": { "$in": [oid, hex] } }, "u": { "$set": { "tags": [1, "x"], "n": Bson::Null } }, "multi": true }] }
        );
        let i = dml::insert_rows(Dialect::Generic, &p, &["_id".into(), "a".into(), "o".into(), "d".into()], &[vec![Value::Text(hex.into()), Value::Int(1), Value::Text("{\"k\": 2.5}".into()), Value::Text("2024-01-02T03:04:05Z".into())]]);
        let (_, Op::Write { cmd }) = sql_op(&i) else { panic!() };
        let d = cmd.get_array("documents").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(d.get("_id"), Some(&Bson::ObjectId(oid)));
        assert_eq!(d.get("a"), Some(&Bson::Int32(1)));
        assert_eq!(d.get("o"), Some(&Bson::Document(doc! { "k": 2.5 })));
        assert!(matches!(d.get("d"), Some(Bson::DateTime(_))));
        let (_, Op::Write { cmd }) = sql_op(&dml::delete_row(Dialect::Generic, &p, &[("_id".into(), Value::Int(7))])) else { panic!() };
        assert_eq!(cmd, doc! { "delete": "t", "deletes": [{ "q": { "_id": { "$in": [7, "7"] } }, "limit": 0 }] });
    }

    #[test]
    fn filter_ops() {
        let f = |s: &str| {
            let Ok(Statement::Select(s)) = sqlmini::parse(&format!("SELECT * FROM t WHERE {}", s)) else { panic!() };
            filter_doc(&s.filter.unwrap())
        };
        assert_eq!(f("a IN (1, 'x') OR NOT (b <> 'y')"), doc! { "$or": [ { "a": { "$in": [1, "1", "x"] } }, { "$nor": [ { "b": { "$ne": "y" } } ] } ] });
        assert_eq!(f("a NOT LIKE 'x%'"), doc! { "a": { "$not": shell::regex("^x.*$".into(), "i") } });
        assert_eq!(f("a IS NULL AND b >= 2.5"), doc! { "$and": [ { "a": Bson::Null }, { "b": { "$gte": 2.5 } } ] });
    }

    #[test]
    fn rows_flatten() {
        let oid = ObjectId::new();
        let docs = vec![
            doc! { "name": "a", "_id": oid, "n": 1, "sub": { "x": 1 } },
            doc! { "_id": 2, "n": 2.5, "when": bson::DateTime::from_millis(0), "dec": Bson::Decimal128("1.5".parse().unwrap()), "bin": Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Generic, bytes: vec![0, 1] }) },
        ];
        let r = docs_result(&docs, None);
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "name", "n", "sub", "when", "dec", "bin"]);
        assert_eq!(r.columns[0].type_name, "mixed");
        assert_eq!(r.columns[1].type_name, "string");
        assert_eq!(r.rows[0][0], Value::Text(oid.to_hex()));
        assert_eq!(r.rows[0][3], Value::Text("{\"x\":1}".into()));
        assert_eq!(r.rows[1][1], Value::Null);
        assert_eq!(r.rows[1][4], Value::Text("1970-01-01T00:00:00Z".into()));
        assert_eq!(r.rows[1][5], Value::Text("1.5".into()));
        assert_eq!(r.rows[1][6], Value::Bytes(vec![0, 1]));
        assert_eq!(json_text(&Bson::Document(doc! { "d": Bson::Decimal128("2".parse().unwrap()) })), "{\"d\":{\"$numberDecimal\":\"2\"}}");
    }

    #[test]
    fn shell_compile() {
        let st = |s: &str| compile("shop", shell::parse_script(s).unwrap().remove(0).1).unwrap();
        let (db, Op::Find { cmd, .. }) = st("db.users.find({a: 1}).sort({b: -1}).skip(2).limit(3)") else { panic!() };
        assert_eq!(db, "shop");
        assert_eq!(cmd, doc! { "find": "users", "filter": { "a": 1 }, "sort": { "b": -1 }, "skip": 2i64, "limit": 3i64 });
        assert!(matches!(st("db.users.find().count()").1, Op::Count { aggregate: true, .. }));
        assert!(matches!(st("db.users.find().explain('executionStats')").1, Op::Explain { verbosity, .. } if verbosity == "executionStats"));
        assert!(matches!(st("db.users.explain().aggregate([{$match: {}}])").1, Op::Explain { .. }));
        let (_, Op::Write { cmd }) = st("db.users.updateMany({a: 1}, {$inc: {n: 1}}, {upsert: true})") else { panic!() };
        assert_eq!(cmd, doc! { "update": "users", "updates": [{ "q": { "a": 1 }, "u": { "$inc": { "n": 1 } }, "multi": true, "upsert": true }] });
        let (_, Op::Cmd { cmd, .. }) = st("db.users.createIndex({a: 1, b: -1}, {unique: true})") else { panic!() };
        assert_eq!(cmd, doc! { "createIndexes": "users", "indexes": [{ "key": { "a": 1, "b": -1 }, "name": "a_1_b_-1", "unique": true }] });
        let (_, Op::Cmd { admin, cmd }) = st("db.users.renameCollection('people')") else { panic!() };
        assert!(admin);
        assert_eq!(cmd.get_str("renameCollection").unwrap(), "shop.users");
        assert_eq!(cmd.get_str("to").unwrap(), "shop.people");
        let (db, _) = st("db.getSiblingDB('other').c.drop()");
        assert_eq!(db, "other");
        assert!(compile("x", shell::parse_script("db.c.frobnicate()").unwrap().remove(0).1).is_err());
        assert!(compile("x", shell::parse_script("db.c.insertOne({a: 1}).sort({})").unwrap().remove(0).1).is_err());
    }

    #[test]
    fn uri() {
        let mut cfg = ConnectionConfig::new(DbKind::MongoDb);
        cfg.host = "h".into();
        cfg.port = 1;
        cfg.user = "u@x".into();
        cfg.password = "p:w".into();
        let u = build_uri(&cfg);
        assert!(u.starts_with("mongodb://u%40x:p%3Aw@h:1/?authSource=admin&directConnection=true&connectTimeoutMS="), "{}", u);
        cfg.options.insert("uri".into(), "mongodb+srv://c.example.net/db?retryWrites=true".into());
        assert!(build_uri(&cfg).starts_with("mongodb+srv://c.example.net/db?retryWrites=true&connectTimeoutMS="));
        cfg.options.insert("uri".into(), "mongodb://a:1,b:2".into());
        assert!(build_uri(&cfg).starts_with("mongodb://a:1,b:2/?connectTimeoutMS="));
    }
}
