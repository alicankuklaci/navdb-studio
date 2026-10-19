//! Vertica driver speaking the native protocol (see [`wire`]).
//!
//! Options (`cfg.options`): `autocommit` ("off" keeps transactions open until
//! an explicit COMMIT), `client_label` (session label, default "NavDB Studio"),
//! `workload` (workload routing name).

mod wire;

use anyhow::{bail, Result};
use std::time::Duration;

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Generic;

fn lit(s: &str) -> String {
    D.string_literal(s)
}

pub struct VerticaDriver {
    cfg: ConnectionConfig,
    conn: wire::Conn,
    db: String,
    search_path: Option<String>,
}

fn options(cfg: &ConnectionConfig) -> wire::Options {
    let label = cfg.option("client_label");
    wire::Options {
        host: cfg.host.clone(),
        port: cfg.port,
        user: cfg.user.clone(),
        password: cfg.password.clone(),
        database: cfg.database.clone(),
        label: if label.is_empty() { "NavDB Studio".into() } else { label.into() },
        autocommit: !cfg.option("autocommit").eq_ignore_ascii_case("off"),
        workload: cfg.option("workload").into(),
        timeout: Duration::from_secs(cfg.connect_timeout_secs.max(1)),
        tls: cfg.ssl.enabled.then(|| wire::Tls { verify: cfg.ssl.verify_server, ca_cert: cfg.ssl.ca_cert.clone() }),
    }
}

fn open(cfg: &ConnectionConfig) -> Result<wire::Conn> {
    let o = options(cfg);
    let mut c = wire::Conn::connect(&o)?;
    // older servers ignore the startup parameter
    let _ = c.simple_query(if o.autocommit { "SET SESSION AUTOCOMMIT TO ON" } else { "SET SESSION AUTOCOMMIT TO OFF" });
    Ok(c)
}

/// Type name for a Vertica type oid (+ modifier).
fn type_name(f: &wire::Field) -> String {
    if let Some(u) = &f.user_type {
        return u.clone();
    }
    let m = f.type_mod;
    let len = |base: &str| if m > 4 { format!("{}({})", base, m - 4) } else { base.to_string() };
    match f.type_oid {
        5 => "Boolean".into(),
        6 => "Integer".into(),
        7 => "Float".into(),
        8 => len("Char"),
        9 => len("Varchar"),
        10 => "Date".into(),
        11 => "Time".into(),
        12 => "Timestamp".into(),
        13 => "TimestampTz".into(),
        14 => "Interval".into(),
        114 => "Interval Year to Month".into(),
        15 => "TimeTz".into(),
        16 => {
            if m >= 4 {
                format!("Numeric({},{})", (m - 4) >> 16, (m - 4) & 0xffff)
            } else {
                "Numeric".into()
            }
        }
        17 => len("Varbinary"),
        20 => "Uuid".into(),
        115 => len("Long Varchar"),
        116 => len("Long Varbinary"),
        117 => len("Binary"),
        4 => "Unknown".into(),
        o if (1500..1600).contains(&o) || o == 301 => "Array".into(),
        o if (2700..2800).contains(&o) => "Set".into(),
        300 => "Row".into(),
        302 => "Map".into(),
        o => format!("oid {}", o),
    }
}

fn is_binary(f: &wire::Field) -> bool {
    matches!(f.type_oid, 17 | 116 | 117) || f.user_type.is_some()
}

fn to_value(f: &wire::Field, raw: Option<Vec<u8>>) -> Value {
    let Some(raw) = raw else { return Value::Null };
    if is_binary(f) {
        return Value::Bytes(wire::decode_binary(&raw));
    }
    let s = match String::from_utf8(raw) {
        Ok(s) => s,
        Err(e) => return Value::Bytes(e.into_bytes()),
    };
    match f.type_oid {
        5 => match s.as_str() {
            "t" | "1" | "true" => Value::Bool(true),
            "f" | "0" | "false" => Value::Bool(false),
            _ => Value::Text(s),
        },
        6 => s.parse().map(Value::Int).unwrap_or(Value::Text(s)),
        7 => s.parse().map(Value::Float).unwrap_or(Value::Text(s)),
        _ => Value::Text(s),
    }
}

fn affected(tag: &str) -> u64 {
    tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Vertica has no multi-row `VALUES (..), (..)`: split such an INSERT into
/// single-row INSERTs. Returns `None` when the statement is not one.
fn split_multi_insert(stmt: &str) -> Option<Vec<String>> {
    let trimmed = stmt.trim_start();
    if !trimmed.get(..6)?.eq_ignore_ascii_case("INSERT") {
        return None;
    }
    let b = stmt.as_bytes();
    let n = b.len();
    // find top-level VALUES keyword
    let mut i = 0;
    let mut depth = 0i32;
    let mut values_at = None;
    while i < n {
        match b[i] {
            b'\'' | b'"' => i = skip_quoted(b, i),
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            c if depth == 0 && (c == b'V' || c == b'v') && stmt.get(i..i + 6).map(|w| w.eq_ignore_ascii_case("VALUES")).unwrap_or(false) => {
                let before_ok = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                let after_ok = i + 6 >= n || !(b[i + 6].is_ascii_alphanumeric() || b[i + 6] == b'_');
                if before_ok && after_ok {
                    values_at = Some(i);
                    break;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    let at = values_at?;
    let head = &stmt[..at + 6];
    let mut tuples = Vec::new();
    let mut i = at + 6;
    loop {
        while i < n && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= n {
            break;
        }
        if b[i] != b'(' {
            return None;
        }
        let start = i;
        let mut depth = 0;
        while i < n {
            match b[i] {
                b'\'' | b'"' => {
                    i = skip_quoted(b, i);
                    continue;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return None;
        }
        tuples.push(&stmt[start..i]);
    }
    if tuples.len() < 2 {
        return None;
    }
    Some(tuples.into_iter().map(|t| format!("{} {}", head, t)).collect())
}

/// Split a script; Vertica SQL functions (`AS BEGIN RETURN ..; END;`) stay whole
/// and `$$` bodies of stored procedures are honoured.
fn split(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open = false;
    for s in crate::sqlutil::split_statements(sql, Dialect::Postgres) {
        if open {
            if let Some(last) = out.last_mut() {
                last.push_str("; ");
                last.push_str(&s);
            }
            let w = s.trim().to_ascii_uppercase();
            if w == "END" || w.starts_with("END ") || w.starts_with("END\n") {
                open = false;
            }
            continue;
        }
        let up = s.to_ascii_uppercase();
        let words: Vec<&str> = up.split_whitespace().collect();
        let is_fn = words.first() == Some(&"CREATE") && words.iter().take(4).any(|w| *w == "FUNCTION");
        if is_fn && words.contains(&"BEGIN") && !words.last().map(|w| *w == "END").unwrap_or(false) {
            open = true;
        }
        out.push(s);
    }
    out
}

fn skip_quoted(b: &[u8], start: usize) -> usize {
    let q = b[start];
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

impl VerticaDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut conn = open(cfg)?;
        let db = conn
            .simple_query("SELECT CURRENT_DATABASE()")?
            .into_iter()
            .next()
            .and_then(|r| r.rows.into_iter().next())
            .and_then(|r| r.into_iter().next().flatten())
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_else(|| cfg.database.clone());
        Ok(Self { cfg: cfg.clone(), conn, db, search_path: None })
    }

    fn ensure_conn(&mut self) -> Result<()> {
        if self.conn.is_broken() {
            self.conn = open(&self.cfg)?;
            if let Some(s) = self.search_path.clone() {
                self.conn.simple_query(&format!("SET SEARCH_PATH TO {}, public", D.quote_ident(&s)))?;
            }
        }
        Ok(())
    }

    fn use_scope(&mut self, scope: Option<&Scope>) -> Result<()> {
        self.ensure_conn()?;
        if let Some(schema) = scope.and_then(|s| s.schema.as_ref()).filter(|s| !s.is_empty()) {
            if self.search_path.as_ref() != Some(schema) {
                self.conn.simple_query(&format!("SET SEARCH_PATH TO {}, public", D.quote_ident(schema)))?;
                self.search_path = Some(schema.clone());
            }
        }
        Ok(())
    }

    fn run(&mut self, sql: &str) -> Result<QueryResult> {
        let start = std::time::Instant::now();
        let replies = match split_multi_insert(sql) {
            Some(parts) => self.conn.simple_query(&parts.join(";\n"))?,
            None => self.conn.simple_query(sql)?,
        };
        let mut cur = QueryResult { sql: sql.to_string(), ..Default::default() };
        let verb = crate::sqlutil::strip_leading_comments(sql).split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        let dml = matches!(verb.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY");
        for r in replies {
            // DML answers with a one-row "OUTPUT" result holding the row count
            let output = r.fields.as_ref().map(|f| f.len() == 1 && f[0].name.eq_ignore_ascii_case("OUTPUT")).unwrap_or(false);
            if dml && output && r.rows.len() <= 1 {
                let n = r.rows.first().and_then(|row| row.first().cloned().flatten());
                cur.affected_rows += n.and_then(|v| String::from_utf8_lossy(&v).trim().parse().ok()).unwrap_or_else(|| affected(&r.tag));
                continue;
            }
            match r.fields {
                Some(fields) if !cur.is_query => {
                    cur.is_query = true;
                    cur.columns = fields
                        .iter()
                        .map(|f| ColumnMeta { name: f.name.clone(), type_name: type_name(f) })
                        .collect();
                    cur.rows = r
                        .rows
                        .into_iter()
                        .map(|row| fields.iter().zip(row).map(|(f, v)| to_value(f, v)).collect())
                        .collect();
                    cur.affected_rows = cur.rows.len() as u64;
                }
                Some(_) => {}
                None => {
                    if !cur.is_query {
                        cur.affected_rows += affected(&r.tag);
                    }
                }
            }
        }
        cur.elapsed_ms = start.elapsed().as_millis();
        Ok(cur)
    }

    fn texts(&mut self, sql: &str) -> Result<Vec<String>> {
        let r = self.query(None, sql)?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn schema_of(p: &ObjPath) -> String {
        p.schema.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "public".into())
    }

    /// `schema.name` argument for EXPORT_OBJECTS.
    fn export(&mut self, p: &ObjPath) -> Result<String> {
        let obj = format!("{}.{}", D.quote_ident(&Self::schema_of(p)), D.quote_ident(base_name(&p.name)));
        let r = self.query(None, &format!("SELECT EXPORT_OBJECTS('', {}, false)", lit(&obj)))?;
        let s = r.text(0, 0).trim().to_string();
        if s.is_empty() {
            bail!("{} not found", p.display());
        }
        Ok(s)
    }
}

/// Function names are listed as `name(args)`.
fn base_name(n: &str) -> &str {
    n.split('(').next().unwrap_or(n)
}

impl Driver for VerticaDriver {
    fn kind(&self) -> DbKind {
        DbKind::Vertica
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.query(None, "SELECT version()")?;
        Ok(r.text(0, 0))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec![self.db.clone()])
    }

    fn list_schemas(&mut self, _db: &str) -> Result<Vec<String>> {
        self.texts(
            "SELECT schema_name FROM v_catalog.schemata WHERE NOT is_system_schema AND LEFT(schema_name, 2) <> 'v_' ORDER BY schema_name",
        )
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Function, ObjectKind::Procedure, ObjectKind::Sequence]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let schema = lit(scope.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("public"));
        let sql = match kind {
            ObjectKind::Table => format!(
                "SELECT t.table_name, s.row_count, s.used_bytes, COALESCE(c.comment, ''), t.owner_name, t.create_time::varchar, \
                 COALESCE(t.partition_expression, ''), CASE WHEN t.is_temp_table THEN 'temporary' WHEN t.is_flextable THEN 'flex' ELSE '' END \
                 FROM v_catalog.tables t \
                 LEFT JOIN (SELECT anchor_table_id, MAX(rc) AS row_count, SUM(ub) AS used_bytes FROM \
                   (SELECT anchor_table_id, projection_id, SUM(row_count) AS rc, SUM(used_bytes) AS ub FROM v_monitor.projection_storage GROUP BY 1, 2) p GROUP BY 1) s \
                   ON s.anchor_table_id = t.table_id \
                 LEFT JOIN v_catalog.comments c ON c.object_id = t.table_id \
                 WHERE t.table_schema = {} ORDER BY t.table_name",
                schema
            ),
            ObjectKind::View => format!(
                "SELECT v.table_name, NULL, NULL, COALESCE(c.comment, ''), v.owner_name, v.create_time::varchar, '', '' \
                 FROM v_catalog.views v LEFT JOIN v_catalog.comments c ON c.object_id = v.table_id \
                 WHERE v.table_schema = {} ORDER BY v.table_name",
                schema
            ),
            ObjectKind::Function => format!(
                "SELECT function_name || '(' || COALESCE(function_argument_type, '') || ')', NULL, NULL, COALESCE(comment, ''), owner, '', \
                 COALESCE(function_return_type, ''), procedure_type \
                 FROM v_catalog.user_functions WHERE schema_name = {} ORDER BY 1",
                schema
            ),
            ObjectKind::Procedure => format!(
                "SELECT procedure_name || '(' || COALESCE(procedure_arguments, '') || ')', NULL, NULL, '', owner, '', '', '' \
                 FROM v_catalog.user_procedures WHERE schema_name = {} ORDER BY 1",
                schema
            ),
            ObjectKind::Sequence => format!(
                "SELECT sequence_name, NULL, NULL, '', owner_name, '', COALESCE(identity_table_name, ''), current_value::varchar \
                 FROM v_catalog.sequences WHERE sequence_schema = {} ORDER BY sequence_name",
                schema
            ),
            _ => return Ok(vec![]),
        };
        let r = match self.query(None, &sql) {
            Ok(r) => r,
            // user_procedures is missing on some versions
            Err(_) if kind == ObjectKind::Procedure => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        Ok((0..r.rows.len())
            .map(|i| {
                let mut info = ObjectInfo {
                    name: r.text(i, 0),
                    rows: r.rows[i][1].as_f64().map(|v| v as i64),
                    data_length: r.rows[i][2].as_f64().map(|v| v as i64),
                    comment: r.text(i, 3),
                    created: r.text(i, 5),
                    ..Default::default()
                };
                info.extra = vec![("Owner".into(), r.text(i, 4))];
                let mut add = |k: &str, v: String| {
                    if !v.is_empty() {
                        info.extra.push((k.into(), v));
                    }
                };
                match kind {
                    ObjectKind::Table => {
                        add("Partition By", r.text(i, 6));
                        add("Type", r.text(i, 7));
                    }
                    ObjectKind::Function => {
                        add("Returns", r.text(i, 6));
                        add("Type", r.text(i, 7));
                    }
                    ObjectKind::Sequence => {
                        add("Identity Table", r.text(i, 6));
                        add("Current Value", r.text(i, 7));
                    }
                    _ => {}
                }
                info
            })
            .collect())
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let schema = lit(&Self::schema_of(path));
        let table = lit(&path.name);
        let r = self.query(
            None,
            &format!(
                "SELECT column_name, data_type, is_nullable, column_default, is_identity FROM v_catalog.columns \
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                schema, table
            ),
        )?;
        let mut columns: Vec<ColumnDef> = (0..r.rows.len())
            .map(|i| ColumnDef {
                name: r.text(i, 0),
                data_type: r.text(i, 1),
                nullable: r.rows[i][2] == Value::Bool(true),
                default: match &r.rows[i][3] {
                    Value::Null => None,
                    v => Some(v.to_string()).filter(|s| !s.is_empty()),
                },
                auto_increment: r.rows[i][4] == Value::Bool(true),
                ..Default::default()
            })
            .collect();
        if columns.is_empty() {
            let v = self.query(
                None,
                &format!(
                    "SELECT column_name, data_type FROM v_catalog.view_columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                    schema, table
                ),
            )?;
            columns = (0..v.rows.len()).map(|i| ColumnDef::new(v.text(i, 0), v.text(i, 1))).collect();
        }
        if columns.is_empty() {
            bail!("table {} not found", path.display());
        }
        for c in columns.iter_mut().filter(|c| c.auto_increment) {
            c.default = None;
        }
        let k = self.query(
            None,
            &format!(
                "SELECT constraint_name, column_name, constraint_type, COALESCE(reference_table_schema, ''), COALESCE(reference_table_name, ''), \
                 COALESCE(reference_column_name, '') FROM v_catalog.constraint_columns \
                 WHERE table_schema = {} AND table_name = {} AND constraint_type IN ('p', 'u', 'f') ORDER BY constraint_name, column_name",
                schema, table
            ),
        )?;
        // constraint_columns has no ordinal: use primary_keys / foreign_keys for ordering when available
        let pk_order = self
            .query(
                None,
                &format!(
                    "SELECT column_name FROM v_catalog.primary_keys WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                    schema, table
                ),
            )
            .map(|r| (0..r.rows.len()).map(|i| r.text(i, 0)).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut indexes: Vec<IndexDef> = Vec::new();
        let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
        for i in 0..k.rows.len() {
            let (name, col, ty) = (k.text(i, 0), k.text(i, 1), k.text(i, 2));
            if ty == "f" {
                match foreign_keys.iter_mut().find(|f| f.name == name) {
                    Some(f) => {
                        f.columns.push(col);
                        f.ref_columns.push(k.text(i, 5));
                    }
                    None => foreign_keys.push(ForeignKeyDef {
                        name,
                        columns: vec![col],
                        ref_db: path.db.clone(),
                        ref_schema: Some(k.text(i, 3)),
                        ref_table: k.text(i, 4),
                        ref_columns: vec![k.text(i, 5)],
                        ..Default::default()
                    }),
                }
            } else {
                match indexes.iter_mut().find(|x| x.name == name) {
                    Some(x) => x.columns.push(col),
                    None => indexes.push(IndexDef { name, columns: vec![col], unique: true, primary: ty == "p", ..Default::default() }),
                }
            }
        }
        if let Ok(fo) = self.query(
            None,
            &format!(
                "SELECT constraint_name, column_name, reference_column_name FROM v_catalog.foreign_keys \
                 WHERE table_schema = {} AND table_name = {} ORDER BY constraint_name, ordinal_position",
                schema, table
            ),
        ) {
            for f in foreign_keys.iter_mut() {
                let rows: Vec<usize> = (0..fo.rows.len()).filter(|&i| fo.text(i, 0) == f.name).collect();
                if rows.len() == f.columns.len() {
                    f.columns = rows.iter().map(|&i| fo.text(i, 1)).collect();
                    f.ref_columns = rows.iter().map(|&i| fo.text(i, 2)).collect();
                }
            }
        }
        if let Some(pk) = indexes.iter_mut().find(|x| x.primary) {
            if pk_order.len() == pk.columns.len() {
                pk.columns = pk_order;
            }
        }
        indexes.sort_by_key(|x| !x.primary);
        let pk_cols: Vec<String> = indexes.iter().find(|x| x.primary).map(|x| x.columns.clone()).unwrap_or_default();
        for c in columns.iter_mut() {
            c.primary_key = pk_cols.contains(&c.name);
        }
        let checks = self
            .query(
                None,
                &format!(
                    "SELECT c.constraint_name, c.predicate FROM v_catalog.table_constraints c JOIN v_catalog.tables t ON t.table_id = c.table_id \
                     WHERE t.table_schema = {} AND t.table_name = {} AND c.constraint_type = 'c' ORDER BY 1",
                    schema, table
                ),
            )
            .map(|r| {
                (0..r.rows.len())
                    .map(|i| {
                        let e = r.text(i, 1);
                        let e = e.trim();
                        let e = e.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(e);
                        CheckDef { name: r.text(i, 0), expression: e.to_string() }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let comment = self
            .query(
                None,
                &format!(
                    "SELECT c.comment FROM v_catalog.comments c WHERE c.object_type IN ('TABLE', 'VIEW') AND c.object_schema = {} AND c.object_name = {}",
                    schema, table
                ),
            )
            .map(|r| r.text(0, 0))
            .unwrap_or_default();
        Ok(TableDef { path: path.clone(), columns, indexes, foreign_keys, checks, comment, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let schema = lit(&Self::schema_of(path));
        let name = base_name(&path.name).to_string();
        match kind {
            ObjectKind::Table => match self.export(path) {
                Ok(s) => Ok(s),
                Err(_) => {
                    let def = self.table_def(path)?;
                    Ok(crate::ddl::script(&crate::ddl::create_table(D, &def)))
                }
            },
            ObjectKind::View => {
                let r = self.query(
                    None,
                    &format!(
                        "SELECT view_definition FROM v_catalog.views WHERE table_schema = {} AND table_name = {}",
                        schema,
                        lit(&name)
                    ),
                )?;
                if r.rows.is_empty() {
                    bail!("view {} not found", path.display());
                }
                Ok(format!(
                    "CREATE OR REPLACE VIEW {}.{} AS\n{};",
                    D.quote_ident(&Self::schema_of(path)),
                    D.quote_ident(&name),
                    r.text(0, 0).trim().trim_end_matches(';')
                ))
            }
            ObjectKind::Function => {
                let r = self.query(
                    None,
                    &format!(
                        "SELECT function_name, COALESCE(function_argument_type, ''), COALESCE(function_return_type, ''), COALESCE(function_definition, ''), procedure_type \
                         FROM v_catalog.user_functions WHERE schema_name = {} AND function_name = {}",
                        schema,
                        lit(&name)
                    ),
                )?;
                let i = (0..r.rows.len())
                    .find(|&i| format!("{}({})", r.text(i, 0), r.text(i, 1)) == path.name)
                    .unwrap_or(0);
                if r.rows.is_empty() {
                    bail!("function {} not found", path.display());
                }
                let def = r.text(i, 3);
                let qualified = format!("{}.{}", D.quote_ident(&Self::schema_of(path)), D.quote_ident(&name));
                let body = def.trim();
                if body.to_ascii_uppercase().starts_with("RETURN") {
                    // the catalog keeps "x Integer" (or bare types on old versions)
                    let args = r
                        .text(i, 1)
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .enumerate()
                        .map(|(n, t)| if t.contains(' ') { t.to_string() } else { format!("a{} {}", n + 1, t) })
                        .collect::<Vec<_>>()
                        .join(", ");
                    Ok(format!(
                        "CREATE OR REPLACE FUNCTION {}({}) RETURN {}\nAS BEGIN\n  {};\nEND;",
                        qualified,
                        args,
                        r.text(i, 2),
                        body.trim_end_matches(';')
                    ))
                } else {
                    Ok(format!("-- {} {}({}) RETURN {}\n-- {}", r.text(i, 4), qualified, r.text(i, 1), r.text(i, 2), def))
                }
            }
            ObjectKind::Sequence | ObjectKind::Procedure => self.export(path),
            _ => bail!("DDL not available for {:?}", kind),
        }
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        self.use_scope(scope)?;
        let mut out = Vec::new();
        for stmt in split(sql) {
            out.push(self.run(&stmt)?);
        }
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.query(
            None,
            "SELECT user_name, is_super_user::varchar, is_locked::varchar, COALESCE(profile_name, ''), COALESCE(resource_pool, ''), \
             COALESCE(all_roles, '') FROM v_catalog.users ORDER BY user_name",
        )?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: vec![
                    ("Superuser".into(), r.text(i, 1)),
                    ("Locked".into(), r.text(i, 2)),
                    ("Profile".into(), r.text(i, 3)),
                    ("Resource Pool".into(), r.text(i, 4)),
                    ("Roles".into(), r.text(i, 5)),
                ],
            })
            .collect())
    }

    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        let who = D.quote_ident(&spec.name);
        let mut out = Vec::new();
        match old {
            None => {
                let mut s = format!("CREATE USER {}", who);
                if !spec.password.is_empty() {
                    s.push_str(&format!(" IDENTIFIED BY {}", lit(&spec.password)));
                }
                out.push(s);
            }
            Some(o) => {
                if o.name != spec.name {
                    out.push(format!("ALTER USER {} RENAME TO {}", D.quote_ident(&o.name), who));
                }
                if !spec.password.is_empty() {
                    out.push(format!("ALTER USER {} IDENTIFIED BY {}", who, lit(&spec.password)));
                }
            }
        }
        out.push(format!("ALTER USER {} ACCOUNT {}", who, if spec.can_login || old.is_none() { "UNLOCK" } else { "LOCK" }));
        for (privilege, obj) in &spec.grants {
            if !privilege.is_empty() && !obj.is_empty() {
                out.push(format!("GRANT {} ON {} TO {}", privilege, obj, who));
            }
        }
        Ok(out)
    }

    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        let r = self.query(
            None,
            &format!(
                "SELECT 'GRANT ' || privileges_description || ' ON ' || object_type || ' ' || COALESCE(object_schema || '.', '') || object_name || ' TO ' || grantee \
                 FROM v_catalog.grants WHERE grantee = {} ORDER BY 1",
                lit(&user.name)
            ),
        )?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        Ok(format!("DROP USER {} CASCADE", D.quote_ident(&user.name)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let processes = self.query(
            None,
            "SELECT session_id, user_name, client_hostname, node_name, login_timestamp, statement_start, transaction_id, statement_id, \
             client_label, current_statement FROM v_monitor.sessions ORDER BY login_timestamp",
        )?;
        let variables = self.query(
            None,
            "SELECT DISTINCT parameter_name, current_value, default_value, description FROM v_monitor.configuration_parameters ORDER BY parameter_name",
        )?;
        let sys = self.query(None, "SELECT * FROM v_monitor.system")?;
        let mut status = QueryResult {
            sql: "SELECT * FROM v_monitor.system".into(),
            columns: vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            is_query: true,
            ..Default::default()
        };
        if let Some(row) = sys.rows.first() {
            for (c, v) in sys.columns.iter().zip(row) {
                status.rows.push(vec![Value::Text(c.name.clone()), v.clone()]);
            }
        }
        if let Ok(n) = self.query(None, "SELECT node_name, node_state FROM v_catalog.nodes ORDER BY node_name") {
            for i in 0..n.rows.len() {
                status.rows.push(vec![Value::Text(format!("node {}", n.text(i, 0))), Value::Text(n.text(i, 1))]);
            }
        }
        Ok(ServerSnapshot { processes, variables, status })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.query(None, &format!("SELECT CLOSE_SESSION({})", lit(id.trim())))?;
        Ok(())
    }

    fn ping(&mut self) -> Result<()> {
        self.query(None, "SELECT 1").map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_insert_split() {
        let sql = "INSERT INTO \"t\" (\"a\", \"b\") VALUES\n('x,(y)', 1),\n('it''s', NULL)";
        assert_eq!(
            split_multi_insert(sql).unwrap(),
            vec![
                "INSERT INTO \"t\" (\"a\", \"b\") VALUES ('x,(y)', 1)".to_string(),
                "INSERT INTO \"t\" (\"a\", \"b\") VALUES ('it''s', NULL)".to_string()
            ]
        );
        assert!(split_multi_insert("INSERT INTO t VALUES (1)").is_none());
        assert!(split_multi_insert("INSERT INTO t SELECT * FROM u").is_none());
        assert!(split_multi_insert("SELECT 'VALUES (1), (2)'").is_none());
        assert!(split_multi_insert("INSERT INTO \"values\" (a) VALUES (1), (2)").unwrap().len() == 2);
    }

    #[test]
    fn split_function_bodies() {
        let v = split("CREATE FUNCTION f(x INT) RETURN INT AS BEGIN RETURN x * 2; END; SELECT f(1);\nSELECT 2");
        assert_eq!(v, vec!["CREATE FUNCTION f(x INT) RETURN INT AS BEGIN RETURN x * 2; END", "SELECT f(1)", "SELECT 2"]);
        assert_eq!(split("CREATE PROCEDURE p() LANGUAGE PLvSQL AS $$ BEGIN PERFORM 1; END; $$; SELECT 1").len(), 2);
    }

    #[test]
    fn values_and_types() {
        let f = |oid, m| wire::Field { name: "c".into(), type_oid: oid, user_type: None, type_mod: m };
        assert_eq!(to_value(&f(6, -1), Some(b"-12".to_vec())), Value::Int(-12));
        assert_eq!(to_value(&f(7, -1), Some(b"1.5".to_vec())), Value::Float(1.5));
        assert_eq!(to_value(&f(5, -1), Some(b"t".to_vec())), Value::Bool(true));
        assert_eq!(to_value(&f(16, -1), Some(b"1.50".to_vec())), Value::Text("1.50".into()));
        assert_eq!(to_value(&f(17, -1), Some(b"\\001a".to_vec())), Value::Bytes(vec![1, b'a']));
        assert_eq!(to_value(&f(9, -1), None), Value::Null);
        assert_eq!(type_name(&f(9, 54)), "Varchar(50)");
        assert_eq!(type_name(&f(16, ((10 << 16) | 2) + 4)), "Numeric(10,2)");
        assert_eq!(affected("INSERT 0 3"), 3);
        assert_eq!(affected("SELECT"), 0);
    }

    #[test]
    fn mock_driver() {
        fn answer(sql: &str) -> Vec<u8> {
            use wire::tests::*;
            if sql == "SELECT CURRENT_DATABASE()" {
                let mut v = row_description(&[("current_database", 9, 132)]);
                v.extend(data_row(&[Some(b"VMart")]));
                v.extend(msg(b'C', b"SELECT\0"));
                v
            } else if sql.starts_with("SET ") {
                msg(b'C', b"SET\0")
            } else if sql.starts_with("INSERT") {
                let n = sql.matches("INSERT").count();
                (0..n)
                    .flat_map(|_| {
                        let mut v = row_description(&[("OUTPUT", 6, -1)]);
                        v.extend(data_row(&[Some(b"1")]));
                        v.extend(msg(b'C', b"\0"));
                        v
                    })
                    .collect()
            } else {
                error("42601", "Syntax error")
            }
        }
        let port = wire::tests::mock_server("pw", answer);
        let mut cfg = ConnectionConfig::new(DbKind::Vertica);
        cfg.host = "127.0.0.1".into();
        cfg.port = port;
        cfg.password = "pw".into();
        let mut d = VerticaDriver::connect(&cfg).unwrap();
        assert_eq!(d.list_databases().unwrap(), vec!["VMart".to_string()]);
        let r = d.execute(Some(&Scope::new("VMart", Some("s".into()))), "INSERT INTO t (a) VALUES (1), (2), (3)").unwrap();
        assert_eq!(r[0].affected_rows, 3);
        assert!(d.execute(None, "nonsense").unwrap_err().to_string().contains("Syntax error"));
    }
}
