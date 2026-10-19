//! Live SQL Server tests: `NAVDB_LIVE=1 cargo test -p navdb-core --test live_mssql -- --test-threads=1`.
//! Defaults match the `navdb-test-mssql` container: 127.0.0.1:11433 sa/NavDb_2024!.

use navdb_core::driver::{self, Driver};
use navdb_core::*;

fn live() -> bool {
    std::env::var("NAVDB_LIVE").is_ok()
}

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        host: std::env::var("NAVDB_MSSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("NAVDB_MSSQL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(11433),
        user: "sa".into(),
        password: std::env::var("NAVDB_MSSQL_PASSWORD").unwrap_or_else(|_| "NavDb_2024!".into()),
        ..ConnectionConfig::new(DbKind::SqlServer)
    }
}

fn run(d: &mut dyn Driver, scope: &Scope, sql: &str) -> Vec<QueryResult> {
    d.execute(Some(scope), sql).unwrap_or_else(|e| panic!("{}: {:#}", sql, e))
}

#[test]
fn mssql_live() {
    if !live() {
        return;
    }
    let mut d = driver::connect(&cfg()).expect("mssql connect");
    let dl = Dialect::SqlServer;
    let v = d.server_version().unwrap();
    assert!(v.contains("Microsoft SQL"), "{}", v);

    d.execute(None, "IF DB_ID('navdb_test') IS NOT NULL BEGIN ALTER DATABASE navdb_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE navdb_test; END").unwrap();
    d.execute(None, "CREATE DATABASE navdb_test").unwrap();
    assert!(d.list_databases().unwrap().contains(&"navdb_test".to_string()));
    let scope = Scope::new("navdb_test", Some("dbo".into()));
    run(d.as_mut(), &scope, "CREATE SCHEMA sales");
    let schemas = d.list_schemas("navdb_test").unwrap();
    assert!(schemas.contains(&"dbo".to_string()) && schemas.contains(&"sales".to_string()), "{:?}", schemas);
    assert!(!schemas.iter().any(|s| s.starts_with("db_") || s == "sys"), "{:?}", schemas);

    // create via the DDL generator, then read back
    let p = scope.path("items");
    let def = TableDef {
        path: p.clone(),
        columns: vec![
            ColumnDef { auto_increment: true, primary_key: true, nullable: false, ..ColumnDef::new("id", "int") },
            ColumnDef { nullable: false, default: Some("N'x'".into()), ..ColumnDef::new("name", "nvarchar(50)") },
            ColumnDef::new("price", "decimal(10,2)"),
            ColumnDef::new("made", "datetime2"),
            ColumnDef::new("flag", "bit"),
            ColumnDef::new("uid", "uniqueidentifier"),
            ColumnDef::new("blob", "varbinary(max)"),
        ],
        indexes: vec![IndexDef { name: "ix_items_name".into(), columns: vec!["name".into()], ..Default::default() }],
        checks: vec![CheckDef { name: "ck_price".into(), expression: "[price]>=(0)".into() }],
        ..Default::default()
    };
    for s in ddl::create_table(dl, &def) {
        run(d.as_mut(), &scope, &s);
    }
    let back = d.table_def(&p).unwrap();
    let types: Vec<&str> = back.columns.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(types, vec!["int", "nvarchar(50)", "decimal(10,2)", "datetime2", "bit", "uniqueidentifier", "varbinary(max)"]);
    assert!(back.columns[0].auto_increment && back.columns[0].primary_key && !back.columns[0].nullable);
    assert_eq!(back.columns[1].default.as_deref(), Some("N'x'"));
    assert_eq!(back.primary_key(), vec!["id".to_string()]);
    assert!(back.indexes.iter().any(|i| i.name == "ix_items_name" && i.columns == vec!["name".to_string()]), "{:?}", back.indexes);
    assert_eq!(back.checks.len(), 1);

    // child table with FK
    let child = TableDef {
        path: scope.path("child"),
        columns: vec![
            ColumnDef { primary_key: true, nullable: false, ..ColumnDef::new("id", "int") },
            ColumnDef::new("item_id", "int"),
        ],
        foreign_keys: vec![ForeignKeyDef {
            name: "fk_child_item".into(),
            columns: vec!["item_id".into()],
            ref_db: "navdb_test".into(),
            ref_schema: Some("dbo".into()),
            ref_table: "items".into(),
            ref_columns: vec!["id".into()],
            on_delete: "CASCADE".into(),
            on_update: String::new(),
        }],
        ..Default::default()
    };
    for s in ddl::create_table(dl, &child) {
        run(d.as_mut(), &scope, &s);
    }
    let cdef = d.table_def(&child.path).unwrap();
    assert_eq!(cdef.foreign_keys.len(), 1);
    assert_eq!(cdef.foreign_keys[0].ref_table, "items");
    assert_eq!(cdef.foreign_keys[0].on_delete, "CASCADE");
    assert_eq!(cdef.foreign_keys[0].on_update, "NO ACTION");

    // inserts
    let cols: Vec<String> = ["name", "price", "made", "flag", "uid", "blob"].iter().map(|s| s.to_string()).collect();
    let r = run(d.as_mut(), &scope, &dml::insert_row(dl, &p, &cols, &[
        Value::Text("a'b".into()),
        Value::Text("1.50".into()),
        Value::Text("2024-01-02 03:04:05.123".into()),
        Value::Bool(true),
        Value::Text("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()),
        Value::Bytes(vec![0, 1, 255]),
    ]));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].affected_rows, 1);
    assert!(!r[0].is_query);
    let rows: Vec<Vec<Value>> = (0..5).map(|i| vec![Value::Text(format!("n{}", i)), Value::Int(i), Value::Null, Value::Bool(false), Value::Null, Value::Null]).collect();
    assert_eq!(run(d.as_mut(), &scope, &dml::insert_rows(dl, &p, &cols, &rows))[0].affected_rows, 5);
    assert_eq!(d.count(&p).unwrap(), 6);

    // value mapping
    let r = d.query(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
    assert_eq!(r.rows[0][1], Value::Text("a'b".into()));
    assert_eq!(r.rows[0][2], Value::Text("1.50".into()));
    assert_eq!(r.rows[0][3], Value::Text("2024-01-02 03:04:05.123".into()));
    assert_eq!(r.rows[0][4], Value::Bool(true));
    assert_eq!(r.rows[0][5], Value::Text("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()));
    assert_eq!(r.rows[0][6], Value::Bytes(vec![0, 1, 255]));
    assert_eq!(r.columns[1].type_name, "nvarchar");
    assert_eq!(r.columns[2].type_name, "decimal");
    let r = d
        .query(
            None,
            "SELECT CAST(5 AS bigint), CAST(2.5 AS float), CAST(1.25 AS real), CAST(3.5 AS money), CAST('2024-05-06' AS date), \
             CAST('10:11:12' AS time(0)), CAST('2024-05-06 10:11:12 +02:00' AS datetimeoffset(0)), CAST('<a>1</a>' AS xml), CAST('2020-01-01T10:00:00' AS datetime), CAST('x' AS char(3)), CAST(N'ü' AS ntext)",
        )
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(5));
    assert_eq!(r.rows[0][1], Value::Float(2.5));
    assert_eq!(r.rows[0][2], Value::Float(1.25));
    assert_eq!(r.rows[0][3].as_f64(), Some(3.5));
    assert_eq!(r.text(0, 4), "2024-05-06");
    assert_eq!(r.text(0, 5), "10:11:12");
    assert_eq!(r.text(0, 6), "2024-05-06 10:11:12 +02:00");
    assert_eq!(r.text(0, 7), "<a>1</a>");
    assert_eq!(r.text(0, 8), "2020-01-01 10:00:00");
    assert_eq!(r.text(0, 9), "x  ");
    assert_eq!(r.text(0, 10), "ü");

    // paging with filter (no ORDER BY -> rewritten), and with sort
    let mut f = dml::FilterRule::new("name");
    f.op = dml::FilterOp::BeginsWith;
    f.value = "n".into();
    let w = dml::filters_to_where(dl, &[f]);
    let page = d.query(Some(&scope), &dml::select_page(dl, &p, &w, &[], 2, 1)).unwrap();
    assert_eq!(page.rows.len(), 2);
    let sorted = d.query(Some(&scope), &dml::select_page(dl, &p, &w, &[dml::SortSpec { column: "id".into(), descending: true }], 2, 1)).unwrap();
    assert_eq!(sorted.text(0, 1), "n3");
    let cnt = d.query(Some(&scope), &dml::count_rows(dl, &p, &w)).unwrap();
    assert_eq!(cnt.rows[0][0], Value::Int(5));

    // edit and delete
    let upd = dml::update_row(dl, &p, &[("name".into(), Value::Text("z".into()))], &[("id".into(), Value::Int(1))]);
    assert_eq!(run(d.as_mut(), &scope, &upd)[0].affected_rows, 1);
    // key on all original values, including a binary and a NULL
    let orig = d.query(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    let key: Vec<(String, Value)> = orig.columns.iter().map(|c| c.name.clone()).zip(orig.rows[0].iter().cloned()).filter(|(c, _)| c != "made").collect();
    assert_eq!(run(d.as_mut(), &scope, &dml::update_row(dl, &p, &[("price".into(), Value::Text("2.00".into()))], &key))[0].affected_rows, 1);
    let del = dml::delete_row(dl, &p, &[("id".into(), Value::Int(2))]);
    assert_eq!(run(d.as_mut(), &scope, &del)[0].affected_rows, 1);
    assert_eq!(d.count(&p).unwrap(), 5);

    // multi statements, GO batches, multiple result sets
    let r = run(
        d.as_mut(),
        &scope,
        "UPDATE items SET flag = 1 WHERE name LIKE 'n%'; SELECT COUNT(*) AS c FROM items;\nGO\n\
         CREATE VIEW sales.v_items AS SELECT id, name FROM dbo.items;\nGO\n\
         CREATE OR ALTER PROCEDURE dbo.p_two AS BEGIN SET NOCOUNT ON; SELECT 1 AS a; SELECT 2 AS b; END\nGO\n\
         EXEC dbo.p_two",
    );
    assert_eq!(r.len(), 6, "{:#?}", r);
    assert_eq!(r[0].affected_rows, 4);
    assert!(r[1].is_query && r[1].rows[0][0] == Value::Int(5));
    assert!(!r[2].is_query && !r[3].is_query);
    assert_eq!((r[4].text(0, 0).as_str(), r[5].text(0, 0).as_str()), ("1", "2"), "{:#?}", r);
    let r = run(d.as_mut(), &scope, "DECLARE @n int = 3; SELECT @n * 2 AS six; SELECT 'x' AS y");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].rows[0][0], Value::Int(6));
    let r = run(d.as_mut(), &scope, "SELECT TOP 0 * FROM items");
    assert!(r[0].is_query && r[0].rows.is_empty() && r[0].columns.len() == 7);

    run(
        d.as_mut(),
        &scope,
        "CREATE FUNCTION dbo.f_add(@a int, @b int) RETURNS int AS BEGIN RETURN @a + @b; END\nGO\n\
         CREATE TRIGGER dbo.tr_items ON dbo.items AFTER INSERT AS BEGIN SET NOCOUNT ON; END\nGO\n\
         CREATE SEQUENCE dbo.seq1 AS bigint START WITH 10 INCREMENT BY 5",
    );

    // navigator listings
    let tables = d.list_objects(&scope, ObjectKind::Table).unwrap();
    let items = tables.iter().find(|t| t.name == "items").expect("items listed");
    assert_eq!(items.rows, Some(5));
    assert!(items.data_length.unwrap_or(0) > 0 && !items.created.is_empty());
    assert!(d.list_objects(&Scope::new("navdb_test", Some("sales".into())), ObjectKind::View).unwrap().iter().any(|v| v.name == "v_items"));
    assert!(d.list_objects(&scope, ObjectKind::Procedure).unwrap().iter().any(|v| v.name == "p_two"));
    assert!(d.list_objects(&scope, ObjectKind::Function).unwrap().iter().any(|v| v.name == "f_add"));
    let trg = d.list_objects(&scope, ObjectKind::Trigger).unwrap();
    assert!(trg.iter().any(|v| v.name == "tr_items" && v.extra[0].1 == "items"), "{:?}", trg);
    assert!(d.list_objects(&scope, ObjectKind::Sequence).unwrap().iter().any(|v| v.name == "seq1"));

    // DDL
    let tddl = d.object_ddl(&p, ObjectKind::Table).unwrap();
    assert!(tddl.contains("IDENTITY(1,1)") && tddl.contains("CREATE TRIGGER"), "{}", tddl);
    let vddl = d.object_ddl(&ObjPath::new("navdb_test", Some("sales".into()), "v_items"), ObjectKind::View).unwrap();
    assert!(vddl.starts_with("CREATE VIEW sales.v_items"), "{}", vddl);
    assert!(d.object_ddl(&scope.path("f_add"), ObjectKind::Function).unwrap().contains("RETURN @a + @b"));
    assert!(d.object_ddl(&scope.path("p_two"), ObjectKind::Procedure).unwrap().contains("SELECT 2 AS b"));
    assert!(d.object_ddl(&scope.path("tr_items"), ObjectKind::Trigger).unwrap().contains("AFTER INSERT"));
    let sddl = d.object_ddl(&scope.path("seq1"), ObjectKind::Sequence).unwrap();
    assert!(sddl.contains("START WITH 10") && sddl.contains("INCREMENT BY 5"), "{}", sddl);
    assert_eq!(d.query(Some(&scope), "SELECT dbo.f_add(2, 3)").unwrap().rows[0][0], Value::Int(5));

    // the generated table DDL recreates the table (copy under another name, GO-separated)
    let copy = tddl.replace("[items]", "[items_copy]").replace("[ix_items_name]", "[ix_items_copy]").replace("[ck_price]", "[ck_price2]").replace("tr_items", "tr_items2").replace("dbo.items", "dbo.items_copy");
    run(d.as_mut(), &scope, &copy);
    assert_eq!(d.table_def(&scope.path("items_copy")).unwrap().columns.len(), 7);

    // designer-style alter
    let mut new = back.clone();
    new.columns[1].name = "title".into();
    for ix in new.indexes.iter_mut() {
        for c in ix.columns.iter_mut() {
            if c == "name" {
                *c = "title".into();
            }
        }
    }
    new.columns.push(ColumnDef::new("qty", "int"));
    new.columns[2].data_type = "decimal(12,2)".into();
    for s in ddl::alter_table_with_renames(dl, &back, &new, &[("name".into(), "title".into())]) {
        run(d.as_mut(), &scope, &s);
    }
    let after = d.table_def(&p).unwrap();
    assert_eq!(after.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "title", "price", "made", "flag", "uid", "blob", "qty"]);
    assert_eq!(after.columns[2].data_type, "decimal(12,2)");

    // explain
    let e = d.explain(Some(&scope), "SELECT * FROM items WHERE id = 1").unwrap();
    assert!(e.is_query && !e.rows.is_empty(), "{:?}", e);
    assert!(e.rows.iter().any(|r| r[0].to_string().contains("items")), "{:?}", e.rows);
    assert_eq!(d.query(Some(&scope), "SELECT 1 AS one").unwrap().rows[0][0], Value::Int(1));

    // errors carry the server message
    let err = d.execute(Some(&scope), "SELECT nope FROM items").unwrap_err();
    assert!(format!("{:#}", err).contains("Invalid column name 'nope'"), "{:#}", err);
    let err = d.execute(Some(&scope), "SELECT FROM WHERE").unwrap_err();
    assert!(format!("{:#}", err).contains("Incorrect syntax"), "{:#}", err);
    assert_eq!(d.query(Some(&scope), "SELECT 2").unwrap().rows[0][0], Value::Int(2));

    // scope switching
    assert_eq!(d.query(Some(&Scope::new("master", None)), "SELECT DB_NAME()").unwrap().text(0, 0), "master");
    assert_eq!(d.query(Some(&scope), "SELECT DB_NAME()").unwrap().text(0, 0), "navdb_test");
    d.execute(None, "USE master").unwrap();
    assert_eq!(d.query(Some(&scope), "SELECT DB_NAME()").unwrap().text(0, 0), "navdb_test");

    // server monitor / users
    let snap = d.server_snapshot().unwrap();
    assert!(!snap.processes.rows.is_empty() && !snap.variables.rows.is_empty() && !snap.status.rows.is_empty());
    assert!(d.list_users().unwrap().iter().any(|u| u.name == "sa"));
    let spec = UserSpec { name: "navdb_tmp".into(), password: "Tmp_pass_123!".into(), can_login: true, create_db: true, ..Default::default() };
    for s in d.user_sql(None, &spec).unwrap() {
        d.execute(None, &s).unwrap_or_else(|e| panic!("{}: {:#}", s, e));
    }
    assert!(d.list_users().unwrap().iter().any(|u| u.name == "navdb_tmp"));
    let drop = d.drop_user_sql(&UserInfo { name: "navdb_tmp".into(), ..Default::default() }).unwrap();
    d.execute(None, &drop).unwrap();
    let mut other = driver::connect(&cfg()).unwrap();
    let spid = other.query(None, "SELECT @@SPID").unwrap().text(0, 0);
    d.kill_process(&spid).unwrap();
    assert!(other.query(None, "SELECT 1").is_err());

    // drops
    run(d.as_mut(), &scope, &ddl::drop_object(dl, ObjectKind::View, &ObjPath::new("navdb_test", Some("sales".into()), "v_items"), None));
    run(d.as_mut(), &scope, &ddl::drop_object(dl, ObjectKind::Procedure, &scope.path("p_two"), None));
    run(d.as_mut(), &scope, &ddl::drop_object(dl, ObjectKind::Trigger, &scope.path("tr_items"), None));
    run(d.as_mut(), &scope, &format!("DROP TABLE {}", dl.qualify(&child.path)));
    run(d.as_mut(), &scope, &format!("DROP TABLE {}", dl.qualify(&p)));
    assert!(!d.list_objects(&scope, ObjectKind::Table).unwrap().iter().any(|t| t.name == "items"));
    d.execute(None, "USE master; ALTER DATABASE navdb_test SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE navdb_test").unwrap();
}
