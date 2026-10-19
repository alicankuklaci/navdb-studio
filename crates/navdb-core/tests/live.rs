//! Live driver tests. Run against real servers with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live -- --test-threads=1`.
//!
//! Defaults match the docker containers used in development:
//! MySQL 127.0.0.1:13306 root/navdb, PostgreSQL 127.0.0.1:15432 postgres/navdb,
//! Redis 127.0.0.1:16379.

use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn mysql_cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_MYSQL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(13306),
        user: "root".into(),
        password: "navdb".into(),
        ..ConnectionConfig::new(DbKind::MySql)
    }
}

fn pg_cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(15432),
        user: "postgres".into(),
        password: "navdb".into(),
        database: "shop".into(),
        ..ConnectionConfig::new(DbKind::PostgreSql)
    }
}

fn exercise_table_roundtrip(d: &mut dyn Driver, scope: &Scope) {
    let dialect = d.dialect();
    let p = scope.path("navdb_items");
    let _ = d.execute(Some(scope), &format!("DROP TABLE IF EXISTS {}", dialect.qualify(&scope.path("navdb_child"))));
    let _ = d.execute(Some(scope), &format!("DROP TABLE IF EXISTS {}", dialect.qualify(&p)));
    let def = TableDef {
        path: p.clone(),
        columns: vec![
            ColumnDef { auto_increment: true, primary_key: true, nullable: false, ..ColumnDef::new("id", if dialect == Dialect::Postgres { "int4" } else { "int" }) },
            ColumnDef { nullable: false, default: Some("'x'".into()), comment: if dialect == Dialect::Sqlite { String::new() } else { "the name".into() }, ..ColumnDef::new("name", "varchar(50)") },
            ColumnDef::new("price", if dialect == Dialect::Postgres { "numeric(10,2)" } else { "decimal(10,2)" }),
        ],
        indexes: vec![IndexDef { name: "ix_navdb_items_name".into(), columns: vec!["name".into()], ..Default::default() }],
        comment: if dialect == Dialect::Sqlite { String::new() } else { "items table".into() },
        ..Default::default()
    };
    for s in ddl::create_table(dialect, &def) {
        d.execute(Some(scope), &s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    d.execute(Some(scope), &dml::insert_rows(dialect, &p, &["name".into(), "price".into()], &[
        vec![Value::Text("a'b".into()), Value::Text("1.50".into())],
        vec![Value::Text("c".into()), Value::Null],
    ]))
    .unwrap();
    let back = d.table_def(&p).unwrap();
    assert_eq!(back.columns.len(), 3);
    assert!(back.columns[0].auto_increment, "{:?}", back.columns[0]);
    assert_eq!(back.primary_key(), vec!["id".to_string()]);
    assert!(back.indexes.iter().any(|i| i.name == "ix_navdb_items_name"), "{:?}", back.indexes);
    assert_eq!(back.columns[1].comment, def.columns[1].comment);

    // designer-style alter: rename column, add column, change type, add FK
    let mut new = back.clone();
    new.columns[1].name = "title".into();
    for ix in new.indexes.iter_mut() {
        for c in ix.columns.iter_mut() {
            if c == "name" {
                *c = "title".into();
            }
        }
    }
    new.columns.push(ColumnDef::new("qty", if dialect == Dialect::Postgres { "int4" } else { "int" }));
    let stmts = ddl::alter_table_with_renames(dialect, &back, &new, &[("name".into(), "title".into())]);
    for s in &stmts {
        d.execute(Some(scope), s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    let after = d.table_def(&p).unwrap();
    assert_eq!(after.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "title", "price", "qty"]);
    // a second diff against the reloaded definition must be empty
    let mut again = after.clone();
    again.auto_increment = new.auto_increment;
    let extra = ddl::alter_table(dialect, &after, &again);
    assert!(extra.is_empty(), "non-idempotent diff: {:?}", extra);

    let child = TableDef {
        path: scope.path("navdb_child"),
        columns: vec![
            ColumnDef { primary_key: true, nullable: false, ..ColumnDef::new("id", "int") },
            ColumnDef::new("item_id", "int"),
        ],
        foreign_keys: vec![ForeignKeyDef {
            name: "fk_child_item".into(),
            columns: vec!["item_id".into()],
            ref_db: if dialect == Dialect::Postgres { scope.db.clone() } else { String::new() },
            ref_schema: scope.schema.clone(),
            ref_table: "navdb_items".into(),
            ref_columns: vec!["id".into()],
            on_delete: "CASCADE".into(),
            on_update: String::new(),
        }],
        ..Default::default()
    };
    for s in ddl::create_table(dialect, &child) {
        d.execute(Some(scope), &s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    let cdef = d.table_def(&child.path).unwrap();
    assert_eq!(cdef.foreign_keys.len(), 1);
    assert_eq!(cdef.foreign_keys[0].ref_table, "navdb_items");
    assert_eq!(cdef.foreign_keys[0].on_delete, "CASCADE");

    // listing + DDL + paging + editing
    let tables = d.list_objects(scope, ObjectKind::Table).unwrap();
    assert!(tables.iter().any(|t| t.name == "navdb_items"));
    let ddl_text = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl_text.to_lowercase().contains("navdb_items"));
    let page = d.query(Some(scope), &dml::select_page(dialect, &p, "", &[dml::SortSpec { column: "id".into(), descending: false }], 10, 0)).unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.text(0, 1), "a'b");
    let upd = dml::update_row(dialect, &p, &[("title".into(), Value::Text("z".into()))], &[("id".into(), page.rows[0][0].clone())]);
    assert_eq!(d.execute(Some(scope), &upd).unwrap()[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 2);

    // dump & restore into the same scope
    let mut buf = Vec::new();
    navdb_core::dump::dump(d, scope, &navdb_core::dump::DumpOptions { tables: vec!["navdb_items".into(), "navdb_child".into()], include_routines: false, include_views: false, ..Default::default() }, &mut buf, &mut |_, _, _| {}).unwrap();
    let sql = String::from_utf8(buf).unwrap();
    d.execute(Some(scope), &format!("DROP TABLE {}", dialect.qualify(&child.path))).unwrap();
    d.execute(Some(scope), &format!("DROP TABLE {}", dialect.qualify(&p))).unwrap();
    d.execute(Some(scope), &sql).unwrap_or_else(|e| panic!("restore failed: {:#}\n{}", e, sql));
    assert_eq!(d.count(&p).unwrap(), 2);

    d.execute(Some(scope), &format!("DROP TABLE {}", dialect.qualify(&child.path))).unwrap();
    d.execute(Some(scope), &format!("DROP TABLE {}", dialect.qualify(&p))).unwrap();
}

#[test]
fn mysql_live() {
    if !live() {
        return;
    }
    let mut d = driver::connect(&mysql_cfg()).expect("mysql connect");
    assert!(d.server_version().unwrap().starts_with('8'));
    assert!(d.list_databases().unwrap().contains(&"shop".to_string()));
    exercise_table_roundtrip(d.as_mut(), &Scope::new("shop", None));
    // routines, multi result sets
    d.execute(Some(&Scope::new("shop", None)), "DROP PROCEDURE IF EXISTS navdb_p").unwrap();
    d.execute(Some(&Scope::new("shop", None)), "DELIMITER $$\nCREATE PROCEDURE navdb_p() BEGIN SELECT 1 AS a; SELECT 2 AS b; END$$\nDELIMITER ;").unwrap();
    let r = d.execute(Some(&Scope::new("shop", None)), "CALL navdb_p()").unwrap();
    assert!(r.iter().filter(|x| x.is_query).count() >= 2, "{:?}", r);
    let ddl = d.object_ddl(&ObjPath::new("shop", None, "navdb_p"), ObjectKind::Procedure).unwrap();
    assert!(ddl.contains("CREATE"));
    d.execute(Some(&Scope::new("shop", None)), "DROP PROCEDURE navdb_p").unwrap();
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty());
    assert!(!d.list_users().unwrap().is_empty());
    assert!(d.charsets().unwrap().contains(&"utf8mb4".to_string()));
    let e = d.explain(Some(&Scope::new("shop", None)), "SELECT 1").unwrap();
    assert!(e.is_query);
}

#[test]
fn postgres_live() {
    if !live() {
        return;
    }
    let mut d = driver::connect(&pg_cfg()).expect("pg connect");
    assert!(d.server_version().unwrap().contains("PostgreSQL"));
    assert!(d.list_schemas("shop").unwrap().contains(&"public".to_string()));
    exercise_table_roundtrip(d.as_mut(), &Scope::new("shop", Some("public".into())));
    let scope = Scope::new("shop", Some("public".into()));
    d.execute(Some(&scope), "CREATE OR REPLACE FUNCTION navdb_add(a integer, b integer) RETURNS integer AS $$ BEGIN RETURN a + b; END; $$ LANGUAGE plpgsql").unwrap();
    let fns = d.list_objects(&scope, ObjectKind::Function).unwrap();
    let f = fns.iter().find(|f| f.name.starts_with("navdb_add(")).expect("function listed");
    let ddl = d.object_ddl(&scope.path(f.name.clone()), ObjectKind::Function).unwrap();
    assert!(ddl.contains("RETURN a + b"));
    let r = d.query(Some(&scope), "SELECT navdb_add(2, 3)").unwrap();
    assert_eq!(r.text(0, 0), "5");
    d.execute(Some(&scope), "DROP FUNCTION navdb_add(integer, integer)").unwrap();
    // switching database reconnects
    assert!(d.list_databases().unwrap().contains(&"postgres".to_string()));
    let r = d.query(Some(&Scope::new("postgres", None)), "SELECT current_database()").unwrap();
    assert_eq!(r.text(0, 0), "postgres");
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.variables.rows.is_empty());
    let user = UserSpec { name: "navdb_tmp_user".into(), password: "pw".into(), can_login: true, ..Default::default() };
    let _ = d.execute(None, "DROP ROLE IF EXISTS navdb_tmp_user");
    for s in d.user_sql(None, &user).unwrap() {
        d.execute(None, &s).unwrap();
    }
    assert!(d.list_users().unwrap().iter().any(|u| u.name == "navdb_tmp_user"));
    d.execute(None, &d.drop_user_sql(&UserInfo { name: "navdb_tmp_user".into(), ..Default::default() }).unwrap()).unwrap();
}

#[test]
fn redis_live() {
    if !live() {
        return;
    }
    let cfg = ConnectionConfig { host: "127.0.0.1".into(), port: std::env::var("NAVDB_REDIS_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(16379), ..ConnectionConfig::new(DbKind::Redis) };
    let Ok(mut d) = driver::connect(&cfg) else {
        eprintln!("redis not reachable; skipping");
        return;
    };
    assert!(d.server_version().unwrap().starts_with("Redis"));
    let scope = Scope::new("db15", None);
    d.execute(Some(&scope), "SET navdb:test \"hello world\"\nHSET navdb:h f1 v1 f2 v2").unwrap();
    let keys = d.list_objects(&scope, ObjectKind::Key).unwrap();
    assert!(keys.iter().any(|k| k.name == "navdb:test"));
    let r = d.query(Some(&scope), "GET navdb:test").unwrap();
    assert_eq!(r.text(0, 0), "hello world");
    let h = d.query(Some(&scope), "HGETALL navdb:h").unwrap();
    assert!(!h.rows.is_empty());
    d.execute(Some(&scope), "DEL navdb:test navdb:h").unwrap();
}
