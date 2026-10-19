//! Live MongoDB test. Run with
//! `NAVDB_LIVE=1 cargo test -p navdb-core --test live_mongo -- --test-threads=1`.
//! Defaults match the docker container `navdb-test-mongo` (mongo:7 on
//! 127.0.0.1:17017, root/navdb).

use navdb_core::driver;
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("NAVDB_MONGO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(17017),
        user: "root".into(),
        password: "navdb".into(),
        database: "navdb_test".into(),
        ..ConnectionConfig::new(DbKind::MongoDb)
    }
}

#[test]
fn mongo_live() {
    if !live() {
        return;
    }
    let mut d = driver::connect(&cfg()).unwrap();
    let dialect = d.dialect();
    let v = d.server_version().unwrap();
    assert!(v.starts_with("MongoDB 7"), "{}", v);

    let scope = Scope::new("navdb_test", None);
    let p = scope.path("items");
    d.execute(Some(&scope), "db.dropDatabase()").unwrap();

    // create via shell, with validator + index
    let r = d
        .execute(Some(&scope), "db.createCollection('items', {validator: {$jsonSchema: {bsonType: 'object', required: ['name']}}});\ndb.items.createIndex({name: 1}, {unique: true})")
        .unwrap();
    assert_eq!(r.len(), 2);

    let dbs = d.list_databases().unwrap();
    assert!(dbs.contains(&"navdb_test".to_string()), "{:?}", dbs);
    assert_eq!(dbs.last().map(|s| s.as_str()), Some("local"), "{:?}", dbs);
    let objs = d.list_objects(&scope, ObjectKind::Table).unwrap();
    assert!(objs.iter().any(|o| o.name == "items"), "{:?}", objs);

    // grid inserts
    d.execute(Some(&scope), &dml::insert_row(dialect, &p, &["name".into(), "price".into(), "tags".into()], &[Value::Text("a'b".into()), Value::Float(1.5), Value::Text("[\"x\", {\"k\": 1}]".into())]))
        .unwrap();
    let r = d
        .execute(
            Some(&scope),
            &dml::insert_rows(dialect, &p, &["name".into(), "price".into(), "when".into()], &[
                vec![Value::Text("c".into()), Value::Int(3), Value::Text("2024-01-02T03:04:05Z".into())],
                vec![Value::Text("d".into()), Value::Null, Value::Null],
                vec![Value::Text("e".into()), Value::Int(10), Value::Null],
            ]),
        )
        .unwrap();
    assert_eq!(r[0].affected_rows, 3);
    // shell insert with helpers
    d.execute(Some(&scope), "db.items.insertOne({_id: ObjectId('64b7f0c2a1b2c3d4e5f60718'), name: 'f', price: NumberDecimal('9.99'), when: ISODate('2023-05-06'), nested: {a: [1, 2]}, re: /x/i})").unwrap();

    let def = d.table_def(&p).unwrap();
    let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&names[..4], &["_id", "name", "price", "tags"], "{:?}", names);
    assert_eq!(def.primary_key(), vec!["_id".to_string()]);
    assert_eq!(def.column("price").unwrap().data_type, "mixed");
    assert_eq!(def.column("when").unwrap().data_type, "date");
    assert!(def.indexes.iter().any(|i| i.name == "name_1" && i.unique && i.columns == vec!["name".to_string()]), "{:?}", def.indexes);
    assert!(def.indexes.iter().any(|i| i.primary));

    // paging + filter
    let mut f = dml::FilterRule::new("price");
    f.op = dml::FilterOp::Ge;
    f.value = "2".into();
    let w = dml::filters_to_where(dialect, &[f]);
    let sql = dml::select_page(dialect, &p, &w, &[dml::SortSpec { column: "name".into(), descending: false }], 1, 1);
    let r = d.query(Some(&scope), &sql).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.get(0, "name"), Some(&Value::Text("e".into())));
    assert_eq!(d.count(&p).unwrap(), 5);
    let r = d.query(Some(&scope), &dml::count_rows(dialect, &p, &w)).unwrap();
    assert_eq!(r.rows[0][0], Value::Int(3));

    // types in the grid
    let r = d.query(Some(&scope), &dml::select_page(dialect, &p, "", &[], 100, 0)).unwrap();
    let f_row = r.rows.iter().position(|row| row[1] == Value::Text("f".into())).unwrap();
    assert_eq!(r.get(f_row, "_id"), Some(&Value::Text("64b7f0c2a1b2c3d4e5f60718".into())));
    assert_eq!(r.get(f_row, "price"), Some(&Value::Text("9.99".into())));
    assert_eq!(r.get(f_row, "when"), Some(&Value::Text("2023-05-06T00:00:00Z".into())));
    assert_eq!(r.get(f_row, "nested"), Some(&Value::Text("{\"a\":[1,2]}".into())));
    assert_eq!(r.columns[0].type_name, "objectId");
    let c_row = r.rows.iter().position(|row| row[1] == Value::Text("c".into())).unwrap();
    assert_eq!(r.get(c_row, "when"), Some(&Value::Text("2024-01-02T03:04:05Z".into())));
    let c_id = r.get(c_row, "_id").cloned().unwrap();

    // edit + delete by _id (as the grid does)
    let r = d
        .execute(Some(&scope), &dml::update_row(dialect, &p, &[("price".into(), Value::Int(4)), ("extra".into(), Value::Text("{\"z\": true}".into()))], &[("_id".into(), c_id.clone())]))
        .unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let r = d.query(Some(&scope), "db.items.findOne({name: 'c'})").unwrap();
    assert_eq!(r.get(0, "price"), Some(&Value::Int(4)));
    assert_eq!(r.get(0, "extra"), Some(&Value::Text("{\"z\":true}".into())));
    let r = d.execute(Some(&scope), &dml::delete_row(dialect, &p, &[("_id".into(), c_id)])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    let r = d.execute(Some(&scope), &dml::delete_row(dialect, &p, &[("_id".into(), Value::Text("64b7f0c2a1b2c3d4e5f60718".into()))])).unwrap();
    assert_eq!(r[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 3);

    // ddl
    let ddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(ddl.starts_with("db.createCollection(\"items\", {"), "{}", ddl);
    assert!(ddl.contains("$jsonSchema"), "{}", ddl);
    assert!(ddl.contains("createIndex({\"name\":1}, {\"name\":\"name_1\",\"unique\":true})"), "{}", ddl);

    // multi-statement shell script, view
    let r = d
        .execute(
            None,
            "use navdb_test\ndb.items.updateMany({price: {$gte: 2}}, {$set: {big: true}});\ndb.createView('big_items', 'items', [{$match: {big: true}}, {$project: {name: 1}}])\ndb.big_items.find().sort({name: -1})\ndb.items.aggregate([{$group: {_id: null, n: {$sum: 1}}}])\ndb.items.countDocuments({big: true})\ndb.items.distinct('name')\nshow collections\ndb.runCommand({ping: 1})",
        )
        .unwrap();
    assert_eq!(r.len(), 9);
    assert_eq!(r[1].affected_rows, 1);
    assert_eq!(r[3].rows.len(), 1);
    assert_eq!(r[3].get(0, "name"), Some(&Value::Text("e".into())));
    assert_eq!(r[4].get(0, "n"), Some(&Value::Int(3)));
    assert_eq!(r[5].rows[0][0], Value::Int(1));
    assert_eq!(r[6].rows.len(), 3);
    assert!(r[7].rows.contains(&vec![Value::Text("big_items".into())]), "{:?}", r[7].rows);
    assert_eq!(r[8].get(0, "Value"), Some(&Value::Float(1.0)));
    let views = d.list_objects(&scope, ObjectKind::View).unwrap();
    assert_eq!(views.len(), 1);
    let vddl = d.object_ddl(&scope.path("big_items"), ObjectKind::View).unwrap();
    assert!(vddl.starts_with("db.createView(\"big_items\", \"items\", ["), "{}", vddl);

    // explain
    let e = d.explain(Some(&scope), &dml::select_page(dialect, &p, &w, &[], 10, 0)).unwrap();
    assert!(e.rows.iter().any(|r| r[0].display().contains("queryPlanner")), "{:?}", e.rows);
    let e = d.explain(Some(&scope), "db.items.find({name: 'e'})").unwrap();
    assert!(e.rows.iter().any(|r| r[0].display().contains("IXSCAN")), "{:?}", e.rows);

    // errors carry the server message
    let err = d.execute(Some(&scope), "db.items.insertOne({name: 'e'})").unwrap_err();
    assert!(format!("{:#}", err).contains("duplicate key"), "{:#}", err);
    let err = d.execute(Some(&scope), "db.items.insertOne({price: 1})").unwrap_err();
    assert!(format!("{:#}", err).contains("validation"), "{:#}", err);
    let err = d.execute(Some(&scope), "db.items.find({$bogus: 1})").unwrap_err();
    assert!(format!("{:#}", err).contains("$bogus"), "{:#}", err);
    assert!(d.execute(Some(&scope), "SELECT * FROM").is_err());

    // monitor
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty());
    assert!(snap.status.rows.iter().any(|r| r[0] == Value::Text("connections.current".into())));
    assert!(!snap.variables.rows.is_empty());
    let users = d.list_users().unwrap();
    assert!(users.iter().any(|u| u.name == "root"), "{:?}", users);

    // drop
    d.execute(Some(&scope), "db.big_items.drop(); db.items.drop()").unwrap();
    assert!(d.list_objects(&scope, ObjectKind::Table).unwrap().is_empty());
    d.execute(Some(&scope), "db.dropDatabase()").unwrap();
}
