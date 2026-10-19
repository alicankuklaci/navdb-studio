//! User manager: user list plus a User Designer (general options,
//! privileges editor, current grants, SQL preview).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use egui::{RichText, Sense};
use egui_extras::{Column, TableBuilder};
use navdb_core::{DbKind, Dialect, ObjectKind, Pending, Scope, Session, UserInfo, UserSpec};

use super::Tab;
use crate::state::{AppState, Loadable};
use crate::ui::style::icon;

const MYSQL_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "EXECUTE", "ALL PRIVILEGES"];
const PG_TABLE_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "ALL PRIVILEGES"];
const PG_SCHEMA_PRIVS: &[&str] = &["USAGE", "CREATE", "ALL PRIVILEGES"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum PgTarget {
    Schema,
    AllTables,
    Table,
}

impl PgTarget {
    fn label(self) -> &'static str {
        match self {
            PgTarget::Schema => "Schema",
            PgTarget::AllTables => "All tables in schema",
            PgTarget::Table => "Table",
        }
    }
}

#[derive(Clone)]
struct PrivRow {
    /// MySQL database ("*" = global) or PostgreSQL schema.
    db: String,
    /// Table name, "*" = every table.
    table: String,
    target: PgTarget,
    privs: Vec<String>,
}

impl PrivRow {
    fn object(&self, kind: DbKind) -> String {
        match kind {
            DbKind::PostgreSql => {
                let d = Dialect::Postgres;
                match self.target {
                    PgTarget::Schema => format!("SCHEMA {}", d.quote_ident(&self.db)),
                    PgTarget::AllTables => format!("ALL TABLES IN SCHEMA {}", d.quote_ident(&self.db)),
                    PgTarget::Table => format!("{}.{}", d.quote_ident(&self.db), d.quote_ident(&self.table)),
                }
            }
            _ => {
                let d = Dialect::MySql;
                if self.db == "*" || self.db.is_empty() {
                    "*.*".into()
                } else if self.table == "*" || self.table.is_empty() {
                    format!("{}.*", d.quote_ident(&self.db))
                } else {
                    format!("{}.{}", d.quote_ident(&self.db), d.quote_ident(&self.table))
                }
            }
        }
    }

    fn available(&self, kind: DbKind) -> &'static [&'static str] {
        match kind {
            DbKind::PostgreSql if self.target == PgTarget::Schema => PG_SCHEMA_PRIVS,
            DbKind::PostgreSql => PG_TABLE_PRIVS,
            _ => MYSQL_PRIVS,
        }
    }

    fn privilege_text(&self) -> String {
        if self.privs.iter().any(|p| p == "ALL PRIVILEGES") {
            "ALL PRIVILEGES".into()
        } else {
            self.privs.join(", ")
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Privileges,
    Grants,
    Sql,
}

struct Designer {
    id: String,
    old: Option<UserInfo>,
    name: String,
    host: String,
    password: String,
    confirm: String,
    superuser: bool,
    can_login: bool,
    create_db: bool,
    create_role: bool,
    page: Page,
    privs: Vec<PrivRow>,
    grants: Loadable<Vec<String>>,
    preview: Loadable<Vec<String>>,
    preview_key: String,
    saving: Option<Pending<Vec<String>>>,
    error: Option<String>,
}

fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "yes" | "y" | "on")
}

impl Designer {
    fn new(kind: DbKind, old: Option<UserInfo>, session: Option<&Session>) -> Self {
        let attr = |k: &str| old.as_ref().and_then(|u| u.attributes.iter().find(|(n, _)| n == k)).map(|(_, v)| truthy(v));
        let grants = match (&old, session) {
            (Some(u), Some(s)) => {
                let u = u.clone();
                Loadable::Loading(s.run(move |d| d.user_grants(&u)))
            }
            _ => Loadable::NotLoaded,
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: old.as_ref().map(|u| u.name.clone()).unwrap_or_default(),
            host: old.as_ref().map(|u| u.host.clone()).unwrap_or_else(|| if kind.base() == DbKind::PostgreSql { String::new() } else { "%".into() }),
            password: String::new(),
            confirm: String::new(),
            superuser: attr("Superuser").unwrap_or(false),
            can_login: attr("Can Login").unwrap_or(true),
            create_db: attr("Create DB").unwrap_or(false),
            create_role: attr("Create Role").unwrap_or(false),
            page: Page::General,
            privs: Vec::new(),
            grants,
            preview: Loadable::NotLoaded,
            preview_key: String::new(),
            saving: None,
            error: None,
            old,
        }
    }

    fn spec(&self, kind: DbKind) -> UserSpec {
        UserSpec {
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            password: self.password.clone(),
            superuser: self.superuser,
            can_login: self.can_login,
            create_db: self.create_db,
            create_role: self.create_role,
            grants: self
                .privs
                .iter()
                .filter(|r| !r.privs.is_empty() && !(kind.base() == DbKind::PostgreSql && r.db.is_empty()))
                .map(|r| (r.privilege_text(), r.object(kind)))
                .collect(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("User name is required.".into());
        }
        if self.password != self.confirm {
            return Err("Password and confirmation do not match.".into());
        }
        if self.privs.iter().any(|r| r.target == PgTarget::Table && r.table.is_empty() && !r.privs.is_empty()) {
            return Err("Choose a table for every table privilege.".into());
        }
        Ok(())
    }

    fn title(&self) -> String {
        match &self.old {
            None => "New User".into(),
            Some(u) => format!("Edit User - {}", u.display()),
        }
    }

    fn masked(&self, sql: &str) -> String {
        if self.password.is_empty() {
            sql.to_string()
        } else {
            sql.replace(&self.password, "********")
        }
    }
}

pub struct UsersTab {
    conn: String,
    conn_name: String,
    kind: DbKind,
    pg_db: String,
    session: Option<Session>,
    users: Loadable<Vec<UserInfo>>,
    filter: String,
    selected: Option<UserInfo>,
    designer: Option<Designer>,
    delete_request: Rc<RefCell<Option<UserInfo>>>,
    deleting: Option<(UserInfo, Pending<String>)>,
    objects: Loadable<Vec<String>>,
    tables: HashMap<String, Loadable<Vec<String>>>,
}

impl UsersTab {
    pub fn new(st: &mut AppState, conn: &str) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        let session = st.session(conn);
        let mut me = Self {
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            kind,
            pg_db: cfg.as_ref().map(|c| c.default_database()).unwrap_or_default(),
            session,
            users: Loadable::NotLoaded,
            filter: String::new(),
            selected: None,
            designer: None,
            delete_request: Rc::new(RefCell::new(None)),
            deleting: None,
            objects: Loadable::NotLoaded,
            tables: HashMap::new(),
        };
        me.reload();
        me
    }

    fn reload(&mut self) {
        if let Some(s) = &self.session {
            self.users = Loadable::Loading(s.run(|d| d.list_users()));
        } else {
            self.users = Loadable::Failed("Connection not found".into());
        }
    }

    fn load_objects(&mut self) {
        if !self.objects.is_not_loaded() {
            return;
        }
        let Some(s) = &self.session else { return };
        self.objects = match self.kind {
            DbKind::PostgreSql => {
                let db = self.pg_db.clone();
                Loadable::Loading(s.run(move |d| d.list_schemas(&db)))
            }
            _ => Loadable::Loading(s.run(|d| d.list_databases())),
        };
    }

    fn open_designer(&mut self, user: Option<UserInfo>) {
        self.load_objects();
        self.designer = Some(Designer::new(self.kind, user, self.session.as_ref()));
    }

    fn poll(&mut self, st: &mut AppState) {
        if self.users.poll() {
            // keep the selection pointing at an existing user
            if let (Some(sel), Some(list)) = (&self.selected, self.users.ready()) {
                if !list.iter().any(|u| u.name == sel.name && u.host == sel.host) {
                    self.selected = None;
                }
            }
        }
        self.objects.poll();
        for t in self.tables.values_mut() {
            t.poll();
        }
        let req = self.delete_request.borrow_mut().take();
        if let (Some(u), Some(s)) = (req, &self.session) {
            let user = u.clone();
            self.deleting = Some((u, s.run(move |d| {
                let sql = d.drop_user_sql(&user)?;
                d.execute(None, &sql)?;
                Ok(sql)
            })));
        }
        if let Some((u, p)) = &mut self.deleting {
            if let Some(r) = p.poll() {
                match r {
                    Ok(sql) => {
                        st.toast(format!("User {} deleted", u.display()));
                        st.record(&self.conn, None, &sql, true, "OK", 0);
                        if self.designer.as_ref().and_then(|d| d.old.as_ref()) == Some(u) {
                            self.designer = None;
                        }
                        self.selected = None;
                    }
                    Err(e) => st.error(format!("Delete user failed: {:#}", e)),
                }
                self.deleting = None;
                self.reload();
            }
        }
        let mut saved = false;
        if let Some(d) = &mut self.designer {
            d.grants.poll();
            d.preview.poll();
            if let Some(p) = &mut d.saving {
                if let Some(r) = p.poll() {
                    d.saving = None;
                    match r {
                        Ok(stmts) => {
                            for s in &stmts {
                                st.record(&self.conn, None, &d.masked(s), true, "OK", 0);
                            }
                            st.toast("User saved");
                            let host = if self.kind.base() == DbKind::PostgreSql {
                                String::new()
                            } else if d.host.trim().is_empty() {
                                "%".into()
                            } else {
                                d.host.trim().to_string()
                            };
                            let u = UserInfo { name: d.name.trim().to_string(), host, attributes: vec![] };
                            d.old = Some(u.clone());
                            d.password.clear();
                            d.confirm.clear();
                            d.privs.clear();
                            d.error = None;
                            d.preview_key.clear();
                            if let Some(s) = &self.session {
                                d.grants = Loadable::Loading(s.run(move |dr| dr.user_grants(&u)));
                            }
                            saved = true;
                        }
                        Err(e) => {
                            let m = format!("{:#}", e);
                            d.error = Some(m.clone());
                            st.error(format!("Saving user failed: {}", m));
                        }
                    }
                }
            }
        }
        if saved {
            self.reload();
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} Users - {}", icon::USER, self.conn_name)).strong());
            ui.separator();
            let supported = !matches!(self.kind.base(), DbKind::Sqlite | DbKind::Redis);
            if ui.add_enabled(supported, egui::Button::new(format!("{} New User", icon::ADD))).clicked() {
                self.open_designer(None);
            }
            let has_sel = self.selected.is_some();
            if ui.add_enabled(has_sel, egui::Button::new(format!("{} Edit User", icon::DESIGN))).clicked() {
                let u = self.selected.clone();
                self.open_designer(u);
            }
            if ui.add_enabled(has_sel && self.deleting.is_none(), egui::Button::new(format!("{} Delete User", icon::DELETE))).clicked() {
                if let Some(u) = self.selected.clone() {
                    let req = self.delete_request.clone();
                    let name = u.display();
                    st.open_dialog(crate::dialogs::confirm::ConfirmDialog::new(
                        "Delete User",
                        format!("Are you sure you want to delete user \"{}\"?", name),
                        Box::new(move |_st: &mut AppState| {
                            *req.borrow_mut() = Some(u);
                        }),
                    ));
                }
            }
            if ui.add_enabled(!self.users.is_loading(), egui::Button::new(format!("{} Refresh", icon::REFRESH))).clicked() {
                self.reload();
            }
            if self.users.is_loading() || self.deleting.is_some() {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.filter.is_empty() && ui.small_button("✖").clicked() {
                    self.filter.clear();
                }
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search users").desired_width(180.0).min_size(egui::vec2(180.0, 0.0)));
                ui.label("🔍");
            });
        });
        ui.separator();
    }

    fn user_list(&mut self, ui: &mut egui::Ui) {
        let list = match &self.users {
            Loadable::Ready(v) => v.clone(),
            Loadable::Failed(e) => {
                crate::ui::error_text(ui, e);
                return;
            }
            Loadable::Loading(_) => {
                crate::ui::loading(ui, "Loading users...");
                return;
            }
            Loadable::NotLoaded => return,
        };
        let f = self.filter.to_lowercase();
        let rows: Vec<&UserInfo> = list.iter().filter(|u| f.is_empty() || u.display().to_lowercase().contains(&f)).collect();
        let show_host = self.kind.base() != DbKind::PostgreSql;
        let attr_names: Vec<String> = list.first().map(|u| u.attributes.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
        let mut clicked: Option<UserInfo> = None;
        let mut dbl: Option<UserInfo> = None;
        let mut tb = TableBuilder::new(ui)
            .id_salt(format!("users-{}", self.conn))
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(180.0).at_least(80.0).clip(true));
        if show_host {
            tb = tb.column(Column::initial(160.0).at_least(60.0).clip(true));
        }
        for _ in &attr_names {
            tb = tb.column(Column::initial(100.0).at_least(50.0).clip(true));
        }
        tb = tb.column(Column::remainder());
        tb.auto_shrink([false, false])
            .header(22.0, |mut h| {
                h.col(|ui| {
                    ui.strong("User");
                });
                if show_host {
                    h.col(|ui| {
                        ui.strong("Host");
                    });
                }
                for a in &attr_names {
                    h.col(|ui| {
                        ui.strong(a);
                    });
                }
                h.col(|_| {});
            })
            .body(|body| {
                body.rows(22.0, rows.len(), |mut row| {
                    let u = rows[row.index()];
                    let sel = self.selected.as_ref().map(|s| s.name == u.name && s.host == u.host).unwrap_or(false);
                    row.set_selected(sel);
                    row.col(|ui| {
                        ui.label(format!("{} {}", icon::USER, u.name));
                    });
                    if show_host {
                        row.col(|ui| {
                            ui.label(&u.host);
                        });
                    }
                    for a in &attr_names {
                        row.col(|ui| {
                            let v = u.attributes.iter().find(|(k, _)| k == a).map(|(_, v)| v.as_str()).unwrap_or("");
                            let shown = match v {
                                "t" | "true" => "✔",
                                "f" | "false" => "",
                                other => other,
                            };
                            ui.label(shown);
                        });
                    }
                    row.col(|_| {});
                    let r = row.response();
                    if r.clicked() {
                        clicked = Some(u.clone());
                    }
                    if r.double_clicked() {
                        dbl = Some(u.clone());
                    }
                });
            });
        if rows.is_empty() {
            ui.label(RichText::new("No users").weak());
        }
        if let Some(u) = clicked {
            self.selected = Some(u);
        }
        if let Some(u) = dbl {
            self.selected = Some(u.clone());
            self.open_designer(Some(u));
        }
    }

    fn ensure_tables(&mut self, db: &str) {
        if db.is_empty() || db == "*" || self.tables.contains_key(db) {
            return;
        }
        let Some(s) = &self.session else { return };
        let scope = if self.kind.base() == DbKind::PostgreSql { Scope::new(self.pg_db.clone(), Some(db.to_string())) } else { Scope::new(db, None) };
        let p = s.run(move |d| Ok(d.list_objects(&scope, ObjectKind::Table)?.into_iter().map(|o| o.name).collect()));
        self.tables.insert(db.to_string(), Loadable::Loading(p));
    }

    fn designer_ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        let kind = self.kind;
        // privilege rows need table lists: request them before borrowing the designer
        let dbs: Vec<String> = self.designer.as_ref().map(|d| d.privs.iter().map(|r| r.db.clone()).collect()).unwrap_or_default();
        for db in dbs {
            self.ensure_tables(&db);
        }
        let objects = self.objects.ready().cloned().unwrap_or_default();
        let objects_loading = self.objects.is_loading();
        let Some(d) = &mut self.designer else { return };
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", icon::USER, d.title())).strong().size(14.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✖ Close").clicked() {
                    close = true;
                }
            });
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut d.page, Page::General, "General");
            ui.selectable_value(&mut d.page, Page::Privileges, "Privileges");
            if d.old.is_some() {
                ui.selectable_value(&mut d.page, Page::Grants, "Current Grants");
            }
            ui.selectable_value(&mut d.page, Page::Sql, "SQL Preview");
        });
        ui.separator();

        egui::TopBottomPanel::bottom(format!("ud-bottom-{}", d.id)).show_separator_line(true).show_inside(ui, |ui| {
            ui.add_space(4.0);
            if let Err(e) = d.validate() {
                ui.label(RichText::new(e).color(ui.visuals().warn_fg_color).small());
            }
            if let Some(e) = &d.error {
                crate::ui::error_text(ui, e);
            }
            ui.horizontal(|ui| {
                let can_save = d.validate().is_ok() && d.saving.is_none();
                if ui.add_enabled(can_save, egui::Button::new(RichText::new(format!("{} Save", icon::SAVE)).strong())).clicked() {
                    if let Some(s) = &self.session {
                        let spec = d.spec(kind);
                        let old = d.old.clone();
                        d.error = None;
                        d.saving = Some(s.run(move |dr| {
                            let stmts = dr.user_sql(old.as_ref(), &spec)?;
                            for q in &stmts {
                                dr.execute(None, q)?;
                            }
                            Ok(stmts)
                        }));
                    }
                }
                if d.saving.is_some() {
                    ui.spinner();
                    ui.label(RichText::new("Saving…").weak());
                }
            });
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(format!("ud-scroll-{}", d.id)).auto_shrink([false, false]).show(ui, |ui| match d.page {
                Page::General => {
                    egui::Grid::new(format!("ud-gen-{}", d.id)).num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("User name:");
                        ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(240.0).min_size(egui::vec2(240.0, 0.0)));
                        ui.end_row();
                        if kind.base() != DbKind::PostgreSql {
                            ui.label("Host:");
                            ui.add(egui::TextEdit::singleline(&mut d.host).hint_text("% = any host").desired_width(240.0).min_size(egui::vec2(240.0, 0.0)));
                            ui.end_row();
                        }
                        let hint = if d.old.is_some() { "leave empty to keep" } else { "" };
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut d.password).password(true).hint_text(hint).desired_width(240.0).min_size(egui::vec2(240.0, 0.0)));
                        ui.end_row();
                        ui.label("Confirm password:");
                        let r = ui.add(egui::TextEdit::singleline(&mut d.confirm).password(true).desired_width(240.0).min_size(egui::vec2(240.0, 0.0)));
                        if d.password != d.confirm && !d.confirm.is_empty() {
                            r.on_hover_text("Passwords do not match");
                        }
                        ui.end_row();
                    });
                    ui.add_space(8.0);
                    if kind.base() == DbKind::PostgreSql {
                        ui.label(RichText::new("Role attributes").strong());
                        ui.checkbox(&mut d.superuser, "Superuser");
                        ui.checkbox(&mut d.can_login, "Can login");
                        ui.checkbox(&mut d.create_db, "Can create databases");
                        ui.checkbox(&mut d.create_role, "Can create roles");
                    } else {
                        ui.checkbox(&mut d.superuser, "Grant ALL PRIVILEGES ON *.* WITH GRANT OPTION (administrator)");
                    }
                }
                Page::Privileges => {
                    ui.horizontal(|ui| {
                        if ui.button(format!("{} Add Privilege", icon::ADD)).clicked() {
                            d.privs.push(PrivRow {
                                db: if kind.base() == DbKind::PostgreSql { "public".into() } else { objects.first().cloned().unwrap_or_else(|| "*".into()) },
                                table: "*".into(),
                                target: PgTarget::AllTables,
                                privs: vec!["SELECT".into()],
                            });
                        }
                        if objects_loading {
                            ui.spinner();
                        }
                    });
                    ui.label(
                        RichText::new(if kind.base() == DbKind::PostgreSql {
                            format!("Privileges are granted in database \"{}\". Existing grants are not revoked.", self.pg_db)
                        } else {
                            "New privileges are added with GRANT; existing grants are not revoked.".to_string()
                        })
                        .weak()
                        .small(),
                    );
                    ui.add_space(4.0);
                    let mut remove = None;
                    for (i, r) in d.privs.iter_mut().enumerate() {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let db_label = if kind.base() == DbKind::PostgreSql { "Schema" } else { "Database" };
                                ui.label(db_label);
                                let shown = if r.db == "*" { "* (all databases)".to_string() } else { r.db.clone() };
                                egui::ComboBox::from_id_salt(format!("ud-db-{}-{}", d.id, i)).selected_text(shown).width(150.0).show_ui(ui, |ui| {
                                    if kind.base() != DbKind::PostgreSql && ui.selectable_label(r.db == "*", "* (all databases)").clicked() {
                                        r.db = "*".into();
                                        r.table = "*".into();
                                    }
                                    for o in &objects {
                                        if ui.selectable_label(&r.db == o, o).clicked() {
                                            r.db = o.clone();
                                            r.table = "*".into();
                                        }
                                    }
                                });
                                if kind.base() == DbKind::PostgreSql {
                                    egui::ComboBox::from_id_salt(format!("ud-tg-{}-{}", d.id, i)).selected_text(r.target.label()).width(150.0).show_ui(ui, |ui| {
                                        for t in [PgTarget::Schema, PgTarget::AllTables, PgTarget::Table] {
                                            if ui.selectable_value(&mut r.target, t, t.label()).changed() {
                                                let avail = r.available(kind);
                                                r.privs.retain(|p| avail.contains(&p.as_str()));
                                                if t == PgTarget::Table && r.table == "*" {
                                                    r.table.clear();
                                                }
                                            }
                                        }
                                    });
                                }
                                let table_enabled = if kind.base() == DbKind::PostgreSql { r.target == PgTarget::Table } else { r.db != "*" };
                                if table_enabled {
                                    ui.label("Table");
                                    let tables = self.tables.get(&r.db);
                                    let shown = if r.table == "*" { "* (all tables)".to_string() } else if r.table.is_empty() { "Select…".into() } else { r.table.clone() };
                                    egui::ComboBox::from_id_salt(format!("ud-tb-{}-{}", d.id, i)).selected_text(shown).width(150.0).height(300.0).show_ui(ui, |ui| {
                                        if kind.base() != DbKind::PostgreSql && ui.selectable_label(r.table == "*", "* (all tables)").clicked() {
                                            r.table = "*".into();
                                        }
                                        match tables {
                                            Some(Loadable::Ready(v)) => {
                                                for t in v {
                                                    if ui.selectable_label(&r.table == t, t).clicked() {
                                                        r.table = t.clone();
                                                    }
                                                }
                                            }
                                            Some(Loadable::Loading(_)) => {
                                                ui.spinner();
                                            }
                                            Some(Loadable::Failed(e)) => {
                                                ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                                            }
                                            _ => {}
                                        }
                                    });
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("✖").on_hover_text("Remove").clicked() {
                                        remove = Some(i);
                                    }
                                });
                            });
                            ui.horizontal_wrapped(|ui| {
                                let all = r.privs.iter().any(|p| p == "ALL PRIVILEGES");
                                for p in r.available(kind) {
                                    let mut on = r.privs.iter().any(|x| x == p);
                                    let enabled = *p == "ALL PRIVILEGES" || !all;
                                    if ui.add_enabled(enabled, egui::Checkbox::new(&mut on, *p)).changed() {
                                        if on {
                                            r.privs.push(p.to_string());
                                        } else {
                                            r.privs.retain(|x| x != p);
                                        }
                                    }
                                }
                            });
                            ui.label(RichText::new(format!("GRANT {} ON {}", if r.privs.is_empty() { "…".into() } else { r.privilege_text() }, r.object(kind))).monospace().weak().small());
                        });
                    }
                    if let Some(i) = remove {
                        d.privs.remove(i);
                    }
                    if d.privs.is_empty() {
                        ui.label(RichText::new("No additional privileges. Click \"Add Privilege\" to grant some.").weak());
                    }
                }
                Page::Grants => match &d.grants {
                    Loadable::Ready(v) if v.is_empty() => {
                        ui.label(RichText::new("No grants").weak());
                    }
                    Loadable::Ready(v) => {
                        for g in v {
                            ui.add(egui::Label::new(RichText::new(g).monospace()).wrap());
                            ui.separator();
                        }
                    }
                    Loadable::Loading(_) => crate::ui::loading(ui, "Loading grants..."),
                    Loadable::Failed(e) => crate::ui::error_text(ui, e),
                    Loadable::NotLoaded => {}
                },
                Page::Sql => {
                    let spec = d.spec(kind);
                    let key = serde_json::to_string(&(&spec, &d.old)).unwrap_or_default();
                    if key != d.preview_key && !d.preview.is_loading() {
                        if let Some(s) = &self.session {
                            d.preview_key = key;
                            let old = d.old.clone();
                            d.preview = Loadable::Loading(s.run(move |dr| dr.user_sql(old.as_ref(), &spec)));
                        }
                    }
                    match &d.preview {
                        Loadable::Ready(v) if v.is_empty() => {
                            ui.label(RichText::new("-- No changes").monospace().weak());
                        }
                        Loadable::Ready(v) => {
                            let mut text = d.masked(&navdb_core::ddl::script(v));
                            ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY).desired_rows(8));
                        }
                        Loadable::Loading(_) => crate::ui::loading(ui, "Generating SQL..."),
                        Loadable::Failed(e) => crate::ui::error_text(ui, e),
                        Loadable::NotLoaded => {}
                    }
                }
            });
        });
        if close {
            self.designer = None;
        }
        let _ = st;
    }
}

impl Tab for UsersTab {
    fn title(&self) -> String {
        format!("{} Users - {}", icon::USER, self.conn_name)
    }

    fn key(&self) -> String {
        format!("users:{}", self.conn)
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut egui::Ui, st: &mut AppState) {
        self.poll(st);
        self.toolbar(ui, st);
        if matches!(self.kind.base(), DbKind::Sqlite | DbKind::Redis) {
            ui.add_space(12.0);
            ui.label(RichText::new(format!("User management is not available for {} connections.", self.kind.label())).weak());
            return;
        }
        if self.designer.is_some() {
            egui::SidePanel::right(format!("users-designer-{}", self.conn))
                .resizable(true)
                .default_width(560.0)
                .width_range(380.0..=1000.0)
                .show_inside(ui, |ui| self.designer_ui(ui, st));
        }
        egui::CentralPanel::default().show_inside(ui, |ui| self.user_list(ui));
    }
}
