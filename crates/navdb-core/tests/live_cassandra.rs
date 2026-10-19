//! Live Cassandra / ScyllaDB tests. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_cassandra scylla -- --nocapture`
//! (ScyllaDB on 127.0.0.1:19042) or `... cassandra` (Cassandra on 127.0.0.1:19043).

use navdb_core::driver::{self, Driver};
use navdb_core::dml::{self, FilterOp, FilterRule, SortSpec};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn cfg(kind: DbKind, env: &str, port: u16) -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var(env).ok().and_then(|p| p.parse().ok()).unwrap_or(port),
        connect_timeout_secs: 20,
        ..ConnectionConfig::new(kind)
    }
}

fn names(r: &QueryResult, col: &str) -> Vec<String> {
    (0..r.rows.len()).map(|i| r.get(i, col).map(|v| v.to_string()).unwrap_or_default()).collect()
}

fn roundtrip(kind: DbKind, mut d: Box<dyn Driver>) {
    let v = d.server_version().unwrap();
    println!("version: {}", v);
    assert!(v.contains(if kind == DbKind::ScyllaDb { "ScyllaDB" } else { "Cassandra" }), "{}", v);

    let ks = "navdb_test";
    let _ = d.execute(None, &format!("DROP KEYSPACE IF EXISTS {}", ks));
    let tablets = if v.contains("ScyllaDB") { " AND tablets = {'enabled': false}" } else { "" };
    d.execute(None, &format!("CREATE KEYSPACE {} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{}", ks, tablets)).unwrap();
    assert!(d.list_databases().unwrap().contains(&ks.to_string()));
    assert!(!d.list_databases().unwrap().iter().any(|k| k.starts_with("system")));
    assert!(d.list_schemas(ks).unwrap().is_empty());

    let scope = Scope::new(ks, None);
    d.execute(
        Some(&scope),
        "CREATE TABLE items (id int, ck text, name text, flag boolean, uid uuid, ts timestamp, ip inet, data blob, \
         tags list<text>, attrs map<text, int>, price decimal, PRIMARY KEY ((id), ck)) WITH CLUSTERING ORDER BY (ck DESC) AND comment = 'items table';\n\
         CREATE INDEX items_name_idx ON items (name)",
    )
    .unwrap();
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["items"]);
    assert_eq!(tables[0].comment, "items table");
    let idx = d.list_objects(&scope, ObjectKind::Index).unwrap();
    assert!(idx.iter().any(|i| i.name == "items_name_idx"), "{:?}", idx);

    let p = scope.path("items");
    let def = d.table_def(&p).unwrap();
    assert_eq!(def.primary_key(), vec!["id".to_string(), "ck".to_string()]);
    assert_eq!(def.column("uid").unwrap().data_type, "uuid");
    assert_eq!(def.column("tags").unwrap().data_type, "list<text>");
    assert!(def.indexes.iter().any(|i| i.name == "items_name_idx" && i.columns == vec!["name".to_string()]), "{:?}", def.indexes);

    let dl = d.dialect();
    let cols: Vec<String> = ["id", "ck", "name", "flag", "uid", "ts", "ip", "data", "tags", "attrs", "price"].iter().map(|s| s.to_string()).collect();
    let uid = "123e4567-e89b-12d3-a456-426614174000";
    let row = vec![
        Value::Int(1),
        Value::Text("a".into()),
        Value::Text("it's alpha".into()),
        Value::Text("true".into()),
        Value::Text(uid.into()),
        Value::Text("2024-01-02 03:04:05".into()),
        Value::Text("10.0.0.1".into()),
        Value::Bytes(vec![0, 1, 0xff]),
        Value::Text("[\"x\",\"y\"]".into()),
        Value::Text("{\"k\":1}".into()),
        Value::Text("1.50".into()),
    ];
    let r = d.execute(Some(&scope), &dml::insert_row(dl, &p, &cols, &row)).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let more: Vec<Vec<Value>> = (2..7)
        .map(|i| vec![Value::Int(i), Value::Text(format!("c{}", i)), Value::Text(format!("name{}", i)), Value::Bool(i % 2 == 0)])
        .collect();
    let r = d.execute(Some(&scope), &dml::insert_rows(dl, &p, &cols[..4], &more)).unwrap();
    assert_eq!(r[0].affected_rows, 5);
    assert_eq!(d.count(&p).unwrap(), 6);

    // full row read back
    let r = d.query(Some(&scope), &dml::select_page(dl, &p, "\"id\" = 1", &[], 10, 0)).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.get(0, "name"), Some(&Value::Text("it's alpha".into())));
    assert_eq!(r.get(0, "flag"), Some(&Value::Bool(true)));
    assert_eq!(r.get(0, "uid"), Some(&Value::Text(uid.into())));
    assert_eq!(r.get(0, "ts"), Some(&Value::Text("2024-01-02T03:04:05.000Z".into())));
    assert_eq!(r.get(0, "ip"), Some(&Value::Text("10.0.0.1".into())));
    assert_eq!(r.get(0, "data"), Some(&Value::Bytes(vec![0, 1, 0xff])));
    assert_eq!(r.get(0, "tags"), Some(&Value::Text("[\"x\",\"y\"]".into())));
    assert_eq!(r.get(0, "attrs"), Some(&Value::Text("{\"k\":1}".into())));
    assert_eq!(r.get(0, "price"), Some(&Value::Text("1.50".into())));
    assert_eq!(r.columns.iter().find(|c| c.name == "tags").unwrap().type_name, "list<text>");

    // paging + sorting + client-side LIKE
    let page = d.query(Some(&scope), &dml::select_page(dl, &p, "", &[SortSpec { column: "name".into(), descending: false }], 2, 1)).unwrap();
    assert_eq!(names(&page, "name"), vec!["name2", "name3"]);
    let mut f = FilterRule::new("name");
    f.op = FilterOp::BeginsWith;
    f.value = "name".into();
    let mut f2 = FilterRule::new("flag");
    f2.value = "true".into();
    let w = dml::filters_to_where(dl, &[f, f2]);
    let page = d.query(Some(&scope), &dml::select_page(dl, &p, &w, &[SortSpec { column: "id".into(), descending: true }], 10, 0)).unwrap();
    assert_eq!(names(&page, "id"), vec!["6", "4", "2"]);
    let n = d.query(Some(&scope), &dml::count_rows(dl, &p, &w)).unwrap();
    assert_eq!(n.rows[0][0], Value::Int(3));
    // typed literals in filters: uuid / IN / between / OR
    let mut fu = FilterRule::new("uid");
    fu.value = uid.into();
    let r = d.query(Some(&scope), &dml::select_page(dl, &p, &dml::filters_to_where(dl, &[fu]), &[], 10, 0)).unwrap();
    assert_eq!(names(&r, "id"), vec!["1"]);
    let mut fi = FilterRule::new("id");
    fi.op = FilterOp::In;
    fi.value = "2, 3".into();
    let mut fb = FilterRule::new("id");
    fb.op = FilterOp::Between;
    fb.value = "5, 6".into();
    fb.and = false;
    let r = d.query(Some(&scope), &dml::select_page(dl, &p, &dml::filters_to_where(dl, &[fi, fb]), &[SortSpec { column: "id".into(), descending: false }], 10, 0)).unwrap();
    assert_eq!(names(&r, "id"), vec!["2", "3", "5", "6"]);
    let r = d.query(Some(&scope), &dml::count_rows(dl, &p, "\"id\" IN (2, 3)")).unwrap();
    assert_eq!(r.rows[0][0], Value::Int(2));

    // update with the full row as key (non-key columns are dropped from WHERE)
    let key = vec![("id".to_string(), Value::Int(1)), ("ck".to_string(), Value::Text("a".into())), ("name".to_string(), Value::Text("it's alpha".into()))];
    let sql = dml::update_row(dl, &p, &[("flag".into(), Value::Text("false".into())), ("tags".into(), Value::Text("[\"z\"]".into()))], &key);
    let r = d.execute(Some(&scope), &sql).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let r = d.query(Some(&scope), "SELECT flag, tags FROM items WHERE id = 1 AND ck = 'a'").unwrap();
    assert_eq!(r.rows[0], vec![Value::Bool(false), Value::Text("[\"z\"]".into())]);
    assert!(d.execute(Some(&scope), &dml::update_row(dl, &p, &[("ck".into(), Value::Text("b".into()))], &key[..2])).is_err());
    d.execute(Some(&scope), &dml::delete_row(dl, &p, &[("id".into(), Value::Int(2)), ("ck".into(), Value::Text("c2".into()))])).unwrap();
    assert_eq!(d.count(&p).unwrap(), 5);

    // DDL
    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    println!("{}", ddl);
    assert!(ddl.contains("CREATE TABLE") && ddl.contains("PRIMARY KEY"), "{}", ddl);
    let ddl = d.object_ddl(&scope.path("items_name_idx"), ObjectKind::Index).unwrap();
    assert!(ddl.contains("CREATE") && ddl.contains("INDEX"), "{}", ddl);

    // multi-statement + USE + raw CQL
    let r = d
        .execute(None, &format!("USE {};\nINSERT INTO items (id, ck, name) VALUES (9, 'z', 'nine') USING TTL 3600;\nSELECT id, name FROM items WHERE id = 9", ks))
        .unwrap();
    assert_eq!(r.len(), 3);
    assert!(r[2].is_query);
    assert_eq!(r[2].rows, vec![vec![Value::Int(9), Value::Text("nine".into())]]);
    let e = d.execute(Some(&scope), "SELEC * FROM items").unwrap_err();
    println!("error: {:#}", e);
    assert!(format!("{:#}", e).to_lowercase().contains("syntax") || format!("{:#}", e).contains("SELEC"), "{:#}", e);
    let e = d.execute(Some(&scope), "SELECT * FROM missing_table").unwrap_err();
    assert!(!format!("{:#}", e).is_empty());

    let ex = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert!(!ex.rows.is_empty());
    assert!(d.explain(Some(&scope), "DELETE FROM items WHERE id = 1").is_err());
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.status.rows.is_empty());
    println!("snapshot: {} status, {} variables, {} processes", snap.status.rows.len(), snap.variables.rows.len(), snap.processes.rows.len());

    d.execute(None, &format!("DROP KEYSPACE {}", ks)).unwrap();
    assert!(!d.list_databases().unwrap().contains(&ks.to_string()));
}

#[test]
fn scylla() {
    if !live() {
        return;
    }
    let c = cfg(DbKind::ScyllaDb, "NAVDB_SCYLLA_PORT", 19042);
    let d = driver::connect(&c).unwrap();
    assert_eq!(d.kind(), DbKind::ScyllaDb);
    roundtrip(DbKind::ScyllaDb, d);
}

#[test]
fn cassandra() {
    if !live() {
        return;
    }
    let c = cfg(DbKind::Cassandra, "NAVDB_CASSANDRA_PORT", 19043);
    roundtrip(DbKind::Cassandra, driver::connect(&c).unwrap());
}

#[test]
fn bad_port_errors() {
    if !live() {
        return;
    }
    let mut c = cfg(DbKind::Cassandra, "NAVDB_NOPE", 1);
    c.connect_timeout_secs = 2;
    assert!(driver::connect(&c).is_err());
}
