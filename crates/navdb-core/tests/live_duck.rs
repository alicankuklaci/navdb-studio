//! DuckDB / Iceberg driver tests.
//!
//! DuckDB tests use temporary database files and always run. Iceberg tests need
//! network access (the `iceberg` extension is downloaded from
//! extensions.duckdb.org) and test data, so they are skipped unless `NAVDB_LIVE`
//! is set:
//! - path mode: `NAVDB_ICEBERG_PATH` = directory of Iceberg table folders
//!   (expects a table `navdb_events` with columns id, name, ts, score);
//! - REST mode: `NAVDB_ICEBERG_URI` (default `http://127.0.0.1:18181`) and optional
//!   `NAVDB_ICEBERG_WAREHOUSE`; the test creates its own namespace and table.

use navdb_core::dml::{FilterOp, FilterRule};
use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("navdb-duck-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn duck_cfg(path: &std::path::Path) -> ConnectionConfig {
    ConnectionConfig { database: path.to_string_lossy().to_string(), ..ConnectionConfig::new(DbKind::DuckDb) }
}

#[test]
fn duckdb_file_roundtrip() {
    let dir = temp_dir("rt");
    let file = dir.join("shop.duckdb");
    let other = dir.join("extra.db");
    let mut cfg = duck_cfg(&file);
    cfg.attached.push(("side".into(), other.to_string_lossy().to_string()));
    let mut d = driver::connect(&cfg).unwrap();
    assert_eq!(d.kind(), DbKind::DuckDb);
    let v = d.server_version().unwrap();
    assert!(v.starts_with("DuckDB v"), "{}", v);

    let db = cfg.default_database();
    assert_eq!(db, "shop");
    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&db) && dbs.contains(&"side".to_string()), "{:?}", dbs);
    assert!(!dbs.iter().any(|x| x == "system" || x == "temp"));
    let schemas = d.list_schemas(&db).unwrap();
    assert_eq!(schemas, vec!["main".to_string()]);

    let scope = Scope::new(db.clone(), Some("main".into()));
    let dl = d.dialect();
    assert_eq!(dl, Dialect::Generic);
    d.execute(
        Some(&scope),
        "CREATE SEQUENCE seq_items START 1;
         CREATE TABLE cats (id INTEGER PRIMARY KEY, label VARCHAR UNIQUE);
         CREATE TABLE items (
            id INTEGER PRIMARY KEY DEFAULT nextval('seq_items'),
            name VARCHAR NOT NULL DEFAULT 'x',
            price DECIMAL(10,2),
            cat_id INTEGER REFERENCES cats(id),
            tags VARCHAR[],
            CHECK (price >= 0)
         );
         CREATE INDEX ix_items_name ON items(name);
         COMMENT ON TABLE items IS 'items table';
         CREATE VIEW v_items AS SELECT id, name FROM items;
         CREATE MACRO add_one(x) AS x + 1;
         INSERT INTO cats VALUES (1, 'a'), (2, 'b');",
    )
    .unwrap();

    let names = |d: &mut Box<dyn Driver>, k| d.list_objects(&scope, k).unwrap().into_iter().map(|o| o.name).collect::<Vec<_>>();
    assert_eq!(names(&mut d, ObjectKind::Table), vec!["cats", "items"]);
    assert_eq!(names(&mut d, ObjectKind::View), vec!["v_items"]);
    assert_eq!(names(&mut d, ObjectKind::Function), vec!["add_one"]);
    assert_eq!(names(&mut d, ObjectKind::Sequence), vec!["seq_items"]);
    assert_eq!(names(&mut d, ObjectKind::Index), vec!["ix_items_name"]);
    let items_info = d.list_objects(&scope, ObjectKind::Table).unwrap().into_iter().find(|o| o.name == "items").unwrap();
    assert_eq!(items_info.comment, "items table");

    let p = scope.path("items");
    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.len(), 5);
    assert_eq!(def.primary_key(), vec!["id".to_string()]);
    assert!(def.columns[0].auto_increment, "{:?}", def.columns[0]);
    assert!(!def.columns[1].nullable);
    assert_eq!(def.columns[1].default.as_deref(), Some("'x'"));
    assert_eq!(def.columns[2].data_type, "DECIMAL(10,2)");
    assert_eq!(def.columns[4].data_type, "VARCHAR[]");
    assert!(def.indexes.iter().any(|i| i.name == "ix_items_name" && i.columns == vec!["name".to_string()]), "{:?}", def.indexes);
    assert_eq!(def.foreign_keys.len(), 1, "{:?}", def.foreign_keys);
    assert_eq!(def.foreign_keys[0].ref_table, "cats");
    assert_eq!(def.foreign_keys[0].ref_columns, vec!["id".to_string()]);
    assert_eq!(def.checks.len(), 1);
    assert_eq!(def.comment, "items table");
    let cats = d.table_def(&scope.path("cats")).unwrap();
    assert!(cats.indexes.iter().any(|i| i.unique && i.columns == vec!["label".to_string()]), "{:?}", cats.indexes);
    let vdef = d.table_def(&scope.path("v_items")).unwrap();
    assert_eq!(vdef.columns.len(), 2);

    // grid DML
    let r = d.execute(Some(&scope), &dml::insert_row(dl, &p, &["name".into(), "price".into(), "cat_id".into()], &[
        Value::Text("a'b".into()),
        Value::Text("1.50".into()),
        Value::Int(1),
    ]))
    .unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let rows: Vec<Vec<Value>> = (0..10).map(|i| vec![Value::Text(format!("n{}", i)), Value::Text(format!("{}.25", i)), Value::Null]).collect();
    let r = d.execute(Some(&scope), &dml::insert_rows(dl, &p, &["name".into(), "price".into(), "cat_id".into()], &rows)).unwrap();
    assert_eq!(r[0].affected_rows, 10);
    assert_eq!(d.count(&p).unwrap(), 11);

    let mut f = FilterRule::new("name");
    f.op = FilterOp::BeginsWith;
    f.value = "n".into();
    let w = dml::filters_to_where(dl, &[f]);
    let sort = [dml::SortSpec { column: "id".into(), descending: false }];
    let page = d.query(Some(&scope), &dml::select_page(dl, &p, &w, &sort, 3, 2)).unwrap();
    assert!(page.is_query);
    assert_eq!(page.rows.len(), 3);
    assert_eq!(page.get(0, "name"), Some(&Value::Text("n2".into())));
    assert_eq!(page.get(0, "price"), Some(&Value::Text("2.25".into())));
    assert_eq!(page.columns[2].type_name, "DECIMAL(10,2)");
    let c = d.query(Some(&scope), &dml::count_rows(dl, &p, &w)).unwrap();
    assert_eq!(c.rows[0][0].as_f64(), Some(10.0));

    let first = d.query(Some(&scope), "SELECT id FROM items WHERE name = 'a''b'").unwrap();
    let id = first.rows[0][0].clone();
    let r = d.execute(Some(&scope), &dml::update_row(dl, &p, &[("name".into(), Value::Text("renamed".into()))], &[("id".into(), id.clone())])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let back = d.query(Some(&scope), &format!("SELECT name FROM items WHERE id = {}", id)).unwrap();
    assert_eq!(back.rows[0][0], Value::Text("renamed".into()));
    let r = d.execute(Some(&scope), &dml::delete_row(dl, &p, &[("id".into(), id)])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 10);

    // DDL
    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("CREATE TABLE") && ddl.contains("CREATE INDEX ix_items_name"), "{}", ddl);
    assert!(d.object_ddl(&scope.path("v_items"), ObjectKind::View).unwrap().starts_with("CREATE VIEW"));
    assert!(d.object_ddl(&scope.path("seq_items"), ObjectKind::Sequence).unwrap().starts_with("CREATE SEQUENCE"));
    assert!(d.object_ddl(&scope.path("ix_items_name"), ObjectKind::Index).unwrap().starts_with("CREATE INDEX"));
    let m = d.object_ddl(&scope.path("add_one"), ObjectKind::Function).unwrap();
    assert!(m.starts_with("CREATE MACRO") && m.contains("(x)"), "{}", m);
    let r = d.query(Some(&scope), "SELECT add_one(41)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(42));

    // designer DDL generated with the Generic dialect works on DuckDB
    let t2 = TableDef {
        path: scope.path("designed"),
        columns: vec![
            ColumnDef { primary_key: true, nullable: false, ..ColumnDef::new("id", "INTEGER") },
            ColumnDef { default: Some("'d'".into()), ..ColumnDef::new("v", "VARCHAR") },
        ],
        indexes: vec![IndexDef { name: "ix_designed_v".into(), columns: vec!["v".into()], ..Default::default() }],
        ..Default::default()
    };
    for s in ddl::create_table(dl, &t2) {
        d.execute(Some(&scope), &s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    let back = d.table_def(&t2.path).unwrap();
    assert_eq!(back.primary_key(), vec!["id".to_string()]);
    let mut new = back.clone();
    new.columns.push(ColumnDef::new("extra", "BIGINT"));
    for s in ddl::alter_table(dl, &back, &new) {
        d.execute(Some(&scope), &s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    assert_eq!(d.table_def(&t2.path).unwrap().columns.len(), 3);

    // multi-statement results, scope switch to the attached database
    let side = Scope::new("side", Some("main".into()));
    let r = d.execute(Some(&side), "CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2); SELECT * FROM t ORDER BY x; SELECT current_database()").unwrap();
    assert_eq!(r.len(), 4);
    assert!(!r[0].is_query);
    assert_eq!(r[1].affected_rows, 2);
    assert!(r[2].is_query && r[2].rows.len() == 2);
    assert_eq!(r[3].rows[0][0], Value::Text("side".into()));
    assert_eq!(d.list_objects(&side, ObjectKind::Table).unwrap().len(), 1);

    // explain / snapshot / errors
    let e = d.explain(Some(&scope), "SELECT * FROM items WHERE id > 3").unwrap();
    assert!(e.rows.len() > 1 && e.rows.iter().any(|r| r[0].to_string().contains("items")), "{:?}", e.rows);
    let snap = d.server_snapshot().unwrap();
    assert!(snap.variables.rows.iter().any(|r| r[0] == Value::Text("threads".into())));
    assert!(!snap.status.rows.is_empty());
    let err = d.execute(Some(&scope), "SELECT * FROM no_such_table").unwrap_err();
    assert!(format!("{:#}", err).contains("no_such_table"), "{:#}", err);
    let err = d.execute(Some(&scope), "SELEC 1").unwrap_err();
    assert!(format!("{:#}", err).to_lowercase().contains("syntax"), "{:#}", err);

    d.execute(Some(&scope), "DROP VIEW v_items; DROP TABLE items; DROP TABLE designed").unwrap();
    assert_eq!(names(&mut d, ObjectKind::Table), vec!["cats"]);
    drop(d);

    // read-only reopen
    let mut ro = duck_cfg(&file);
    ro.read_only = true;
    let mut d = driver::connect(&ro).unwrap();
    assert_eq!(d.count(&scope.path("cats")).unwrap(), 2);
    assert!(d.execute(Some(&scope), "INSERT INTO cats VALUES (3, 'c')").is_err());
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn duckdb_in_memory_default() {
    let cfg = ConnectionConfig::new(DbKind::DuckDb);
    assert_eq!(cfg.default_database(), "memory");
    let mut d = driver::connect(&cfg).unwrap();
    assert_eq!(d.list_databases().unwrap(), vec!["memory".to_string()]);
    let r = d.query(Some(&Scope::new("memory", Some("main".into()))), "SELECT 42 AS answer").unwrap();
    assert_eq!(r.columns[0].type_name, "INTEGER");
    assert_eq!(r.rows[0][0], Value::Int(42));
}

fn iceberg_path_cfg(path: &str) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::Iceberg);
    cfg.options.insert("catalog_type".into(), "path".into());
    cfg.options.insert("path".into(), path.into());
    cfg
}

#[test]
fn iceberg_path_mode() {
    if !live() {
        return;
    }
    let Ok(path) = std::env::var("NAVDB_ICEBERG_PATH") else {
        eprintln!("NAVDB_ICEBERG_PATH not set, skipping");
        return;
    };
    let cfg = iceberg_path_cfg(&path);
    let mut d = driver::connect(&cfg).unwrap();
    assert_eq!(d.kind(), DbKind::Iceberg);
    let v = d.server_version().unwrap();
    assert!(v.contains("iceberg extension"), "{}", v);
    assert_eq!(cfg.default_database(), "iceberg");
    assert_eq!(d.list_databases().unwrap(), vec!["iceberg".to_string()]);
    assert_eq!(d.list_schemas("iceberg").unwrap(), vec!["main".to_string()]);
    let scope = Scope::new("iceberg", Some("main".into()));
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert!(tables.iter().any(|t| t.name == "navdb_events"), "{:?}", tables);
    let p = scope.path("navdb_events");
    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name", "ts", "score"]);
    let n = d.count(&p).unwrap();
    assert!(n >= 3, "{}", n);
    let mut f = FilterRule::new("id");
    f.op = FilterOp::Ge;
    f.value = "2".into();
    let w = dml::filters_to_where(d.dialect(), &[f]);
    let page = d
        .query(Some(&scope), &dml::select_page(d.dialect(), &p, &w, &[dml::SortSpec { column: "id".into(), descending: false }], 1, 1))
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.get(0, "id"), Some(&Value::Int(3)));
    assert!(d.object_ddl(&p, ObjectKind::Table).unwrap().contains("iceberg_scan"));
    let err = d.execute(Some(&scope), &dml::delete_row(d.dialect(), &p, &[("id".into(), Value::Int(1))])).unwrap_err();
    assert!(format!("{:#}", err).contains("read-only"), "{:#}", err);
    assert!(d.execute(Some(&scope), "SELECT * FROM nope").is_err());
}

#[test]
fn iceberg_rest_mode() {
    if !live() {
        return;
    }
    let uri = std::env::var("NAVDB_ICEBERG_URI").unwrap_or_else(|_| "http://127.0.0.1:18181".into());
    let mut cfg = ConnectionConfig::new(DbKind::Iceberg);
    cfg.options.insert("uri".into(), uri);
    if let Ok(w) = std::env::var("NAVDB_ICEBERG_WAREHOUSE") {
        cfg.options.insert("warehouse".into(), w);
    }
    for (env, key) in [
        ("NAVDB_ICEBERG_S3_ENDPOINT", "s3_endpoint"),
        ("NAVDB_ICEBERG_S3_ACCESS_KEY", "s3_access_key"),
        ("NAVDB_ICEBERG_S3_SECRET_KEY", "s3_secret_key"),
        ("NAVDB_ICEBERG_S3_REGION", "s3_region"),
    ] {
        if let Ok(v) = std::env::var(env) {
            cfg.options.insert(key.into(), v);
        }
    }
    let mut d = match driver::connect(&cfg) {
        Ok(d) => d,
        Err(e) => panic!("connect: {:#}", e),
    };
    assert!(d.server_version().unwrap().contains("iceberg"));
    assert_eq!(d.list_databases().unwrap(), vec!["iceberg".to_string()]);
    let root = Scope::new("iceberg", None);
    // (the iceberg extension does not see a namespace created after a failed lookup of it in the same session)
    d.execute(Some(&root), "CREATE SCHEMA IF NOT EXISTS iceberg.navdb_ns").unwrap();
    let _ = d.execute(Some(&root), "DROP TABLE IF EXISTS iceberg.navdb_ns.items");
    let schemas = d.list_schemas("iceberg").unwrap();
    assert!(schemas.contains(&"navdb_ns".to_string()), "{:?}", schemas);
    let scope = Scope::new("iceberg", Some("navdb_ns".into()));
    d.execute(Some(&scope), "CREATE TABLE iceberg.navdb_ns.items (id INTEGER, name VARCHAR)").unwrap();
    let p = scope.path("items");
    let dl = d.dialect();
    d.execute(Some(&scope), &dml::insert_rows(dl, &p, &["id".into(), "name".into()], &[
        vec![Value::Int(1), Value::Text("a".into())],
        vec![Value::Int(2), Value::Text("b".into())],
    ]))
    .unwrap();
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert!(tables.iter().any(|t| t.name == "items"), "{:?}", tables);
    let def = d.table_def(&p).unwrap();
    assert_eq!(def.columns.len(), 2);
    assert_eq!(d.count(&p).unwrap(), 2);
    let page = d.query(Some(&scope), &dml::select_page(dl, &p, "\"id\" > 1", &[], 10, 0)).unwrap();
    assert_eq!(page.rows, vec![vec![Value::Int(2), Value::Text("b".into())]]);
    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.contains("CREATE TABLE"), "{}", ddl);
    match d.execute(Some(&scope), &dml::update_row(dl, &p, &[("name".into(), Value::Text("z".into()))], &[("id".into(), Value::Int(2))])) {
        Ok(r) => {
            assert_eq!(r[0].affected_rows, 1);
            d.execute(Some(&scope), &dml::delete_row(dl, &p, &[("id".into(), Value::Int(1))])).unwrap();
            assert_eq!(d.count(&p).unwrap(), 1);
        }
        Err(e) => eprintln!("update not supported by this iceberg extension: {:#}", e),
    }
    assert!(d.execute(Some(&scope), "SELECT * FROM iceberg.navdb_ns.missing").is_err());
    d.execute(Some(&scope), "DROP TABLE iceberg.navdb_ns.items").unwrap();
    d.execute(Some(&root), "DROP SCHEMA iceberg.navdb_ns").unwrap();
}
