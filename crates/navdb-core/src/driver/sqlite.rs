use anyhow::{bail, Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use super::Driver;
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Sqlite;

pub struct SqliteDriver {
    conn: Connection,
    path: String,
}

fn lit(s: &str) -> String {
    D.string_literal(s)
}

impl SqliteDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let path = if cfg.database.is_empty() { ":memory:".to_string() } else { cfg.database.clone() };
        let flags = if cfg.read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI
        };
        let conn = Connection::open_with_flags(&path, flags).with_context(|| format!("opening {}", path))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _ = conn.execute_batch("PRAGMA foreign_keys = ON");
        for (alias, file) in &cfg.attached {
            conn.execute_batch(&format!("ATTACH DATABASE {} AS {}", lit(file), D.quote_ident(alias)))?;
        }
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn rows(&mut self, sql: &str) -> Result<QueryResult> {
        self.query(None, sql)
    }

    fn schema_prefix(db: &str) -> String {
        if db.is_empty() || db == "main" {
            String::new()
        } else {
            format!("{}.", D.quote_ident(db))
        }
    }

    fn run_one(&self, stmt: &str) -> Result<QueryResult> {
        let start = std::time::Instant::now();
        let mut st = self.conn.prepare(stmt)?;
        let ncol = st.column_count();
        if ncol == 0 {
            let changes = st.execute([])?;
            return Ok(QueryResult {
                sql: stmt.to_string(),
                affected_rows: changes as u64,
                elapsed_ms: start.elapsed().as_millis(),
                ..Default::default()
            });
        }
        let columns: Vec<ColumnMeta> = st
            .columns()
            .iter()
            .map(|c| ColumnMeta { name: c.name().to_string(), type_name: c.decl_type().unwrap_or("").to_string() })
            .collect();
        let mut rows = Vec::new();
        let mut q = st.query([])?;
        while let Some(r) = q.next()? {
            let mut vals = Vec::with_capacity(ncol);
            for i in 0..ncol {
                vals.push(match r.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(v) => Value::Int(v),
                    ValueRef::Real(v) => Value::Float(v),
                    ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).to_string()),
                    ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
                });
            }
            rows.push(vals);
        }
        Ok(QueryResult {
            sql: stmt.to_string(),
            columns,
            rows,
            affected_rows: 0,
            elapsed_ms: start.elapsed().as_millis(),
            is_query: true,
        })
    }
}

impl Driver for SqliteDriver {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    fn server_version(&mut self) -> Result<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.rows("PRAGMA database_list")?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 1)).filter(|n| n != "temp").collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Index, ObjectKind::Trigger]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let ty = match kind {
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::Index => "index",
            ObjectKind::Trigger => "trigger",
            _ => return Ok(vec![]),
        };
        let pre = Self::schema_prefix(&scope.db);
        let r = self.rows(&format!(
            "SELECT name, tbl_name FROM {}sqlite_master WHERE type = {} AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
            pre,
            lit(ty)
        ))?;
        let mut out = Vec::new();
        for i in 0..r.rows.len() {
            let name = r.text(i, 0);
            let mut info = ObjectInfo::named(name.clone());
            if kind == ObjectKind::Table {
                if let Ok(c) = self.rows(&format!("SELECT COUNT(*) FROM {}{}", pre, D.quote_ident(&name))) {
                    info.rows = c.rows.first().and_then(|r| r.first()).and_then(|v| v.as_f64()).map(|v| v as i64);
                }
            }
            if kind == ObjectKind::Index || kind == ObjectKind::Trigger {
                info.extra.push(("Table".into(), r.text(i, 1)));
            }
            out.push(info);
        }
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let pre = Self::schema_prefix(&path.db);
        let t = D.quote_ident(&path.name);
        let sql_r = self.rows(&format!(
            "SELECT sql FROM {}sqlite_master WHERE type IN ('table','view') AND name = {}",
            pre,
            lit(&path.name)
        ))?;
        if sql_r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let create_sql = sql_r.text(0, 0);
        let autoinc = create_sql.to_ascii_uppercase().contains("AUTOINCREMENT");
        let r = self.rows(&format!("PRAGMA {}table_info({})", pre, t))?;
        let mut columns = Vec::new();
        let mut pk: Vec<(i64, String)> = Vec::new();
        for i in 0..r.rows.len() {
            let name = r.text(i, 1);
            let pk_pos = r.rows[i][5].as_f64().unwrap_or(0.0) as i64;
            if pk_pos > 0 {
                pk.push((pk_pos, name.clone()));
            }
            let ty = r.text(i, 2);
            columns.push(ColumnDef {
                name,
                data_type: ty.clone(),
                nullable: r.text(i, 3) == "0",
                default: match &r.rows[i][4] {
                    Value::Null => None,
                    v => Some(v.to_string()),
                },
                auto_increment: pk_pos > 0 && autoinc && ty.eq_ignore_ascii_case("INTEGER"),
                primary_key: pk_pos > 0,
                ..Default::default()
            });
        }
        pk.sort();
        let mut indexes = Vec::new();
        if !pk.is_empty() {
            indexes.push(IndexDef {
                name: "PRIMARY".into(),
                columns: pk.into_iter().map(|(_, n)| n).collect(),
                primary: true,
                unique: true,
                ..Default::default()
            });
        }
        let il = self.rows(&format!("PRAGMA {}index_list({})", pre, t))?;
        for i in 0..il.rows.len() {
            let name = il.text(i, 1);
            let origin = il.text(i, 3);
            if origin == "pk" {
                continue;
            }
            let ii = self.rows(&format!("PRAGMA {}index_info({})", pre, D.quote_ident(&name)))?;
            let cols = (0..ii.rows.len()).map(|j| ii.text(j, 2)).collect();
            indexes.push(IndexDef {
                // auto indexes for UNIQUE constraints keep their generated names
                name,
                columns: cols,
                unique: il.text(i, 2) == "1",
                primary: false,
                method: if origin == "u" { "CONSTRAINT".into() } else { String::new() },
                comment: String::new(),
            });
        }
        // constraint-backed unique indexes cannot be recreated by name; keep as plain unique indexes
        for ix in indexes.iter_mut() {
            if ix.method == "CONSTRAINT" {
                ix.method.clear();
                if ix.name.starts_with("sqlite_autoindex_") {
                    ix.name = format!("ux_{}_{}", path.name, ix.columns.join("_"));
                }
            }
        }
        let fl = self.rows(&format!("PRAGMA {}foreign_key_list({})", pre, t))?;
        let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
        for i in 0..fl.rows.len() {
            let id = fl.text(i, 0);
            let name = format!("fk_{}_{}", path.name, id);
            let from = fl.text(i, 3);
            let to = fl.text(i, 4);
            if let Some(fk) = foreign_keys.iter_mut().find(|f| f.name == name) {
                fk.columns.push(from);
                fk.ref_columns.push(to);
            } else {
                foreign_keys.push(ForeignKeyDef {
                    name,
                    columns: vec![from],
                    ref_db: String::new(),
                    ref_schema: None,
                    ref_table: fl.text(i, 2),
                    ref_columns: vec![to],
                    on_update: fl.text(i, 5),
                    on_delete: fl.text(i, 6),
                });
            }
        }
        let tr = self.rows(&format!(
            "SELECT name, sql FROM {}sqlite_master WHERE type = 'trigger' AND tbl_name = {} ORDER BY name",
            pre,
            lit(&path.name)
        ))?;
        let triggers = (0..tr.rows.len())
            .map(|i| {
                let sql = tr.text(i, 1);
                let up = sql.to_ascii_uppercase();
                let timing = if up.contains(" BEFORE ") {
                    "BEFORE"
                } else if up.contains(" INSTEAD OF ") {
                    "INSTEAD OF"
                } else {
                    "AFTER"
                };
                let event = ["INSERT", "UPDATE", "DELETE"].iter().find(|e| up.contains(&format!(" {} ", e))).copied().unwrap_or("INSERT");
                let body = match up.find("BEGIN") {
                    Some(p) => sql[p..].to_string(),
                    None => sql.clone(),
                };
                TriggerDef { name: tr.text(i, 0), timing: timing.into(), event: event.into(), body }
            })
            .collect();
        Ok(TableDef { path: path.clone(), columns, indexes, foreign_keys, triggers, ..Default::default() })
    }

    fn object_ddl(&mut self, path: &ObjPath, _kind: ObjectKind) -> Result<String> {
        let pre = Self::schema_prefix(&path.db);
        let r = self.rows(&format!("SELECT sql FROM {}sqlite_master WHERE name = {}", pre, lit(&path.name)))?;
        if r.rows.is_empty() {
            bail!("object {} not found", path.display());
        }
        Ok(format!("{};", r.text(0, 0)))
    }

    fn execute(&mut self, _scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            out.push(self.run_one(&stmt)?);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        self.query(scope, &format!("EXPLAIN QUERY PLAN {}", sql.trim().trim_end_matches(';')))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let mut vars = QueryResult {
            columns: vec![ColumnMeta::new("Variable_name"), ColumnMeta::new("Value")],
            is_query: true,
            ..Default::default()
        };
        for p in ["page_size", "page_count", "freelist_count", "journal_mode", "synchronous", "foreign_keys", "encoding", "user_version", "auto_vacuum", "cache_size"] {
            if let Ok(r) = self.rows(&format!("PRAGMA {}", p)) {
                vars.rows.push(vec![Value::Text(p.into()), r.rows.first().and_then(|r| r.first()).cloned().unwrap_or_default()]);
            }
        }
        Ok(ServerSnapshot { processes: QueryResult::default(), variables: vars, status: self.rows("PRAGMA database_list")? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl;

    fn mem() -> SqliteDriver {
        SqliteDriver::connect(&ConnectionConfig { database: ":memory:".into(), ..ConnectionConfig::new(DbKind::Sqlite) }).unwrap()
    }

    #[test]
    fn roundtrip_table_def_and_alter() {
        let mut d = mem();
        d.execute(
            None,
            "CREATE TABLE b (id INTEGER PRIMARY KEY); CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL DEFAULT 'x', b_id INTEGER REFERENCES b(id));
             CREATE INDEX ix_name ON a(name);
             INSERT INTO a (name) VALUES ('one'), ('two');",
        )
        .unwrap();
        let p = ObjPath::new("main", None, "a");
        let def = d.table_def(&p).unwrap();
        assert_eq!(def.columns.len(), 3);
        assert!(def.columns[0].auto_increment);
        assert_eq!(def.primary_key(), vec!["id".to_string()]);
        assert!(def.indexes.iter().any(|i| i.name == "ix_name"));
        assert_eq!(def.foreign_keys.len(), 1);

        let mut new = def.clone();
        new.columns[1].data_type = "VARCHAR(20)".into();
        new.columns.push(ColumnDef::new("extra", "TEXT"));
        let stmts = ddl::alter_table(D, &def, &new);
        for s in &stmts {
            d.execute(None, s).unwrap();
        }
        let after = d.table_def(&p).unwrap();
        assert_eq!(after.columns.len(), 4);
        assert_eq!(after.columns[1].data_type, "VARCHAR(20)");
        assert_eq!(d.count(&p).unwrap(), 2);
        assert!(after.indexes.iter().any(|i| i.name == "ix_name"));
    }

    #[test]
    fn multi_statement_results() {
        let mut d = mem();
        let r = d.execute(None, "CREATE TABLE t(x); INSERT INTO t VALUES (1),(2); SELECT * FROM t;").unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].affected_rows, 2);
        assert!(r[2].is_query);
        assert_eq!(r[2].rows.len(), 2);
    }
}
