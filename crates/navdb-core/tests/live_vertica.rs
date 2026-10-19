//! Live Vertica tests: `NAVDB_LIVE=1 cargo test -p navdb-core --test live_vertica -- --test-threads=1`.
//!
//! Defaults match the dev container (`datagrip/vertica:9.1.1` on 127.0.0.1:15433,
//! database `testdb`, user `dbadmin` without password, plus `tester`/`guest1234`).
//! Also verified against `opentext/vertica-k8s:24.4.0-3-multiarch` (database created
//! with `vcluster create_db`): `NAVDB_VERTICA_PORT=15434 NAVDB_VERTICA_DB=VMart`.

use navdb_core::driver;
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.into())
}

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: env("NAVDB_VERTICA_HOST", "127.0.0.1"),
        port: env("NAVDB_VERTICA_PORT", "15433").parse().unwrap(),
        user: env("NAVDB_VERTICA_USER", "dbadmin"),
        password: env("NAVDB_VERTICA_PASSWORD", ""),
        database: env("NAVDB_VERTICA_DB", "testdb"),
        ..ConnectionConfig::new(DbKind::Vertica)
    }
}

#[test]
fn vertica_password_auth() {
    if !live() {
        return;
    }
    let mut c = cfg();
    c.user = "tester".into();
    c.password = "guest1234".into();
    let mut d = driver::connect(&c).unwrap();
    assert!(d.server_version().unwrap().contains("Vertica"));
    c.password = "wrong".into();
    let e = driver::connect(&c).err().expect("bad password must fail");
    let m = format!("{:#}", e).to_lowercase();
    assert!(m.contains("password") || m.contains("authentication"), "{}", m);
}

#[test]
fn vertica_roundtrip() {
    if !live() {
        return;
    }
    let c = cfg();
    let mut d = driver::connect(&c).unwrap();
    let dl = d.dialect();
    let v = d.server_version().unwrap();
    println!("version: {}", v);
    assert!(v.contains("Vertica"));
    let dbs = d.list_databases().unwrap();
    assert_eq!(dbs.len(), 1);
    let db = dbs[0].clone();
    let _ = d.execute(None, "DROP SCHEMA IF EXISTS navdb_t CASCADE");
    d.execute(None, "CREATE SCHEMA navdb_t").unwrap();
    let schemas = d.list_schemas(&db).unwrap();
    assert!(schemas.contains(&"navdb_t".to_string()) && schemas.contains(&"public".to_string()), "{:?}", schemas);
    assert!(!schemas.iter().any(|s| s.starts_with("v_")), "{:?}", schemas);
    let scope = Scope::new(db.clone(), Some("navdb_t".into()));
    let p = scope.path("items");

    let res = d
        .execute(
            Some(&scope),
            "CREATE TABLE items (id INT NOT NULL PRIMARY KEY, name VARCHAR(50) NOT NULL DEFAULT 'x', price NUMERIC(10,2), ok BOOLEAN, \
             at TIMESTAMP, raw VARBINARY(10), f FLOAT, uid UUID);\n\
             CREATE TABLE child (cid IDENTITY(1,1) PRIMARY KEY, item_id INT REFERENCES items(id), note LONG VARCHAR);\n\
             CREATE VIEW v_items AS SELECT id, name FROM items WHERE ok;\n\
             CREATE SEQUENCE seq_x START 5;\n\
             CREATE FUNCTION twice(x INT) RETURN INT AS BEGIN RETURN x * 2; END;",
        )
        .unwrap_or_else(|e| panic!("{:#}", e));
    assert_eq!(res.len(), 5);

    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["child", "items"]);
    assert_eq!(d.list_objects(&scope, ObjectKind::View).unwrap()[0].name, "v_items");
    assert_eq!(d.list_objects(&scope, ObjectKind::Sequence).unwrap().iter().filter(|s| s.name == "seq_x").count(), 1);
    let funcs = d.list_objects(&scope, ObjectKind::Function).unwrap();
    assert!(funcs.iter().any(|f| f.name.starts_with("twice(")), "{:?}", funcs);
    let _ = d.list_objects(&scope, ObjectKind::Procedure).unwrap();

    let def = d.table_def(&p).unwrap();
    println!("{:#?}", def.columns);
    assert_eq!(def.columns.len(), 8);
    assert_eq!(def.primary_key(), vec!["id".to_string()]);
    assert!(!def.columns[1].nullable && def.columns[2].nullable);
    assert_eq!(def.columns[1].data_type, "varchar(50)");
    assert_eq!(def.columns[1].default.as_deref(), Some("'x'"));
    let child = d.table_def(&scope.path("child")).unwrap();
    assert!(child.columns[0].auto_increment, "{:?}", child.columns[0]);
    assert_eq!(child.foreign_keys.len(), 1, "{:?}", child.foreign_keys);
    assert_eq!(child.foreign_keys[0].ref_table, "items");
    assert_eq!(child.foreign_keys[0].ref_columns, vec!["id".to_string()]);
    let vdef = d.table_def(&scope.path("v_items")).unwrap();
    assert_eq!(vdef.columns.len(), 2);

    // grid DML generated for the Generic dialect
    let cols: Vec<String> = ["id", "name", "price", "ok", "raw"].iter().map(|s| s.to_string()).collect();
    let r = d
        .execute(
            Some(&scope),
            &dml::insert_rows(dl, &p, &cols, &[
                vec![Value::Int(1), Value::Text("a'b".into()), Value::Text("1.50".into()), Value::Bool(true), Value::Bytes(vec![0, 255])],
                vec![Value::Int(2), Value::Text("c".into()), Value::Null, Value::Bool(false), Value::Null],
                vec![Value::Int(3), Value::Text("d".into()), Value::Float(2.25), Value::Bool(true), Value::Null],
            ]),
        )
        .unwrap_or_else(|e| panic!("{:#}", e));
    assert_eq!(r[0].affected_rows, 3);
    d.execute(Some(&scope), &dml::insert_row(dl, &p, &cols[..2], &[Value::Int(4), Value::Text("e".into())])).unwrap();
    d.execute(None, &format!("UPDATE {} SET at = '2024-01-02 03:04:05', f = 1.5, uid = '6ba7b810-9dad-11d1-80b4-00c04fd430c8' WHERE id = 1", dl.qualify(&p)))
        .unwrap();
    assert_eq!(d.count(&p).unwrap(), 4);

    // committed: visible from a second session
    let mut d2 = driver::connect(&c).unwrap();
    assert_eq!(d2.count(&p).unwrap(), 4);

    let page = d
        .query(Some(&scope), &dml::select_page(dl, &p, "", &[dml::SortSpec { column: "id".into(), descending: false }], 2, 1))
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0][0], Value::Int(2));
    let all = d.query(None, &format!("SELECT * FROM {} WHERE id = 1", dl.qualify(&p))).unwrap();
    println!("{:?}\n{:?}", all.columns, all.rows);
    let row = &all.rows[0];
    assert_eq!(row[1], Value::Text("a'b".into()));
    assert_eq!(row[2], Value::Text("1.50".into()));
    assert_eq!(row[3], Value::Bool(true));
    assert_eq!(row[4], Value::Text("2024-01-02 03:04:05".into()));
    assert_eq!(row[5], Value::Bytes(vec![0, 255]));
    assert_eq!(row[6], Value::Float(1.5));
    assert_eq!(row[7], Value::Text("6ba7b810-9dad-11d1-80b4-00c04fd430c8".into()));
    assert_eq!(all.columns[0].type_name, "Integer");

    let w = dml::filters_to_where(dl, &[dml::FilterRule { value: "c".into(), ..dml::FilterRule::new("name") }]);
    let f = d.query(Some(&scope), &dml::select_page(dl, &p, &w, &[], 10, 0)).unwrap();
    assert_eq!(f.rows.len(), 1);
    assert_eq!(f.rows[0][0], Value::Int(2));
    let cnt = d.query(Some(&scope), &dml::count_rows(dl, &p, &w)).unwrap();
    assert_eq!(cnt.rows[0][0], Value::Int(1));

    let u = d
        .execute(Some(&scope), &dml::update_row(dl, &p, &[("name".into(), Value::Text("zz".into()))], &[("id".into(), Value::Int(2))]))
        .unwrap();
    assert_eq!(u[0].affected_rows, 1);
    let del = d.execute(Some(&scope), &dml::delete_row(dl, &p, &[("id".into(), Value::Int(4))])).unwrap();
    assert_eq!(del[0].affected_rows, 1);
    assert_eq!(d2.count(&p).unwrap(), 3);

    // unqualified names resolve through the scope's search path
    let r = d.execute(Some(&scope), "SELECT COUNT(*) FROM items; SELECT twice(21); SELECT name FROM v_items ORDER BY 1").unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].rows[0][0], Value::Int(3));
    assert_eq!(r[1].rows[0][0], Value::Int(42));
    assert_eq!(r[2].rows.len(), 2);

    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    println!("{}", ddl);
    assert!(ddl.contains("CREATE TABLE") && ddl.contains("items"), "{}", ddl);
    let vddl = d.object_ddl(&scope.path("v_items"), ObjectKind::View).unwrap();
    println!("{}", vddl);
    assert!(vddl.starts_with("CREATE OR REPLACE VIEW"), "{}", vddl);
    let sddl = d.object_ddl(&scope.path("seq_x"), ObjectKind::Sequence).unwrap();
    println!("{}", sddl);
    assert!(sddl.contains("SEQUENCE"), "{}", sddl);
    let fname = funcs.iter().find(|f| f.name.starts_with("twice(")).unwrap().name.clone();
    let fddl = d.object_ddl(&scope.path(fname), ObjectKind::Function).unwrap();
    println!("{}", fddl);
    assert!(fddl.contains("FUNCTION") && fddl.contains("twice"), "{}", fddl);
    // the generated function DDL is executable
    d.execute(Some(&scope), &fddl).unwrap_or_else(|e| panic!("{}: {:#}", fddl, e));

    let ex = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert!(!ex.rows.is_empty());

    let err = d.execute(Some(&scope), "SELEC nonsense").unwrap_err();
    let msg = format!("{:#}", err);
    assert!(msg.contains("42601") || msg.to_lowercase().contains("syntax"), "{}", msg);
    let err = d.execute(Some(&scope), "SELECT * FROM no_such_table").unwrap_err();
    assert!(format!("{:#}", err).contains("no_such_table"), "{:#}", err);
    // the session is still usable after errors
    assert_eq!(d.count(&p).unwrap(), 3);

    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty());
    assert!(!snap.variables.rows.is_empty());
    assert!(!snap.status.rows.is_empty());
    let users = d.list_users().unwrap();
    assert!(users.iter().any(|u| u.name == "dbadmin"));

    // kill the second session
    let sid = d2.query(None, "SELECT session_id FROM v_monitor.current_session").unwrap().text(0, 0);
    d.kill_process(&sid).unwrap();
    assert!(d2.query(None, "SELECT 1").is_err());

    d.execute(None, "DROP SCHEMA navdb_t CASCADE").unwrap();
    assert!(!d.list_schemas(&db).unwrap().contains(&"navdb_t".to_string()));
}
