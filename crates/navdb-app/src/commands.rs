//! Commands shared by menus, the navigator, the toolbar and the object list.

use navdb_core::config::ConnectionConfig;
use navdb_core::{ddl, DbKind, ObjectKind, Scope};

use crate::dialogs;
use crate::state::{Action, AppState};
use crate::store;
use crate::tabs;

/// Saving one more connection needs Pro once the free limit is reached;
/// the sample database does not take one of the free slots.
fn connection_slot(st: &mut AppState) -> bool {
    let demo = demo_path().to_string_lossy().into_owned();
    let used = st.ws.connections.iter().filter(|c| !(c.kind == DbKind::Sqlite && c.database == demo)).count();
    used < store::FREE_CONNECTIONS || store::require_pro(st, "More than two saved connections")
}

/// Where the sample "shop" database lives in the data folder.
pub fn demo_path() -> std::path::PathBuf {
    navdb_core::config::data_dir().join("demo").join("shop.sqlite")
}

/// Writes the sample database and adds its connection and a saved query.
/// Returns the new connection's id.
pub fn add_demo(ws: &mut navdb_core::config::Workspace) -> Option<String> {
    let path = demo_path();
    navdb_core::demo::create(&path).ok()?;
    let c = navdb_core::demo::connection(&path);
    let id = c.id.clone();
    ws.queries.push(navdb_core::demo::query(&c));
    ws.connections.insert(0, c);
    let _ = ws.save();
    Some(id)
}

/// Opens the sample database, recreating it (and its connection) when it was removed.
pub fn open_demo(st: &mut AppState) {
    let demo = demo_path().to_string_lossy().into_owned();
    let existing = st.ws.connections.iter().find(|c| c.kind == DbKind::Sqlite && c.database == demo).map(|c| c.id.clone());
    let id = match existing {
        Some(id) if demo_path().exists() => Some(id),
        Some(id) => navdb_core::demo::create(&demo_path()).ok().map(|_| id),
        None => add_demo(&mut st.ws),
    };
    match id {
        Some(id) => {
            st.push(Action::OpenConnection(id.clone()));
            st.selection = crate::state::Selection { conn: Some(id), ..Default::default() };
        }
        None => st.error("Could not create the demo database"),
    }
}

pub fn new_connection(st: &mut AppState, kind: DbKind) {
    if !connection_slot(st) {
        return;
    }
    st.open_dialog(dialogs::connection::ConnectionDialog::new(ConnectionConfig::new(kind), true));
}

pub fn edit_connection(st: &mut AppState, id: &str) {
    if let Some(c) = st.config(id).cloned() {
        st.open_dialog(dialogs::connection::ConnectionDialog::new(c, false));
    }
}

pub fn duplicate_connection(st: &mut AppState, id: &str) {
    if !connection_slot(st) {
        return;
    }
    if let Some(mut c) = st.config(id).cloned() {
        c.id = uuid::Uuid::new_v4().to_string();
        c.name = format!("{} copy", c.display_name());
        st.ws.connections.push(c);
        st.save_workspace();
    }
}

pub fn delete_connection(st: &mut AppState, id: &str) {
    let name = st.conn_name(id);
    let id = id.to_string();
    st.open_dialog(dialogs::confirm::ConfirmDialog::new(
        "Delete Connection",
        format!("Are you sure you want to delete connection \"{}\"?", name),
        Box::new(move |st: &mut AppState| {
            st.push(Action::CloseConnection(id.clone()));
            st.ws.connections.retain(|c| c.id != id);
            st.save_workspace();
        }),
    ));
}

pub fn new_query(st: &mut AppState, conn: Option<String>, scope: Option<Scope>) {
    let t = tabs::query::QueryTab::new(st, conn, scope, String::new(), None);
    st.open_tab(t);
}

pub fn new_query_with(st: &mut AppState, conn: Option<String>, scope: Option<Scope>, sql: String) {
    let t = tabs::query::QueryTab::new(st, conn, scope, sql, None);
    st.open_tab(t);
}

pub fn open_saved_query(st: &mut AppState, query_id: &str) {
    if let Some(q) = st.ws.queries.iter().find(|q| q.id == query_id).cloned() {
        let scope = if q.database.is_empty() { None } else { Some(parse_scope(&q.database)) };
        let t = tabs::query::QueryTab::new(st, Some(q.connection_id.clone()), scope, q.sql.clone(), Some(q.id.clone()));
        st.open_tab(t);
    }
}

/// Inverse of `Scope::display` ("db" or "db.schema").
pub fn parse_scope(s: &str) -> Scope {
    match s.split_once('.') {
        Some((db, schema)) => Scope::new(db, Some(schema.to_string())),
        None => Scope::new(s, None),
    }
}

pub fn open_object(st: &mut AppState, conn: &str, scope: &Scope, kind: ObjectKind, name: &str) {
    match kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
            let t = tabs::table_data::TableDataTab::new(st, conn, scope.path(name), kind);
            st.open_tab(t);
        }
        ObjectKind::Key => {
            let t = tabs::redis_key::RedisKeyTab::new(st, conn, scope.db.clone(), name.to_string());
            st.open_tab(t);
        }
        _ => design_object(st, conn, scope, kind, Some(name.to_string())),
    }
}

pub fn design_object(st: &mut AppState, conn: &str, scope: &Scope, kind: ObjectKind, name: Option<String>) {
    match kind {
        ObjectKind::Table => {
            let t = tabs::table_design::TableDesignTab::new(st, conn, scope.clone(), name);
            st.open_tab(t);
        }
        ObjectKind::View => {
            let t = tabs::view_design::ViewDesignTab::new(st, conn, scope.clone(), name);
            st.open_tab(t);
        }
        ObjectKind::Key => {
            if let Some(n) = name {
                let t = tabs::redis_key::RedisKeyTab::new(st, conn, scope.db.clone(), n);
                st.open_tab(t);
            } else {
                let t = tabs::redis_key::RedisKeyTab::new_key(st, conn, scope.db.clone());
                st.open_tab(t);
            }
        }
        _ => {
            let t = tabs::routine::RoutineTab::new(st, conn, scope.clone(), kind, name);
            st.open_tab(t);
        }
    }
}

pub fn drop_objects(st: &mut AppState, conn: &str, scope: &Scope, kind: ObjectKind, names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let Some(cfg) = st.config(conn).cloned() else { return };
    let d = cfg.dialect();
    // PG triggers need their table; it is stored in the object's extra info
    let infos = st
        .conn(conn)
        .and_then(|c| c.objects(scope, kind))
        .and_then(|l| l.ready())
        .cloned()
        .unwrap_or_default();
    let stmts: Vec<String> = names
        .iter()
        .map(|n| {
            let extra_table = infos
                .iter()
                .find(|o| &o.name == n)
                .and_then(|o| o.extra.iter().find(|(k, _)| k == "Table").map(|(_, v)| v.clone()));
            match cfg.kind.base() {
                DbKind::Redis => return format!("DEL {}", quote_redis(n)),
                DbKind::MongoDb => return format!("db.getCollection({}).drop()", serde_json::Value::String(n.clone())),
                DbKind::OpenSearch => return format!("DELETE /{}", n),
                _ => {}
            }
            match kind {
                ObjectKind::Function | ObjectKind::Procedure if cfg.kind.base() == DbKind::PostgreSql => {
                    let (base, args) = match n.find('(') {
                        Some(p) => (&n[..p], n[p + 1..].trim_end_matches(')')),
                        None => (n.as_str(), ""),
                    };
                    ddl::drop_object(d, kind, &scope.path(base), Some(args))
                }
                ObjectKind::Trigger if cfg.kind.base() == DbKind::PostgreSql => {
                    let t = scope.path(extra_table.unwrap_or_default());
                    ddl::drop_trigger(d, &t, n)
                }
                ObjectKind::Index if cfg.kind.base() == DbKind::MySql || cfg.kind.base() == DbKind::MariaDb => {
                    let t = d.qualify(&scope.path(extra_table.unwrap_or_default()));
                    ddl::drop_object(d, kind, &scope.path(n.clone()), Some(&t))
                }
                _ => ddl::drop_object(d, kind, &scope.path(n.clone()), None),
            }
        })
        .collect();
    let sql = stmts.join(if cfg.kind.base() == DbKind::OpenSearch { "\n\n" } else { ";\n" });
    let conn = conn.to_string();
    let scope = scope.clone();
    let msg = format!("Are you sure you want to delete {} {}?\n\n{}", names.len(), if names.len() == 1 { kind.singular().to_lowercase() } else { kind.label().to_lowercase() }, names.join(", "));
    let do_it = move |st: &mut AppState| {
        st.run_task(&conn, Some(scope.clone()), sql.clone(), "Deleted", true);
    };
    if st.prefs().confirm_delete {
        st.open_dialog(dialogs::confirm::ConfirmDialog::new("Delete Object", msg, Box::new(do_it)));
    } else {
        do_it(st);
    }
}

pub fn quote_redis(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn empty_tables(st: &mut AppState, conn: &str, scope: &Scope, names: Vec<String>, truncate: bool) {
    let Some(cfg) = st.config(conn) else { return };
    let d = cfg.dialect();
    let sql = names
        .iter()
        .map(|n| {
            // document stores only understand the DELETE form
            if truncate && cfg.kind.is_relational() {
                ddl::truncate_table(d, &scope.path(n.clone()))
            } else {
                ddl::empty_table(d, &scope.path(n.clone()))
            }
        })
        .collect::<Vec<_>>()
        .join(";\n");
    let conn = conn.to_string();
    let scope = scope.clone();
    let verb = if truncate { "truncate" } else { "empty" };
    st.open_dialog(dialogs::confirm::ConfirmDialog::new(
        if truncate { "Truncate Table" } else { "Empty Table" },
        format!("Are you sure you want to {} {}? All records will be lost.", verb, names.join(", ")),
        Box::new(move |st: &mut AppState| st.run_task(&conn, Some(scope.clone()), sql.clone(), "Done", true)),
    ));
}

pub fn rename_object(st: &mut AppState, conn: &str, scope: &Scope, kind: ObjectKind, name: &str) {
    let Some(cfg) = st.config(conn) else { return };
    let d = cfg.dialect();
    let is_redis = cfg.kind.base() == DbKind::Redis;
    let conn = conn.to_string();
    let scope = scope.clone();
    let old = name.to_string();
    st.open_dialog(dialogs::prompt::PromptDialog::new(
        format!("Rename {}", kind.singular()),
        "New name:",
        name.to_string(),
        Box::new(move |st: &mut AppState, new_name: String| {
            if new_name.is_empty() || new_name == old {
                return;
            }
            let sql = if is_redis {
                format!("RENAME {} {}", quote_redis(&old), quote_redis(&new_name))
            } else {
                ddl::rename_object(d, kind, &scope.path(old.clone()), &new_name)
            };
            st.run_task(&conn, Some(scope.clone()), sql, "Renamed", true);
        }),
    ));
}

pub fn duplicate_table(st: &mut AppState, conn: &str, scope: &Scope, name: &str, with_data: bool) {
    let Some(session) = st.session(conn) else { return };
    let path = scope.path(name);
    let new_name = format!("{}_copy", name);
    let conn_id = conn.to_string();
    let sc = scope.clone();
    // read the definition on the worker, then run the generated script there too
    let pending = session.run(move |d| {
        let def = d.table_def(&path)?;
        let stmts = ddl::duplicate_table(d.dialect(), &def, &new_name, with_data);
        let mut out = Vec::new();
        for s in &stmts {
            out.extend(d.execute(Some(&path.scope()), s)?);
        }
        Ok(out)
    });
    st.tasks.push(crate::state::BgTask {
        conn: conn_id,
        scope: Some(sc),
        sql: format!("-- duplicate {}", name),
        pending,
        success: "Table duplicated".into(),
        refresh: true,
        refresh_databases: false,
        started: std::time::Instant::now(),
    });
}

pub fn new_database(st: &mut AppState, conn: &str) {
    let d = dialogs::new_database::DatabaseDialog::new(st, conn, None);
    st.open_dialog(d);
}

pub fn edit_database(st: &mut AppState, conn: &str, db: &str) {
    let d = dialogs::new_database::DatabaseDialog::new(st, conn, Some(db.to_string()));
    st.open_dialog(d);
}

pub fn drop_database(st: &mut AppState, conn: &str, db: &str) {
    let Some(cfg) = st.config(conn) else { return };
    let sql = ddl::drop_database(cfg.dialect(), db);
    let conn = conn.to_string();
    st.open_dialog(dialogs::confirm::ConfirmDialog::new(
        "Delete Database",
        format!("Are you sure you want to delete database \"{}\"? This cannot be undone.", db),
        Box::new(move |st: &mut AppState| st.run_task_ex(&conn, None, sql.clone(), "Database deleted", true, true)),
    ));
}

pub fn new_schema(st: &mut AppState, conn: &str, db: &str) {
    let conn = conn.to_string();
    let db = db.to_string();
    st.open_dialog(dialogs::prompt::PromptDialog::new(
        "New Schema",
        "Schema name:",
        String::new(),
        Box::new(move |st: &mut AppState, name: String| {
            if name.is_empty() {
                return;
            }
            let sql = ddl::create_schema(navdb_core::Dialect::Postgres, &name);
            if let Some(c) = st.conn_mut(&conn) {
                c.schemas.remove(&db);
            }
            st.run_task(&conn, Some(Scope::new(db.clone(), None)), sql, "Schema created", true);
        }),
    ));
}

pub fn drop_schema(st: &mut AppState, conn: &str, db: &str, schema: &str) {
    let sql = format!("DROP SCHEMA {} CASCADE", navdb_core::Dialect::Postgres.quote_ident(schema));
    let conn = conn.to_string();
    let db = db.to_string();
    st.open_dialog(dialogs::confirm::ConfirmDialog::new(
        "Delete Schema",
        format!("Delete schema \"{}\" and ALL objects in it?", schema),
        Box::new(move |st: &mut AppState| {
            if let Some(c) = st.conn_mut(&conn) {
                c.schemas.remove(&db);
            }
            st.run_task(&conn, Some(Scope::new(db.clone(), None)), sql.clone(), "Schema deleted", true)
        }),
    ));
}

pub fn export_wizard(st: &mut AppState, conn: &str, scope: &Scope, tables: Vec<String>) {
    let d = dialogs::export::ExportWizard::new(st, conn, scope.clone(), tables);
    st.open_dialog(d);
}

pub fn export_result(st: &mut AppState, result: navdb_core::QueryResult, name: &str, dialect: navdb_core::Dialect) {
    st.open_dialog(dialogs::export::ExportWizard::from_result(result, name, dialect));
}

pub fn import_wizard(st: &mut AppState, conn: &str, scope: &Scope, table: Option<String>) {
    let d = dialogs::import::ImportWizard::new(st, conn, scope.clone(), table);
    st.open_dialog(d);
}

pub fn dump_sql(st: &mut AppState, conn: &str, scope: &Scope, tables: Vec<String>, data: bool) {
    let d = dialogs::dump::DumpDialog::new(st, conn, scope.clone(), tables, data);
    st.open_dialog(d);
}

pub fn execute_sql_file(st: &mut AppState, conn: &str, scope: Option<Scope>) {
    let d = dialogs::execute_sql::ExecuteSqlDialog::new(st, conn, scope);
    st.open_dialog(d);
}

pub fn backup(st: &mut AppState, conn: &str, scope: &Scope) {
    let d = dialogs::backup::BackupDialog::new(st, conn, scope.clone());
    st.open_dialog(d);
}

pub fn data_transfer(st: &mut AppState, source: Option<(String, Scope)>) {
    if !store::require_pro(st, "Data Transfer") {
        return;
    }
    let d = dialogs::data_transfer::DataTransferDialog::new(st, source);
    st.open_dialog(d);
}

pub fn structure_sync(st: &mut AppState, source: Option<(String, Scope)>) {
    if !store::require_pro(st, "Structure Sync") {
        return;
    }
    let d = dialogs::structure_sync::StructureSyncDialog::new(st, source);
    st.open_dialog(d);
}

pub fn data_sync(st: &mut AppState, source: Option<(String, Scope)>) {
    if !store::require_pro(st, "Data Sync") {
        return;
    }
    let d = dialogs::data_sync::DataSyncDialog::new(st, source);
    st.open_dialog(d);
}

pub fn data_generation(st: &mut AppState, conn: &str, scope: &Scope, tables: Vec<String>) {
    let d = dialogs::data_gen::DataGenDialog::new(st, conn, scope.clone(), tables);
    st.open_dialog(d);
}

pub fn server_monitor(st: &mut AppState, conn: &str) {
    let t = tabs::server_monitor::ServerMonitorTab::new(st, conn);
    st.open_tab(t);
}

pub fn users(st: &mut AppState, conn: &str) {
    let t = tabs::users::UsersTab::new(st, conn);
    st.open_tab(t);
}

pub fn console(st: &mut AppState, conn: &str) {
    let t = tabs::console::ConsoleTab::new(st, conn);
    st.open_tab(t);
}

pub fn er_model(st: &mut AppState, conn: &str, scope: &Scope) {
    if !store::require_pro(st, "The ER model") {
        return;
    }
    let t = tabs::er_model::ErModelTab::new(st, conn, scope.clone());
    st.open_tab(t);
}

pub fn charts(st: &mut AppState, source: Option<(String, Scope, String)>) {
    if !store::require_pro(st, "Charts") {
        return;
    }
    let t = tabs::charts::ChartsTab::new(st, source);
    st.open_tab(t);
}

pub fn automation(st: &mut AppState) {
    if !store::require_pro(st, "Automation") {
        return;
    }
    let t = tabs::automation::AutomationTab::new(st);
    st.open_tab(t);
}

pub fn history(st: &mut AppState) {
    st.open_tab(tabs::history::HistoryTab::new());
}

pub fn find_in_database(st: &mut AppState, conn: &str, scope: &Scope) {
    let t = tabs::find::FindInDbTab::new(st, conn, scope.clone());
    st.open_tab(t);
}

pub fn preferences(st: &mut AppState) {
    let d = dialogs::preferences::PreferencesDialog::new(st);
    st.open_dialog(d);
}

pub fn about(st: &mut AppState) {
    st.open_dialog(dialogs::about::AboutDialog::new());
}

pub fn export_connections(st: &mut AppState) {
    st.open_dialog(dialogs::manage_connections::ExportConnectionsDialog::new(st));
}

pub fn import_connections(st: &mut AppState) {
    if !store::require_pro(st, "Importing connections") {
        return;
    }
    let Some(path) = crate::ui::open_file_dialog(&["json", "ncx"]) else { return };
    match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|s| navdb_core::config::import_connections(&s)) {
        Ok(list) => {
            let n = list.len();
            for mut c in list {
                if st.ws.connections.iter().any(|x| x.id == c.id) {
                    c.id = uuid::Uuid::new_v4().to_string();
                }
                st.ws.connections.push(c);
            }
            st.save_workspace();
            st.toast(format!("Imported {} connection(s)", n));
        }
        Err(e) => st.error(format!("Import failed: {:#}", e)),
    }
}
