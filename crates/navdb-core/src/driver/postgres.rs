use anyhow::{bail, Context, Result};
use postgres::config::SslMode;
use postgres::{Client, NoTls, SimpleQueryMessage};
use std::time::Duration;

use super::{split_list, Driver};
use crate::config::{ConnectionConfig, DbKind};
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{ColumnMeta, QueryResult, Value};

const D: Dialect = Dialect::Postgres;

fn lit(s: &str) -> String {
    D.string_literal(s)
}

pub struct PgDriver {
    cfg: ConnectionConfig,
    client: Client,
    current_db: String,
}

fn open(cfg: &ConnectionConfig, db: &str) -> Result<Client> {
    let mut c = postgres::Config::new();
    c.host(&cfg.host)
        .port(cfg.port)
        .user(&cfg.user)
        .dbname(db)
        .application_name("NavDB Studio")
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs.max(1)));
    if !cfg.password.is_empty() {
        c.password(&cfg.password);
    }
    if cfg.keepalive_secs > 0 {
        c.keepalives(true).keepalives_idle(Duration::from_secs(cfg.keepalive_secs));
    }
    let client = if cfg.ssl.enabled {
        c.ssl_mode(SslMode::Require);
        let mut b = native_tls::TlsConnector::builder();
        b.danger_accept_invalid_certs(!cfg.ssl.verify_server);
        b.danger_accept_invalid_hostnames(!cfg.ssl.verify_server);
        if !cfg.ssl.ca_cert.is_empty() {
            let pem = std::fs::read(&cfg.ssl.ca_cert).context("reading CA certificate")?;
            b.add_root_certificate(native_tls::Certificate::from_pem(&pem)?);
        }
        if !cfg.ssl.client_cert.is_empty() && !cfg.ssl.client_key.is_empty() {
            let cert = std::fs::read(&cfg.ssl.client_cert)?;
            let key = std::fs::read(&cfg.ssl.client_key)?;
            b.identity(native_tls::Identity::from_pkcs8(&cert, &key)?);
        }
        let tls = postgres_native_tls::MakeTlsConnector::new(b.build()?);
        c.connect(tls)
    } else {
        c.connect(NoTls)
    };
    client.with_context(|| format!("connecting to {}:{}/{}", cfg.host, cfg.port, db))
}

impl PgDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let db = cfg.default_database();
        let client = open(cfg, &db)?;
        Ok(Self { cfg: cfg.clone(), client, current_db: db })
    }

    fn use_db(&mut self, scope: Option<&Scope>) -> Result<()> {
        if let Some(s) = scope {
            if !s.db.is_empty() && s.db != self.current_db {
                self.client = open(&self.cfg, &s.db)?;
                self.current_db = s.db.clone();
            }
            if let Some(schema) = &s.schema {
                self.client.batch_execute(&format!("SET search_path TO {}, public", D.quote_ident(schema)))?;
            }
        }
        Ok(())
    }

    fn rows_in(&mut self, db: &str, sql: &str) -> Result<QueryResult> {
        self.query(Some(&Scope::new(db, None)), sql)
    }

    fn regclass(p: &ObjPath) -> String {
        lit(&D.qualify(p))
    }
}

fn fk_action(c: &str) -> String {
    match c {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
    .into()
}

impl Driver for PgDriver {
    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.query(None, "SHOW server_version")?;
        Ok(format!("PostgreSQL {}", r.text(0, 0)))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.query(None, "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname")?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn list_schemas(&mut self, db: &str) -> Result<Vec<String>> {
        let r = self.rows_in(
            db,
            "SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' ORDER BY nspname",
        )?;
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
        ]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let schema = lit(scope.schema.as_deref().unwrap_or("public"));
        let rel = |k: &str| {
            format!(
                "SELECT c.relname, CASE WHEN c.reltuples < 0 THEN s.n_live_tup ELSE c.reltuples::bigint END, pg_total_relation_size(c.oid), COALESCE(obj_description(c.oid, 'pg_class'), ''), \
                 pg_get_userbyid(c.relowner), COALESCE(t.spcname, 'pg_default') \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_tablespace t ON t.oid = c.reltablespace LEFT JOIN pg_stat_all_tables s ON s.relid = c.oid \
                 WHERE n.nspname = {} AND c.relkind IN ({}) ORDER BY c.relname",
                schema, k
            )
        };
        let sql = match kind {
            ObjectKind::Table => rel("'r','p'"),
            ObjectKind::View => rel("'v'"),
            ObjectKind::MaterializedView => rel("'m'"),
            ObjectKind::Sequence => rel("'S'"),
            ObjectKind::Function | ObjectKind::Procedure => format!(
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')', NULL, NULL, COALESCE(obj_description(p.oid, 'pg_proc'), ''), \
                 pg_get_userbyid(p.proowner), pg_get_function_result(p.oid) \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = {} AND p.prokind = {} ORDER BY 1",
                schema,
                if kind == ObjectKind::Function { "'f'" } else { "'p'" }
            ),
            ObjectKind::Trigger => format!(
                "SELECT t.tgname, NULL, NULL, '', c.relname, CASE WHEN t.tgenabled = 'D' THEN 'disabled' ELSE 'enabled' END \
                 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {} AND NOT t.tgisinternal ORDER BY t.tgname",
                schema
            ),
            _ => return Ok(vec![]),
        };
        let r = self.query(Some(scope), &sql)?;
        Ok((0..r.rows.len())
            .map(|i| {
                let mut info = ObjectInfo {
                    name: r.text(i, 0),
                    rows: r.rows[i][1].as_f64().map(|v| v as i64),
                    data_length: r.rows[i][2].as_f64().map(|v| v as i64),
                    comment: r.text(i, 3),
                    ..Default::default()
                };
                match kind {
                    ObjectKind::Trigger => {
                        info.extra = vec![("Table".into(), r.text(i, 4)), ("Status".into(), r.text(i, 5))];
                    }
                    ObjectKind::Function | ObjectKind::Procedure => {
                        info.extra = vec![("Owner".into(), r.text(i, 4)), ("Returns".into(), r.text(i, 5))];
                    }
                    _ => {
                        info.extra = vec![("Owner".into(), r.text(i, 4)), ("Tablespace".into(), r.text(i, 5))];
                    }
                }
                info
            })
            .collect())
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        let scope = Scope::new(path.db.clone(), None);
        let rc = Self::regclass(path);
        let r = self.query(
            Some(&scope),
            &format!(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
                 COALESCE(col_description(a.attrelid, a.attnum), ''), a.attidentity, COALESCE(co.collname, '') \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 LEFT JOIN pg_collation co ON co.oid = a.attcollation AND a.attcollation <> 100 \
                 WHERE a.attrelid = {}::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                rc
            ),
        )?;
        let mut columns = Vec::new();
        for i in 0..r.rows.len() {
            let default = match &r.rows[i][3] {
                Value::Null => None,
                v => Some(v.to_string()),
            };
            let identity = r.text(i, 5);
            let serial = default.as_deref().map(|d| d.starts_with("nextval(")).unwrap_or(false);
            columns.push(ColumnDef {
                name: r.text(i, 0),
                data_type: short_type(&r.text(i, 1)),
                nullable: r.text(i, 2) == "t",
                default: if serial { None } else { default.clone() },
                auto_increment: !identity.is_empty() || serial,
                primary_key: false,
                comment: r.text(i, 4),
                collation: r.text(i, 6),
                extra: if serial { default.unwrap_or_default() } else { String::new() },
            });
        }
        // a serial column keeps its sequence default so DDL roundtrips cleanly
        for c in columns.iter_mut() {
            if c.auto_increment && !c.extra.is_empty() {
                let lower = c.data_type.to_ascii_lowercase();
                c.data_type = match lower.as_str() {
                    "bigint" => "bigserial".into(),
                    "smallint" => "smallserial".into(),
                    _ => "serial".into(),
                };
                c.extra.clear();
            }
        }
        let ir = self.query(
            Some(&scope),
            &format!(
                "SELECT i.relname, ix.indisunique, ix.indisprimary, am.amname, \
                 array_to_string(ARRAY(SELECT pg_get_indexdef(ix.indexrelid, k + 1, true) FROM generate_subscripts(ix.indkey, 1) k ORDER BY k), '\u{1f}') \
                 FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid JOIN pg_am am ON am.oid = i.relam \
                 WHERE ix.indrelid = {}::regclass ORDER BY ix.indisprimary DESC, i.relname",
                rc
            ),
        )?;
        let mut indexes = Vec::new();
        for i in 0..ir.rows.len() {
            let method = ir.text(i, 3);
            indexes.push(IndexDef {
                name: ir.text(i, 0),
                unique: ir.text(i, 1) == "t",
                primary: ir.text(i, 2) == "t",
                method: if method == "btree" { String::new() } else { method },
                columns: ir.text(i, 4).split('\u{1f}').map(|s| s.trim_matches('"').to_string()).collect(),
                comment: String::new(),
            });
        }
        let pk_cols: Vec<String> = indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default();
        for c in columns.iter_mut() {
            c.primary_key = pk_cols.contains(&c.name);
        }
        // unique constraints are represented as unique indexes
        let fr = self.query(
            Some(&scope),
            &format!(
                "SELECT con.conname, \
                 (SELECT string_agg(a.attname, ',' ORDER BY k.n) FROM unnest(con.conkey) WITH ORDINALITY k(attnum, n) JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum), \
                 nf.nspname, cf.relname, \
                 (SELECT string_agg(a.attname, ',' ORDER BY k.n) FROM unnest(con.confkey) WITH ORDINALITY k(attnum, n) JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum), \
                 con.confupdtype, con.confdeltype \
                 FROM pg_constraint con JOIN pg_class cf ON cf.oid = con.confrelid JOIN pg_namespace nf ON nf.oid = cf.relnamespace \
                 WHERE con.contype = 'f' AND con.conrelid = {}::regclass ORDER BY con.conname",
                rc
            ),
        )?;
        let foreign_keys = (0..fr.rows.len())
            .map(|i| ForeignKeyDef {
                name: fr.text(i, 0),
                columns: split_list(&fr.text(i, 1)),
                ref_db: path.db.clone(),
                ref_schema: Some(fr.text(i, 2)),
                ref_table: fr.text(i, 3),
                ref_columns: split_list(&fr.text(i, 4)),
                on_update: fk_action(&fr.text(i, 5)),
                on_delete: fk_action(&fr.text(i, 6)),
            })
            .collect();
        let cr = self.query(
            Some(&scope),
            &format!(
                "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE contype = 'c' AND conrelid = {}::regclass ORDER BY conname",
                rc
            ),
        )?;
        let checks = (0..cr.rows.len())
            .map(|i| {
                let def = cr.text(i, 1);
                let expr = def.strip_prefix("CHECK ").unwrap_or(&def).trim().to_string();
                let expr = expr.strip_prefix('(').and_then(|e| e.strip_suffix(')')).map(|e| e.to_string()).unwrap_or(expr);
                CheckDef { name: cr.text(i, 0), expression: expr }
            })
            .collect();
        let tr = self.query(
            Some(&scope),
            &format!(
                "SELECT t.tgname, pg_get_triggerdef(t.oid, true) FROM pg_trigger t WHERE t.tgrelid = {}::regclass AND NOT t.tgisinternal ORDER BY t.tgname",
                rc
            ),
        )?;
        let triggers = (0..tr.rows.len())
            .map(|i| {
                let def = tr.text(i, 1);
                let up = def.to_ascii_uppercase();
                let timing = if up.contains(" BEFORE ") {
                    "BEFORE"
                } else if up.contains(" INSTEAD OF ") {
                    "INSTEAD OF"
                } else {
                    "AFTER"
                };
                let event = ["INSERT", "UPDATE", "DELETE", "TRUNCATE"]
                    .iter()
                    .filter(|e| up.contains(&format!(" {}", e)))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" OR ");
                TriggerDef { name: tr.text(i, 0), timing: timing.into(), event, body: def }
            })
            .collect();
        let comment = self
            .query(Some(&scope), &format!("SELECT COALESCE(obj_description({}::regclass, 'pg_class'), '')", rc))?
            .text(0, 0);
        Ok(TableDef {
            path: path.clone(),
            columns,
            indexes,
            foreign_keys,
            checks,
            triggers,
            comment,
            ..Default::default()
        })
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        let scope = Scope::new(path.db.clone(), None);
        let schema = lit(path.schema.as_deref().unwrap_or("public"));
        match kind {
            ObjectKind::Table => {
                let def = self.table_def(path)?;
                Ok(crate::ddl::script(&crate::ddl::create_table(D, &def)))
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let r = self.query(Some(&scope), &format!("SELECT pg_get_viewdef({}::regclass, true)", Self::regclass(path)))?;
                let kw = if kind == ObjectKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
                Ok(format!("{} {} AS\n{}", kw, D.qualify(path), r.text(0, 0)))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                // name may contain the identity signature "f(int)"
                let (name, args) = match path.name.find('(') {
                    Some(p) => (&path.name[..p], Some(&path.name[p..])),
                    None => (path.name.as_str(), None),
                };
                let mut sql = format!(
                    "SELECT pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = {} AND p.proname = {}",
                    schema,
                    lit(name)
                );
                if let Some(a) = args {
                    sql.push_str(&format!(
                        " AND '(' || pg_get_function_identity_arguments(p.oid) || ')' = {}",
                        lit(a)
                    ));
                }
                let r = self.query(Some(&scope), &sql)?;
                if r.rows.is_empty() {
                    bail!("routine {} not found", path.display());
                }
                Ok(r.text(0, 0))
            }
            ObjectKind::Trigger => {
                let r = self.query(
                    Some(&scope),
                    &format!(
                        "SELECT pg_get_triggerdef(t.oid, true) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = {} AND t.tgname = {}",
                        schema,
                        lit(&path.name)
                    ),
                )?;
                Ok(format!("{};", r.text(0, 0)))
            }
            ObjectKind::Sequence => {
                let r = self.query(
                    Some(&scope),
                    &format!(
                        "SELECT start_value, minimum_value, maximum_value, increment, cycle_option FROM information_schema.sequences \
                         WHERE sequence_schema = {} AND sequence_name = {}",
                        schema,
                        lit(&path.name)
                    ),
                )?;
                Ok(format!(
                    "CREATE SEQUENCE {}\n  INCREMENT {}\n  MINVALUE {}\n  MAXVALUE {}\n  START {}{};",
                    D.qualify(path),
                    r.text(0, 3),
                    r.text(0, 1),
                    r.text(0, 2),
                    r.text(0, 0),
                    if r.text(0, 4) == "YES" { "\n  CYCLE" } else { "" }
                ))
            }
            _ => bail!("DDL not available for {:?}", kind),
        }
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        self.use_db(scope)?;
        let mut out = Vec::new();
        for stmt in crate::sqlutil::split_statements(sql, D) {
            let start = std::time::Instant::now();
            let msgs = self.client.simple_query(&stmt)?;
            let mut cur = QueryResult { sql: stmt.clone(), ..Default::default() };
            for m in msgs {
                match m {
                    SimpleQueryMessage::RowDescription(cols) => {
                        cur.is_query = true;
                        cur.columns = cols.iter().map(|c| ColumnMeta::new(c.name())).collect();
                    }
                    SimpleQueryMessage::Row(row) => {
                        if cur.columns.is_empty() {
                            cur.is_query = true;
                            cur.columns = row.columns().iter().map(|c| ColumnMeta::new(c.name())).collect();
                        }
                        let vals = (0..row.len())
                            .map(|i| match row.get(i) {
                                None => Value::Null,
                                Some(s) => Value::Text(s.to_string()),
                            })
                            .collect();
                        cur.rows.push(vals);
                    }
                    SimpleQueryMessage::CommandComplete(n) => {
                        cur.affected_rows = n;
                    }
                    _ => {}
                }
            }
            cur.elapsed_ms = start.elapsed().as_millis();
            out.push(cur);
        }
        Ok(out)
    }

    fn list_users(&mut self) -> Result<Vec<UserInfo>> {
        let r = self.query(
            None,
            "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolcreaterole, COALESCE(rolvaliduntil::text, '') FROM pg_roles WHERE rolname NOT LIKE 'pg\\_%' ORDER BY rolname",
        )?;
        Ok((0..r.rows.len())
            .map(|i| UserInfo {
                name: r.text(i, 0),
                host: String::new(),
                attributes: vec![
                    ("Superuser".into(), r.text(i, 1)),
                    ("Can Login".into(), r.text(i, 2)),
                    ("Create DB".into(), r.text(i, 3)),
                    ("Create Role".into(), r.text(i, 4)),
                    ("Valid Until".into(), r.text(i, 5)),
                ],
            })
            .collect())
    }

    fn user_sql(&self, old: Option<&UserInfo>, spec: &UserSpec) -> Result<Vec<String>> {
        let flag = |b: bool, yes: &str, no: &str| if b { yes.to_string() } else { no.to_string() };
        let opts = format!(
            "{} {} {} {}",
            flag(spec.superuser, "SUPERUSER", "NOSUPERUSER"),
            flag(spec.can_login, "LOGIN", "NOLOGIN"),
            flag(spec.create_db, "CREATEDB", "NOCREATEDB"),
            flag(spec.create_role, "CREATEROLE", "NOCREATEROLE"),
        );
        let who = D.quote_ident(&spec.name);
        let mut out = Vec::new();
        match old {
            None => {
                let mut s = format!("CREATE ROLE {} WITH {}", who, opts);
                if !spec.password.is_empty() {
                    s.push_str(&format!(" PASSWORD {}", lit(&spec.password)));
                }
                out.push(s);
            }
            Some(o) => {
                if o.name != spec.name {
                    out.push(format!("ALTER ROLE {} RENAME TO {}", D.quote_ident(&o.name), who));
                }
                let mut s = format!("ALTER ROLE {} WITH {}", who, opts);
                if !spec.password.is_empty() {
                    s.push_str(&format!(" PASSWORD {}", lit(&spec.password)));
                }
                out.push(s);
            }
        }
        for (privilege, obj) in &spec.grants {
            if privilege.is_empty() || obj.is_empty() {
                continue;
            }
            out.push(format!("GRANT {} ON {} TO {}", privilege, obj, who));
        }
        Ok(out)
    }

    fn user_grants(&mut self, user: &UserInfo) -> Result<Vec<String>> {
        let r = self.query(
            None,
            &format!(
                "SELECT 'GRANT ' || string_agg(privilege_type, ', ') || ' ON ' || table_schema || '.' || table_name || ' TO ' || grantee \
                 FROM information_schema.role_table_grants WHERE grantee = {} GROUP BY table_schema, table_name, grantee ORDER BY 1",
                lit(&user.name)
            ),
        )?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    }

    fn drop_user_sql(&self, user: &UserInfo) -> Result<String> {
        Ok(format!("DROP ROLE {}", D.quote_ident(&user.name)))
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot {
            processes: self.query(
                None,
                "SELECT pid, usename, datname, COALESCE(client_addr::text, 'local') AS client_addr, state, backend_start, query_start, wait_event, query \
                 FROM pg_stat_activity WHERE pid IS NOT NULL ORDER BY pid",
            )?,
            variables: self.query(None, "SELECT name, setting, unit, short_desc FROM pg_settings ORDER BY name")?,
            status: self.query(
                None,
                "SELECT datname, numbackends, xact_commit, xact_rollback, blks_read, blks_hit, tup_returned, tup_fetched, tup_inserted, tup_updated, tup_deleted, pg_size_pretty(pg_database_size(datid)) AS size \
                 FROM pg_stat_database WHERE datname IS NOT NULL ORDER BY datname",
            )?,
        })
    }

    fn kill_process(&mut self, id: &str) -> Result<()> {
        let id: i64 = id.trim().parse().context("invalid pid")?;
        self.client.simple_query(&format!("SELECT pg_terminate_backend({})", id))?;
        Ok(())
    }

    fn charsets(&mut self) -> Result<Vec<String>> {
        Ok(["UTF8", "LATIN1", "SQL_ASCII", "WIN1252", "EUC_JP", "GBK"].iter().map(|s| s.to_string()).collect())
    }
}

/// Short spellings for `format_type` output, e.g. `character varying(100)` -> `varchar(100)`.
fn short_type(t: &str) -> String {
    const MAP: [(&str, &str); 6] = [
        ("character varying", "varchar"),
        ("timestamp without time zone", "timestamp"),
        ("timestamp with time zone", "timestamptz"),
        ("time without time zone", "time"),
        ("time with time zone", "timetz"),
        ("character", "char"),
    ];
    for (long, short) in MAP {
        if let Some(rest) = t.strip_prefix(long) {
            if rest.is_empty() || rest.starts_with('(') || rest.starts_with('[') {
                return format!("{short}{rest}");
            }
        }
    }
    t.to_string()
}

#[cfg(test)]
mod type_tests {
    #[test]
    fn short_types() {
        assert_eq!(super::short_type("character varying(100)"), "varchar(100)");
        assert_eq!(super::short_type("character(3)"), "char(3)");
        assert_eq!(super::short_type("timestamp without time zone"), "timestamp");
        assert_eq!(super::short_type("timestamp(3) with time zone"), "timestamp(3) with time zone");
        assert_eq!(super::short_type("character varying[]"), "varchar[]");
        assert_eq!(super::short_type("integer"), "integer");
    }
}
