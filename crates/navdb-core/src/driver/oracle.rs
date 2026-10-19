//! Oracle Database driver (rust-oracle / ODPI-C, needs Oracle Instant Client
//! at runtime). "Databases" are schemas (users); there are no sub-schemas.

use anyhow::{anyhow, bail, Context as _, Result};
use oracle::sql_type::{OracleType, ToSql};
use oracle::{Connection, Connector, InitParams, Privilege, SqlValue};
use std::time::Instant;

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Oracle;

const CLIENT_HINT: &str = "Oracle Instant Client not found: install it from \
https://www.oracle.com/database/technologies/instant-client/downloads.html and set the 'Oracle client directory' option";

const TS: &str = "'YYYY-MM-DD HH24:MI:SS'";

pub struct OracleDriver {
    cfg: ConnectionConfig,
    conn: Connection,
    /// Current schema of the session (`ALTER SESSION SET CURRENT_SCHEMA`).
    schema: String,
    major: i32,
    version: String,
    metadata_ready: bool,
}

pub(crate) fn service_name(cfg: &ConnectionConfig) -> String {
    let s = cfg.option("service_name").trim();
    if !s.is_empty() {
        s.to_string()
    } else if !cfg.database.trim().is_empty() {
        cfg.database.trim().to_string()
    } else {
        "FREEPDB1".into()
    }
}

/// Connect string: option `connect_string` (TNS alias / descriptor), Easy Connect
/// `//host:port/service`, or a descriptor for `sid` / TLS.
pub(crate) fn connect_string(cfg: &ConnectionConfig) -> String {
    let custom = cfg.option("connect_string").trim();
    if !custom.is_empty() {
        return custom.to_string();
    }
    let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
    let port = if cfg.port == 0 { 1521 } else { cfg.port };
    let sid = cfg.option("sid").trim();
    let tls = cfg.ssl.enabled;
    if sid.is_empty() && !tls {
        return format!("//{}:{}/{}", host, port, service_name(cfg));
    }
    let data = if sid.is_empty() { format!("(SERVICE_NAME={})", service_name(cfg)) } else { format!("(SID={})", sid) };
    let timeout = if cfg.connect_timeout_secs > 0 { format!("(CONNECT_TIMEOUT={})", cfg.connect_timeout_secs) } else { String::new() };
    let security = if tls && !cfg.ssl.verify_server { "(SECURITY=(SSL_SERVER_DN_MATCH=FALSE))" } else { "" };
    format!(
        "(DESCRIPTION={}(ADDRESS=(PROTOCOL={})(HOST={})(PORT={}))(CONNECT_DATA={}){})",
        timeout,
        if tls { "TCPS" } else { "TCP" },
        host,
        port,
        data,
        security
    )
}

fn is_client_load_error(e: &oracle::Error) -> bool {
    let m = e.to_string();
    e.dpi_code() == Some(1047) || m.contains("DPI-1047") || m.contains("Cannot locate a 64-bit Oracle Client")
}

fn client_error(e: oracle::Error) -> anyhow::Error {
    anyhow!("{}\n{}", CLIENT_HINT, e)
}

/// Loads the Oracle client library once per process (option `oracle_client_dir`).
fn init_client(cfg: &ConnectionConfig) -> Result<()> {
    if InitParams::is_initialized() {
        return Ok(());
    }
    let mut p = InitParams::new();
    p.default_driver_name("NavDB Studio")?;
    let dir = cfg.option("oracle_client_dir").trim();
    if !dir.is_empty() {
        p.oracle_client_lib_dir(dir)?;
    }
    match p.init() {
        Ok(_) => Ok(()),
        Err(e) if is_client_load_error(&e) => Err(client_error(e)),
        Err(e) => Err(anyhow::Error::new(e).context("initializing the Oracle client library")),
    }
}

fn privilege(role: &str) -> Result<Option<Privilege>> {
    Ok(Some(match role.trim().to_ascii_uppercase().as_str() {
        "" | "DEFAULT" | "NORMAL" => return Ok(None),
        "SYSDBA" => Privilege::Sysdba,
        "SYSOPER" => Privilege::Sysoper,
        "SYSASM" => Privilege::Sysasm,
        "SYSBACKUP" => Privilege::Sysbackup,
        "SYSDG" => Privilege::Sysdg,
        "SYSKM" => Privilege::Syskm,
        r => bail!("unknown Oracle role '{}' (use SYSDBA or SYSOPER)", r),
    }))
}

fn soft_error(sql: &str, e: anyhow::Error) -> QueryResult {
    QueryResult {
        sql: sql.into(),
        columns: vec![ColumnMeta::new("Error")],
        rows: vec![vec![Value::Text(format!("{:#}", e))]],
        is_query: true,
        ..Default::default()
    }
}

impl OracleDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        init_client(cfg)?;
        let cs = connect_string(cfg);
        let mut c = Connector::new(cfg.user.as_str(), cfg.password.as_str(), cs.as_str());
        if let Some(p) = privilege(cfg.option("role"))? {
            c.privilege(p);
        }
        let mut conn = match c.connect() {
            Ok(c) => c,
            Err(e) if is_client_load_error(&e) => return Err(client_error(e)),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("connecting to {}", cs))),
        };
        conn.set_autocommit(true);
        let (ver, banner) = conn.server_version()?;
        let mut me = Self {
            cfg: cfg.clone(),
            conn,
            schema: String::new(),
            major: ver.major(),
            version: format!("{} ({})", banner.lines().next().unwrap_or("Oracle Database").trim(), ver),
            metadata_ready: false,
        };
        for s in [
            format!("ALTER SESSION SET NLS_DATE_FORMAT = {}", TS),
            "ALTER SESSION SET NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF'".into(),
            "ALTER SESSION SET NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'".into(),
            "ALTER SESSION SET NLS_NUMERIC_CHARACTERS = '.,'".into(),
        ] {
            me.run(&s, &[])?;
        }
        me.refresh_schema()?;
        let initial = cfg.option("schema").trim().to_string();
        if !initial.is_empty() {
            me.use_scope(Some(&Scope::new(initial, None)))?;
        }
        Ok(me)
    }

    fn refresh_schema(&mut self) -> Result<()> {
        self.schema = self.run("SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM DUAL", &[])?.text(0, 0);
        Ok(())
    }

    fn use_scope(&mut self, scope: Option<&Scope>) -> Result<()> {
        let Some(s) = scope else { return Ok(()) };
        let target = s.schema.as_deref().filter(|x| !x.is_empty()).unwrap_or(&s.db);
        if target.is_empty() || target == self.schema {
            return Ok(());
        }
        match self.run(&format!("ALTER SESSION SET CURRENT_SCHEMA = {}", D.quote_ident(target)), &[]) {
            Ok(_) => {
                self.schema = target.to_string();
                Ok(())
            }
            // `database` doubles as the service name; it is not a schema then
            Err(_) if target == self.cfg.database.trim() && self.cfg.option("service_name").trim().is_empty() => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn owner(&self, db: &str, schema: Option<&str>) -> String {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None if !db.is_empty() => db.to_string(),
            None => self.schema.clone(),
        }
    }

    fn path_owner(&self, p: &ObjPath) -> String {
        self.owner(&p.db, p.schema.as_deref())
    }

    /// Runs one statement (no splitting / rewriting).
    fn run(&self, sql: &str, params: &[&dyn ToSql]) -> Result<QueryResult> {
        let start = Instant::now();
        let mut stmt = self.conn.statement(sql).fetch_array_size(500).build()?;
        let mut out = QueryResult { sql: sql.to_string(), ..Default::default() };
        if stmt.is_query() {
            out.is_query = true;
            let rows = stmt.query(params)?;
            out.columns = rows
                .column_info()
                .iter()
                .map(|c| ColumnMeta { name: c.name().to_string(), type_name: c.oracle_type().to_string() })
                .collect();
            for row in rows {
                let row = row?;
                out.rows.push(row.sql_values().iter().map(to_value).collect());
            }
        } else {
            stmt.execute(params)?;
            if stmt.is_dml() {
                out.affected_rows = stmt.row_count()?;
            }
        }
        out.elapsed_ms = start.elapsed().as_millis();
        Ok(out)
    }

    fn run_owner(&self, sql: &str, owner: &str) -> Result<QueryResult> {
        let o = owner.to_string();
        self.run(sql, &[&o])
    }

    fn run_table(&self, sql: &str, p: &ObjPath) -> Result<QueryResult> {
        let o = self.path_owner(p);
        let n = p.name.clone();
        self.run(sql, &[&o, &n])
    }

    fn rewrite(&self, stmt: &str) -> String {
        if self.major < 23 {
            if let Some(s) = rewrite_multi_insert(stmt) {
                return s;
            }
        }
        stmt.to_string()
    }

    fn setup_metadata(&mut self) {
        if self.metadata_ready {
            return;
        }
        self.metadata_ready = true;
        let _ = self.run(
            "BEGIN \
             DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
             DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
             DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
             END;",
            &[],
        );
    }

    fn ddl_fallback(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        match kind {
            ObjectKind::Table => {
                let def = self.table_def(path)?;
                Ok(crate::ddl::script(&crate::ddl::create_table(D, &def)))
            }
            ObjectKind::View => {
                let r = self.run_table("SELECT text FROM all_views WHERE owner = :1 AND view_name = :2", path)?;
                if r.rows.is_empty() {
                    bail!("view {} not found", path.display());
                }
                Ok(format!("CREATE OR REPLACE VIEW {} AS\n{};", D.qualify(path), r.text(0, 0).trim()))
            }
            ObjectKind::MaterializedView => {
                let r = self.run_table("SELECT query FROM all_mviews WHERE owner = :1 AND mview_name = :2", path)?;
                if r.rows.is_empty() {
                    bail!("materialized view {} not found", path.display());
                }
                Ok(format!("CREATE MATERIALIZED VIEW {} AS\n{};", D.qualify(path), r.text(0, 0).trim()))
            }
            ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger => {
                let o = self.path_owner(path);
                let n = path.name.clone();
                let t = kind.sql_keyword().to_string();
                let r = self.run(
                    "SELECT text FROM all_source WHERE owner = :1 AND name = :2 AND type = :3 ORDER BY line",
                    &[&o, &n, &t],
                )?;
                if r.rows.is_empty() {
                    bail!("{} {} not found", kind.singular(), path.display());
                }
                let body: String = (0..r.rows.len()).map(|i| r.text(i, 0)).collect();
                Ok(format!("CREATE OR REPLACE {}\n/", body.trim_end()))
            }
            _ => bail!("DDL not available for {:?}", kind),
        }
    }
}

fn normalize_number(s: &str) -> String {
    let s = s.trim();
    if let Some(r) = s.strip_prefix("-.") {
        format!("-0.{}", r)
    } else if let Some(r) = s.strip_prefix('.') {
        format!("0.{}", r)
    } else {
        s.to_string()
    }
}

/// NUMBER text -> Int when integral and in range; scaled decimals stay exact text.
pub(crate) fn number_value(raw: &str, scale: i8) -> Value {
    let s = normalize_number(raw);
    if scale > 0 {
        return Value::Text(s);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int(i);
    }
    if scale == 0 {
        return Value::Text(s);
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Value::Float(f),
        _ => Value::Text(s),
    }
}

fn to_value(v: &SqlValue) -> Value {
    if v.is_null().unwrap_or(false) {
        return Value::Null;
    }
    let text = |v: &SqlValue| match v.get::<String>() {
        Ok(s) => Value::Text(s),
        Err(e) => Value::Text(format!("({})", e)),
    };
    let Ok(ty) = v.oracle_type() else { return text(v) };
    match ty {
        OracleType::Number(_, scale) => match v.get::<String>() {
            Ok(s) => number_value(&s, *scale),
            Err(_) => text(v),
        },
        OracleType::Float(_) => match v.get::<String>() {
            Ok(s) => number_value(&s, -127),
            Err(_) => text(v),
        },
        OracleType::Int64 => v.get::<i64>().map(Value::Int).unwrap_or_else(|_| text(v)),
        OracleType::UInt64 => v.get::<u64>().map(Value::UInt).unwrap_or_else(|_| text(v)),
        OracleType::BinaryFloat | OracleType::BinaryDouble => v.get::<f64>().map(Value::Float).unwrap_or_else(|_| text(v)),
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => {
            v.get::<Vec<u8>>().map(Value::Bytes).unwrap_or_else(|_| text(v))
        }
        OracleType::Boolean => v.get::<bool>().map(Value::Bool).unwrap_or_else(|_| text(v)),
        OracleType::BFILE => Value::Text("(BFILE)".into()),
        _ => text(v),
    }
}

/// Column type text from ALL_TAB_COLUMNS, e.g. `VARCHAR2(50)`, `NUMBER(10,2)`.
pub(crate) fn col_type(
    dt: &str,
    len: Option<i64>,
    char_len: Option<i64>,
    char_used: &str,
    prec: Option<i64>,
    scale: Option<i64>,
) -> String {
    match dt {
        "VARCHAR2" | "CHAR" => {
            if char_used == "C" {
                format!("{}({} CHAR)", dt, char_len.unwrap_or(1))
            } else {
                format!("{}({})", dt, len.unwrap_or(1))
            }
        }
        "NVARCHAR2" | "NCHAR" => format!("{}({})", dt, char_len.unwrap_or(1)),
        "RAW" | "UROWID" => format!("{}({})", dt, len.unwrap_or(1)),
        "NUMBER" => match (prec, scale) {
            (None, None) => "NUMBER".into(),
            (None, Some(0)) => "INTEGER".into(),
            (None, Some(s)) => format!("NUMBER(*,{})", s),
            (Some(p), None) | (Some(p), Some(0)) => format!("NUMBER({})", p),
            (Some(p), Some(s)) => format!("NUMBER({},{})", p, s),
        },
        "FLOAT" => match prec {
            Some(p) if p != 126 => format!("FLOAT({})", p),
            _ => "FLOAT".into(),
        },
        _ => dt.to_string(),
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

/// True when the statement is a PL/SQL unit (terminated by `/`, keeps `;`).
pub(crate) fn is_plsql(stmt: &str) -> bool {
    let s = crate::sqlutil::strip_leading_comments(stmt).to_ascii_uppercase();
    let words: Vec<&str> = s.split(|c: char| !is_ident_char(c)).filter(|w| !w.is_empty()).take(8).collect();
    let mut it = words.iter().copied().peekable();
    match it.next() {
        Some("BEGIN") | Some("DECLARE") => true,
        Some("CREATE") => {
            if it.peek() == Some(&"OR") {
                it.next();
                it.next();
            }
            while matches!(it.peek(), Some(&"EDITIONABLE") | Some(&"NONEDITIONABLE") | Some(&"EDITIONING")) {
                it.next();
            }
            matches!(it.next(), Some("FUNCTION" | "PROCEDURE" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY" | "JAVA"))
        }
        _ => false,
    }
}

fn rest_of_line_blank(chars: &[char], from: usize) -> bool {
    chars[from..].iter().take_while(|c| **c != '\n').all(|c| c.is_whitespace())
}

/// Splits an Oracle script: plain SQL ends at `;` (stripped), PL/SQL units end
/// at a line containing only `/` and keep their semicolons.
pub(crate) fn split_script(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut line_start = true;
    let mut i = 0;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let s = cur.trim();
        if !crate::sqlutil::strip_leading_comments(s).is_empty() {
            out.push(s.to_string());
        }
        cur.clear();
    };
    while i < n {
        let c = chars[i];
        if line_start && c == '/' && rest_of_line_blank(&chars, i + 1) {
            flush(&mut cur, &mut out);
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '\n' {
            line_start = true;
            cur.push(c);
            i += 1;
            continue;
        }
        if !c.is_whitespace() {
            line_start = false;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < n && chars[i] != '\n' {
                cur.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            cur.push_str("/*");
            i += 2;
            while i < n && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                cur.push(chars[i]);
                i += 1;
            }
            if i < n {
                cur.push_str("*/");
                i += 2;
            }
            continue;
        }
        // q'[...]' alternative quoting (also nq'...')
        if (c == 'q' || c == 'Q') && chars.get(i + 1) == Some(&'\'') && i + 2 < n {
            let prev_ok = i == 0
                || !is_ident_char(chars[i - 1])
                || (matches!(chars[i - 1], 'n' | 'N') && (i < 2 || !is_ident_char(chars[i - 2])));
            if prev_ok {
                let open = chars[i + 2];
                let close = match open {
                    '[' => ']',
                    '(' => ')',
                    '{' => '}',
                    '<' => '>',
                    o => o,
                };
                cur.push(c);
                cur.push('\'');
                cur.push(open);
                i += 3;
                while i < n {
                    if chars[i] == close && chars.get(i + 1) == Some(&'\'') {
                        cur.push(close);
                        cur.push('\'');
                        i += 2;
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
        }
        if c == '\'' || c == '"' {
            cur.push(c);
            i += 1;
            while i < n {
                let ch = chars[i];
                cur.push(ch);
                i += 1;
                if ch == c {
                    if chars.get(i) == Some(&c) {
                        cur.push(c);
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            continue;
        }
        if c == ';' {
            if is_plsql(&cur) {
                cur.push(';');
            } else {
                flush(&mut cur, &mut out);
            }
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut cur, &mut out);
    out
}

/// Index of the matching `)` for the `(` at `open`, skipping string literals.
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let q = chars[i];
                i += 1;
                while i < chars.len() {
                    if chars[i] == q {
                        if chars.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `INSERT INTO t (..) VALUES (..), (..)` -> `INSERT INTO t (..) SELECT .. FROM DUAL UNION ALL ..`
/// for servers before 23ai, which only accept a single VALUES row.
pub(crate) fn rewrite_multi_insert(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    let head: String = s.chars().take(12).collect::<String>().to_ascii_uppercase();
    if !head.starts_with("INSERT") {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    // find top-level VALUES keyword
    let mut i = 0;
    let mut values_at = None;
    while i < n {
        match chars[i] {
            '\'' | '"' => {
                let q = chars[i];
                i += 1;
                while i < n && chars[i] != q {
                    i += 1;
                }
            }
            '(' => i = matching_paren(&chars, i)?,
            c if c.is_ascii_alphabetic() && (i == 0 || !is_ident_char(chars[i - 1])) => {
                let w: String = chars[i..].iter().take_while(|c| is_ident_char(**c)).collect();
                if w.eq_ignore_ascii_case("VALUES") {
                    values_at = Some(i);
                    break;
                }
                if w.eq_ignore_ascii_case("SELECT") {
                    return None;
                }
                i += w.chars().count();
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let values_at = values_at?;
    let mut tuples = Vec::new();
    let mut j = values_at + 6;
    loop {
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j >= n || chars[j] != '(' {
            return None;
        }
        let close = matching_paren(&chars, j)?;
        tuples.push(chars[j + 1..close].iter().collect::<String>());
        j = close + 1;
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        if j >= n {
            break;
        }
        if chars[j] != ',' {
            return None;
        }
        j += 1;
    }
    if tuples.len() < 2 {
        return None;
    }
    let prefix: String = chars[..values_at].iter().collect();
    Some(format!(
        "{}\n{}",
        prefix.trim_end(),
        tuples.iter().map(|t| format!("SELECT {} FROM DUAL", t.trim())).collect::<Vec<_>>().join("\nUNION ALL ")
    ))
}

fn int(r: &QueryResult, row: usize, col: usize) -> Option<i64> {
    r.rows.get(row).and_then(|x| x.get(col)).and_then(|v| v.as_f64()).map(|v| v as i64)
}

/// Rows of an object-list query: name, rows, data length, comment, created,
/// modified, then the `extras` columns.
fn object_rows(r: &QueryResult, extras: &[&str]) -> Vec<ObjectInfo> {
    (0..r.rows.len())
        .map(|i| ObjectInfo {
            name: r.text(i, 0),
            rows: int(r, i, 1),
            data_length: int(r, i, 2),
            comment: r.text(i, 3),
            created: r.text(i, 4),
            modified: r.text(i, 5),
            extra: extras.iter().enumerate().map(|(k, l)| (l.to_string(), r.text(i, 6 + k))).collect(),
            ..Default::default()
        })
        .collect()
}

fn not_null_check(cond: &str) -> bool {
    let c = cond.trim();
    c.strip_prefix('"')
        .and_then(|x| x.strip_suffix("\" IS NOT NULL"))
        .map(|x| !x.contains('"'))
        .unwrap_or(false)
}

impl Driver for OracleDriver {
    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn server_version(&mut self) -> Result<String> {
        Ok(self.version.clone())
    }

    fn ping(&mut self) -> Result<()> {
        Ok(self.conn.ping()?)
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self
            .run(
                "SELECT username FROM all_users WHERE oracle_maintained = 'N' \
                 OR username IN (SYS_CONTEXT('USERENV', 'SESSION_USER'), SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) ORDER BY username",
                &[],
            )
            .or_else(|_| self.run("SELECT username FROM all_users ORDER BY username", &[]))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::Sequence,
            ObjectKind::Index,
        ]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let owner = self.owner(&scope.db, scope.schema.as_deref());
        let obj = |ty: &str| {
            format!(
                "LEFT JOIN all_objects o ON o.owner = x.owner AND o.object_name = x.{{n}} AND o.object_type = '{}'",
                ty
            )
        };
        let dates = format!("TO_CHAR(o.created, {0}), TO_CHAR(o.last_ddl_time, {0})", TS);
        let (sql, extras): (String, &[&str]) = match kind {
            ObjectKind::Table => (
                format!(
                    "SELECT x.table_name, x.num_rows, x.num_rows * x.avg_row_len, c.comments, {d}, x.tablespace_name, TO_CHAR(x.last_analyzed, {ts}) \
                     FROM all_tables x {o} LEFT JOIN all_tab_comments c ON c.owner = x.owner AND c.table_name = x.table_name \
                     WHERE x.owner = :1 AND x.nested = 'NO' AND x.secondary = 'N' AND x.dropped = 'NO' AND (x.iot_type IS NULL OR x.iot_type = 'IOT') \
                     AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = x.owner AND m.mview_name = x.table_name) ORDER BY x.table_name",
                    d = dates,
                    ts = TS,
                    o = obj("TABLE").replace("{n}", "table_name")
                ),
                &["Tablespace", "Last Analyzed"],
            ),
            ObjectKind::View => (
                format!(
                    "SELECT x.view_name, NULL, NULL, c.comments, {d}, o.status FROM all_views x {o} \
                     LEFT JOIN all_tab_comments c ON c.owner = x.owner AND c.table_name = x.view_name WHERE x.owner = :1 ORDER BY x.view_name",
                    d = dates,
                    o = obj("VIEW").replace("{n}", "view_name")
                ),
                &["Status"],
            ),
            ObjectKind::MaterializedView => (
                format!(
                    "SELECT x.mview_name, t.num_rows, NULL, c.comments, {d}, x.refresh_mode, x.refresh_method, TO_CHAR(x.last_refresh_date, {ts}), x.staleness \
                     FROM all_mviews x {o} LEFT JOIN all_tables t ON t.owner = x.owner AND t.table_name = x.mview_name \
                     LEFT JOIN all_mview_comments c ON c.owner = x.owner AND c.mview_name = x.mview_name WHERE x.owner = :1 ORDER BY x.mview_name",
                    d = dates,
                    ts = TS,
                    o = obj("MATERIALIZED VIEW").replace("{n}", "mview_name")
                ),
                &["Refresh Mode", "Refresh Method", "Last Refresh", "Staleness"],
            ),
            ObjectKind::Function | ObjectKind::Procedure => (
                format!(
                    "SELECT o.object_name, NULL, NULL, NULL, {d}, o.status FROM all_objects o \
                     WHERE o.owner = :1 AND o.object_type = '{t}' ORDER BY o.object_name",
                    d = dates,
                    t = kind.sql_keyword()
                ),
                &["Status"],
            ),
            ObjectKind::Trigger => (
                format!(
                    "SELECT x.trigger_name, NULL, NULL, NULL, {d}, x.table_name, x.trigger_type, x.triggering_event, x.status \
                     FROM all_triggers x {o} WHERE x.owner = :1 ORDER BY x.trigger_name",
                    d = dates,
                    o = obj("TRIGGER").replace("{n}", "trigger_name")
                ),
                &["Table", "Type", "Event", "Status"],
            ),
            ObjectKind::Sequence => (
                format!(
                    "SELECT x.sequence_name, NULL, NULL, NULL, {d}, TO_CHAR(x.min_value), TO_CHAR(x.max_value), TO_CHAR(x.increment_by), \
                     TO_CHAR(x.last_number), x.cycle_flag FROM all_sequences x \
                     LEFT JOIN all_objects o ON o.owner = x.sequence_owner AND o.object_name = x.sequence_name AND o.object_type = 'SEQUENCE' \
                     WHERE x.sequence_owner = :1 ORDER BY x.sequence_name",
                    d = dates
                ),
                &["Min Value", "Max Value", "Increment", "Last Number", "Cycle"],
            ),
            ObjectKind::Index => (
                format!(
                    "SELECT x.index_name, x.num_rows, NULL, NULL, {d}, x.table_name, x.uniqueness, x.index_type, x.status \
                     FROM all_indexes x {o} WHERE x.owner = :1 AND x.index_type <> 'LOB' ORDER BY x.index_name",
                    d = dates,
                    o = obj("INDEX").replace("{n}", "index_name")
                ),
                &["Table", "Uniqueness", "Type", "Status"],
            ),
            _ => return Ok(vec![]),
        };
        let r = self.run_owner(&sql, &owner)?;
        let mut out = object_rows(&r, extras);
        if kind == ObjectKind::Table {
            for o in out.iter_mut() {
                o.engine = o.extra.first().map(|e| e.1.clone()).unwrap_or_default();
            }
        }
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let cols_sql = |identity: &str| {
            format!(
                "SELECT column_name, data_type, data_length, char_length, char_used, data_precision, data_scale, nullable, data_default, {} \
                 FROM all_tab_columns WHERE owner = :1 AND table_name = :2 ORDER BY column_id",
                identity
            )
        };
        let r = self
            .run_table(&cols_sql("identity_column"), path)
            .or_else(|_| self.run_table(&cols_sql("'NO'"), path))?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let cr = self.run_table(
            "SELECT column_name, comments FROM all_col_comments WHERE owner = :1 AND table_name = :2 AND comments IS NOT NULL",
            path,
        )?;
        let comment_of = |name: &str| {
            (0..cr.rows.len()).find(|&i| cr.text(i, 0) == name).map(|i| cr.text(i, 1)).unwrap_or_default()
        };
        let mut columns = Vec::new();
        for i in 0..r.rows.len() {
            let identity = r.text(i, 9) == "YES";
            let default = match &r.rows[i][8] {
                Value::Null => None,
                v => Some(v.to_string().trim().to_string()).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("NULL")),
            };
            let name = r.text(i, 0);
            columns.push(ColumnDef {
                data_type: col_type(&r.text(i, 1), int(&r, i, 2), int(&r, i, 3), &r.text(i, 4), int(&r, i, 5), int(&r, i, 6)),
                nullable: r.text(i, 7) != "N",
                default: if identity { None } else { default },
                auto_increment: identity,
                comment: comment_of(&name),
                name,
                ..Default::default()
            });
        }

        let kr = self.run_table(
            "SELECT c.constraint_name, c.constraint_type, cc.column_name, c.index_name, r.owner, r.table_name, rc.column_name, c.delete_rule \
             FROM all_constraints c \
             JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name \
             LEFT JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name \
             LEFT JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position \
             WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type IN ('P', 'U', 'R') \
             ORDER BY c.constraint_type, c.constraint_name, cc.position",
            path,
        )?;
        let mut pk: Option<(String, String, Vec<String>)> = None;
        let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
        for i in 0..kr.rows.len() {
            let name = kr.text(i, 0);
            match kr.text(i, 1).as_str() {
                "P" => {
                    let e = pk.get_or_insert_with(|| (name.clone(), kr.text(i, 3), vec![]));
                    e.2.push(kr.text(i, 2));
                }
                "R" => {
                    if foreign_keys.last().map(|f| f.name != name).unwrap_or(true) {
                        let rule = kr.text(i, 7);
                        foreign_keys.push(ForeignKeyDef {
                            name: name.clone(),
                            ref_db: kr.text(i, 4),
                            ref_schema: None,
                            ref_table: kr.text(i, 5),
                            on_delete: if rule == "NO ACTION" { String::new() } else { rule },
                            ..Default::default()
                        });
                    }
                    if let Some(f) = foreign_keys.last_mut() {
                        f.columns.push(kr.text(i, 2));
                        f.ref_columns.push(kr.text(i, 6));
                    }
                }
                _ => {}
            }
        }

        let ir = self.run_table(
            "SELECT i.index_name, i.uniqueness, i.index_type, ic.column_name FROM all_indexes i \
             JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
             WHERE i.table_owner = :1 AND i.table_name = :2 AND i.index_type <> 'LOB' ORDER BY i.index_name, ic.column_position",
            path,
        )?;
        let mut indexes: Vec<IndexDef> = Vec::new();
        for i in 0..ir.rows.len() {
            let name = ir.text(i, 0);
            if indexes.last().map(|x| x.name != name).unwrap_or(true) {
                let ty = ir.text(i, 2);
                indexes.push(IndexDef {
                    primary: pk.as_ref().map(|p| p.1 == name).unwrap_or(false),
                    unique: ir.text(i, 1) == "UNIQUE",
                    method: if ty == "NORMAL" { String::new() } else { ty },
                    name,
                    ..Default::default()
                });
            }
            if let Some(x) = indexes.last_mut() {
                x.columns.push(ir.text(i, 3));
            }
        }
        if let Some((name, _, cols)) = &pk {
            match indexes.iter_mut().find(|x| x.primary) {
                Some(x) => x.name = name.clone(),
                None => indexes.insert(
                    0,
                    IndexDef { name: name.clone(), columns: cols.clone(), unique: true, primary: true, ..Default::default() },
                ),
            }
            for c in columns.iter_mut() {
                c.primary_key = cols.contains(&c.name);
            }
        }
        indexes.sort_by_key(|x| !x.primary);

        let chk = self.run_table(
            "SELECT constraint_name, search_condition FROM all_constraints WHERE owner = :1 AND table_name = :2 AND constraint_type = 'C' ORDER BY constraint_name",
            path,
        )?;
        let checks = (0..chk.rows.len())
            .filter(|&i| !not_null_check(&chk.text(i, 1)))
            .map(|i| CheckDef { name: chk.text(i, 0), expression: chk.text(i, 1).trim().to_string() })
            .collect();

        let tr = self.run_table(
            "SELECT trigger_name, trigger_type, triggering_event, description, trigger_body FROM all_triggers \
             WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name",
            path,
        )?;
        let triggers = (0..tr.rows.len())
            .map(|i| {
                let ty = tr.text(i, 1).to_ascii_uppercase();
                let timing = if ty.starts_with("BEFORE") {
                    "BEFORE"
                } else if ty.starts_with("INSTEAD OF") {
                    "INSTEAD OF"
                } else {
                    "AFTER"
                };
                TriggerDef {
                    name: tr.text(i, 0),
                    timing: timing.into(),
                    event: tr.text(i, 2).trim().to_string(),
                    body: format!("CREATE OR REPLACE TRIGGER {}{}", tr.text(i, 3), tr.text(i, 4)),
                }
            })
            .collect();

        let comment = self
            .run_table("SELECT comments FROM all_tab_comments WHERE owner = :1 AND table_name = :2", path)?
            .text(0, 0);
        Ok(TableDef { path: path.clone(), columns, indexes, foreign_keys, checks, triggers, comment, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let ty = match kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Sequence => "SEQUENCE",
            ObjectKind::Index => "INDEX",
            _ => bail!("DDL not available for {:?}", kind),
        };
        self.setup_metadata();
        let (t, n, o) = (ty.to_string(), path.name.clone(), self.path_owner(path));
        match self.run("SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM DUAL", &[&t, &n, &o]) {
            Ok(r) if !r.text(0, 0).trim().is_empty() => Ok(r.text(0, 0).trim().to_string()),
            Ok(_) => self.ddl_fallback(path, kind),
            Err(e) => self.ddl_fallback(path, kind).map_err(|_| e),
        }
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        self.use_scope(scope)?;
        let mut out = Vec::new();
        for stmt in split_script(sql) {
            let mut r = self.run(&self.rewrite(&stmt), &[])?;
            r.sql = stmt.clone();
            if stmt.to_ascii_uppercase().contains("CURRENT_SCHEMA") && !r.is_query {
                self.refresh_schema()?;
            }
            out.push(r);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        self.use_scope(scope)?;
        let s = sql.trim().trim_end_matches(';').trim();
        self.run(&format!("EXPLAIN PLAN FOR {}", s), &[])?;
        self.run("SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY())", &[])
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self
            .run(
                &format!(
                    "SELECT username, user_id AS \"User ID\", account_status AS \"Status\", default_tablespace AS \"Default Tablespace\", \
                     TO_CHAR(created, {}) AS \"Created\", oracle_maintained AS \"Oracle Maintained\" FROM dba_users ORDER BY username",
                    TS
                ),
                &[],
            )
            .or_else(|_| {
                self.run(
                    &format!("SELECT username, user_id AS \"User ID\", TO_CHAR(created, {}) AS \"Created\" FROM all_users ORDER BY username", TS),
                    &[],
                )
            })?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: r.columns.iter().enumerate().skip(1).map(|(k, c)| (c.name.clone(), r.text(i, k))).collect(),
            })
            .collect())
    }

    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        if spec.password.contains('"') {
            bail!("Oracle passwords cannot contain double quotes");
        }
        let who = D.quote_ident(&spec.name);
        let mut out = Vec::new();
        match old {
            None => {
                if spec.password.is_empty() {
                    out.push(format!("CREATE USER {} NO AUTHENTICATION", who));
                } else {
                    out.push(format!("CREATE USER {} IDENTIFIED BY \"{}\"", who, spec.password));
                }
            }
            Some(o) => {
                if o.name != spec.name {
                    bail!("Oracle users cannot be renamed");
                }
                if !spec.password.is_empty() {
                    out.push(format!("ALTER USER {} IDENTIFIED BY \"{}\"", who, spec.password));
                }
            }
        }
        if spec.can_login {
            out.push(format!("GRANT CREATE SESSION TO {}", who));
        }
        if spec.superuser {
            out.push(format!("GRANT DBA TO {}", who));
        }
        if spec.create_role {
            out.push(format!("GRANT CREATE ROLE TO {}", who));
        }
        for (privilege, obj) in &spec.grants {
            if privilege.is_empty() {
                continue;
            }
            if obj.is_empty() {
                out.push(format!("GRANT {} TO {}", privilege, who));
            } else {
                out.push(format!("GRANT {} ON {} TO {}", privilege, obj, who));
            }
        }
        Ok(out)
    }

    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        let n = user.name.clone();
        let q = |sql: &str| self.run(sql, &[&n]).map(|r| (0..r.rows.len()).map(|i| r.text(i, 0)).collect::<Vec<_>>());
        let mut out = q("SELECT 'GRANT ' || privilege || ' TO \"' || grantee || '\"' FROM dba_sys_privs WHERE grantee = :1 ORDER BY 1")
            .unwrap_or_default();
        out.extend(
            q("SELECT 'GRANT \"' || granted_role || '\" TO \"' || grantee || '\"' FROM dba_role_privs WHERE grantee = :1 ORDER BY 1")
                .unwrap_or_default(),
        );
        out.extend(
            q("SELECT 'GRANT ' || privilege || ' ON \"' || table_schema || '\".\"' || table_name || '\" TO \"' || grantee || '\"' \
               FROM all_tab_privs WHERE grantee = :1 ORDER BY 1")
            .unwrap_or_default(),
        );
        Ok(out)
    }

    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        Ok(format!("DROP USER {} CASCADE", D.quote_ident(&user.name)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let soft = |me: &Self, sql: &str| me.run(sql, &[]).unwrap_or_else(|e| soft_error(sql, e));
        Ok(ServerSnapshot {
            processes: soft(
                self,
                &format!(
                    "SELECT s.sid || ',' || s.serial# AS \"ID\", s.username AS \"USER\", s.status AS \"STATE\", s.schemaname AS \"SCHEMA\", \
                     s.osuser, s.machine, s.program, TO_CHAR(s.logon_time, {}) AS logon_time, s.last_call_et AS \"TIME\", s.event, s.sql_id \
                     FROM v$session s WHERE s.type = 'USER' ORDER BY s.sid",
                    TS
                ),
            ),
            variables: soft(self, "SELECT name, value, description FROM v$parameter ORDER BY name"),
            status: soft(
                self,
                "SELECT name, value FROM v$sysstat WHERE name IN ('logons current', 'logons cumulative', 'opened cursors current', \
                 'user commits', 'user rollbacks', 'user calls', 'execute count', 'parse count (total)', 'parse count (hard)', \
                 'session logical reads', 'physical reads', 'physical writes', 'redo size', 'DB time', 'CPU used by this session', \
                 'bytes sent via SQL*Net to client', 'bytes received via SQL*Net from client', 'sorts (memory)', 'sorts (disk)', \
                 'table scans (long tables)') ORDER BY name",
            ),
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let parts: Vec<&str> = id.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
            bail!("invalid session id '{}' (expected 'sid,serial#')", id);
        }
        self.run(&format!("ALTER SYSTEM KILL SESSION '{},{}' IMMEDIATE", parts[0], parts[1]), &[])
            .context("killing session")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sql_and_plsql() {
        let script = "SELECT 1 FROM dual;\n\
            -- comment; with semicolon\n\
            INSERT INTO t VALUES ('a;b', q'[it's; ok]');\n\
            CREATE OR REPLACE FUNCTION f(x NUMBER) RETURN NUMBER IS\nBEGIN\n  RETURN x + 1;\nEND;\n/\n\
            BEGIN\n  NULL;\nEND;\n/\n\
            create or replace editionable trigger trg before insert on t for each row\nbegin\n  :new.id := 1;\nend;\n  /  \n\
            UPDATE t SET a = 1 / 2\n/\n\
            SELECT \"a;b\" FROM t";
        let v = split_script(script);
        assert_eq!(v.len(), 7, "{:#?}", v);
        assert_eq!(v[0], "SELECT 1 FROM dual");
        assert_eq!(v[1], "-- comment; with semicolon\nINSERT INTO t VALUES ('a;b', q'[it's; ok]')");
        assert!(v[2].starts_with("CREATE OR REPLACE FUNCTION") && v[2].ends_with("END;"), "{}", v[2]);
        assert!(v[2].contains("RETURN x + 1;"));
        assert_eq!(v[3], "BEGIN\n  NULL;\nEND;");
        assert!(v[4].starts_with("create or replace editionable trigger") && v[4].ends_with("end;"));
        assert_eq!(v[5], "UPDATE t SET a = 1 / 2");
        assert_eq!(v[6], "SELECT \"a;b\" FROM t");
    }

    #[test]
    fn splits_misc() {
        assert_eq!(split_script("select 1 from dual;\n/\n"), vec!["select 1 from dual"]);
        assert!(split_script("  -- only a comment\n").is_empty());
        assert_eq!(split_script("DECLARE x NUMBER; BEGIN x := 1; END;"), vec!["DECLARE x NUMBER; BEGIN x := 1; END;"]);
        assert_eq!(split_script("select nq'{a;}' from dual; select 2 from dual"), vec!["select nq'{a;}' from dual", "select 2 from dual"]);
        assert_eq!(split_script("select 'it''s;' x from dual;"), vec!["select 'it''s;' x from dual"]);
        assert_eq!(split_script("/* a; */ select 1 from dual"), vec!["/* a; */ select 1 from dual"]);
        assert!(is_plsql("CREATE TYPE pt AS OBJECT (x NUMBER);"));
        assert!(is_plsql("create or replace package body p as"));
        assert!(!is_plsql("CREATE TABLE t (x NUMBER)"));
        assert!(!is_plsql("CREATE OR REPLACE VIEW v AS SELECT 1 x FROM dual"));
    }

    #[test]
    fn multi_insert_rewrite() {
        let p = ObjPath::new("HR", None, "T");
        let sql = crate::dml::insert_rows(
            D,
            &p,
            &["A".into(), "B".into()],
            &[vec![Value::Text("x),('y".into()), Value::Int(1)], vec![Value::Null, Value::Float(2.5)]],
        );
        assert_eq!(
            rewrite_multi_insert(&sql).unwrap(),
            "INSERT INTO \"HR\".\"T\" (\"A\", \"B\")\nSELECT 'x),(''y', 1 FROM DUAL\nUNION ALL SELECT NULL, 2.5 FROM DUAL"
        );
        let single = crate::dml::insert_row(D, &p, &["A".into()], &[Value::Int(1)]);
        assert!(rewrite_multi_insert(&single).is_none());
        assert!(rewrite_multi_insert("INSERT INTO t SELECT * FROM u").is_none());
        assert!(rewrite_multi_insert("UPDATE t SET a = 1").is_none());
        assert!(rewrite_multi_insert("INSERT INTO t VALUES (1), (2) RETURNING x INTO :y").is_none());
    }

    #[test]
    fn types_and_numbers() {
        assert_eq!(col_type("VARCHAR2", Some(50), Some(50), "B", None, None), "VARCHAR2(50)");
        assert_eq!(col_type("VARCHAR2", Some(200), Some(50), "C", None, None), "VARCHAR2(50 CHAR)");
        assert_eq!(col_type("NVARCHAR2", Some(100), Some(50), "C", None, None), "NVARCHAR2(50)");
        assert_eq!(col_type("NUMBER", Some(22), None, "", Some(10), Some(2)), "NUMBER(10,2)");
        assert_eq!(col_type("NUMBER", Some(22), None, "", Some(10), Some(0)), "NUMBER(10)");
        assert_eq!(col_type("NUMBER", Some(22), None, "", None, Some(0)), "INTEGER");
        assert_eq!(col_type("NUMBER", Some(22), None, "", None, None), "NUMBER");
        assert_eq!(col_type("FLOAT", Some(22), None, "", Some(126), None), "FLOAT");
        assert_eq!(col_type("RAW", Some(16), None, "", None, None), "RAW(16)");
        assert_eq!(col_type("TIMESTAMP(6)", Some(11), None, "", None, Some(6)), "TIMESTAMP(6)");
        assert_eq!(number_value("42", 0), Value::Int(42));
        assert_eq!(number_value("42", -127), Value::Int(42));
        assert_eq!(number_value("1.50", 2), Value::Text("1.50".into()));
        assert_eq!(number_value(".5", -127), Value::Float(0.5));
        assert_eq!(number_value("-.5", 2), Value::Text("-0.5".into()));
        assert_eq!(number_value("123456789012345678901234567890", 0), Value::Text("123456789012345678901234567890".into()));
        assert!(not_null_check("\"ID\" IS NOT NULL"));
        assert!(!not_null_check("price > 0"));
    }

    #[test]
    fn connect_strings() {
        let mut cfg = ConnectionConfig { host: "db".into(), port: 1522, ..ConnectionConfig::new(DbKind::Oracle) };
        assert_eq!(connect_string(&cfg), "//db:1522/FREEPDB1");
        cfg.database = "ORCLPDB".into();
        assert_eq!(connect_string(&cfg), "//db:1522/ORCLPDB");
        cfg.options.insert("service_name".into(), "XEPDB1".into());
        assert_eq!(connect_string(&cfg), "//db:1522/XEPDB1");
        cfg.options.insert("sid".into(), "XE".into());
        cfg.connect_timeout_secs = 0;
        assert_eq!(
            connect_string(&cfg),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db)(PORT=1522))(CONNECT_DATA=(SID=XE)))"
        );
        cfg.options.insert("connect_string".into(), "MYTNS".into());
        assert_eq!(connect_string(&cfg), "MYTNS");
        assert!(privilege("sysdba").unwrap().is_some());
        assert!(privilege("").unwrap().is_none());
        assert!(privilege("root").is_err());
    }
}
