use anyhow::{bail, Context, Result};
use mysql::consts::ColumnType;
use mysql::prelude::*;
use mysql::{Conn, OptsBuilder, SslOpts};
use std::time::Duration;

use super::{timed, Driver};
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

pub struct MySqlDriver {
    conn: Conn,
    kind: DbKind,
    current_db: String,
}

const D: Dialect = Dialect::MySql;

fn lit(s: &str) -> String {
    D.string_literal(s)
}

impl MySqlDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let mut b = OptsBuilder::new()
            .ip_or_hostname(Some(cfg.host.clone()))
            .tcp_port(cfg.port)
            .user(Some(cfg.user.clone()))
            .pass(if cfg.password.is_empty() { None } else { Some(cfg.password.clone()) })
            .tcp_connect_timeout(Some(Duration::from_secs(cfg.connect_timeout_secs.max(1))))
            .prefer_socket(false);
        if !cfg.database.is_empty() {
            b = b.db_name(Some(cfg.database.clone()));
        }
        if cfg.ssl.enabled {
            let mut ssl = SslOpts::default().with_danger_accept_invalid_certs(!cfg.ssl.verify_server);
            if !cfg.ssl.ca_cert.is_empty() {
                ssl = ssl.with_root_cert_path(Some(std::path::PathBuf::from(&cfg.ssl.ca_cert)));
            }
            b = b.ssl_opts(Some(ssl));
        }
        let mut conn = Conn::new(b).with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
        let charset = if cfg.encoding.is_empty() { "utf8mb4".to_string() } else { cfg.encoding.clone() };
        let _ = conn.query_drop(format!("SET NAMES {}", charset));
        Ok(Self { conn, kind: cfg.kind, current_db: cfg.database.clone() })
    }

    fn use_db(&mut self, scope: Option<&Scope>) -> Result<()> {
        if let Some(s) = scope {
            if !s.db.is_empty() && s.db != self.current_db {
                self.conn.query_drop(format!("USE {}", D.quote_ident(&s.db)))?;
                self.current_db = s.db.clone();
            }
        }
        Ok(())
    }

    fn rows(&mut self, sql: &str) -> Result<QueryResult> {
        self.query(None, sql)
    }
}

fn convert(v: mysql::Value, ty: ColumnType, binary: bool) -> Value {
    use mysql::Value as M;
    match v {
        M::NULL => Value::Null,
        M::Int(i) => Value::Int(i),
        M::UInt(u) => Value::UInt(u),
        M::Float(f) => Value::Float(f as f64),
        M::Double(f) => Value::Float(f),
        M::Date(y, mo, d, h, mi, s, us) => {
            if h == 0 && mi == 0 && s == 0 && us == 0 && ty == ColumnType::MYSQL_TYPE_DATE {
                Value::Text(format!("{:04}-{:02}-{:02}", y, mo, d))
            } else {
                Value::Text(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s))
            }
        }
        M::Time(neg, d, h, mi, s, _) => {
            Value::Text(format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, d * 24 + h as u32, mi, s))
        }
        M::Bytes(b) => {
            use ColumnType::*;
            match ty {
                MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24
                | MYSQL_TYPE_YEAR => {
                    let s = String::from_utf8_lossy(&b);
                    if let Ok(i) = s.parse::<i64>() {
                        Value::Int(i)
                    } else if let Ok(u) = s.parse::<u64>() {
                        Value::UInt(u)
                    } else {
                        Value::Text(s.to_string())
                    }
                }
                MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
                    let s = String::from_utf8_lossy(&b);
                    s.parse::<f64>().map(Value::Float).unwrap_or_else(|_| Value::Text(s.to_string()))
                }
                MYSQL_TYPE_BIT if b.len() <= 8 => {
                    let mut n: u64 = 0;
                    for x in &b {
                        n = (n << 8) | *x as u64;
                    }
                    Value::UInt(n)
                }
                _ => {
                    if binary {
                        match String::from_utf8(b) {
                            Ok(s) if !s.contains('\0') => Value::Text(s),
                            Ok(s) => Value::Bytes(s.into_bytes()),
                            Err(e) => Value::Bytes(e.into_bytes()),
                        }
                    } else {
                        match String::from_utf8(b) {
                            Ok(s) => Value::Text(s),
                            Err(e) => Value::Bytes(e.into_bytes()),
                        }
                    }
                }
            }
        }
    }
}

fn type_name(ty: ColumnType) -> String {
    let s = format!("{:?}", ty);
    s.trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase()
}

impl Driver for MySqlDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.rows("SELECT VERSION()")?;
        Ok(r.text(0, 0))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.rows("SHOW DATABASES")?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Function, ObjectKind::Procedure, ObjectKind::Trigger, ObjectKind::Event]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let db = lit(&scope.db);
        match kind {
            ObjectKind::Table | ObjectKind::View => {
                let tt = if kind == ObjectKind::Table { "'BASE TABLE','SYSTEM VERSIONED'" } else { "'VIEW'" };
                let r = self.rows(&format!(
                    "SELECT TABLE_NAME, TABLE_ROWS, DATA_LENGTH + IFNULL(INDEX_LENGTH,0), IFNULL(ENGINE,''), IFNULL(TABLE_COMMENT,''), \
                     IFNULL(CREATE_TIME,''), IFNULL(UPDATE_TIME,''), IFNULL(TABLE_COLLATION,''), IFNULL(AUTO_INCREMENT,''), IFNULL(ROW_FORMAT,'') \
                     FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_TYPE IN ({}) ORDER BY TABLE_NAME",
                    db, tt
                ))?;
                Ok((0..r.rows.len())
                    .map(|i| ObjectInfo {
                        name: r.text(i, 0),
                        rows: r.rows[i][1].as_f64().map(|v| v as i64),
                        data_length: r.rows[i][2].as_f64().map(|v| v as i64),
                        engine: r.text(i, 3),
                        comment: if kind == ObjectKind::View { String::new() } else { r.text(i, 4) },
                        created: r.text(i, 5),
                        modified: r.text(i, 6),
                        extra: vec![
                            ("Collation".into(), r.text(i, 7)),
                            ("Auto Increment".into(), r.text(i, 8)),
                            ("Row Format".into(), r.text(i, 9)),
                        ],
                    })
                    .collect())
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let r = self.rows(&format!(
                    "SELECT ROUTINE_NAME, IFNULL(ROUTINE_COMMENT,''), CREATED, LAST_ALTERED, DTD_IDENTIFIER, DEFINER \
                     FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {} AND ROUTINE_TYPE = {} ORDER BY ROUTINE_NAME",
                    db,
                    lit(kind.sql_keyword())
                ))?;
                Ok((0..r.rows.len())
                    .map(|i| ObjectInfo {
                        name: r.text(i, 0),
                        comment: r.text(i, 1),
                        created: r.text(i, 2),
                        modified: r.text(i, 3),
                        extra: vec![("Returns".into(), r.text(i, 4)), ("Definer".into(), r.text(i, 5))],
                        ..Default::default()
                    })
                    .collect())
            }
            ObjectKind::Trigger => {
                let r = self.rows(&format!(
                    "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, IFNULL(CREATED,'') \
                     FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {} ORDER BY TRIGGER_NAME",
                    db
                ))?;
                Ok((0..r.rows.len())
                    .map(|i| ObjectInfo {
                        name: r.text(i, 0),
                        created: r.text(i, 4),
                        extra: vec![
                            ("Table".into(), r.text(i, 1)),
                            ("Timing".into(), r.text(i, 2)),
                            ("Event".into(), r.text(i, 3)),
                        ],
                        ..Default::default()
                    })
                    .collect())
            }
            ObjectKind::Event => {
                let r = self.rows(&format!(
                    "SELECT EVENT_NAME, STATUS, IFNULL(INTERVAL_VALUE,''), IFNULL(INTERVAL_FIELD,''), CREATED, LAST_ALTERED, IFNULL(EVENT_COMMENT,'') \
                     FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {} ORDER BY EVENT_NAME",
                    db
                ))?;
                Ok((0..r.rows.len())
                    .map(|i| ObjectInfo {
                        name: r.text(i, 0),
                        created: r.text(i, 4),
                        modified: r.text(i, 5),
                        comment: r.text(i, 6),
                        extra: vec![
                            ("Status".into(), r.text(i, 1)),
                            ("Interval".into(), format!("{} {}", r.text(i, 2), r.text(i, 3))),
                        ],
                        ..Default::default()
                    })
                    .collect())
            }
            _ => Ok(vec![]),
        }
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let db = lit(&path.db);
        let t = lit(&path.name);
        let r = self.rows(&format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_KEY, IFNULL(COLUMN_COMMENT,''), IFNULL(COLLATION_NAME,''), DATA_TYPE \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY ORDINAL_POSITION",
            db, t
        ))?;
        if r.rows.is_empty() {
            bail!("table {} not found", path.display());
        }
        let mut columns = Vec::new();
        for i in 0..r.rows.len() {
            let extra = r.text(i, 4);
            let data_kind = r.text(i, 8).to_ascii_lowercase();
            let default = match &r.rows[i][3] {
                Value::Null => None,
                v => {
                    let s = v.to_string();
                    let is_expr = extra.contains("DEFAULT_GENERATED")
                        || s.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
                        || s.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP(")
                        || (s.starts_with('(') && s.ends_with(')'));
                    let numeric = ["int", "decimal", "float", "double", "bit", "tinyint", "smallint", "mediumint", "bigint", "year"]
                        .iter()
                        .any(|k| data_kind == *k);
                    if is_expr || (numeric && s.parse::<f64>().is_ok()) {
                        Some(s)
                    } else if s.starts_with('\'') {
                        Some(s)
                    } else {
                        Some(lit(&s))
                    }
                }
            };
            columns.push(ColumnDef {
                name: r.text(i, 0),
                data_type: r.text(i, 1),
                nullable: r.text(i, 2) == "YES",
                default,
                auto_increment: extra.to_ascii_lowercase().contains("auto_increment"),
                primary_key: r.text(i, 5) == "PRI",
                comment: r.text(i, 6),
                collation: String::new(),
                extra: if extra.to_ascii_lowercase().contains("auto_increment") { String::new() } else { extra.replace("DEFAULT_GENERATED", "").trim().to_string() },
            });
        }
        let ir = self.rows(&format!(
            "SELECT INDEX_NAME, COLUMN_NAME, NON_UNIQUE, INDEX_TYPE, IFNULL(INDEX_COMMENT,'') FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
            db, t
        ))?;
        let mut indexes: Vec<IndexDef> = Vec::new();
        for i in 0..ir.rows.len() {
            let name = ir.text(i, 0);
            let col = ir.text(i, 1);
            if let Some(idx) = indexes.iter_mut().find(|x| x.name == name) {
                idx.columns.push(col);
            } else {
                let method = ir.text(i, 3);
                indexes.push(IndexDef {
                    primary: name == "PRIMARY",
                    unique: ir.text(i, 2) == "0",
                    name,
                    columns: vec![col],
                    method: if method == "BTREE" { String::new() } else { method },
                    comment: ir.text(i, 4),
                });
            }
        }
        let fr = self.rows(&format!(
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
             ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
             WHERE k.TABLE_SCHEMA = {} AND k.TABLE_NAME = {} AND k.REFERENCED_TABLE_NAME IS NOT NULL ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            db, t
        ))?;
        let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
        for i in 0..fr.rows.len() {
            let name = fr.text(i, 0);
            if let Some(fk) = foreign_keys.iter_mut().find(|x| x.name == name) {
                fk.columns.push(fr.text(i, 1));
                fk.ref_columns.push(fr.text(i, 4));
            } else {
                let ref_db = fr.text(i, 2);
                foreign_keys.push(ForeignKeyDef {
                    name,
                    columns: vec![fr.text(i, 1)],
                    ref_db: if ref_db == path.db { String::new() } else { ref_db },
                    ref_schema: None,
                    ref_table: fr.text(i, 3),
                    ref_columns: vec![fr.text(i, 4)],
                    on_update: fr.text(i, 5),
                    on_delete: fr.text(i, 6),
                });
            }
        }
        // removes FK-backing indexes that MySQL creates implicitly with the same name
        indexes.retain(|ix| ix.primary || ix.unique || !foreign_keys.iter().any(|fk| fk.name == ix.name && fk.columns == ix.columns));

        let tr = self.rows(&format!(
            "SELECT IFNULL(ENGINE,''), IFNULL(TABLE_COMMENT,''), IFNULL(TABLE_COLLATION,''), AUTO_INCREMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
            db, t
        ))?;
        let collation = tr.text(0, 2);
        let charset = collation.split('_').next().unwrap_or("").to_string();
        let trg = self.rows(&format!(
            "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = {} AND EVENT_OBJECT_TABLE = {} ORDER BY TRIGGER_NAME",
            db, t
        ))?;
        let triggers = (0..trg.rows.len())
            .map(|i| TriggerDef { name: trg.text(i, 0), timing: trg.text(i, 1), event: trg.text(i, 2), body: trg.text(i, 3) })
            .collect();
        let checks = self
            .rows(&format!(
                "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
                 JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
                 WHERE tc.TABLE_SCHEMA = {} AND tc.TABLE_NAME = {} AND tc.CONSTRAINT_TYPE = 'CHECK'",
                db, t
            ))
            .map(|r| (0..r.rows.len()).map(|i| CheckDef { name: r.text(i, 0), expression: r.text(i, 1) }).collect())
            .unwrap_or_default();
        Ok(TableDef {
            path: path.clone(),
            columns,
            indexes,
            foreign_keys,
            checks,
            triggers,
            engine: tr.text(0, 0),
            comment: tr.text(0, 1),
            charset,
            collation,
            auto_increment: tr.rows.first().and_then(|r| r.get(3)).and_then(|v| v.as_f64()).map(|v| v as u64),
        })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let q = D.qualify(path);
        let (sql, col) = match kind {
            ObjectKind::Table => (format!("SHOW CREATE TABLE {}", q), "Create Table"),
            ObjectKind::View => (format!("SHOW CREATE VIEW {}", q), "Create View"),
            ObjectKind::Function => (format!("SHOW CREATE FUNCTION {}", q), "Create Function"),
            ObjectKind::Procedure => (format!("SHOW CREATE PROCEDURE {}", q), "Create Procedure"),
            ObjectKind::Trigger => (format!("SHOW CREATE TRIGGER {}", q), "SQL Original Statement"),
            ObjectKind::Event => (format!("SHOW CREATE EVENT {}", q), "Create Event"),
            _ => bail!("DDL not available for {:?}", kind),
        };
        let r = self.rows(&sql)?;
        let v = r.get(0, col).cloned().unwrap_or_default();
        if v.is_null() {
            bail!("insufficient privileges to read definition of {}", path.display());
        }
        Ok(v.to_string())
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        self.use_db(scope)?;
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            let start = std::time::Instant::now();
            let mut res = self.conn.query_iter(stmt.as_str())?;
            let mut got_any = false;
            while let Some(set) = res.iter() {
                got_any = true;
                let cols = set.columns();
                let cols_ref = cols.as_ref();
                let meta: Vec<ColumnMeta> = cols_ref
                    .iter()
                    .map(|c| ColumnMeta { name: c.name_str().to_string(), type_name: type_name(c.column_type()) })
                    .collect();
                let types: Vec<(ColumnType, bool)> = cols_ref.iter().map(|c| (c.column_type(), c.character_set() == 63)).collect();
                let is_query = !meta.is_empty();
                let affected = set.affected_rows();
                let mut rows = Vec::new();
                for row in set {
                    let row = row?;
                    let vals = row.unwrap();
                    rows.push(
                        vals.into_iter()
                            .enumerate()
                            .map(|(i, v)| {
                                let (t, bin) = types.get(i).copied().unwrap_or((ColumnType::MYSQL_TYPE_VAR_STRING, false));
                                convert(v, t, bin)
                            })
                            .collect(),
                    );
                }
                out.push(QueryResult {
                    sql: stmt.clone(),
                    columns: meta,
                    rows,
                    affected_rows: affected,
                    elapsed_ms: start.elapsed().as_millis(),
                    is_query,
                });
            }
            if !got_any {
                out.push(QueryResult { sql: stmt.clone(), elapsed_ms: start.elapsed().as_millis(), ..Default::default() });
            }
            // track USE statements
            let up = stmt.trim().to_ascii_uppercase();
            if up.starts_with("USE ") {
                self.current_db = stmt.trim()[4..].trim().trim_matches('`').trim_end_matches(';').to_string();
            }
        }
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.rows("SELECT User, Host FROM mysql.user ORDER BY User, Host")?;
        Ok((0..r.rows.len()).map(|i| UserInfo { name: r.text(i, 0), host: r.text(i, 1), attributes: vec![] }).collect())
    }

    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        let host = if spec.host.is_empty() { "%".to_string() } else { spec.host.clone() };
        let who = format!("{}@{}", lit(&spec.name), lit(&host));
        let mut out = Vec::new();
        match old {
            None => {
                out.push(format!("CREATE USER {} IDENTIFIED BY {}", who, lit(&spec.password)));
            }
            Some(o) => {
                if o.name != spec.name || o.host != host {
                    out.push(format!("RENAME USER {}@{} TO {}", lit(&o.name), lit(&o.host), who));
                }
                if !spec.password.is_empty() {
                    out.push(format!("ALTER USER {} IDENTIFIED BY {}", who, lit(&spec.password)));
                }
            }
        }
        for (privilege, obj) in &spec.grants {
            if privilege.is_empty() {
                continue;
            }
            out.push(format!("GRANT {} ON {} TO {}", privilege, if obj.is_empty() { "*.*" } else { obj }, who));
        }
        if spec.superuser {
            out.push(format!("GRANT ALL PRIVILEGES ON *.* TO {} WITH GRANT OPTION", who));
        }
        Ok(out)
    }

    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        let r = self.rows(&format!("SHOW GRANTS FOR {}@{}", lit(&user.name), lit(&user.host)))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        Ok(format!("DROP USER {}@{}", lit(&user.name), lit(&user.host)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.rows("SHOW FULL PROCESSLIST")?,
            variables: self.rows("SHOW GLOBAL VARIABLES")?,
            status: self.rows("SHOW GLOBAL STATUS")?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let id: u64 = id.trim().parse().context("invalid process id")?;
        self.conn.query_drop(format!("KILL {}", id))?;
        Ok(())
    }

    fn charsets(&mut self) -> Result<Vec<String>> {
        let r = self.rows("SHOW CHARACTER SET")?;
        let mut v: Vec<String> = (0..r.rows.len()).map(|i| r.text(i, 0)).collect();
        v.sort();
        Ok(v)
    }

    fn collations(&mut self, charset: &str) -> Result<Vec<String>> {
        let r = self.rows(&format!("SHOW COLLATION WHERE Charset = {}", lit(charset)))?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let (r, _) = timed(|| self.query(scope, &format!("EXPLAIN {}", sql.trim().trim_end_matches(';'))))?;
        Ok(r)
    }
}
