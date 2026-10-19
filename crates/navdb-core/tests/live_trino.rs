//! Live Trino / Presto test. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_trino -- --nocapture`.
//! Defaults match the dev container: 127.0.0.1:18080, user navdb, no password.
//! `NAVDB_TRINO_KIND=presto NAVDB_TRINO_PORT=18081` runs it against Presto.

use navdb_core::driver;
use navdb_core::*;

fn cfg() -> ConnectionConfig {
    let presto = std::env::var("NAVDB_TRINO_KIND").is_ok_and(|k| k.eq_ignore_ascii_case("presto"));
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_TRINO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(18080),
        user: "navdb".into(),
        database: "tpch".into(),
        options: [("schema".to_string(), "tiny".to_string())].into_iter().collect(),
        ..ConnectionConfig::new(if presto { DbKind::Presto } else { DbKind::Trino })
    }
}

#[test]
fn trino_live() {
    if std::env::var("NAVDB_LIVE").is_err() {
        return;
    }
    let c = cfg();
    let trino = c.kind == DbKind::Trino;
    let mut d = driver::connect(&c).unwrap();
    let v = d.server_version().unwrap();
    println!("{v}");
    assert!(v.starts_with(c.kind.label()), "{v}");
    let dialect = d.dialect();

    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&"memory".to_string()) && dbs.contains(&"tpch".to_string()), "{dbs:?}");
    assert_eq!(dbs.last().map(|s| s.as_str()), Some("system"), "{dbs:?}");
    let schemas = d.list_schemas("tpch").unwrap();
    assert!(schemas.contains(&"tiny".to_string()) && !schemas.contains(&"information_schema".to_string()), "{schemas:?}");

    // reads from tpch.tiny (default catalog/schema from the config)
    let tiny = Scope::new("tpch", Some("tiny".into()));
    let tables = d.list_objects(&tiny, ObjectKind::Table).unwrap();
    assert!(tables.iter().any(|t| t.name == "nation"), "{tables:?}");
    let nation = tiny.path("nation");
    let def = d.table_def(&nation).unwrap();
    assert_eq!(def.columns[0].name, "nationkey");
    assert!(def.primary_key().is_empty());
    assert_eq!(d.count(&nation).unwrap(), 25);
    let r = d.query(None, "SELECT count(*) FROM nation").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(25));
    let sort = [dml::SortSpec { column: "nationkey".into(), descending: false }];
    let page = d.query(Some(&tiny), &dml::select_page(dialect, &nation, "", &sort, 5, 10)).unwrap();
    assert_eq!(page.rows.len(), 5);
    assert_eq!(page.rows[0][0], Value::Int(10));
    assert_eq!(page.columns[0].type_name, "bigint");

    // writes in the memory catalog
    for s in ["DROP VIEW IF EXISTS memory.navdb_test.v_items", "DROP TABLE IF EXISTS memory.navdb_test.items", "DROP SCHEMA IF EXISTS memory.navdb_test"] {
        d.execute(None, s).ok();
    }
    d.execute(None, "CREATE SCHEMA memory.navdb_test;").unwrap();
    let scope = Scope::new("memory", Some("navdb_test".into()));
    assert!(d.list_schemas("memory").unwrap().contains(&"navdb_test".to_string()));
    let p = scope.path("items");
    // Presto's memory connector supports neither NOT NULL nor comments
    let create = if trino {
        "CREATE TABLE items (id integer NOT NULL, name varchar COMMENT 'the name', price decimal(10, 2), tags array(varchar), raw varbinary) COMMENT 'items table'"
    } else {
        "CREATE TABLE items (id integer, name varchar, price decimal(10, 2), tags array(varchar), raw varbinary)"
    };
    d.execute(Some(&scope), create).unwrap();
    d.execute(Some(&scope), "CREATE VIEW v_items AS SELECT id, name FROM items").unwrap();
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["items"]);
    if trino {
        assert_eq!(tables[0].comment, "items table");
    }
    let views = d.list_objects(&scope, ObjectKind::View).unwrap();
    assert_eq!(views.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["v_items"]);
    assert!(d.list_objects(&scope, ObjectKind::MaterializedView).unwrap().is_empty());

    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.len(), 5);
    assert_eq!(def.columns[2].data_type, "decimal(10,2)");
    assert!(def.columns[1].nullable);
    if trino {
        assert!(!def.columns[0].nullable);
        assert_eq!(def.columns[1].comment, "the name");
        assert_eq!(def.comment, "items table");
    }

    let cols: Vec<String> = ["id", "name", "price"].iter().map(|s| s.to_string()).collect();
    // grid values arrive as text: the driver casts them to the column types
    let r = d
        .execute(Some(&scope), &dml::insert_row(dialect, &p, &cols, &[Value::Text("1".into()), Value::Text("a'b".into()), Value::Text("1.50".into())]))
        .unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let rows: Vec<Vec<Value>> = (2..=12).map(|i| vec![Value::Int(i), Value::Text(format!("n{i}")), Value::Null]).collect();
    let r = d.execute(Some(&scope), &dml::insert_rows(dialect, &p, &cols, &rows)).unwrap();
    assert_eq!(r[0].affected_rows, 11);
    assert!(!r[0].is_query);
    assert_eq!(d.count(&p).unwrap(), 12);
    d.execute(Some(&scope), "INSERT INTO items (id, tags, raw) VALUES (13, ARRAY['t1'], X'0102')").unwrap();

    let sort = [dml::SortSpec { column: "id".into(), descending: false }];
    let page = d.query(Some(&scope), &dml::select_page(dialect, &p, "", &sort, 5, 5)).unwrap();
    assert!(page.is_query);
    assert_eq!(page.rows.len(), 5);
    assert_eq!(page.rows[0][0], Value::Int(6));
    assert_eq!(page.columns[2].type_name.replace(" ", ""), "decimal(10,2)");

    let mut rule = dml::FilterRule::new("name");
    rule.op = dml::FilterOp::Contains;
    rule.value = "'b".into();
    let w = dml::filters_to_where(dialect, &[rule]);
    let page = d.query(Some(&scope), &dml::select_page(dialect, &p, &w, &sort, 10, 0)).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(&page.rows[0][..3], &[Value::Int(1), Value::Text("a'b".into()), Value::Text("1.50".into())]);
    let n = d.query(Some(&scope), &dml::count_rows(dialect, &p, &w)).unwrap();
    assert_eq!(n.rows[0][0], Value::Int(1));
    let r = d.query(Some(&scope), "SELECT tags, raw FROM items WHERE id = 13").unwrap();
    assert_eq!(r.rows[0], vec![Value::Text("[\"t1\"]".into()), Value::Bytes(vec![1, 2])]);

    // the memory connector cannot modify rows: the server error is surfaced
    let upd = dml::update_row(dialect, &p, &[("name".into(), Value::Text("z".into()))], &[("id".into(), Value::Int(1))]);
    let err = d.execute(Some(&scope), &upd).unwrap_err();
    println!("update: {err:#}");
    assert!(format!("{err:#}").contains("NOT_SUPPORTED") || format!("{err:#}").contains("not support"), "{err:#}");
    let del = dml::delete_row(dialect, &p, &[("id".into(), Value::Int(3))]);
    let err = d.execute(Some(&scope), &del).unwrap_err();
    assert!(format!("{err:#}").contains("not support"), "{err:#}");
    assert_eq!(d.count(&p).unwrap(), 13);

    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("CREATE TABLE memory.navdb_test.items"), "{ddl}");
    let ddl = d.object_ddl(&scope.path("v_items"), ObjectKind::View).unwrap();
    assert!(ddl.contains("VIEW"), "{ddl}");

    // multi-statement script with USE and SET SESSION tracked across requests
    let res = d
        .execute(
            None,
            "USE memory.navdb_test; SET SESSION query_max_run_time = '1h'; SELECT count(*) FROM items; \
             SELECT table_schema FROM information_schema.tables WHERE table_name = 'items'; SHOW SESSION LIKE 'query_max_run_time';",
        )
        .unwrap();
    assert_eq!(res.len(), 5);
    assert!(!res[0].is_query && !res[1].is_query);
    assert_eq!(res[2].rows[0][0], Value::Int(13));
    assert_eq!(res[3].rows, vec![vec![Value::Text("navdb_test".into())]]);
    assert_eq!(res[4].rows[0][1], Value::Text("1h".into()), "{:?}", res[4].rows);

    // types
    let r = d
        .query(
            None,
            "SELECT BIGINT '-5' a, 1.5e0 b, true c, MAP(ARRAY['k'], ARRAY[1]) d, ROW(1, 'x') e, DATE '2024-01-02' f, NULL g, DECIMAL '1.50' h, X'ff00' i",
        )
        .unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Int(-5),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Text("{\"k\":1}".into()),
            Value::Text("[1,\"x\"]".into()),
            Value::Text("2024-01-02".into()),
            Value::Null,
            Value::Text("1.50".into()),
            Value::Bytes(vec![0xff, 0]),
        ]
    );

    let ex = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1;").unwrap();
    assert!(!ex.rows.is_empty());

    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty() && !snap.variables.rows.is_empty() && !snap.status.rows.is_empty());
    let err = d.kill_process("20200101_000000_00000_xxxxx").unwrap_err();
    println!("kill: {err:#}");

    let err = d.execute(Some(&scope), "SELEC 1").unwrap_err();
    assert!(format!("{err:#}").contains("SYNTAX_ERROR"), "{err:#}");
    let err = d.execute(Some(&scope), "SELECT * FROM missing_table").unwrap_err();
    assert!(format!("{err:#}").contains("missing_table"), "{err:#}");

    // read-only connections reject writes client-side
    let mut ro = driver::connect(&ConnectionConfig { read_only: true, ..cfg() }).unwrap();
    assert!(ro.execute(Some(&scope), "INSERT INTO items (id) VALUES (99)").is_err());
    assert_eq!(ro.count(&p).unwrap(), 13);

    // password over plain http is refused as Trino requires
    assert!(driver::connect(&ConnectionConfig { password: "x".into(), ..cfg() }).is_err());
    // session properties option
    let mut opts = cfg();
    opts.options.insert("session_properties".into(), "query_max_run_time=2h".into());
    let mut s = driver::connect(&opts).unwrap();
    let r = s.query(None, "SHOW SESSION LIKE 'query_max_run_time'").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("2h".into()));

    d.execute(None, "DROP VIEW memory.navdb_test.v_items; DROP TABLE memory.navdb_test.items; DROP SCHEMA memory.navdb_test").unwrap();
    assert!(!d.list_schemas("memory").unwrap().contains(&"navdb_test".to_string()));
}
