//! Live ClickHouse driver test. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_clickhouse -- --test-threads=1`.
//!
//! Defaults match the docker container `navdb-test-clickhouse`: 127.0.0.1:18123 default/navdb.

use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(18123),
        user: "default".into(),
        password: "navdb".into(),
        ..ConnectionConfig::new(DbKind::ClickHouse)
    }
}

#[test]
fn clickhouse_roundtrip() {
    if !live() {
        return;
    }
    let mut d = driver::connect(&cfg()).expect("connect");
    let v = d.server_version().unwrap();
    assert!(v.starts_with("ClickHouse "), "{v}");
    let dialect = d.dialect();

    d.execute(None, "CREATE DATABASE IF NOT EXISTS navdb_test").unwrap();
    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&"navdb_test".to_string()) && dbs.contains(&"default".to_string()), "{dbs:?}");
    assert_eq!(dbs.last().map(|s| s.as_str()), Some("system"), "{dbs:?}");
    assert!(d.list_schemas("navdb_test").unwrap().is_empty());

    let scope = Scope::new("navdb_test", None);
    let p = scope.path("items");
    d.execute(Some(&scope), "DROP TABLE IF EXISTS items; DROP VIEW IF EXISTS items_v").unwrap();
    d.execute(
        Some(&scope),
        "CREATE TABLE items (id UInt32, name String DEFAULT 'x' COMMENT 'the name', price Nullable(Decimal(10, 2)), tags Array(String), \
         INDEX ix_name name TYPE bloom_filter GRANULARITY 1) ENGINE = MergeTree ORDER BY id COMMENT 'items table'",
    )
    .unwrap();
    d.execute(Some(&scope), "CREATE VIEW items_v AS SELECT id, name FROM items").unwrap();

    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["items"]);
    assert_eq!(tables[0].engine, "MergeTree");
    assert_eq!(tables[0].comment, "items table");
    let views = d.list_objects(&scope, ObjectKind::View).unwrap();
    assert_eq!(views.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["items_v"]);

    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.len(), 4);
    assert_eq!(def.primary_key(), vec!["id".to_string()]);
    assert_eq!(def.engine, "MergeTree");
    assert_eq!(def.columns[1].default.as_deref(), Some("'x'"));
    assert_eq!(def.columns[1].comment, "the name");
    assert!(def.columns[2].nullable && !def.columns[0].nullable);
    assert!(def.indexes.iter().any(|i| i.name == "ix_name"), "{:?}", def.indexes);

    let cols: Vec<String> = ["id", "name", "price"].iter().map(|s| s.to_string()).collect();
    d.execute(Some(&scope), &dml::insert_row(dialect, &p, &cols, &[Value::Int(1), Value::Text("a'b".into()), Value::Text("1.50".into())]))
        .unwrap();
    let rows: Vec<Vec<Value>> = (2..=10).map(|i| vec![Value::Int(i), Value::Text(format!("n{i}")), Value::Null]).collect();
    let r = d.execute(Some(&scope), &dml::insert_rows(dialect, &p, &cols, &rows)).unwrap();
    assert_eq!(r[0].affected_rows, 9);
    assert_eq!(d.count(&p).unwrap(), 10);

    let sort = [dml::SortSpec { column: "id".into(), descending: false }];
    let page = d.query(Some(&scope), &dml::select_page(dialect, &p, "", &sort, 3, 2)).unwrap();
    assert!(page.is_query);
    assert_eq!(page.rows.len(), 3);
    assert_eq!(page.rows[0][0], Value::UInt(3));
    assert_eq!(page.columns[2].type_name, "Nullable(Decimal(10, 2))");
    assert_eq!(page.rows[0][3], Value::Text("[]".into()));

    let mut f = dml::FilterRule::new("name");
    f.op = dml::FilterOp::BeginsWith;
    f.value = "a'".into();
    let w = dml::filters_to_where(dialect, &[f]);
    let r = d.query(Some(&scope), &dml::select_page(dialect, &p, &w, &[], 100, 0)).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][2], Value::Text("1.50".into()));

    let key = [("id".to_string(), Value::UInt(1))];
    d.execute(Some(&scope), &dml::update_row(dialect, &p, &[("name".into(), Value::Text("changed".into()))], &key)).unwrap();
    let r = d.query(Some(&scope), "SELECT name FROM items WHERE id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("changed".into()));

    d.execute(Some(&scope), &dml::delete_row(dialect, &p, &[("id".to_string(), Value::UInt(2))])).unwrap();
    assert_eq!(d.count(&p).unwrap(), 9);

    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("CREATE TABLE navdb_test.items"), "{ddl}");
    assert!(ddl.contains("MergeTree"), "{ddl}");
    let vddl = d.object_ddl(&scope.path("items_v"), ObjectKind::View).unwrap();
    assert!(vddl.contains("VIEW"), "{vddl}");

    let multi = d
        .execute(None, "USE navdb_test; SELECT count() AS c FROM items; INSERT INTO items (id) VALUES (100); SELECT max(id) FROM items")
        .unwrap();
    assert_eq!(multi.len(), 4);
    assert!(!multi[0].is_query);
    assert_eq!(multi[1].rows[0][0], Value::UInt(9));
    assert!(!multi[2].is_query);
    assert_eq!(multi[3].rows[0][0], Value::UInt(100));

    let ex = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert!(!ex.rows.is_empty());

    let err = d.execute(Some(&scope), "SELEC 1").unwrap_err();
    assert!(format!("{err:#}").contains("Syntax error"), "{err:#}");
    let err = d.execute(Some(&scope), "SELECT * FROM no_such_table").unwrap_err();
    assert!(format!("{err:#}").contains("UNKNOWN_TABLE"), "{err:#}");

    d.execute(None, "CREATE FUNCTION IF NOT EXISTS navdb_plus1 AS (x) -> x + 1").unwrap();
    let fns = d.list_objects(&scope, ObjectKind::Function).unwrap();
    assert!(fns.iter().any(|f| f.name == "navdb_plus1"));
    let fddl = d.object_ddl(&scope.path("navdb_plus1"), ObjectKind::Function).unwrap();
    assert!(fddl.starts_with("CREATE FUNCTION navdb_plus1"), "{fddl}");
    d.execute(None, "DROP FUNCTION navdb_plus1").unwrap();

    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty());
    assert!(!snap.variables.rows.is_empty());
    assert!(!snap.status.rows.is_empty());
    d.kill_process("no-such-query").unwrap();
    assert!(d.list_users().unwrap().iter().any(|u| u.name == "default"));

    let r = d.query(None, "SELECT toInt64(-9223372036854775808) a, 18446744073709551615 b, [1, 2] c, map('k', 1) m, true t, 0.5 f, toDate('2024-01-02') dt").unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(i64::MIN),
            Value::UInt(u64::MAX),
            Value::Text("[1,2]".into()),
            Value::Text("{\"k\":1}".into()),
            Value::Bool(true),
            Value::Float(0.5),
            Value::Text("2024-01-02".into()),
        ]
    );

    d.execute(None, "DROP DATABASE navdb_test").unwrap();
    assert!(!d.list_databases().unwrap().contains(&"navdb_test".to_string()));
}
