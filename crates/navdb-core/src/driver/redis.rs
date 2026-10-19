use anyhow::{bail, Context, Result};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

pub struct RedisDriver {
    conn: redis::Connection,
    current_db: i64,
    kind: DbKind,
}

/// Split a redis-cli style command line honouring single/double quotes.
pub fn parse_command(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        cur.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                } else if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    has = true;
                } else if c.is_whitespace() {
                    if has || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn db_index(name: &str) -> i64 {
    name.trim_start_matches("db").parse().unwrap_or(0)
}

fn to_text(v: &redis::Value) -> Value {
    use redis::Value as R;
    match v {
        R::Nil => Value::Null,
        R::Int(i) => Value::Int(*i),
        R::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.to_string()),
            Err(_) => Value::Bytes(b.clone()),
        },
        R::SimpleString(s) => Value::Text(s.clone()),
        R::Okay => Value::Text("OK".into()),
        R::Double(d) => Value::Float(*d),
        R::Boolean(b) => Value::Bool(*b),
        R::Array(items) | R::Set(items) => {
            Value::Text(items.iter().map(|i| to_text(i).display()).collect::<Vec<_>>().join("\n"))
        }
        other => Value::Text(format!("{:?}", other)),
    }
}

fn to_result(cmd: &str, v: redis::Value, elapsed: u128) -> QueryResult {
    use redis::Value as R;
    let mut r = QueryResult { sql: cmd.to_string(), is_query: true, elapsed_ms: elapsed, ..Default::default() };
    match v {
        R::Array(items) | R::Set(items) => {
            // HGETALL-like replies come as flat arrays; keep index/value layout
            r.columns = vec![ColumnMeta::new("#"), ColumnMeta::new("Value")];
            for (i, it) in items.iter().enumerate() {
                r.rows.push(vec![Value::Int(i as i64 + 1), to_text(it)]);
            }
        }
        R::Map(pairs) => {
            r.columns = vec![ColumnMeta::new("Field"), ColumnMeta::new("Value")];
            for (k, v) in pairs.iter() {
                r.rows.push(vec![to_text(k), to_text(v)]);
            }
        }
        other => {
            r.columns = vec![ColumnMeta::new("Result")];
            r.rows.push(vec![to_text(&other)]);
        }
    }
    r
}

impl RedisDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let db = db_index(&cfg.default_database());
        let auth = match (cfg.user.is_empty(), cfg.password.is_empty()) {
            (_, true) => String::new(),
            (true, false) => format!(":{}@", urlencode(&cfg.password)),
            (false, false) => format!("{}:{}@", urlencode(&cfg.user), urlencode(&cfg.password)),
        };
        let scheme = if cfg.ssl.enabled { "rediss" } else { "redis" };
        let url = format!("{}://{}{}:{}/{}", scheme, auth, cfg.host, cfg.port, db);
        let client = redis::Client::open(url)?;
        let conn = client
            .get_connection_with_timeout(std::time::Duration::from_secs(cfg.connect_timeout_secs.max(1)))
            .with_context(|| format!("connecting to redis {}:{}", cfg.host, cfg.port))?;
        Ok(Self { conn, current_db: db, kind: cfg.kind })
    }

    fn raw(&mut self, args: &[String]) -> Result<redis::Value> {
        if args.is_empty() {
            bail!("empty command");
        }
        let mut c = redis::cmd(&args[0]);
        for a in &args[1..] {
            c.arg(a);
        }
        Ok(c.query::<redis::Value>(&mut self.conn)?)
    }

    fn select(&mut self, db: i64) -> Result<()> {
        if db != self.current_db {
            self.raw(&["SELECT".into(), db.to_string()])?;
            self.current_db = db;
        }
        Ok(())
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

impl Driver for RedisDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let info = self.raw(&["INFO".into(), "server".into()])?;
        let text = to_text(&info).display();
        let ver = text
            .lines()
            .find_map(|l| l.strip_prefix("redis_version:"))
            .unwrap_or("unknown")
            .trim()
            .to_string();
        Ok(format!("Redis {}", ver))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let n = match self.raw(&["CONFIG".into(), "GET".into(), "databases".into()]) {
            Ok(v) => {
                let t = to_text(&v).display();
                t.lines().nth(1).and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(16)
            }
            Err(_) => 16,
        };
        Ok((0..n).map(|i| format!("db{}", i)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Key]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        if kind != ObjectKind::Key {
            return Ok(vec![]);
        }
        self.select(db_index(&scope.db))?;
        let mut cursor = "0".to_string();
        let mut out = Vec::new();
        loop {
            let v = self.raw(&["SCAN".into(), cursor.clone(), "COUNT".into(), "1000".into()])?;
            let redis::Value::Array(parts) = v else { break };
            if parts.len() != 2 {
                break;
            }
            cursor = to_text(&parts[0]).display();
            if let redis::Value::Array(keys) = &parts[1] {
                for k in keys {
                    out.push(ObjectInfo::named(to_text(k).display()));
                }
            }
            if cursor == "0" || out.len() >= 10_000 {
                break;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        bail!("{} is a Redis key, not a table", path.name)
    }

    fn object_ddl(&mut self, path: &ObjPath, _kind: ObjectKind) -> Result<String> {
        self.select(db_index(&path.db))?;
        let ty = to_text(&self.raw(&["TYPE".into(), path.name.clone()])?).display();
        let ttl = to_text(&self.raw(&["TTL".into(), path.name.clone()])?).display();
        Ok(format!("TYPE {}\nTTL {}", ty, ttl))
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope {
            self.select(db_index(&s.db))?;
        }
        let mut out = Vec::new();
        for line in sql.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("--")) {
            let args = parse_command(line);
            let start = std::time::Instant::now();
            let v = self.raw(&args)?;
            if args[0].eq_ignore_ascii_case("SELECT") {
                self.current_db = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(self.current_db);
            }
            out.push(to_result(line, v, start.elapsed().as_millis()));
        }
        Ok(out)
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let info = to_text(&self.raw(&["INFO".into()])?).display();
        let mut status = QueryResult {
            columns: vec![ColumnMeta::new("Section"), ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            is_query: true,
            ..Default::default()
        };
        let mut section = String::new();
        for l in info.lines() {
            if let Some(s) = l.strip_prefix("# ") {
                section = s.trim().to_string();
            } else if let Some((k, v)) = l.split_once(':') {
                status.rows.push(vec![Value::Text(section.clone()), Value::Text(k.into()), Value::Text(v.trim().into())]);
            }
        }
        let clients = to_text(&self.raw(&["CLIENT".into(), "LIST".into()])?).display();
        let mut processes = QueryResult { columns: vec![], is_query: true, ..Default::default() };
        for l in clients.lines() {
            let fields: Vec<(&str, &str)> = l.split(' ').filter_map(|kv| kv.split_once('=')).collect();
            if processes.columns.is_empty() {
                processes.columns = fields.iter().map(|(k, _)| ColumnMeta::new(*k)).collect();
            }
            processes.rows.push(fields.iter().map(|(_, v)| Value::Text(v.to_string())).collect());
        }
        let cfg = self.raw(&["CONFIG".into(), "GET".into(), "*".into()]);
        let mut variables = QueryResult {
            columns: vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            is_query: true,
            ..Default::default()
        };
        match cfg {
            Ok(redis::Value::Array(items)) => {
                for pair in items.chunks(2) {
                    if pair.len() == 2 {
                        variables.rows.push(vec![to_text(&pair[0]), to_text(&pair[1])]);
                    }
                }
            }
            Ok(redis::Value::Map(pairs)) => {
                for (k, v) in pairs {
                    variables.rows.push(vec![to_text(&k), to_text(&v)]);
                }
            }
            _ => {}
        }
        variables.rows.sort_by(|a, b| a[0].display().cmp(&b[0].display()));
        Ok(ServerSnapshot { processes, variables, status })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        self.raw(&["CLIENT".into(), "KILL".into(), "ID".into(), id.to_string()])?;
        Ok(())
    }

    fn explain(&mut self, _scope: Option<&Scope>, _sql: &str) -> Result<QueryResult> {
        bail!("EXPLAIN is not available for Redis")
    }
}

#[cfg(test)]
mod tests {
    use super::parse_command;

    #[test]
    fn parse() {
        assert_eq!(parse_command(r#"SET k "a b" 'c'"#), vec!["SET", "k", "a b", "c"]);
        assert_eq!(parse_command(r#"SET k """#), vec!["SET", "k", ""]);
    }
}
