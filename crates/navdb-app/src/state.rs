//! Application-wide state shared by all tabs and dialogs.

use std::collections::{HashMap, HashSet};

use navdb_core::config::{self, ConnectionConfig, HistoryEntry, Workspace};
use navdb_core::session::Notify;
use navdb_core::{ColumnDef, ForeignKeyDef, ObjectInfo, ObjectKind, Pending, Scope, Session};

use crate::dialogs::Dialog;
use crate::tabs::Tab;

/// Value that is loaded asynchronously from a session.
pub enum Loadable<T> {
    NotLoaded,
    Loading(Pending<T>),
    Ready(T),
    Failed(String),
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Loadable::NotLoaded
    }
}

impl<T> Loadable<T> {
    /// Move a finished `Pending` into `Ready`/`Failed`. Returns true when it changed.
    pub fn poll(&mut self) -> bool {
        if let Loadable::Loading(p) = self {
            if let Some(r) = p.poll() {
                *self = match r {
                    Ok(v) => Loadable::Ready(v),
                    Err(e) => Loadable::Failed(format!("{:#}", e)),
                };
                return true;
            }
        }
        false
    }

    pub fn ready(&self) -> Option<&T> {
        match self {
            Loadable::Ready(v) => Some(v),
            _ => None,
        }
    }

    pub fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Loadable::Ready(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Loadable::Loading(_))
    }

    pub fn is_not_loaded(&self) -> bool {
        matches!(self, Loadable::NotLoaded)
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Loadable::Failed(e) => Some(e),
            _ => None,
        }
    }
}

/// An opened connection in the navigator.
pub struct OpenConn {
    pub cfg: ConnectionConfig,
    pub session: Session,
    pub version: Loadable<String>,
    pub databases: Loadable<Vec<String>>,
    pub schemas: HashMap<String, Loadable<Vec<String>>>,
    pub objects: HashMap<(Scope, ObjectKind), Loadable<Vec<ObjectInfo>>>,
    /// Table columns loaded on demand for auto-completion, shared by all tabs.
    pub columns: HashMap<(Scope, String), Loadable<Vec<ColumnDef>>>,
    /// Foreign keys per table, for JOIN conditions and hover info.
    pub foreign_keys: HashMap<(Scope, String), Loadable<Vec<ForeignKeyDef>>>,
    pub kinds: Loadable<Vec<ObjectKind>>,
    pub expanded: HashSet<String>,
}

impl OpenConn {
    pub fn new(cfg: ConnectionConfig, notify: Notify) -> Self {
        let session = Session::spawn(cfg.clone(), notify);
        let version = Loadable::Loading(session.run(|d| d.server_version()));
        let databases = Loadable::Loading(session.run(|d| d.list_databases()));
        let kinds = Loadable::Loading(session.run(|d| Ok(d.object_kinds())));
        Self {
            cfg,
            session,
            version,
            databases,
            schemas: HashMap::new(),
            objects: HashMap::new(),
            columns: HashMap::new(),
            foreign_keys: HashMap::new(),
            kinds,
            expanded: HashSet::new(),
        }
    }

    pub fn poll(&mut self) {
        self.version.poll();
        self.databases.poll();
        self.kinds.poll();
        for v in self.schemas.values_mut() {
            v.poll();
        }
        for v in self.objects.values_mut() {
            v.poll();
        }
        for v in self.columns.values_mut() {
            v.poll();
        }
        for v in self.foreign_keys.values_mut() {
            v.poll();
        }
    }

    pub fn object_kinds(&self) -> Vec<ObjectKind> {
        self.kinds.ready().cloned().unwrap_or_default()
    }

    /// Databases filtered by the connection's custom database list.
    pub fn visible_databases(&self) -> Vec<String> {
        let all = self.databases.ready().cloned().unwrap_or_default();
        if self.cfg.custom_databases.is_empty() {
            all
        } else {
            all.into_iter().filter(|d| self.cfg.custom_databases.contains(d)).collect()
        }
    }

    pub fn load_schemas(&mut self, db: &str) {
        let e = self.schemas.entry(db.to_string()).or_default();
        if e.is_not_loaded() {
            let db = db.to_string();
            *e = Loadable::Loading(self.session.run(move |d| d.list_schemas(&db)));
        }
    }

    pub fn load_objects(&mut self, scope: &Scope, kind: ObjectKind) {
        let e = self.objects.entry((scope.clone(), kind)).or_default();
        if e.is_not_loaded() {
            let s = scope.clone();
            *e = Loadable::Loading(self.session.run(move |d| d.list_objects(&s, kind)));
        }
    }

    pub fn objects(&self, scope: &Scope, kind: ObjectKind) -> Option<&Loadable<Vec<ObjectInfo>>> {
        self.objects.get(&(scope.clone(), kind))
    }

    /// Columns of `table`, loading them in the background on first use.
    pub fn columns(&mut self, scope: &Scope, table: &str) -> Option<&[ColumnDef]> {
        let e = self.columns.entry((scope.clone(), table.to_string())).or_default();
        if e.is_not_loaded() {
            let p = scope.path(table);
            *e = Loadable::Loading(self.session.run(move |d| d.columns(&p)));
        }
        e.poll();
        match e {
            Loadable::Ready(v) => Some(v.as_slice()),
            Loadable::Failed(_) => Some(&[]),
            _ => None,
        }
    }

    /// Foreign keys of `table`, loading them in the background on first use.
    pub fn foreign_keys(&mut self, scope: &Scope, table: &str) -> Option<&[ForeignKeyDef]> {
        let e = self.foreign_keys.entry((scope.clone(), table.to_string())).or_default();
        if e.is_not_loaded() {
            let p = scope.path(table);
            *e = Loadable::Loading(self.session.run(move |d| Ok(d.table_def(&p)?.foreign_keys)));
        }
        e.poll();
        match e {
            Loadable::Ready(v) => Some(v.as_slice()),
            Loadable::Failed(_) => Some(&[]),
            _ => None,
        }
    }

    pub fn refresh_scope(&mut self, scope: &Scope) {
        self.objects.retain(|(s, _), _| s != scope);
        self.columns.retain(|(s, _), _| s != scope);
        self.foreign_keys.retain(|(s, _), _| s != scope);
    }

    pub fn refresh_all(&mut self) {
        self.objects.clear();
        self.columns.clear();
        self.foreign_keys.clear();
        self.schemas.clear();
        self.databases = Loadable::Loading(self.session.run(|d| d.list_databases()));
    }
}

/// What is selected in the navigator; drives the Objects tab and info pane.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub conn: Option<String>,
    pub scope: Option<Scope>,
    pub kind: Option<ObjectKind>,
    pub object: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub until: f64,
}

/// Deferred operations requested by UI code and applied by the app loop.
pub enum Action {
    OpenTab(Box<dyn Tab>),
    OpenDialog(Box<dyn Dialog>),
    OpenConnection(String),
    CloseConnection(String),
    /// Re-read databases and drop cached objects.
    RefreshConnection(String),
    /// Re-read objects of one database/schema.
    RefreshScope { conn: String, scope: Scope },
    Select(Selection),
    SaveWorkspace,
    Toast { text: String, error: bool },
    History(HistoryEntry),
    CloseTabByKey(String),
}

/// Fire-and-forget statement run by commands (drop, truncate, rename...).
pub struct BgTask {
    pub conn: String,
    pub scope: Option<Scope>,
    pub sql: String,
    pub pending: Pending<Vec<navdb_core::QueryResult>>,
    pub success: String,
    pub refresh: bool,
    /// Also reload the database list (create/drop database).
    pub refresh_databases: bool,
    pub started: std::time::Instant,
}

pub struct AppState {
    pub ws: Workspace,
    pub tasks: Vec<BgTask>,
    pub conns: HashMap<String, OpenConn>,
    pub selection: Selection,
    pub notify: Notify,
    pub actions: Vec<Action>,
    pub history: Vec<HistoryEntry>,
    pub toasts: Vec<Toast>,
    pub time: f64,
    /// Object list filter text (search box in the Objects tab).
    pub object_filter: String,
}

impl AppState {
    pub fn new(notify: Notify) -> Self {
        let ws = Workspace::load();
        let max = ws.preferences.max_history;
        Self {
            ws,
            conns: HashMap::new(),
            tasks: Vec::new(),
            selection: Selection::default(),
            notify,
            actions: Vec::new(),
            history: config::load_history(max),
            toasts: Vec::new(),
            time: 0.0,
            object_filter: String::new(),
        }
    }

    pub fn push(&mut self, a: Action) {
        self.actions.push(a);
    }

    pub fn open_tab(&mut self, t: impl Tab + 'static) {
        self.actions.push(Action::OpenTab(Box::new(t)));
    }

    pub fn open_dialog(&mut self, d: impl Dialog + 'static) {
        self.actions.push(Action::OpenDialog(Box::new(d)));
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.actions.push(Action::Toast { text: text.into(), error: false });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.actions.push(Action::Toast { text: text.into(), error: true });
    }

    pub fn save_workspace(&mut self) {
        self.actions.push(Action::SaveWorkspace);
    }

    pub fn refresh_scope(&mut self, conn: &str, scope: &Scope) {
        self.actions.push(Action::RefreshScope { conn: conn.to_string(), scope: scope.clone() });
    }

    pub fn config(&self, id: &str) -> Option<&ConnectionConfig> {
        self.ws.connections.iter().find(|c| c.id == id)
    }

    pub fn conn(&self, id: &str) -> Option<&OpenConn> {
        self.conns.get(id)
    }

    pub fn conn_mut(&mut self, id: &str) -> Option<&mut OpenConn> {
        self.conns.get_mut(id)
    }

    /// Shared session for a connection, opening the connection if needed.
    pub fn session(&mut self, id: &str) -> Option<Session> {
        if !self.conns.contains_key(id) {
            let cfg = self.config(id)?.clone();
            self.conns.insert(id.to_string(), OpenConn::new(cfg, self.notify.clone()));
        }
        self.conns.get(id).map(|c| c.session.clone())
    }

    pub fn conn_name(&self, id: &str) -> String {
        self.config(id).map(|c| c.display_name()).unwrap_or_else(|| "?".into())
    }

    pub fn record(&mut self, conn: &str, scope: Option<&Scope>, sql: &str, ok: bool, message: &str, elapsed_ms: u128) {
        let e = HistoryEntry {
            time: chrono::Local::now(),
            connection: self.conn_name(conn),
            database: scope.map(|s| s.display()).unwrap_or_default(),
            sql: sql.to_string(),
            ok,
            message: message.to_string(),
            elapsed_ms,
        };
        self.actions.push(Action::History(e));
    }

    /// Run `sql` in the background, then toast/refresh when finished.
    pub fn run_task(&mut self, conn: &str, scope: Option<Scope>, sql: String, success: impl Into<String>, refresh: bool) {
        self.run_task_ex(conn, scope, sql, success, refresh, false);
    }

    pub fn run_task_ex(&mut self, conn: &str, scope: Option<Scope>, sql: String, success: impl Into<String>, refresh: bool, refresh_databases: bool) {
        let Some(session) = self.session(conn) else {
            self.error("connection not found");
            return;
        };
        let sc = scope.clone();
        let q = sql.clone();
        let pending = session.run(move |d| d.execute(sc.as_ref(), &q));
        self.tasks.push(BgTask {
            conn: conn.to_string(),
            scope,
            sql,
            pending,
            success: success.into(),
            refresh,
            refresh_databases,
            started: std::time::Instant::now(),
        });
    }

    pub fn prefs(&self) -> &config::Preferences {
        &self.ws.preferences
    }

    /// Every open connection's tables (for completion and pickers).
    pub fn cached_tables(&self, conn: &str, scope: &Scope) -> Vec<String> {
        self.conns
            .get(conn)
            .and_then(|c| c.objects(scope, ObjectKind::Table))
            .and_then(|l| l.ready())
            .map(|v| v.iter().map(|o| o.name.clone()).collect())
            .unwrap_or_default()
    }

    /// (conn id, display name) for every configured connection.
    pub fn connection_choices(&self) -> Vec<(String, String)> {
        self.ws.connections.iter().map(|c| (c.id.clone(), c.display_name())).collect()
    }
}
