//! Microsoft SQL Server driver (tiberius over a driver-owned tokio runtime).

use anyhow::{anyhow, bail, Context, Result};
use futures_util::TryStreamExt;
use std::time::Duration;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::SqlServer;
const ROWCOUNT_COL: &str = "__navdb_rowcount";
const SEP: char = '\u{1f}';

type Conn = Client<Compat<TcpStream>>;

fn lit(s: &str) -> String {
    format!("N{}", D.string_literal(s))
}

fn server_err(e: tiberius::error::Error) -> anyhow::Error {
    match e {
        tiberius::error::Error::Server(t) => anyhow!("{} (error {}, line {})", t.message(), t.code(), t.line()),
        e => anyhow!(e),
    }
}

pub struct MsSqlDriver {
    cfg: ConnectionConfig,
    rt: Runtime,
    client: Conn,
    current_db: String,
}

struct ResultSet {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
}

async fn connect_once(config: Config) -> tiberius::Result<Conn> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

async fn connect_routed(mut config: Config) -> tiberius::Result<Conn> {
    match connect_once(config.clone()).await {
        Err(tiberius::error::Error::Routing { host, port }) => {
            config.host(&host);
            config.port(port);
            connect_once(config).await
        }
        r => r,
    }
}

fn build_config(cfg: &ConnectionConfig, port: u16, enc: EncryptionLevel) -> Result<Config> {
    let mut c = Config::new();
    c.host(&cfg.host);
    c.port(port);
    c.authentication(AuthMethod::sql_server(&cfg.user, &cfg.password));
    c.application_name("NavDB Studio");
    if !cfg.database.is_empty() {
        c.database(&cfg.database);
    }
    c.encryption(enc);
    let trust = !cfg.option("trust_cert").eq_ignore_ascii_case("false") && !(cfg.ssl.enabled && cfg.ssl.verify_server);
    if cfg.ssl.enabled && !cfg.ssl.ca_cert.is_empty() {
        c.trust_cert_ca(&cfg.ssl.ca_cert);
    } else if trust {
        c.trust_cert();
    }
    Ok(c)
}

/// Resolve a named instance's TCP port through the SQL Server Browser (UDP 1434).
fn browse_instance(host: &str, instance: &str, timeout: Duration) -> Result<u16> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    let mut msg = vec![0x04u8];
    msg.extend_from_slice(instance.as_bytes());
    sock.send_to(&msg, (host, 1434)).context("contacting SQL Server Browser")?;
    let mut buf = [0u8; 4096];
    let n = sock.recv(&mut buf).context("no answer from SQL Server Browser (UDP 1434)")?;
    let text = String::from_utf8_lossy(buf.get(3..n).unwrap_or_default()).to_string();
    browser_port(&text, instance).ok_or_else(|| anyhow!("instance {} not found or has no TCP port", instance))
}

fn browser_port(resp: &str, instance: &str) -> Option<u16> {
    for entry in resp.split(";;") {
        let parts: Vec<&str> = entry.split(';').collect();
        let get = |k: &str| parts.chunks(2).find(|p| p[0].eq_ignore_ascii_case(k)).and_then(|p| p.get(1)).copied();
        if get("InstanceName").map(|n| n.eq_ignore_ascii_case(instance)).unwrap_or(false) {
            return get("tcp").and_then(|p| p.parse().ok());
        }
    }
    None
}

impl MsSqlDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let instance = cfg.option("instance").trim();
        let port = if instance.is_empty() { cfg.port } else { browse_instance(&cfg.host, instance, timeout)? };
        let attempt = |enc: EncryptionLevel| -> Result<Conn> {
            let c = build_config(cfg, port, enc)?;
            match rt.block_on(async { tokio::time::timeout(timeout, connect_routed(c)).await }) {
                Ok(r) => r.map_err(server_err),
                Err(_) => bail!("connection timed out after {}s", timeout.as_secs()),
            }
        };
        let client = if cfg.ssl.enabled {
            attempt(EncryptionLevel::Required)
        } else {
            // plain connection when the server allows it, otherwise login-only encryption
            attempt(EncryptionLevel::NotSupported).or_else(|e| attempt(EncryptionLevel::Off).map_err(|_| e))
        }
        .with_context(|| format!("connecting to {}:{}", cfg.host, port))?;
        let mut d = Self { cfg: cfg.clone(), rt, client, current_db: String::new() };
        d.refresh_db()?;
        Ok(d)
    }

    fn run_raw(&mut self, sql: &str) -> Result<Vec<ResultSet>> {
        let client = &mut self.client;
        self.rt
            .block_on(async {
                let mut stream = client.simple_query(sql).await?;
                let mut sets: Vec<ResultSet> = Vec::new();
                while let Some(item) = stream.try_next().await? {
                    if let Some(m) = item.as_metadata() {
                        let columns = m
                            .columns()
                            .iter()
                            .map(|c| ColumnMeta { name: c.name().to_string(), type_name: type_name(c.column_type()).into() })
                            .collect();
                        sets.push(ResultSet { columns, rows: vec![] });
                    } else if let Some(row) = item.into_row() {
                        let vals = row.cells().map(|(_, d)| to_value(d)).collect();
                        match sets.last_mut() {
                            Some(s) => s.rows.push(vals),
                            None => sets.push(ResultSet { columns: vec![], rows: vec![vals] }),
                        }
                    }
                }
                Ok::<_, tiberius::error::Error>(sets)
            })
            .map_err(server_err)
    }

    fn refresh_db(&mut self) -> Result<()> {
        let r = self.run_raw("SELECT DB_NAME()")?;
        self.current_db = r.first().and_then(|s| s.rows.first()).and_then(|r| r.first()).map(|v| v.to_string()).unwrap_or_default();
        Ok(())
    }

    fn use_db(&mut self, db: &str) -> Result<()> {
        if !db.is_empty() && db != self.current_db {
            self.run_raw(&format!("USE {}", D.quote_ident(db)))?;
            self.current_db = db.to_string();
        }
        Ok(())
    }

    /// First result set of a catalog query run in `db`.
    fn rows_in(&mut self, db: &str, sql: &str) -> Result<QueryResult> {
        self.use_db(db)?;
        let set = self.run_raw(sql)?.into_iter().next();
        Ok(match set {
            Some(s) => QueryResult { sql: sql.into(), columns: s.columns, rows: s.rows, is_query: true, ..Default::default() },
            None => QueryResult { sql: sql.into(), is_query: true, ..Default::default() },
        })
    }

    /// Run one statement (or a whole batch); returns one result per result set,
    /// or a single non-query result carrying the affected row count.
    fn run_unit(&mut self, text: &str, count_rows: bool) -> Result<Vec<QueryResult>> {
        let start = std::time::Instant::now();
        let sql = if count_rows { format!("{}\n;SELECT @@ROWCOUNT AS [{}]", text, ROWCOUNT_COL) } else { text.to_string() };
        let mut sets = self.run_raw(&sql)?;
        let mut affected = 0u64;
        if count_rows {
            if let Some(last) = sets.last() {
                if last.columns.len() == 1 && last.columns[0].name == ROWCOUNT_COL {
                    affected = last.rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
                    sets.pop();
                }
            }
        }
        let elapsed = start.elapsed().as_millis();
        if sets.is_empty() {
            return Ok(vec![QueryResult { sql: text.into(), affected_rows: affected, elapsed_ms: elapsed, ..Default::default() }]);
        }
        Ok(sets
            .into_iter()
            .map(|s| QueryResult {
                sql: text.into(),
                columns: s.columns,
                rows: s.rows,
                is_query: true,
                elapsed_ms: elapsed,
                ..Default::default()
            })
            .collect())
    }

    fn schema_of(p: &ObjPath) -> &str {
        p.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo")
    }

    fn object_id(p: &ObjPath) -> String {
        format!("OBJECT_ID({})", lit(&format!("{}.{}", D.quote_ident(Self::schema_of(p)), D.quote_ident(&p.name))))
    }

    fn short_name(p: &ObjPath) -> String {
        format!("{}.{}", D.quote_ident(Self::schema_of(p)), D.quote_ident(&p.name))
    }
}

fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln | ColumnType::Numericn => "decimal",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigBinary => "binary",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

fn numeric_text(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{}.{}", if value < 0 { "-" } else { "" }, int, frac)
}

fn chrono_text<'a, T: FromSql<'a>>(d: &'a ColumnData<'static>, f: impl Fn(T) -> String) -> Value {
    match T::from_sql(d) {
        Ok(Some(v)) => Value::Text(f(v)),
        Ok(None) => Value::Null,
        Err(e) => Value::Text(format!("<{}>", e)),
    }
}

fn to_value(d: &ColumnData<'static>) -> Value {
    use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
    match d {
        ColumnData::U8(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I16(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I32(v) => v.map(|x| Value::Int(x as i64)).unwrap_or(Value::Null),
        ColumnData::I64(v) => v.map(Value::Int).unwrap_or(Value::Null),
        ColumnData::F32(v) => v.map(|x| Value::Float(x.to_string().parse().unwrap_or(x as f64))).unwrap_or(Value::Null),
        ColumnData::F64(v) => v.map(Value::Float).unwrap_or(Value::Null),
        ColumnData::Bit(v) => v.map(Value::Bool).unwrap_or(Value::Null),
        ColumnData::String(v) => v.as_ref().map(|s| Value::Text(s.to_string())).unwrap_or(Value::Null),
        ColumnData::Guid(v) => v.map(|g| Value::Text(g.to_string().to_uppercase())).unwrap_or(Value::Null),
        ColumnData::Binary(v) => v.as_ref().map(|b| Value::Bytes(b.to_vec())).unwrap_or(Value::Null),
        ColumnData::Numeric(v) => v.map(|n| Value::Text(numeric_text(n.value(), n.scale()))).unwrap_or(Value::Null),
        ColumnData::Xml(v) => v.as_ref().map(|x| Value::Text(x.to_string())).unwrap_or(Value::Null),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            chrono_text(d, |v: NaiveDateTime| v.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        ColumnData::Date(_) => chrono_text(d, |v: NaiveDate| v.format("%Y-%m-%d").to_string()),
        ColumnData::Time(_) => chrono_text(d, |v: NaiveTime| v.format("%H:%M:%S%.f").to_string()),
        ColumnData::DateTimeOffset(_) => {
            chrono_text(d, |v: chrono::DateTime<chrono::FixedOffset>| v.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())
        }
    }
}

/// `nvarchar(50)`, `decimal(10,2)`, `varchar(max)`...
fn type_text(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let len = |n: i64| if max_length < 0 { "max".to_string() } else { n.to_string() };
    match name {
        "varchar" | "char" | "varbinary" | "binary" => format!("{}({})", name, len(max_length)),
        "nvarchar" | "nchar" => format!("{}({})", name, len(max_length / 2)),
        "decimal" | "numeric" => format!("{}({},{})", name, precision, scale),
        "datetime2" | "time" | "datetimeoffset" if scale != 7 => format!("{}({})", name, scale),
        "float" if precision != 53 => format!("float({})", precision),
        _ => name.to_string(),
    }
}

/// `((0))` -> `0`, `(N'x')` -> `N'x'`; only strips parentheses enclosing the whole text.
fn strip_parens(s: &str) -> String {
    let mut t = s.trim();
    while t.starts_with('(') && t.ends_with(')') && encloses(t) {
        t = t[1..t.len() - 1].trim();
    }
    t.to_string()
}

fn encloses(t: &str) -> bool {
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in t.char_indices() {
        match c {
            '\'' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 && i != t.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Split a script on `GO` lines (optionally `GO n` to repeat a batch).
fn split_batches(sql: &str) -> Vec<String> {
    let go = regex::Regex::new(r"(?i)^\s*GO(?:\s+(\d+))?\s*$").expect("valid regex");
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_str, mut in_ident, mut block) = (None::<char>, false, 0usize);
    for line in sql.split_inclusive('\n') {
        if in_str.is_none() && !in_ident && block == 0 {
            if let Some(m) = go.captures(line.trim_end_matches(['\r', '\n'])) {
                let n = m.get(1).and_then(|x| x.as_str().parse::<usize>().ok()).unwrap_or(1).max(1);
                if !cur.trim().is_empty() {
                    for _ in 0..n {
                        out.push(cur.trim().to_string());
                    }
                }
                cur.clear();
                continue;
            }
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if let Some(q) = in_str {
                if c == q {
                    in_str = None;
                }
            } else if in_ident {
                if c == ']' {
                    in_ident = false;
                }
            } else if block > 0 {
                if c == '*' && next == Some('/') {
                    block -= 1;
                    i += 1;
                } else if c == '/' && next == Some('*') {
                    block += 1;
                    i += 1;
                }
            } else if c == '-' && next == Some('-') {
                break;
            } else if c == '/' && next == Some('*') {
                block += 1;
                i += 1;
            } else if c == '\'' || c == '"' {
                in_str = Some(c);
            } else if c == '[' {
                in_ident = true;
            }
            i += 1;
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Upper-cased words outside strings, quoted identifiers and comments.
fn words(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut w = String::new();
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let flush = |w: &mut String, out: &mut Vec<String>| {
        if !w.is_empty() {
            out.push(w.to_ascii_uppercase());
            w.clear();
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let skip_to = |i: usize, end: &str| -> usize {
            let rest: String = chars[i..].iter().collect();
            match rest.find(end) {
                Some(p) => i + rest[..p].chars().count() + end.chars().count(),
                None => chars.len(),
            }
        };
        if c == '-' && next == Some('-') {
            flush(&mut w, &mut out);
            i = skip_to(i, "\n");
        } else if c == '/' && next == Some('*') {
            flush(&mut w, &mut out);
            i = skip_to(i + 2, "*/");
        } else if c == '\'' || c == '"' || c == '[' {
            flush(&mut w, &mut out);
            let end = if c == '[' { ']' } else { c };
            i += 1;
            while i < chars.len() && chars[i] != end {
                i += 1;
            }
            i += 1;
            out.push("\u{0}".into());
        } else if c.is_alphanumeric() || c == '_' || c == '@' || c == '#' {
            w.push(c);
            i += 1;
        } else {
            flush(&mut w, &mut out);
            if c == '(' || c == ')' {
                out.push(c.to_string());
            }
            i += 1;
        }
    }
    flush(&mut w, &mut out);
    out
}

/// Batches defining a module (view, routine, trigger, schema) must be sent alone.
fn is_module_batch(w: &[String]) -> bool {
    let mut it = w.iter().map(|s| s.as_str());
    match it.next() {
        Some("CREATE") | Some("ALTER") => {}
        _ => return false,
    }
    let mut next = it.next();
    if next == Some("OR") {
        it.next();
        next = it.next();
    }
    matches!(next, Some("PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER" | "VIEW" | "SCHEMA"))
}

/// Batches with variables or control flow are executed as a whole.
fn is_procedural(w: &[String]) -> bool {
    w.iter().any(|x| {
        matches!(x.as_str(), "DECLARE" | "BEGIN" | "IF" | "WHILE" | "GOTO" | "RETURN")
            || (x.starts_with('@') && !x.starts_with("@@"))
    })
}

/// T-SQL accepts `OFFSET .. FETCH` only after `ORDER BY`: add a neutral one when missing.
fn fix_offset(stmt: &str) -> String {
    let re = regex::Regex::new(r"(?is)\s+OFFSET\s+\d+\s+ROWS?\s+FETCH\s+(?:NEXT|FIRST)\s+\d+\s+ROWS?\s+ONLY\s*;?\s*$").expect("valid regex");
    let Some(m) = re.find(stmt) else { return stmt.to_string() };
    let head = &stmt[..m.start()];
    let w = words(head);
    let mut depth = 0i32;
    let mut has_order = false;
    for (i, x) in w.iter().enumerate() {
        match x.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "ORDER" if depth == 0 && w.get(i + 1).map(|s| s == "BY").unwrap_or(false) => has_order = true,
            _ => {}
        }
    }
    if has_order {
        stmt.to_string()
    } else {
        format!("{} ORDER BY (SELECT NULL){}", head, &stmt[m.start()..])
    }
}

fn first_word(stmt: &str) -> String {
    words(stmt).into_iter().next().unwrap_or_default()
}

fn fk_action(s: &str) -> String {
    s.replace('_', " ")
}

impl Driver for MsSqlDriver {
    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.run_raw("SELECT @@VERSION")?;
        let v = r.first().and_then(|s| s.rows.first()).and_then(|r| r.first()).map(|v| v.to_string()).unwrap_or_default();
        Ok(v.lines().next().unwrap_or("").trim().to_string())
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.rows_in("", "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name")?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn list_schemas(&mut self, db: &str) -> Result<Vec<String>> {
        let r = self.rows_in(
            db,
            "SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name NOT IN ('guest', 'INFORMATION_SCHEMA', 'sys') ORDER BY name",
        )?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Function, ObjectKind::Procedure, ObjectKind::Trigger, ObjectKind::Sequence]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let schema = lit(scope.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo"));
        let desc = "COALESCE((SELECT CAST(ep.value AS nvarchar(4000)) FROM sys.extended_properties ep \
                    WHERE ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description'), '')";
        let dates = "CONVERT(varchar(19), o.create_date, 120), CONVERT(varchar(19), o.modify_date, 120)";
        let (sql, labels): (String, [&str; 2]) = match kind {
            ObjectKind::Table => (
                format!(
                    "SELECT o.name, (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)), \
                     (SELECT SUM(a.total_pages) * 8192 FROM sys.partitions p JOIN sys.allocation_units a ON a.container_id = p.partition_id WHERE p.object_id = o.object_id), \
                     {dates}, {desc}, USER_NAME(OBJECTPROPERTY(o.object_id, 'OwnerId')), \
                     (SELECT TOP 1 ds.name FROM sys.indexes i JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id WHERE i.object_id = o.object_id AND i.index_id IN (0, 1)) \
                     FROM sys.tables o WHERE o.schema_id = SCHEMA_ID({schema}) ORDER BY o.name"
                ),
                ["Owner", "Filegroup"],
            ),
            ObjectKind::View => (
                format!(
                    "SELECT o.name, NULL, NULL, {dates}, {desc}, USER_NAME(OBJECTPROPERTY(o.object_id, 'OwnerId')), \
                     CASE WHEN OBJECTPROPERTY(o.object_id, 'IsSchemaBound') = 1 THEN 'yes' ELSE 'no' END \
                     FROM sys.views o WHERE o.schema_id = SCHEMA_ID({schema}) ORDER BY o.name"
                ),
                ["Owner", "Schema Bound"],
            ),
            ObjectKind::Function | ObjectKind::Procedure => (
                format!(
                    "SELECT o.name, NULL, NULL, {dates}, {desc}, USER_NAME(OBJECTPROPERTY(o.object_id, 'OwnerId')), o.type_desc \
                     FROM sys.objects o WHERE o.schema_id = SCHEMA_ID({schema}) AND o.type IN ({}) ORDER BY o.name",
                    if kind == ObjectKind::Function { "'FN', 'IF', 'TF', 'FS', 'FT', 'AF'" } else { "'P', 'PC', 'X'" }
                ),
                ["Owner", "Type"],
            ),
            ObjectKind::Trigger => (
                format!(
                    "SELECT o.name, NULL, NULL, {dates}, {desc}, OBJECT_NAME(o.parent_id), CASE WHEN o.is_disabled = 1 THEN 'disabled' ELSE 'enabled' END \
                     FROM sys.triggers o WHERE o.parent_class = 1 AND OBJECTPROPERTY(o.parent_id, 'SchemaId') = SCHEMA_ID({schema}) ORDER BY o.name"
                ),
                ["Table", "Status"],
            ),
            ObjectKind::Sequence => (
                format!(
                    "SELECT o.name, NULL, NULL, {dates}, {desc}, TYPE_NAME(o.user_type_id), CAST(o.current_value AS nvarchar(40)) \
                     FROM sys.sequences o WHERE o.schema_id = SCHEMA_ID({schema}) ORDER BY o.name"
                ),
                ["Type", "Current Value"],
            ),
            _ => return Ok(vec![]),
        };
        let r = self.rows_in(&scope.db, &sql)?;
        Ok((0..r.rows.len())
            .map(|i| ObjectInfo {
                name: r.text(i, 0),
                rows: r.rows[i][1].as_f64().map(|v| v as i64),
                data_length: r.rows[i][2].as_f64().map(|v| v as i64),
                created: r.text(i, 3),
                modified: r.text(i, 4),
                comment: r.text(i, 5),
                extra: vec![(labels[0].into(), r.text(i, 6)), (labels[1].into(), r.text(i, 7))],
                ..Default::default()
            })
            .collect())
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let oid = Self::object_id(path);
        let r = self.rows_in(
            &path.db,
            &format!(
                "SELECT c.name, TYPE_NAME(c.user_type_id), c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, \
                 OBJECT_DEFINITION(c.default_object_id), COALESCE(c.collation_name, ''), \
                 COALESCE((SELECT CAST(ep.value AS nvarchar(4000)) FROM sys.extended_properties ep WHERE ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = 'MS_Description'), ''), \
                 COALESCE(cc.definition, '') \
                 FROM sys.columns c LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 WHERE c.object_id = {oid} ORDER BY c.column_id"
            ),
        )?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let num = |i: usize, c: usize| r.rows[i][c].as_f64().unwrap_or(0.0) as i64;
        let mut columns: Vec<ColumnDef> = (0..r.rows.len())
            .map(|i| {
                let computed = r.text(i, 10);
                ColumnDef {
                    name: r.text(i, 0),
                    data_type: type_text(&r.text(i, 1), num(i, 2), num(i, 3), num(i, 4)),
                    nullable: r.rows[i][5] == Value::Bool(true),
                    default: match &r.rows[i][7] {
                        Value::Null => None,
                        v => Some(strip_parens(&v.to_string())),
                    },
                    auto_increment: r.rows[i][6] == Value::Bool(true),
                    primary_key: false,
                    collation: r.text(i, 8),
                    comment: r.text(i, 9),
                    extra: if computed.is_empty() { String::new() } else { format!("AS {}", computed) },
                }
            })
            .collect();
        let ir = self.rows_in(
            &path.db,
            &format!(
                "SELECT i.name, i.is_unique, i.is_primary_key, i.type_desc, \
                 STRING_AGG(CAST(c.name AS nvarchar(max)), NCHAR(31)) WITHIN GROUP (ORDER BY ic.key_ordinal) \
                 FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0 \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = {oid} AND i.type > 0 AND i.is_hypothetical = 0 \
                 GROUP BY i.name, i.is_unique, i.is_primary_key, i.type_desc ORDER BY i.is_primary_key DESC, i.name"
            ),
        )?;
        let indexes: Vec<IndexDef> = (0..ir.rows.len())
            .map(|i| {
                let method = ir.text(i, 3);
                IndexDef {
                    name: ir.text(i, 0),
                    unique: ir.rows[i][1] == Value::Bool(true),
                    primary: ir.rows[i][2] == Value::Bool(true),
                    method: if method == "NONCLUSTERED" { String::new() } else { method },
                    columns: ir.text(i, 4).split(SEP).map(String::from).collect(),
                    comment: String::new(),
                }
            })
            .collect();
        let pk: Vec<String> = indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default();
        for c in columns.iter_mut() {
            c.primary_key = pk.contains(&c.name);
        }
        let fr = self.rows_in(
            &path.db,
            &format!(
                "SELECT fk.name, STRING_AGG(CAST(pc.name AS nvarchar(max)), NCHAR(31)) WITHIN GROUP (ORDER BY fkc.constraint_column_id), \
                 OBJECT_SCHEMA_NAME(fk.referenced_object_id), OBJECT_NAME(fk.referenced_object_id), \
                 STRING_AGG(CAST(rc.name AS nvarchar(max)), NCHAR(31)) WITHIN GROUP (ORDER BY fkc.constraint_column_id), \
                 fk.update_referential_action_desc, fk.delete_referential_action_desc \
                 FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 WHERE fk.parent_object_id = {oid} \
                 GROUP BY fk.name, fk.referenced_object_id, fk.update_referential_action_desc, fk.delete_referential_action_desc ORDER BY fk.name"
            ),
        )?;
        let foreign_keys = (0..fr.rows.len())
            .map(|i| ForeignKeyDef {
                name: fr.text(i, 0),
                columns: fr.text(i, 1).split(SEP).map(String::from).collect(),
                ref_db: path.db.clone(),
                ref_schema: Some(fr.text(i, 2)),
                ref_table: fr.text(i, 3),
                ref_columns: fr.text(i, 4).split(SEP).map(String::from).collect(),
                on_update: fk_action(&fr.text(i, 5)),
                on_delete: fk_action(&fr.text(i, 6)),
            })
            .collect();
        let cr = self.rows_in(
            &path.db,
            &format!("SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = {oid} ORDER BY name"),
        )?;
        let checks = (0..cr.rows.len()).map(|i| CheckDef { name: cr.text(i, 0), expression: strip_parens(&cr.text(i, 1)) }).collect();
        let tr = self.rows_in(
            &path.db,
            &format!(
                "SELECT t.name, OBJECT_DEFINITION(t.object_id), t.is_instead_of_trigger, \
                 (SELECT STRING_AGG(e.type_desc, ' OR ') FROM sys.trigger_events e WHERE e.object_id = t.object_id) \
                 FROM sys.triggers t WHERE t.parent_id = {oid} ORDER BY t.name"
            ),
        )?;
        let triggers = (0..tr.rows.len())
            .map(|i| TriggerDef {
                name: tr.text(i, 0),
                timing: if tr.rows[i][2] == Value::Bool(true) { "INSTEAD OF" } else { "AFTER" }.into(),
                event: tr.text(i, 3),
                body: tr.text(i, 1),
            })
            .collect();
        let comment = self
            .rows_in(
                &path.db,
                &format!(
                    "SELECT COALESCE((SELECT CAST(value AS nvarchar(4000)) FROM sys.extended_properties WHERE class = 1 AND major_id = {oid} AND minor_id = 0 AND name = 'MS_Description'), '')"
                ),
            )?
            .text(0, 0);
        Ok(TableDef { path: path.clone(), columns, indexes, foreign_keys, checks, triggers, comment, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        match kind {
            ObjectKind::Table => {
                let mut def = self.table_def(path)?;
                let triggers = std::mem::take(&mut def.triggers);
                let mut out = crate::ddl::script(&crate::ddl::create_table(D, &def));
                for t in triggers {
                    out.push_str(&format!("\nGO\n\n{}", t.body.trim()));
                }
                Ok(out)
            }
            ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger => {
                let r = self.rows_in(&path.db, &format!("SELECT OBJECT_DEFINITION({})", Self::object_id(path)))?;
                match r.rows.first().and_then(|r| r.first()) {
                    Some(Value::Text(s)) => Ok(s.trim().to_string()),
                    _ => bail!("definition of {} is not available (missing or encrypted)", path.display()),
                }
            }
            ObjectKind::Sequence => {
                let r = self.rows_in(
                    &path.db,
                    &format!(
                        "SELECT TYPE_NAME(user_type_id), CAST(start_value AS nvarchar(40)), CAST(increment AS nvarchar(40)), \
                         CAST(minimum_value AS nvarchar(40)), CAST(maximum_value AS nvarchar(40)), is_cycling, is_cached, cache_size \
                         FROM sys.sequences WHERE object_id = {}",
                        Self::object_id(path)
                    ),
                )?;
                if r.rows.is_empty() {
                    bail!("sequence {} not found", path.display());
                }
                let cache = if r.rows[0][6] != Value::Bool(true) {
                    "NO CACHE".to_string()
                } else if r.text(0, 7).is_empty() {
                    "CACHE".to_string()
                } else {
                    format!("CACHE {}", r.text(0, 7))
                };
                Ok(format!(
                    "CREATE SEQUENCE {}\n  AS {}\n  START WITH {}\n  INCREMENT BY {}\n  MINVALUE {}\n  MAXVALUE {}\n  {}\n  {};",
                    Self::short_name(path),
                    r.text(0, 0),
                    r.text(0, 1),
                    r.text(0, 2),
                    r.text(0, 3),
                    r.text(0, 4),
                    if r.rows[0][5] == Value::Bool(true) { "CYCLE" } else { "NO CYCLE" },
                    cache
                ))
            }
            _ => bail!("DDL not available for {:?}", kind),
        }
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope {
            self.use_db(&s.db)?;
        }
        let mut out = Vec::new();
        for batch in split_batches(sql) {
            let w = words(&batch);
            if is_module_batch(&w) {
                out.extend(self.run_unit(&batch, false)?);
            } else if is_procedural(&w) {
                out.extend(self.run_unit(&batch, true)?);
            } else {
                for stmt in crate::sqlutil::split_statements(&batch, D) {
                    let first = first_word(&stmt);
                    out.extend(self.run_unit(&fix_offset(&stmt), first != "SET")?);
                }
            }
            if w.iter().any(|x| x == "USE") {
                self.refresh_db()?;
            }
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        if let Some(s) = scope {
            self.use_db(&s.db)?;
        }
        let stmt = fix_offset(sql.trim().trim_end_matches(';'));
        let start = std::time::Instant::now();
        self.run_raw("SET SHOWPLAN_TEXT ON")?;
        let res = self.run_raw(&stmt);
        self.run_raw("SET SHOWPLAN_TEXT OFF")?;
        let sets = res?;
        let mut out = QueryResult { sql: stmt.clone(), is_query: true, ..Default::default() };
        // each statement yields its text, then the plan rows
        for s in sets.into_iter().skip(1).step_by(2) {
            if out.columns.is_empty() {
                out.columns = s.columns;
            }
            out.rows.extend(s.rows);
        }
        out.elapsed_ms = start.elapsed().as_millis();
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.rows_in(
            "",
            "SELECT name, type_desc, CASE WHEN is_disabled = 1 THEN 'yes' ELSE 'no' END, \
             CASE WHEN IS_SRVROLEMEMBER('sysadmin', name) = 1 THEN 'yes' ELSE 'no' END, COALESCE(default_database_name, ''), CONVERT(varchar(19), create_date, 120) \
             FROM sys.server_principals WHERE type IN ('S', 'U', 'G') AND name NOT LIKE '##%' ORDER BY name",
        )?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: vec![
                    ("Type".into(), r.text(i, 1)),
                    ("Disabled".into(), r.text(i, 2)),
                    ("Sysadmin".into(), r.text(i, 3)),
                    ("Default Database".into(), r.text(i, 4)),
                    ("Created".into(), r.text(i, 5)),
                ],
            })
            .collect())
    }

    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        let who = D.quote_ident(&spec.name);
        let mut out = Vec::new();
        match old {
            None => {
                if spec.password.is_empty() {
                    bail!("a password is required for a SQL Server login");
                }
                out.push(format!("CREATE LOGIN {} WITH PASSWORD = {}", who, lit(&spec.password)));
            }
            Some(o) => {
                if o.name != spec.name {
                    out.push(format!("ALTER LOGIN {} WITH NAME = {}", D.quote_ident(&o.name), who));
                }
                if !spec.password.is_empty() {
                    out.push(format!("ALTER LOGIN {} WITH PASSWORD = {}", who, lit(&spec.password)));
                }
            }
        }
        out.push(format!("ALTER LOGIN {} {}", who, if spec.can_login { "ENABLE" } else { "DISABLE" }));
        for (flag, role) in [(spec.superuser, "sysadmin"), (spec.create_db, "dbcreator"), (spec.create_role, "securityadmin")] {
            if flag {
                out.push(format!("ALTER SERVER ROLE {} ADD MEMBER {}", role, who));
            } else if old.is_some() {
                out.push(format!("IF IS_SRVROLEMEMBER('{}', {}) = 1 ALTER SERVER ROLE {} DROP MEMBER {}", role, lit(&spec.name), role, who));
            }
        }
        for (privilege, obj) in &spec.grants {
            if !privilege.is_empty() && !obj.is_empty() {
                out.push(format!("GRANT {} ON {} TO {}", privilege, obj, who));
            }
        }
        Ok(out)
    }

    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        Ok(format!("DROP LOGIN {}", D.quote_ident(&user.name)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.rows_in(
                "",
                "SELECT s.session_id AS id, s.login_name AS [user], s.host_name AS host, DB_NAME(s.database_id) AS db, s.status, \
                 r.command, r.wait_type, r.total_elapsed_time AS time_ms, s.program_name, t.text AS query \
                 FROM sys.dm_exec_sessions s LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
                 OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE s.is_user_process = 1 ORDER BY s.session_id",
            )?,
            variables: self.rows_in(
                "",
                "SELECT name, CAST(value_in_use AS nvarchar(100)) AS value, CAST(minimum AS nvarchar(100)) AS minimum, \
                 CAST(maximum AS nvarchar(100)) AS maximum, description FROM sys.configurations ORDER BY name",
            )?,
            status: self.rows_in(
                "",
                "SELECT RTRIM(counter_name) AS name, RTRIM(instance_name) AS instance, cntr_value AS value, RTRIM(object_name) AS object \
                 FROM sys.dm_os_performance_counters WHERE RTRIM(instance_name) IN ('', '_Total') AND RTRIM(counter_name) IN \
                 ('User Connections', 'Batch Requests/sec', 'SQL Compilations/sec', 'SQL Re-Compilations/sec', 'Page life expectancy', \
                 'Transactions/sec', 'Lock Waits/sec', 'Processes blocked', 'Target Server Memory (KB)', 'Total Server Memory (KB)', \
                 'Logins/sec', 'Number of Deadlocks/sec', 'Page reads/sec', 'Page writes/sec') ORDER BY 1",
            )?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let id: u32 = id.trim().parse().context("invalid session id")?;
        self.run_raw(&format!("KILL {}", id))?;
        Ok(())
    }

    fn collations(&mut self, _charset: &str) -> Result<Vec<String>> {
        let r = self.rows_in("", "SELECT name FROM sys.fn_helpcollations() ORDER BY name")?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn ping(&mut self) -> Result<()> {
        self.run_raw("SELECT 1").map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches() {
        let s = "SELECT 1\nGO\n-- GO in comment\nSELECT 'x\nGO\n'\ngo 2\n/* \nGO\n*/ SELECT 3\n";
        let b = split_batches(s);
        assert_eq!(b.len(), 4, "{:?}", b);
        assert_eq!(b[0], "SELECT 1");
        assert_eq!(b[1], b[2]);
        assert!(b[1].contains("'x\nGO\n'"));
        assert!(b[3].ends_with("SELECT 3"));
        assert_eq!(split_batches("SELECT 1;\r\nGO\r\nSELECT 2").len(), 2);
    }

    #[test]
    fn classification() {
        assert!(is_module_batch(&words("CREATE OR ALTER PROCEDURE p AS SELECT 1; SELECT 2")));
        assert!(is_module_batch(&words("/* c */ create view v as select 1")));
        assert!(!is_module_batch(&words("CREATE TABLE t (a int)")));
        assert!(is_procedural(&words("DECLARE @x int = 1; SELECT @x")));
        assert!(!is_procedural(&words("SELECT @@VERSION; SELECT 'DECLARE @x'")));
    }

    #[test]
    fn offset_rewrite() {
        let p = ObjPath::new("db", Some("dbo".into()), "t");
        let s = crate::dml::select_page(D, &p, "[a] = 1", &[], 10, 20);
        assert_eq!(fix_offset(&s), "SELECT * FROM [db].[dbo].[t] WHERE [a] = 1 ORDER BY (SELECT NULL) OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY");
        let sorted = crate::dml::select_page(D, &p, "", &[crate::dml::SortSpec { column: "a".into(), descending: true }], 10, 0);
        assert_eq!(fix_offset(&sorted), sorted);
        let sub = "SELECT * FROM (SELECT TOP 5 a FROM t ORDER BY a) x OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY";
        assert!(fix_offset(sub).contains("x ORDER BY (SELECT NULL) OFFSET"));
        assert_eq!(fix_offset("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn types_and_defaults() {
        assert_eq!(type_text("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(type_text("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(type_text("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(type_text("datetime2", 8, 27, 7), "datetime2");
        assert_eq!(type_text("datetime2", 7, 23, 3), "datetime2(3)");
        assert_eq!(type_text("int", 4, 10, 0), "int");
        assert_eq!(strip_parens("((0))"), "0");
        assert_eq!(strip_parens("(N'a(b')"), "N'a(b'");
        assert_eq!(strip_parens("(getdate())"), "getdate()");
        assert_eq!(strip_parens("([a]>(0) AND [b]<(5))"), "[a]>(0) AND [b]<(5)");
        assert_eq!(strip_parens("(1)+(2)"), "(1)+(2)");
    }

    #[test]
    fn numerics() {
        assert_eq!(numeric_text(150, 2), "1.50");
        assert_eq!(numeric_text(-5, 3), "-0.005");
        assert_eq!(numeric_text(42, 0), "42");
    }

    #[test]
    fn browser_response() {
        let r = "ServerName;HOST;InstanceName;SQLEXPRESS;IsClustered;No;Version;16.0.1000.6;tcp;49172;;ServerName;HOST;InstanceName;MSSQLSERVER;IsClustered;No;Version;16.0;tcp;1433;;";
        assert_eq!(browser_port(r, "sqlexpress"), Some(49172));
        assert_eq!(browser_port(r, "MSSQLSERVER"), Some(1433));
        assert_eq!(browser_port(r, "other"), None);
    }
}
