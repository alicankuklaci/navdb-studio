//! Live OpenSearch / Elasticsearch tests. Run one at a time against the dev containers:
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_search opensearch` (127.0.0.1:19200) and
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_search elasticsearch` (127.0.0.1:19201).
//! Ports can be changed with `NAVDB_OPENSEARCH_PORT` / `NAVDB_ELASTIC_PORT`.

use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn cfg(kind: DbKind, env: &str, port: u16) -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var(env).ok().and_then(|p| p.parse().ok()).unwrap_or(port),
        ..ConnectionConfig::new(kind)
    }
}

fn col(r: &QueryResult, row: usize, name: &str) -> Value {
    r.get(row, name).cloned().unwrap_or_else(|| panic!("no column {} in {:?}", name, r.columns))
}

fn rule(c: &str, op: dml::FilterOp, v: &str) -> dml::FilterRule {
    dml::FilterRule { op, value: v.into(), ..dml::FilterRule::new(c) }
}

fn exercise(kind: DbKind, env: &str, port: u16, label: &str) {
    let mut d = driver::connect(&cfg(kind, env, port)).expect("connect");
    let ver = d.server_version().unwrap();
    assert!(ver.starts_with(label), "{}", ver);
    assert_eq!(d.kind(), kind);
    let dbs = d.list_databases().unwrap();
    assert_eq!(dbs.len(), 1);
    let scope = Scope::new(dbs[0].clone(), None);
    let sc = Some(&scope);
    let dl = d.dialect();
    let p = scope.path("navdb_items");

    for s in ["DROP TABLE IF EXISTS navdb_items", "DROP TABLE IF EXISTS \"navdb_big\""] {
        d.execute(sc, s).unwrap();
    }
    let create = r#"PUT /navdb_items
{
  "settings": {"number_of_shards": 1, "number_of_replicas": 0},
  "mappings": {"properties": {
    "name": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
    "price": {"type": "double"},
    "qty": {"type": "long"},
    "active": {"type": "boolean"},
    "addr": {"properties": {"city": {"type": "keyword"}}},
    "tags": {"type": "keyword"}
  }},
  "aliases": {"navdb_items_view": {}}
}"#;
    let r = d.execute(sc, create).unwrap();
    assert_eq!(r[0].get(0, "Value"), Some(&Value::Bool(true)), "{:?}", r[0].rows);

    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert!(tables.iter().any(|t| t.name == "navdb_items" && t.rows == Some(0)));
    assert!(tables.iter().all(|t| !t.name.starts_with('.')));
    let views = d.list_objects(&scope, ObjectKind::View).unwrap();
    assert!(views.iter().any(|v| v.name == "navdb_items_view" && v.comment == "navdb_items"));

    let def = d.table_def(&p).unwrap();
    assert_eq!(def.primary_key(), vec!["_id"]);
    let names: Vec<_> = def.columns.iter().map(|c| format!("{}:{}", c.name, c.data_type)).collect();
    // the server returns properties sorted by name
    assert_eq!(names, vec!["_id:keyword", "active:boolean", "addr.city:keyword", "name:text", "price:double", "qty:long", "tags:keyword"]);

    // inserts
    let cols: Vec<String> = ["_id", "name", "price", "qty", "active", "addr.city"].iter().map(|s| s.to_string()).collect();
    let r = d.execute(sc, &dml::insert_row(dl, &p, &cols, &[
        Value::Text("a1".into()), Value::Text("apple pie".into()), Value::Text("1.5".into()), Value::Text("1".into()), Value::Text("true".into()), Value::Text("Paris".into()),
    ])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let cols2: Vec<String> = ["name", "price", "qty", "active", "addr.city", "tags"].iter().map(|s| s.to_string()).collect();
    let r = d.execute(sc, &dml::insert_rows(dl, &p, &cols2, &[
        vec![Value::Text("banana".into()), Value::Float(0.25), Value::Int(2), Value::Bool(false), Value::Text("Rome".into()), Value::Text(r#"["x","y"]"#.into())],
        vec![Value::Text("apple juice".into()), Value::Float(3.0), Value::Int(3), Value::Bool(true), Value::Null, Value::Null],
        vec![Value::Text("cherry".into()), Value::Null, Value::Int(4), Value::Null, Value::Text("Paris".into()), Value::Null],
    ])).unwrap();
    assert_eq!(r[0].affected_rows, 3);
    assert_eq!(d.count(&p).unwrap(), 4);
    assert_eq!(d.count(&scope.path("navdb_items_view")).unwrap(), 4);

    // paging + sort
    let sort = [dml::SortSpec { column: "qty".into(), descending: false }];
    let page = d.query(sc, &dml::select_page(dl, &p, "", &sort, 2, 1)).unwrap();
    assert_eq!(page.columns[0].name, "_id");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(col(&page, 0, "qty"), Value::Int(2));
    assert_eq!(col(&page, 0, "name"), Value::Text("banana".into()));
    assert_eq!(col(&page, 0, "price"), Value::Float(0.25));
    assert_eq!(col(&page, 0, "addr.city"), Value::Text("Rome".into()));
    assert_eq!(col(&page, 0, "tags"), Value::Text(r#"["x","y"]"#.into()));
    let first = d.query(sc, &dml::select_page(dl, &p, "", &sort, 1, 0)).unwrap();
    assert_eq!(col(&first, 0, "_id"), Value::Text("a1".into()));
    assert_eq!(col(&first, 0, "active"), Value::Bool(true));
    assert_eq!(col(&first, 0, "qty"), Value::Int(1));
    // sort on a text field uses its keyword sub-field
    let by_name = d.query(sc, &dml::select_page(dl, &p, "", &[dml::SortSpec { column: "name".into(), descending: true }], 10, 0)).unwrap();
    assert_eq!(col(&by_name, 0, "name"), Value::Text("cherry".into()));

    // filters
    let count_where = |d: &mut Box<dyn Driver>, rules: &[dml::FilterRule]| -> i64 {
        let w = dml::filters_to_where(dl, rules);
        let r = d.query(sc, &dml::count_rows(dl, &p, &w)).unwrap();
        let page = d.query(sc, &dml::select_page(dl, &p, &w, &[], 100, 0)).unwrap();
        let n = r.rows[0][0].as_f64().unwrap() as i64;
        assert_eq!(page.rows.len() as i64, n, "{}", w);
        n
    };
    use dml::FilterOp::*;
    assert_eq!(count_where(&mut d, &[rule("name", Contains, "APPLE")]), 2);
    assert_eq!(count_where(&mut d, &[rule("name", BeginsWith, "ban")]), 1);
    assert_eq!(count_where(&mut d, &[rule("name", Eq, "cherry")]), 1);
    assert_eq!(count_where(&mut d, &[rule("qty", Ge, "2"), rule("addr.city", Eq, "Paris")]), 1);
    assert_eq!(count_where(&mut d, &[rule("qty", Between, "2, 3")]), 2);
    assert_eq!(count_where(&mut d, &[rule("price", IsNull, "")]), 1);
    assert_eq!(count_where(&mut d, &[rule("active", IsNotNull, "")]), 3);
    assert_eq!(count_where(&mut d, &[rule("qty", In, "1, 4")]), 2);
    assert_eq!(count_where(&mut d, &[rule("addr.city", Ne, "Paris")]), 1);
    assert_eq!(count_where(&mut d, &[rule("_id", Eq, "a1")]), 1);
    assert_eq!(count_where(&mut d, &[rule("qty", Lt, "2"), dml::FilterRule { and: false, ..rule("qty", Gt, "3") }]), 2);
    let w = dml::filters_to_where(dl, &[rule("tags", Eq, "x")]);
    let r = d.query(sc, &dml::select_page(dl, &p, &w, &[], 10, 0)).unwrap();
    assert_eq!(col(&r, 0, "name"), Value::Text("banana".into()));
    let proj = d.query(sc, "SELECT _id, qty FROM navdb_items WHERE qty = 4").unwrap();
    assert_eq!(proj.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["_id", "qty"]);
    assert_eq!(proj.rows[0][1], Value::Int(4));

    // updates
    let r = d.execute(sc, &dml::update_row(dl, &p, &[("qty".into(), Value::Text("10".into())), ("addr.city".into(), Value::Text("Lyon".into()))], &[("_id".into(), Value::Text("a1".into()))])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let one = d.query(sc, &dml::select_page(dl, &p, "\"_id\" = 'a1'", &[], 10, 0)).unwrap();
    assert_eq!(col(&one, 0, "qty"), Value::Int(10));
    assert_eq!(col(&one, 0, "addr.city"), Value::Text("Lyon".into()));
    assert_eq!(col(&one, 0, "name"), Value::Text("apple pie".into()));
    let r = d.execute(sc, "UPDATE navdb_items SET price = 9.5, addr.city = 'Oslo' WHERE qty = 3").unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let r = d.query(sc, &dml::select_page(dl, &p, "\"addr\".\"city\" = 'Oslo'", &[], 10, 0)).unwrap();
    assert_eq!(col(&r, 0, "price"), Value::Float(9.5));
    assert_eq!(col(&r, 0, "name"), Value::Text("apple juice".into()));

    // deletes
    let r = d.execute(sc, &dml::delete_row(dl, &p, &[("_id".into(), Value::Text("a1".into()))])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let r = d.execute(sc, &dml::delete_row(dl, &p, &[("_id".into(), Value::Text("a1".into()))])).unwrap();
    assert_eq!(r[0].affected_rows, 0);
    let r = d.execute(sc, "DELETE FROM navdb_items WHERE qty >= 4").unwrap();
    assert_eq!(r[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 2);

    // DDL
    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("PUT /navdb_items\n{"), "{}", ddl);
    assert!(ddl.contains("\"number_of_shards\": \"1\"") && ddl.contains("\"mappings\"") && ddl.contains("navdb_items_view"), "{}", ddl);
    let vddl = d.object_ddl(&scope.path("navdb_items_view"), ObjectKind::View).unwrap();
    assert!(vddl.starts_with("POST /_aliases") && vddl.contains("\"index\": \"navdb_items\""), "{}", vddl);

    // multi-statement SQL and console
    let r = d.execute(sc, "SELECT COUNT(*) FROM navdb_items;\nSELECT * FROM navdb_items ORDER BY qty LIMIT 1;").unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].rows[0][0], Value::Int(2));
    assert_eq!(col(&r[1], 0, "qty"), Value::Int(2));
    let console = "GET _cat/indices/navdb_*?v\n\n# aggregation\nPOST /navdb_items/_search\n{\"size\": 0, \"aggs\": {\"by_active\": {\"terms\": {\"field\": \"active\"}, \"aggs\": {\"p\": {\"sum\": {\"field\": \"price\"}}}}}}\nGET /navdb_items/_search?q=name:banana\nPOST _bulk\n{\"index\": {\"_index\": \"navdb_items\", \"_id\": \"b1\"}}\n{\"name\": \"bulk\"}\n";
    let r = d.execute(sc, console).unwrap();
    assert_eq!(r.len(), 4, "{:?}", r.iter().map(|x| &x.columns).collect::<Vec<_>>());
    assert!(r[0].column_index("index").is_some() && r[0].column_index("docs.count").is_some());
    assert_eq!(r[1].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["by_active", "by_active.doc_count", "p"]);
    assert_eq!(r[1].rows.len(), 2);
    assert_eq!(col(&r[2], 0, "name"), Value::Text("banana".into()));
    assert!(r[2].column_index("_score").is_some());
    assert_eq!(col(&r[3], 0, "result"), Value::Text("created".into()));

    // server SQL endpoint
    d.execute(sc, "POST /navdb_items/_refresh").unwrap();
    let r = d.query(sc, "SELECT active, COUNT(*) AS c FROM navdb_items WHERE qty IS NOT NULL GROUP BY active ORDER BY active").unwrap();
    assert_eq!(r.columns.len(), 2, "{:?}", r);
    assert_eq!(r.rows.len(), 2, "{:?}", r.rows);

    // explain
    let e = d.explain(sc, &dml::select_page(dl, &p, "\"name\" LIKE '%a%'", &sort, 10, 0)).unwrap();
    let text: Vec<String> = e.rows.iter().map(|r| r[0].to_string()).collect();
    assert_eq!(text[0], "POST /navdb_items/_search");
    assert!(text.iter().any(|l| l.contains("wildcard")));
    let e = d.explain(sc, "SELECT active, COUNT(*) FROM navdb_items GROUP BY active").unwrap();
    assert!(e.rows.len() > 2);

    // errors
    let e = d.execute(sc, "SELEC nonsense FROM x").unwrap_err();
    assert!(!format!("{:#}", e).is_empty());
    let e = d.execute(sc, "GET /navdb_missing_index/_search").unwrap_err();
    assert!(format!("{:#}", e).contains("no such index"), "{:#}", e);
    let e = d.query(sc, "SELECT * FROM navdb_missing_index").unwrap_err();
    assert!(format!("{:#}", e).contains("no such index"), "{:#}", e);

    // deep paging beyond the result window (scroll)
    let big = scope.path("navdb_big");
    for chunk in 0..6 {
        let rows: Vec<Vec<Value>> = (chunk * 2000..(chunk + 1) * 2000).map(|n| vec![Value::Int(n), Value::Text(format!("n{}", n))]).collect();
        d.execute(sc, &dml::insert_rows(dl, &big, &["n".into(), "s".into()], &rows)).unwrap();
    }
    assert_eq!(d.count(&big).unwrap(), 12000);
    let deep = d.query(sc, &dml::select_page(dl, &big, "", &[dml::SortSpec { column: "n".into(), descending: false }], 3, 10_500)).unwrap();
    assert_eq!(deep.rows.len(), 3);
    assert_eq!(col(&deep, 0, "n"), Value::Int(10_500));
    assert_eq!(col(&deep, 2, "n"), Value::Int(10_502));

    // server monitor
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty());
    assert!(snap.variables.rows.len() > 100);
    assert!(snap.status.rows.iter().any(|r| r[1] == Value::Text("status".into())));

    // drop
    d.execute(sc, "DROP TABLE \"navdb_big\"").unwrap();
    d.execute(sc, &format!("DROP TABLE {}", dl.qualify(&p))).unwrap();
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert!(!tables.iter().any(|t| t.name.starts_with("navdb_")));
    assert!(d.execute(sc, "DROP TABLE navdb_items").is_err());
}

#[test]
fn opensearch_live() {
    if !live() {
        return;
    }
    exercise(DbKind::OpenSearch, "NAVDB_OPENSEARCH_PORT", 19200, "OpenSearch");
}

#[test]
fn elasticsearch_live() {
    if !live() {
        return;
    }
    exercise(DbKind::Elasticsearch, "NAVDB_ELASTIC_PORT", 19201, "Elasticsearch");
}
