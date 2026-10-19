//! Wire-compatible products served by the existing drivers. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_compat -- --test-threads=1`
//! against: CockroachDB 127.0.0.1:26257 (insecure, root), TiDB 127.0.0.1:14000
//! (root, no password), Valkey 127.0.0.1:16380.

use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn roundtrip(d: &mut dyn Driver, scope: &Scope) {
    let dialect = d.dialect();
    let p = scope.path("navdb_compat");
    let _ = d.execute(Some(scope), &format!("DROP TABLE IF EXISTS {}", dialect.qualify(&p)));
    d.execute(Some(scope), &format!("CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(50))", dialect.qualify(&p))).unwrap();
    let names: Vec<String> = d.list_objects(scope, ObjectKind::Table).unwrap().into_iter().map(|o| o.name).collect();
    assert!(names.contains(&"navdb_compat".to_string()), "{:?}", names);
    let def = d.table_def(&p).unwrap();
    assert_eq!(def.primary_key(), vec!["id"]);
    let cols = vec!["id".to_string(), "name".to_string()];
    d.execute(Some(scope), &dml::insert_rows(dialect, &p, &cols, &[vec![Value::Int(1), Value::Text("a".into())], vec![Value::Int(2), Value::Text("b'c".into())]])).unwrap();
    d.execute(Some(scope), &dml::update_row(dialect, &p, &[("name".into(), Value::Text("z".into()))], &[("id".into(), Value::Int(1))])).unwrap();
    let r = d.query(Some(scope), &dml::select_page(dialect, &p, "", &[dml::SortSpec { column: "id".into(), descending: false }], 10, 0)).unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::Text("z".into()));
    assert_eq!(r.rows[1][1], Value::Text("b'c".into()));
    assert_eq!(d.count(&p).unwrap(), 2);
    assert!(!d.object_ddl(&p, ObjectKind::Table).unwrap().is_empty());
    d.execute(Some(scope), &format!("DROP TABLE {}", dialect.qualify(&p))).unwrap();
}

#[test]
fn cockroachdb() {
    if !live() {
        return;
    }
    let cfg = ConnectionConfig { host: "127.0.0.1".into(), port: 26257, ..ConnectionConfig::new(DbKind::CockroachDb) };
    let mut d = driver::connect(&cfg).unwrap();
    assert_eq!(d.kind(), DbKind::CockroachDb);
    println!("{}", d.server_version().unwrap());
    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&"defaultdb".to_string()), "{:?}", dbs);
    assert!(d.list_schemas("defaultdb").unwrap().contains(&"public".to_string()));
    roundtrip(d.as_mut(), &Scope::new("defaultdb", Some("public".into())));
}

#[test]
fn tidb() {
    if !live() {
        return;
    }
    let cfg = ConnectionConfig { host: "127.0.0.1".into(), port: 14000, ..ConnectionConfig::new(DbKind::TiDb) };
    let mut d = driver::connect(&cfg).unwrap();
    println!("{}", d.server_version().unwrap());
    assert!(d.list_databases().unwrap().contains(&"test".to_string()));
    roundtrip(d.as_mut(), &Scope::new("test", None));
}

#[test]
fn valkey() {
    if !live() {
        return;
    }
    let cfg = ConnectionConfig { host: "127.0.0.1".into(), port: 16380, ..ConnectionConfig::new(DbKind::Valkey) };
    let mut d = driver::connect(&cfg).unwrap();
    println!("{}", d.server_version().unwrap());
    let s = Scope::new("db0", None);
    d.execute(Some(&s), "SET navdb:compat hello").unwrap();
    let r = d.query(Some(&s), "GET navdb:compat").unwrap();
    assert_eq!(r.rows[0][0].display(), "hello");
    assert!(d.list_objects(&s, ObjectKind::Key).unwrap().iter().any(|o| o.name == "navdb:compat"));
    d.execute(Some(&s), "DEL navdb:compat").unwrap();
}
