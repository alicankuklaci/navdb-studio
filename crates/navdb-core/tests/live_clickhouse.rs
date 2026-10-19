//! Live ClickHouse test. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_clickhouse -- --nocapture`.
//! Defaults match the dev container: 127.0.0.1:18123 default/navdb.

use navdb_core::driver;
use navdb_core::*;

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(18123),
        user: "default".into(),
        password: std::env::var("NAVDB_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "navdb".into()),
        ..ConnectionConfig::new(DbKind::ClickHouse)
    }
}

#[test]
fn clickhouse_live() {
    if std::env::var("NAVDB_LIVE").is_err() {
        return;
    }
    let mut d = driver::connect(&cfg()).unwrap();
    let v = d.server_version().unwrap();
    assert!(v.starts_with("ClickHouse "), "{v}");
    let dialect = d.dialect();

    d.execute(None, "DROP DATABASE IF EXISTS navdb_test; CREATE DATABASE navdb_test").unwrap();
    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&"navdb_test".to_string()), "{dbs:?}");
    assert_eq!(dbs.last().map(|s| s.as_str()), Some("system"), "{dbs:?}");

    let scope = Scope::new("navdb_test", None);
    let p = scope.path("items");
    d.execute(
        Some(&scope),
        "CREATE TABLE items (id UInt32, name String DEFAULT 'x' COMMENT 'the name', price Nullable(Decimal(10, 2)), tags Array(String), \
         INDEX ix_name name TYPE bloom_filter GRANULARITY 1) ENGINE = MergeTree ORDER BY id COMMENT 'items table'",
    )
    .unwrap();
    d.execute(Some(&scope), "CREATE VIEW v_items AS SELECT id, name FROM items").unwrap();

    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["items"]);
    assert_eq!(tables[0].engine, "MergeTree");
    assert_eq!(tables[0].comment, "items table");
    let views = d.list_objects(&scope, ObjectKind::View).unwrap();
    assert_eq!(views.len(), 1);

    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.len(), 4);
    assert_eq!(def.primary_key(), vec!["id".to_string()]);
    assert_eq!(def.engine, "MergeTree");
    assert_eq!(def.columns[1].default.as_deref(), Some("'x'"));
    assert_eq!(def.columns[1].comment, "the name");
    assert!(def.columns[2].nullable && !def.columns[0].nullable);
    assert!(def.indexes.iter().any(|i| i.name == "ix_name"), "{:?}", def.indexes);

    let cols: Vec<String> = ["id", "name", "price", "tags"].iter().map(|s| s.to_string()).collect();
    let r = d
        .execute(
            Some(&scope),
            &dml::insert_row(dialect, &p, &cols, &[Value::Int(1), Value::Text("a'b".into()), Value::Text("1.50".into()), Value::Text("['t1']".into())]),
        )
        .unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let rows: Vec<Vec<Value>> = (2..=12)
        .map(|i| vec![Value::Int(i), Value::Text(format!("n{i}")), Value::Null, Value::Text("[]".into())])
        .collect();
    let r = d.execute(Some(&scope), &dml::insert_rows(dialect, &p, &cols, &rows)).unwrap();
    assert_eq!(r[0].affected_rows, 11);
    assert_eq!(d.count(&p).unwrap(), 12);

    let sort = [dml::SortSpec { column: "id".into(), descending: false }];
    let page = d.query(Some(&scope), &dml::select_page(dialect, &p, "", &sort, 5, 5)).unwrap();
    assert!(page.is_query);
    assert_eq!(page.rows.len(), 5);
    assert_eq!(page.rows[0][0], Value::UInt(6));
    assert_eq!(page.columns[2].type_name, "Nullable(Decimal(10, 2))");

    let mut rule = dml::FilterRule::new("name");
    rule.op = dml::FilterOp::Contains;
    rule.value = "'b".into();
    let w = dml::filters_to_where(dialect, &[rule]);
    let page = d.query(Some(&scope), &dml::select_page(dialect, &p, &w, &sort, 10, 0)).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0], vec![Value::UInt(1), Value::Text("a'b".into()), Value::Text("1.50".into()), Value::Text("[\"t1\"]".into())]);
    let n = d.query(Some(&scope), &dml::count_rows(dialect, &p, &w)).unwrap();
    assert_eq!(n.rows[0][0], Value::UInt(1));

    // grid edits: UPDATE / DELETE are rewritten to synchronous mutations
    let upd = dml::update_row(dialect, &p, &[("name".into(), Value::Text("z".into()))], &[("id".into(), Value::UInt(1))]);
    let r = d.execute(Some(&scope), &upd).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let got = d.query(Some(&scope), "SELECT name FROM items WHERE id = 1").unwrap();
    assert_eq!(got.rows[0][0], Value::Text("z".into()));
    // no-key style update (all original values)
    let upd = dml::update_row(
        dialect,
        &p,
        &[("name".into(), Value::Text("n2b".into()))],
        &[("id".into(), Value::UInt(2)), ("name".into(), Value::Text("n2".into())), ("price".into(), Value::Null)],
    );
    d.execute(Some(&scope), &upd).unwrap();
    let got = d.query(Some(&scope), "SELECT name FROM items WHERE id = 2").unwrap();
    assert_eq!(got.rows[0][0], Value::Text("n2b".into()));
    let del = dml::delete_row(dialect, &p, &[("id".into(), Value::UInt(3))]);
    let r = d.execute(Some(&scope), &del).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 11);

    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("CREATE TABLE navdb_test.items") && ddl.contains("MergeTree"), "{ddl}");
    let ddl = d.object_ddl(&scope.path("v_items"), ObjectKind::View).unwrap();
    assert!(ddl.contains("VIEW"), "{ddl}");

    // multi-statement script with USE and SET tracked across requests
    let res = d
        .execute(None, "USE navdb_test; SET max_threads = 3; SELECT count() FROM items; SELECT getSetting('max_threads') AS t, currentDatabase() AS db")
        .unwrap();
    assert_eq!(res.len(), 4);
    assert!(!res[0].is_query && !res[1].is_query);
    assert_eq!(res[2].rows[0][0], Value::UInt(11));
    assert_eq!(res[3].rows[0][0].to_string(), "3");
    assert_eq!(res[3].rows[0][1], Value::Text("navdb_test".into()));

    // types
    let r = d
        .query(None, "SELECT toInt64(-5) a, toUInt64(18446744073709551615) b, 1.5::Float64 c, true d, map('k', 1) e, (1, 'x') f, toDate('2024-01-02') g, NULL h")
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(-5),
            Value::UInt(u64::MAX),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Text("{\"k\":1}".into()),
            Value::Text("[1,\"x\"]".into()),
            Value::Text("2024-01-02".into()),
            Value::Null
        ]
    );

    // UDF
    d.execute(None, "DROP FUNCTION IF EXISTS navdb_double; CREATE FUNCTION navdb_double AS (x) -> x * 2").unwrap();
    let f = d.list_objects(&scope, ObjectKind::Function).unwrap();
    assert!(f.iter().any(|o| o.name == "navdb_double"), "{f:?}");
    let ddl = d.object_ddl(&scope.path("navdb_double"), ObjectKind::Function).unwrap();
    assert!(ddl.starts_with("CREATE FUNCTION navdb_double"), "{ddl}");
    d.execute(None, "DROP FUNCTION navdb_double").unwrap();

    let ex = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert!(!ex.rows.is_empty());

    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty() && !snap.variables.rows.is_empty() && !snap.status.rows.is_empty());
    let users = d.list_users().unwrap();
    assert!(users.iter().any(|u| u.name == "default"));
    d.kill_process("no-such-query").unwrap();

    let err = d.execute(Some(&scope), "SELEC 1").unwrap_err();
    assert!(format!("{err:#}").contains("Syntax error"), "{err:#}");
    let err = d.execute(Some(&scope), "SELECT * FROM missing_table").unwrap_err();
    assert!(format!("{err:#}").contains("UNKNOWN_TABLE"), "{err:#}");

    // read-only connections reject writes
    let mut ro = driver::connect(&ConnectionConfig { read_only: true, ..cfg() }).unwrap();
    assert!(ro.execute(Some(&scope), "INSERT INTO items (id) VALUES (99)").is_err());
    assert_eq!(ro.count(&p).unwrap(), 11);

    let bad = driver::connect(&ConnectionConfig { password: "wrong".into(), ..cfg() });
    assert!(bad.is_err());

    d.execute(None, "DROP TABLE navdb_test.v_items; DROP TABLE navdb_test.items; DROP DATABASE navdb_test").unwrap();
    assert!(!d.list_databases().unwrap().contains(&"navdb_test".to_string()));
}
