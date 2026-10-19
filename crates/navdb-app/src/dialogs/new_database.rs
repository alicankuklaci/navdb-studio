//! New / Edit Database dialog (MySQL character set + collation, PostgreSQL
//! encoding/owner/template/tablespace, SQLite ATTACH DATABASE).

use egui::RichText;
use navdb_core::{ddl, DbKind, Dialect, Pending, Scope, Session};

use super::Dialog;
use crate::state::{AppState, Loadable};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Sql,
}

pub struct DatabaseDialog {
    id: String,
    conn: String,
    conn_name: String,
    kind: DbKind,
    dialect: Dialect,
    existing: Option<String>,
    session: Option<Session>,
    page: Page,
    // common
    name: String,
    // MySQL
    charset: String,
    collation: String,
    charsets: Loadable<Vec<String>>,
    collations: Loadable<Vec<String>>,
    collations_for: String,
    /// Current (charset, collation) or (encoding, owner, tablespace) of an existing database.
    current: Loadable<Vec<String>>,
    current_applied: bool,
    // PostgreSQL
    owner: String,
    template: String,
    tablespace: String,
    owners: Loadable<Vec<String>>,
    templates: Loadable<Vec<String>>,
    tablespaces: Loadable<Vec<String>>,
    orig_owner: String,
    orig_tablespace: String,
    // SQLite
    file: String,
}

fn column0(session: &Session, scope: Option<Scope>, sql: &'static str) -> Pending<Vec<String>> {
    session.run(move |d| {
        let r = d.query(scope.as_ref(), sql)?;
        Ok((0..r.rows.len()).map(|i| r.text(i, 0)).collect())
    })
}

impl DatabaseDialog {
    pub fn new(st: &mut AppState, conn: &str, existing: Option<String>) -> Self {
        let cfg = st.config(conn).cloned();
        let kind = cfg.as_ref().map(|c| c.kind).unwrap_or_default();
        let dialect = kind.dialect();
        let session = st.session(conn);
        let mut me = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            conn_name: st.conn_name(conn),
            kind,
            dialect,
            existing: existing.clone(),
            session: session.clone(),
            page: Page::General,
            name: existing.clone().unwrap_or_default(),
            charset: String::new(),
            collation: String::new(),
            charsets: Loadable::NotLoaded,
            collations: Loadable::NotLoaded,
            collations_for: String::new(),
            current: Loadable::NotLoaded,
            current_applied: false,
            owner: String::new(),
            template: String::new(),
            tablespace: String::new(),
            owners: Loadable::NotLoaded,
            templates: Loadable::NotLoaded,
            tablespaces: Loadable::NotLoaded,
            orig_owner: String::new(),
            orig_tablespace: String::new(),
            file: String::new(),
        };
        let Some(s) = session else { return me };
        match kind {
            DbKind::MySql | DbKind::MariaDb => {
                me.charsets = Loadable::Loading(s.run(|d| d.charsets()));
                if let Some(db) = existing {
                    let lit = dialect.string_literal(&db);
                    me.current = Loadable::Loading(s.run(move |d| {
                        let r = d.query(
                            None,
                            &format!(
                                "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}",
                                lit
                            ),
                        )?;
                        Ok(vec![r.text(0, 0), r.text(0, 1)])
                    }));
                } else {
                    me.charset = "utf8mb4".into();
                }
            }
            DbKind::PostgreSql => {
                me.charsets = Loadable::Loading(s.run(|d| d.charsets()));
                me.owners = Loadable::Loading(column0(&s, None, "SELECT rolname FROM pg_roles WHERE rolname NOT LIKE 'pg\\_%' ORDER BY rolname"));
                me.templates = Loadable::Loading(column0(&s, None, "SELECT datname FROM pg_database WHERE datistemplate ORDER BY datname"));
                me.tablespaces = Loadable::Loading(column0(&s, None, "SELECT spcname FROM pg_tablespace ORDER BY spcname"));
                if let Some(db) = existing {
                    let lit = dialect.string_literal(&db);
                    me.current = Loadable::Loading(s.run(move |d| {
                        let r = d.query(
                            None,
                            &format!(
                                "SELECT pg_encoding_to_char(d.encoding), pg_get_userbyid(d.datdba), t.spcname FROM pg_database d \
                                 LEFT JOIN pg_tablespace t ON t.oid = d.dattablespace WHERE d.datname = {}",
                                lit
                            ),
                        )?;
                        Ok(vec![r.text(0, 0), r.text(0, 1), r.text(0, 2)])
                    }));
                } else {
                    me.charset = "UTF8".into();
                }
            }
            DbKind::Sqlite => {
                if let Some(db) = &existing {
                    let alias = db.clone();
                    me.current = Loadable::Loading(s.run(move |d| {
                        let r = d.query(None, "PRAGMA database_list")?;
                        let file = (0..r.rows.len()).find(|&i| r.text(i, 1) == alias).map(|i| r.text(i, 2)).unwrap_or_default();
                        Ok(vec![file])
                    }));
                }
            }
            DbKind::Redis => {}
        }
        me
    }

    fn poll(&mut self) {
        self.charsets.poll();
        self.collations.poll();
        self.owners.poll();
        self.templates.poll();
        self.tablespaces.poll();
        if self.current.poll() {
            if let Some(v) = self.current.ready().cloned() {
                if !self.current_applied {
                    self.current_applied = true;
                    match self.kind {
                        DbKind::MySql | DbKind::MariaDb => {
                            self.charset = v.first().cloned().unwrap_or_default();
                            self.collation = v.get(1).cloned().unwrap_or_default();
                        }
                        DbKind::PostgreSql => {
                            self.charset = v.first().cloned().unwrap_or_default();
                            self.owner = v.get(1).cloned().unwrap_or_default();
                            self.tablespace = v.get(2).cloned().unwrap_or_default();
                            self.orig_owner = self.owner.clone();
                            self.orig_tablespace = self.tablespace.clone();
                        }
                        DbKind::Sqlite => {
                            self.file = v.first().cloned().unwrap_or_default();
                        }
                        DbKind::Redis => {}
                    }
                }
            }
        }
        // (re)load collations for the selected MySQL charset
        if matches!(self.kind, DbKind::MySql | DbKind::MariaDb) && self.collations_for != self.charset && !self.collations.is_loading() {
            self.collations_for = self.charset.clone();
            if self.charset.is_empty() {
                self.collations = Loadable::Ready(vec![]);
            } else if let Some(s) = &self.session {
                let cs = self.charset.clone();
                self.collations = Loadable::Loading(s.run(move |d| d.collations(&cs)));
            }
        }
    }

    fn statements(&self) -> Result<Vec<String>, String> {
        let d = self.dialect;
        let name = self.name.trim();
        if name.is_empty() {
            return Err(if self.kind == DbKind::Sqlite { "Enter an alias name." } else { "Enter a database name." }.into());
        }
        match self.kind {
            DbKind::MySql | DbKind::MariaDb => match &self.existing {
                None => Ok(vec![ddl::create_database(d, name, &self.charset, &self.collation)]),
                Some(old) => {
                    if old != name {
                        return Err("MySQL cannot rename a database; keep the original name.".into());
                    }
                    let mut s = format!("ALTER DATABASE {}", d.quote_ident(old));
                    if !self.charset.is_empty() {
                        s.push_str(&format!(" CHARACTER SET {}", self.charset));
                    }
                    if !self.collation.is_empty() {
                        s.push_str(&format!(" COLLATE {}", self.collation));
                    }
                    if self.charset.is_empty() && self.collation.is_empty() {
                        return Ok(vec![]);
                    }
                    Ok(vec![s])
                }
            },
            DbKind::PostgreSql => match &self.existing {
                None => {
                    let mut s = format!("CREATE DATABASE {}", d.quote_ident(name));
                    let mut opts = Vec::new();
                    if !self.owner.is_empty() {
                        opts.push(format!("OWNER = {}", d.quote_ident(&self.owner)));
                    }
                    if !self.charset.is_empty() {
                        opts.push(format!("ENCODING = {}", d.string_literal(&self.charset)));
                    }
                    if !self.template.is_empty() {
                        opts.push(format!("TEMPLATE = {}", d.quote_ident(&self.template)));
                    }
                    if !self.tablespace.is_empty() {
                        opts.push(format!("TABLESPACE = {}", d.quote_ident(&self.tablespace)));
                    }
                    if !opts.is_empty() {
                        s.push_str("\n  WITH ");
                        s.push_str(&opts.join("\n  "));
                    }
                    Ok(vec![s])
                }
                Some(old) => {
                    let mut out = Vec::new();
                    let mut cur = old.clone();
                    if old != name {
                        out.push(format!("ALTER DATABASE {} RENAME TO {}", d.quote_ident(old), d.quote_ident(name)));
                        cur = name.to_string();
                    }
                    if !self.owner.is_empty() && self.owner != self.orig_owner {
                        out.push(format!("ALTER DATABASE {} OWNER TO {}", d.quote_ident(&cur), d.quote_ident(&self.owner)));
                    }
                    if !self.tablespace.is_empty() && self.tablespace != self.orig_tablespace {
                        out.push(format!("ALTER DATABASE {} SET TABLESPACE {}", d.quote_ident(&cur), d.quote_ident(&self.tablespace)));
                    }
                    Ok(out)
                }
            },
            DbKind::Sqlite => {
                if self.existing.is_some() {
                    return Ok(vec![]);
                }
                if self.file.trim().is_empty() {
                    return Err("Choose a database file.".into());
                }
                Ok(vec![ddl::create_database(d, name, self.file.trim(), "")])
            }
            DbKind::Redis => Err("Redis databases are fixed (db0..db15) and cannot be created.".into()),
        }
    }

    fn submit(&mut self, st: &mut AppState) -> bool {
        let stmts = match self.statements() {
            Ok(s) => s,
            Err(e) => {
                st.error(e);
                return false;
            }
        };
        if stmts.is_empty() {
            return true;
        }
        if self.kind == DbKind::Sqlite {
            let alias = self.name.trim().to_string();
            let file = self.file.trim().to_string();
            if let Some(c) = st.ws.connections.iter_mut().find(|c| c.id == self.conn) {
                c.attached.retain(|(a, _)| a != &alias);
                c.attached.push((alias, file));
            }
            st.save_workspace();
        }
        let msg = if self.existing.is_some() {
            "Database altered"
        } else if self.kind == DbKind::Sqlite {
            "Database attached"
        } else {
            "Database created"
        };
        st.run_task_ex(&self.conn, None, ddl::script(&stmts), msg, true, true);
        true
    }

    fn combo(ui: &mut egui::Ui, id: String, value: &mut String, items: &Loadable<Vec<String>>, default_label: &str, width: f32) -> bool {
        let mut changed = false;
        let label = if value.is_empty() { default_label.to_string() } else { value.clone() };
        egui::ComboBox::from_id_salt(id).selected_text(label).width(width).height(320.0).show_ui(ui, |ui| {
            if ui.selectable_label(value.is_empty(), default_label).clicked() {
                value.clear();
                changed = true;
            }
            match items {
                Loadable::Loading(_) => {
                    ui.spinner();
                }
                Loadable::Failed(e) => {
                    ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                }
                Loadable::Ready(v) => {
                    for it in v {
                        if ui.selectable_label(value == it, it).clicked() {
                            *value = it.clone();
                            changed = true;
                        }
                    }
                }
                Loadable::NotLoaded => {}
            }
        });
        changed
    }

    fn general(&mut self, ui: &mut egui::Ui) {
        let editing = self.existing.is_some();
        egui::Grid::new(format!("{}-g", self.id)).num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| match self.kind {
            DbKind::MySql | DbKind::MariaDb => {
                ui.label("Database name:");
                ui.add_enabled(!editing, egui::TextEdit::singleline(&mut self.name).desired_width(280.0));
                ui.end_row();
                ui.label("Character set:");
                if Self::combo(ui, format!("{}-cs", self.id), &mut self.charset, &self.charsets, "(server default)", 260.0) {
                    self.collation.clear();
                }
                ui.end_row();
                ui.label("Collation:");
                Self::combo(ui, format!("{}-coll", self.id), &mut self.collation, &self.collations, "(charset default)", 260.0);
                ui.end_row();
            }
            DbKind::PostgreSql => {
                ui.label("Database name:");
                ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(280.0));
                ui.end_row();
                ui.label("Owner:");
                Self::combo(ui, format!("{}-own", self.id), &mut self.owner, &self.owners, "(current user)", 260.0);
                ui.end_row();
                ui.label("Encoding:");
                ui.add_enabled_ui(!editing, |ui| {
                    Self::combo(ui, format!("{}-enc", self.id), &mut self.charset, &self.charsets, "(default)", 260.0);
                });
                ui.end_row();
                ui.label("Template:");
                ui.add_enabled_ui(!editing, |ui| {
                    Self::combo(ui, format!("{}-tpl", self.id), &mut self.template, &self.templates, "(template1)", 260.0);
                });
                ui.end_row();
                ui.label("Tablespace:");
                Self::combo(ui, format!("{}-ts", self.id), &mut self.tablespace, &self.tablespaces, "(pg_default)", 260.0);
                ui.end_row();
            }
            DbKind::Sqlite => {
                ui.label("Alias:");
                ui.add_enabled(!editing, egui::TextEdit::singleline(&mut self.name).hint_text("e.g. archive").desired_width(280.0));
                ui.end_row();
                ui.label("Database file:");
                ui.horizontal(|ui| {
                    ui.add_enabled(!editing, egui::TextEdit::singleline(&mut self.file).desired_width(240.0));
                    if !editing && ui.button("…").on_hover_text("Choose file").clicked() {
                        let picked = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["db", "sqlite", "sqlite3", "db3"])
                            .add_filter("All files", &["*"])
                            .pick_file()
                            .or_else(|| crate::ui::save_file_dialog("database.db", "db"));
                        if let Some(p) = picked {
                            self.file = p.display().to_string();
                            if self.name.is_empty() {
                                if let Some(stem) = p.file_stem() {
                                    self.name = stem.to_string_lossy().replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
                                }
                            }
                        }
                    }
                });
                ui.end_row();
            }
            DbKind::Redis => {
                ui.label(RichText::new("Redis provides a fixed set of numbered databases (db0, db1, ...).\nCreating or editing databases is not applicable.").weak());
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        if self.current.is_loading() {
            crate::ui::loading(ui, "Loading current properties...");
        }
        if let Some(e) = self.current.error() {
            crate::ui::error_text(ui, e);
        }
        let note = match (self.kind, editing) {
            (DbKind::Sqlite, false) => "The file is attached to this connection and remembered in its settings. A new file is created if it does not exist.",
            (DbKind::Sqlite, true) => "Attached databases cannot be edited. Use Delete Database to detach it.",
            (DbKind::PostgreSql, true) => "Encoding and template cannot be changed after creation. A database cannot be renamed while connections to it are open.",
            (DbKind::MySql | DbKind::MariaDb, true) => "Changing the default character set affects new tables only.",
            _ => "",
        };
        if !note.is_empty() {
            ui.label(RichText::new(note).weak().small());
        }
    }
}

impl Dialog for DatabaseDialog {
    fn show(&mut self, ctx: &egui::Context, st: &mut AppState) -> bool {
        self.poll();
        let mut open = true;
        let mut close = false;
        let title = match (&self.existing, self.kind) {
            (None, DbKind::Sqlite) => "Attach Database".to_string(),
            (None, _) => "New Database".to_string(),
            (Some(n), _) => format!("Edit Database - {}", n),
        };
        super::window(title, &self.id).open(&mut open).default_width(520.0).show(ctx, |ui| {
            ui.label(RichText::new(format!("Connection: {}", self.conn_name)).weak());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.page, Page::General, "General");
                ui.selectable_value(&mut self.page, Page::Sql, "SQL Preview");
            });
            ui.separator();
            egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 6)).show(ui, |ui| {
                ui.set_min_height(170.0);
                match self.page {
                    Page::General => self.general(ui),
                    Page::Sql => match self.statements() {
                        Ok(v) if v.is_empty() => {
                            ui.label(RichText::new("-- No changes").monospace().weak());
                        }
                        Ok(v) => {
                            let mut text = ddl::script(&v);
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY).interactive(true));
                            });
                        }
                        Err(e) => {
                            crate::ui::error_text(ui, &e);
                        }
                    },
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok_enabled = self.kind != DbKind::Redis && self.session.is_some();
                    if ui.add_enabled(ok_enabled, egui::Button::new(RichText::new("   OK   ").strong())).clicked() && self.submit(st) {
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        });
        open && !close
    }
}
