//! Table Designer: fields, indexes, foreign keys, checks, triggers, options,
//! comment and a live SQL preview. New tables are created with
//! `ddl::create_table`, existing ones altered with
//! `ddl::alter_table_with_renames` (column renames are tracked per row).

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Context;
use egui::{RichText, Ui};
use navdb_core::{ddl, CheckDef, ColumnDef, Dialect, ForeignKeyDef, IndexDef, ObjPath, ObjectKind, Pending, Scope, Session, TableDef, TriggerDef};

use super::routine::{exec_statement, new_editor, status_line};
use super::Tab;
use crate::state::{AppState, Loadable};
use crate::ui::sql_editor::SqlEditor;
use crate::ui::{self, style::icon};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Fields,
    Indexes,
    ForeignKeys,
    Checks,
    Triggers,
    Options,
    Comment,
    SqlPreview,
}

impl Page {
    const ALL: [Page; 8] = [
        Page::Fields,
        Page::Indexes,
        Page::ForeignKeys,
        Page::Checks,
        Page::Triggers,
        Page::Options,
        Page::Comment,
        Page::SqlPreview,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::Fields => "Fields",
            Page::Indexes => "Indexes",
            Page::ForeignKeys => "Foreign Keys",
            Page::Checks => "Checks",
            Page::Triggers => "Triggers",
            Page::Options => "Options",
            Page::Comment => "Comment",
            Page::SqlPreview => "SQL Preview",
        }
    }
}

const FK_RULES: [&str; 5] = ["RESTRICT", "CASCADE", "SET NULL", "NO ACTION", "SET DEFAULT"];
const MYSQL_ENGINES: [&str; 5] = ["InnoDB", "MyISAM", "MEMORY", "ARCHIVE", "CSV"];

struct FieldRow {
    /// Name of the column on the server (None for new fields).
    orig: Option<String>,
    col: ColumnDef,
    default: String,
}

struct TriggerRow {
    name: String,
    timing: String,
    event: String,
    editor: SqlEditor,
}

pub struct TableDesignTab {
    id: String,
    conn: String,
    scope: Scope,
    dialect: Dialect,
    session: Option<Session>,
    is_new: bool,
    /// Name on the server (existing tables).
    table: Option<String>,
    name: String,
    orig: Option<TableDef>,
    pk_template: Option<IndexDef>,
    loading: Option<Pending<TableDef>>,
    load_error: Option<String>,
    fields: Vec<FieldRow>,
    indexes: Vec<IndexDef>,
    fks: Vec<ForeignKeyDef>,
    checks: Vec<CheckDef>,
    triggers: Vec<TriggerRow>,
    comment: String,
    engine: String,
    charset: String,
    collation: String,
    auto_inc: String,
    page: Page,
    sel_field: Option<usize>,
    sel_index: Option<usize>,
    sel_fk: Option<usize>,
    sel_check: Option<usize>,
    sel_trigger: Option<usize>,
    saving: Option<(Pending<TableDef>, String, Instant)>,
    message: Option<(String, bool)>,
    charsets: Loadable<Vec<String>>,
    collations: HashMap<String, Loadable<Vec<String>>>,
    ref_cols: HashMap<ObjPath, Loadable<Vec<String>>>,
    confirm_close: bool,
}

/// Bring a definition into the canonical shape the designer produces so
/// that comparing it with the edited copy only reports real changes.
fn normalize(mut def: TableDef) -> (TableDef, Option<IndexDef>) {
    let pk = def.primary_key();
    for c in def.columns.iter_mut() {
        c.primary_key = pk.contains(&c.name);
        if c.default.as_deref().map(|d| d.is_empty()).unwrap_or(false) {
            c.default = None;
        }
    }
    let template = def.indexes.iter().find(|i| i.primary).cloned();
    let mut idx: Vec<IndexDef> = def.indexes.iter().filter(|i| !i.primary).cloned().collect();
    if let Some(t) = &template {
        idx.insert(0, t.clone());
    }
    def.indexes = idx;
    (def, template)
}

fn join_cols(cols: &[String]) -> String {
    if cols.is_empty() {
        "(none)".into()
    } else {
        cols.join(", ")
    }
}

/// Multi-select of column names in a popup; returns true when changed.
fn columns_picker(ui: &mut Ui, id: impl std::hash::Hash, selected: &mut Vec<String>, choices: &[String], width: f32) -> bool {
    let mut changed = false;
    let label = join_cols(selected);
    ui.push_id(id, |ui| {
        ui.add_sized([width, 18.0], egui::Label::new(RichText::new(&label).monospace()).truncate()).on_hover_text(&label);
        ui.menu_button("⏷", |ui| {
            ui.set_min_width(180.0);
            if choices.is_empty() {
                ui.label(RichText::new("No fields available").weak());
            }
            for c in choices {
                let mut on = selected.contains(c);
                let pos = selected.iter().position(|x| x == c).map(|p| format!("  #{}", p + 1)).unwrap_or_default();
                if ui.checkbox(&mut on, format!("{}{}", c, pos)).changed() {
                    if on {
                        selected.push(c.clone());
                    } else {
                        selected.retain(|x| x != c);
                    }
                    changed = true;
                }
            }
            ui.separator();
            if ui.button("Clear").clicked() {
                selected.clear();
                changed = true;
                ui.close_menu();
            }
        });
    });
    changed
}

fn combo(ui: &mut Ui, id: impl std::hash::Hash, value: &mut String, choices: &[&str], width: f32) {
    egui::ComboBox::from_id_salt(id).width(width).selected_text(if value.is_empty() { "(default)" } else { value.as_str() }).show_ui(ui, |ui| {
        ui.selectable_value(value, String::new(), "(default)");
        for c in choices {
            ui.selectable_value(value, c.to_string(), *c);
        }
    });
}

impl TableDesignTab {
    pub fn new(st: &mut AppState, conn: &str, scope: Scope, table: Option<String>) -> Self {
        let dialect = st.config(conn).map(|c| c.dialect()).unwrap_or(Dialect::MySql);
        let session = st.session(conn);
        if let Some(c) = st.conn_mut(conn) {
            c.load_objects(&scope, ObjectKind::Table);
        }
        let mut t = Self {
            id: uuid::Uuid::new_v4().to_string(),
            conn: conn.to_string(),
            scope,
            dialect,
            session,
            is_new: table.is_none(),
            table: table.clone(),
            name: table.clone().unwrap_or_default(),
            orig: None,
            pk_template: None,
            loading: None,
            load_error: None,
            fields: Vec::new(),
            indexes: Vec::new(),
            fks: Vec::new(),
            checks: Vec::new(),
            triggers: Vec::new(),
            comment: String::new(),
            engine: String::new(),
            charset: String::new(),
            collation: String::new(),
            auto_inc: String::new(),
            page: Page::Fields,
            sel_field: None,
            sel_index: None,
            sel_fk: None,
            sel_check: None,
            sel_trigger: None,
            saving: None,
            message: None,
            charsets: Loadable::NotLoaded,
            collations: HashMap::new(),
            ref_cols: HashMap::new(),
            confirm_close: false,
        };
        if t.is_new {
            let mut id = ColumnDef::new("id", match dialect {
                Dialect::MySql => "int",
                Dialect::Postgres => "int4",
                Dialect::Sqlite => "INTEGER",
            });
            id.nullable = false;
            id.primary_key = true;
            id.auto_increment = true;
            t.fields.push(FieldRow { orig: None, col: id, default: String::new() });
            if dialect == Dialect::MySql {
                t.engine = "InnoDB".into();
            }
            t.sel_field = Some(0);
        } else {
            t.reload();
        }
        t
    }

    fn reload(&mut self) {
        let (Some(s), Some(name)) = (&self.session, &self.table) else { return };
        let p = self.scope.path(name.clone());
        self.loading = Some(s.run(move |d| d.table_def(&p)));
        self.load_error = None;
    }

    fn populate(&mut self, st: &AppState, def: TableDef) {
        let (def, template) = normalize(def);
        self.pk_template = template;
        self.table = Some(def.path.name.clone());
        self.name = def.path.name.clone();
        self.fields = def
            .columns
            .iter()
            .map(|c| FieldRow { orig: Some(c.name.clone()), col: c.clone(), default: c.default.clone().unwrap_or_default() })
            .collect();
        self.indexes = def.indexes.iter().filter(|i| !i.primary).cloned().collect();
        self.fks = def.foreign_keys.clone();
        self.checks = def.checks.clone();
        self.triggers = def
            .triggers
            .iter()
            .map(|t| TriggerRow { name: t.name.clone(), timing: t.timing.clone(), event: t.event.clone(), editor: new_editor(st, t.body.clone()) })
            .collect();
        self.comment = def.comment.clone();
        self.engine = def.engine.clone();
        self.charset = def.charset.clone();
        self.collation = def.collation.clone();
        self.auto_inc = def.auto_increment.map(|v| v.to_string()).unwrap_or_default();
        self.orig = Some(def);
        self.is_new = false;
        self.sel_field = self.sel_field.filter(|&i| i < self.fields.len());
        self.sel_index = None;
        self.sel_fk = None;
        self.sel_check = None;
        self.sel_trigger = self.sel_trigger.filter(|&i| i < self.triggers.len());
        self.confirm_close = false;
    }

    fn field_names(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.col.name.trim().to_string()).filter(|n| !n.is_empty()).collect()
    }

    fn path(&self) -> ObjPath {
        self.scope.path(self.name.trim().to_string())
    }

    /// Current edits as a table definition.
    fn build(&self) -> TableDef {
        let d = self.dialect;
        let path = self.path();
        let orig = self.orig.as_ref();
        let columns: Vec<ColumnDef> = self
            .fields
            .iter()
            .map(|f| {
                let mut c = f.col.clone();
                c.name = c.name.trim().to_string();
                c.data_type = c.data_type.trim().to_string();
                c.default = if f.default.trim().is_empty() { None } else { Some(f.default.trim().to_string()) };
                if !d.supports_comments() {
                    c.comment = f.orig.as_ref().and_then(|o| orig.and_then(|t| t.column(o))).map(|oc| oc.comment.clone()).unwrap_or_default();
                }
                c
            })
            .collect();
        // primary key order: keep the existing key order, then field order
        let mut pk: Vec<String> = Vec::new();
        if let Some(t) = &self.pk_template {
            for c in &t.columns {
                // follow renames
                let cur = self.fields.iter().find(|f| f.orig.as_deref() == Some(c.as_str())).map(|f| f.col.name.trim().to_string());
                if let Some(cur) = cur {
                    if columns.iter().any(|x| x.name == cur && x.primary_key) {
                        pk.push(cur);
                    }
                }
            }
        }
        for c in &columns {
            if c.primary_key && !pk.contains(&c.name) {
                pk.push(c.name.clone());
            }
        }
        let mut indexes = Vec::new();
        if !pk.is_empty() {
            if let Some(t) = &self.pk_template {
                indexes.push(IndexDef { columns: pk.clone(), ..t.clone() });
            }
        }
        let tname = path.name.clone();
        for idx in &self.indexes {
            let mut i = idx.clone();
            i.name = i.name.trim().to_string();
            if i.name.is_empty() {
                i.name = format!("idx_{}_{}", tname, i.columns.join("_"));
            }
            i.primary = false;
            indexes.push(i);
        }
        let foreign_keys = self
            .fks
            .iter()
            .map(|fk| {
                let mut f = fk.clone();
                f.name = f.name.trim().to_string();
                if f.name.is_empty() {
                    f.name = format!("fk_{}_{}", tname, f.columns.join("_"));
                }
                f
            })
            .collect();
        let triggers = self
            .triggers
            .iter()
            .map(|t| TriggerDef { name: t.name.trim().to_string(), timing: t.timing.clone(), event: t.event.clone(), body: t.editor.text.clone() })
            .collect();
        let auto_increment = match self.auto_inc.trim() {
            "" => None,
            s => s.parse::<u64>().ok().or_else(|| orig.and_then(|o| o.auto_increment)),
        };
        TableDef {
            path,
            columns,
            indexes,
            foreign_keys,
            checks: self.checks.clone(),
            triggers,
            comment: if d.supports_comments() { self.comment.clone() } else { orig.map(|o| o.comment.clone()).unwrap_or_default() },
            engine: self.engine.clone(),
            charset: self.charset.clone(),
            collation: self.collation.clone(),
            auto_increment,
        }
    }

    fn renames(&self) -> Vec<(String, String)> {
        self.fields
            .iter()
            .filter_map(|f| {
                let o = f.orig.as_ref()?;
                let n = f.col.name.trim();
                (!n.is_empty() && n != o).then(|| (o.clone(), n.to_string()))
            })
            .collect()
    }

    fn validate(&self, def: &TableDef) -> Result<(), String> {
        if def.path.name.is_empty() {
            return Err("Enter a table name.".into());
        }
        if def.columns.is_empty() {
            return Err("A table needs at least one field.".into());
        }
        let mut seen: Vec<String> = Vec::new();
        for (i, c) in def.columns.iter().enumerate() {
            if c.name.is_empty() {
                return Err(format!("Field {} has no name.", i + 1));
            }
            let l = c.name.to_lowercase();
            if seen.contains(&l) {
                return Err(format!("Duplicate field name '{}'.", c.name));
            }
            seen.push(l);
        }
        let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
        for i in def.indexes.iter().filter(|i| !i.primary) {
            if i.columns.is_empty() {
                return Err(format!("Index '{}' has no fields.", i.name));
            }
            if let Some(c) = i.columns.iter().find(|c| !names.contains(&c.as_str()) && self.dialect != Dialect::Postgres) {
                return Err(format!("Index '{}' references unknown field '{}'.", i.name, c));
            }
        }
        for f in &def.foreign_keys {
            if f.columns.is_empty() {
                return Err(format!("Foreign key '{}' has no fields.", f.name));
            }
            if f.ref_table.is_empty() {
                return Err(format!("Foreign key '{}' has no referenced table.", f.name));
            }
            if f.ref_columns.len() != f.columns.len() {
                return Err(format!("Foreign key '{}': {} field(s) but {} referenced field(s).", f.name, f.columns.len(), f.ref_columns.len()));
            }
        }
        for c in &def.checks {
            if c.expression.trim().is_empty() {
                return Err(format!("Check '{}' has an empty expression.", c.name));
            }
        }
        for t in &def.triggers {
            if t.name.is_empty() {
                return Err("A trigger has no name.".into());
            }
        }
        Ok(())
    }

    fn statements(&self) -> Result<Vec<String>, String> {
        let def = self.build();
        self.validate(&def)?;
        match (&self.orig, self.is_new) {
            (Some(orig), false) => Ok(ddl::alter_table_with_renames(self.dialect, orig, &def, &self.renames())),
            _ => Ok(ddl::create_table(self.dialect, &def)),
        }
    }

    fn save(&mut self) {
        if self.saving.is_some() || self.loading.is_some() {
            return;
        }
        if !self.is_new && self.orig.is_none() {
            return;
        }
        let stmts = match self.statements() {
            Ok(s) => s,
            Err(e) => {
                self.message = Some((e, true));
                return;
            }
        };
        if stmts.is_empty() {
            self.message = Some(("No changes to save.".into(), false));
            return;
        }
        let Some(s) = &self.session else {
            self.message = Some(("Connection is not available.".into(), true));
            return;
        };
        let script = ddl::script(&stmts);
        let dialect = self.dialect;
        let scope = self.scope.clone();
        let path = self.path();
        self.message = None;
        self.saving = Some((
            s.run(move |d| {
                let n = stmts.len();
                for (i, stmt) in stmts.iter().enumerate() {
                    if let Err(e) = exec_statement(d, dialect, &scope, stmt) {
                        if dialect == Dialect::Sqlite {
                            let _ = d.execute(Some(&scope), "ROLLBACK");
                            let _ = d.execute(Some(&scope), "PRAGMA foreign_keys = ON");
                        }
                        return Err(e).with_context(|| format!("statement {} of {} failed: {}", i + 1, n, stmt));
                    }
                }
                d.table_def(&path)
            }),
            script,
            Instant::now(),
        ));
    }

    fn poll(&mut self, st: &mut AppState) {
        if let Some(p) = &mut self.loading {
            if let Some(r) = p.poll() {
                self.loading = None;
                match r {
                    Ok(def) => self.populate(st, def),
                    Err(e) => self.load_error = Some(format!("{:#}", e)),
                }
            }
        }
        if let Some((p, _, _)) = &mut self.saving {
            if let Some(r) = p.poll() {
                let (_, script, started) = self.saving.take().unwrap();
                let ms = started.elapsed().as_millis();
                match r {
                    Ok(def) => {
                        let created = self.is_new;
                        st.record(&self.conn, Some(&self.scope), &script, true, if created { "table created" } else { "table altered" }, ms);
                        self.populate(st, def);
                        st.refresh_scope(&self.conn, &self.scope);
                        st.toast(format!("Table {} {}", self.name, if created { "created" } else { "saved" }));
                        self.message = Some((format!("Saved in {} ms", ms), false));
                    }
                    Err(e) => {
                        let m = format!("{:#}", e);
                        st.record(&self.conn, Some(&self.scope), &script, false, &m, ms);
                        self.message = Some((m, true));
                    }
                }
            }
        }
        for l in self.ref_cols.values_mut() {
            l.poll();
        }
        self.charsets.poll();
        for l in self.collations.values_mut() {
            l.poll();
        }
    }

    fn new_field(&self) -> FieldRow {
        let ty = match self.dialect {
            Dialect::Sqlite => "TEXT",
            _ => "varchar(255)",
        };
        FieldRow { orig: None, col: ColumnDef::new("", ty), default: String::new() }
    }

    // ---------------------------------------------------------------- pages

    fn fields_toolbar(&mut self, ui: &mut Ui) {
        if ui::tool_button(ui, icon::ADD, "Add Field").clicked() {
            self.fields.push(self.new_field());
            self.sel_field = Some(self.fields.len() - 1);
        }
        if ui::tool_button(ui, "⤵", "Insert Field").clicked() {
            let at = self.sel_field.unwrap_or(0).min(self.fields.len());
            let f = self.new_field();
            self.fields.insert(at, f);
            self.sel_field = Some(at);
        }
        let has = self.sel_field.is_some_and(|i| i < self.fields.len());
        if ui.add_enabled(has, egui::Button::new(format!("{} Delete Field", icon::DELETE)).small()).clicked() {
            if let Some(i) = self.sel_field {
                let removed = self.fields.remove(i);
                let n = removed.col.name.trim().to_string();
                for idx in self.indexes.iter_mut() {
                    idx.columns.retain(|c| c != &n);
                }
                for fk in self.fks.iter_mut() {
                    fk.columns.retain(|c| c != &n);
                }
                self.sel_field = if self.fields.is_empty() { None } else { Some(i.min(self.fields.len() - 1)) };
            }
        }
        if ui.add_enabled(has, egui::Button::new(format!("{} Primary Key", icon::KEY)).small()).clicked() {
            if let Some(f) = self.sel_field.and_then(|i| self.fields.get_mut(i)) {
                f.col.primary_key = !f.col.primary_key;
                if f.col.primary_key {
                    f.col.nullable = false;
                }
            }
        }
        ui.separator();
        let i = self.sel_field.unwrap_or(usize::MAX);
        if ui.add_enabled(has && i > 0, egui::Button::new("⬆ Move Up").small()).clicked() {
            self.fields.swap(i, i - 1);
            self.sel_field = Some(i - 1);
        }
        if ui.add_enabled(has && i + 1 < self.fields.len(), egui::Button::new("⬇ Move Down").small()).clicked() {
            self.fields.swap(i, i + 1);
            self.sel_field = Some(i + 1);
        }
    }

    fn fields_page(&mut self, ui: &mut Ui) {
        let d = self.dialect;
        let types = d.data_types();
        let comments = d.supports_comments();
        let mut select: Option<usize> = None;
        egui::ScrollArea::both().id_salt(("fields-scroll", &self.id)).auto_shrink([false, true]).max_height(ui.available_height() * 0.62).show(ui, |ui| {
            let cols = if comments { 8 } else { 7 };
            egui::Grid::new(("fields-grid", &self.id)).num_columns(cols).striped(true).spacing([8.0, 4.0]).min_row_height(22.0).show(ui, |ui| {
                for h in ["", "Name", "Type", "Not Null", "Key", "Default", "Auto Inc."] {
                    ui.label(RichText::new(h).strong());
                }
                if comments {
                    ui.label(RichText::new("Comment").strong());
                }
                ui.end_row();
                let sel = self.sel_field;
                for (i, f) in self.fields.iter_mut().enumerate() {
                    let is_sel = sel == Some(i);
                    if ui.selectable_label(is_sel, RichText::new(format!("{:>2}", i + 1)).monospace()).clicked() {
                        select = Some(i);
                    }
                    ui.horizontal(|ui| {
                        let r = ui.add(egui::TextEdit::singleline(&mut f.col.name).desired_width(150.0).hint_text("field name"));
                        if r.has_focus() || r.clicked() {
                            select = Some(i);
                        }
                        if let Some(o) = &f.orig {
                            if o != f.col.name.trim() {
                                ui.label(RichText::new("↻").color(ui.visuals().warn_fg_color)).on_hover_text(format!("renamed from {}", o));
                            }
                        } else {
                            ui.label(RichText::new("new").small().color(egui::Color32::from_rgb(60, 160, 80)));
                        }
                    });
                    ui.horizontal(|ui| {
                        let r = ui.add(egui::TextEdit::singleline(&mut f.col.data_type).desired_width(130.0).font(egui::TextStyle::Monospace).hint_text("type(len)"));
                        if r.has_focus() {
                            select = Some(i);
                        }
                        ui.menu_button("⏷", |ui| {
                            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                                for t in types {
                                    if ui.selectable_label(f.col.data_type.eq_ignore_ascii_case(t), *t).clicked() {
                                        // keep an existing length/precision suffix
                                        let suffix = f.col.data_type.find('(').map(|p| f.col.data_type[p..].to_string()).unwrap_or_default();
                                        let takes_len = matches!(
                                            t.to_ascii_lowercase().as_str(),
                                            "varchar" | "char" | "decimal" | "numeric" | "binary" | "varbinary" | "bit" | "float" | "double"
                                        );
                                        f.col.data_type = if takes_len && !suffix.is_empty() {
                                            format!("{}{}", t, suffix)
                                        } else if *t == "varchar" && suffix.is_empty() {
                                            "varchar(255)".into()
                                        } else {
                                            t.to_string()
                                        };
                                        ui.close_menu();
                                    }
                                }
                            });
                        });
                    });
                    let mut nn = !f.col.nullable;
                    if ui.checkbox(&mut nn, "").changed() {
                        f.col.nullable = !nn;
                        select = Some(i);
                    }
                    let key = if f.col.primary_key { RichText::new(icon::KEY) } else { RichText::new("·").weak() };
                    if ui.selectable_label(f.col.primary_key, key).on_hover_text("Toggle primary key").clicked() {
                        f.col.primary_key = !f.col.primary_key;
                        if f.col.primary_key {
                            f.col.nullable = false;
                        }
                        select = Some(i);
                    }
                    let r = ui.add(egui::TextEdit::singleline(&mut f.default).desired_width(120.0).font(egui::TextStyle::Monospace).hint_text("none"));
                    if r.has_focus() {
                        select = Some(i);
                    }
                    if ui.checkbox(&mut f.col.auto_increment, "").changed() {
                        select = Some(i);
                    }
                    if comments {
                        let r = ui.add(egui::TextEdit::singleline(&mut f.col.comment).desired_width(200.0));
                        if r.has_focus() {
                            select = Some(i);
                        }
                    }
                    ui.end_row();
                }
            });
            if self.fields.is_empty() {
                ui.label(RichText::new("No fields. Use \"Add Field\" to create one.").weak());
            }
        });
        if let Some(i) = select {
            self.sel_field = Some(i);
        }

        // properties of the selected field
        ui.separator();
        let Some(i) = self.sel_field.filter(|&i| i < self.fields.len()) else {
            ui.label(RichText::new("Select a field to see its properties.").weak());
            return;
        };
        let f = &mut self.fields[i];
        ui.strong(format!("Field properties: {}", if f.col.name.is_empty() { "(unnamed)" } else { f.col.name.as_str() }));
        egui::Grid::new(("field-props", &self.id)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            if let Some(o) = &f.orig {
                ui.label(RichText::new("Original name").weak());
                ui.label(o);
                ui.end_row();
            }
            if d == Dialect::MySql {
                ui.label(RichText::new("Collation").weak());
                ui.add(egui::TextEdit::singleline(&mut f.col.collation).desired_width(220.0).hint_text("(table default)"));
                ui.end_row();
                ui.label(RichText::new("Extra").weak());
                ui.add(egui::TextEdit::singleline(&mut f.col.extra).desired_width(320.0).hint_text("e.g. on update CURRENT_TIMESTAMP"));
                ui.end_row();
            }
            ui.label(RichText::new("Default").weak());
            ui.label(RichText::new("SQL expression, e.g. 'text', 0, CURRENT_TIMESTAMP, NULL").weak().small());
            ui.end_row();
            let mut c = f.col.clone();
            c.default = if f.default.trim().is_empty() { None } else { Some(f.default.trim().to_string()) };
            ui.label(RichText::new("Definition").weak());
            ui.label(RichText::new(ddl::column_sql(d, &c, false)).monospace());
            ui.end_row();
        });
    }

    fn indexes_page(&mut self, ui: &mut Ui) {
        let d = self.dialect;
        let names = self.field_names();
        let methods: &[&str] = match d {
            Dialect::MySql => &["BTREE", "HASH", "FULLTEXT", "SPATIAL"],
            Dialect::Postgres => &["btree", "hash", "gin", "gist", "brin"],
            Dialect::Sqlite => &[],
        };
        let mut select = None;
        egui::ScrollArea::both().id_salt(("idx-scroll", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("idx-grid", &self.id)).num_columns(6).striped(true).spacing([8.0, 4.0]).min_row_height(22.0).show(ui, |ui| {
                for h in ["", "Name", "Fields", "Unique", "Method", "Comment"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (i, idx) in self.indexes.iter_mut().enumerate() {
                    if ui.selectable_label(self.sel_index == Some(i), RichText::new(format!("{:>2}", i + 1)).monospace()).clicked() {
                        select = Some(i);
                    }
                    if ui.add(egui::TextEdit::singleline(&mut idx.name).desired_width(170.0).hint_text("auto")).has_focus() {
                        select = Some(i);
                    }
                    ui.horizontal(|ui| {
                        if columns_picker(ui, ("idx-cols", i), &mut idx.columns, &names, 200.0) {
                            select = Some(i);
                        }
                    });
                    ui.checkbox(&mut idx.unique, "");
                    if methods.is_empty() {
                        ui.label(RichText::new("—").weak());
                    } else {
                        combo(ui, ("idx-method", &self.id, i), &mut idx.method, methods, 100.0);
                    }
                    ui.add(egui::TextEdit::singleline(&mut idx.comment).desired_width(180.0));
                    ui.end_row();
                }
            });
            if self.indexes.is_empty() {
                ui.label(RichText::new("No indexes. The primary key is defined on the Fields page.").weak());
            }
        });
        if let Some(i) = select {
            self.sel_index = Some(i);
        }
    }

    fn fks_page(&mut self, ui: &mut Ui, st: &mut AppState) {
        let d = self.dialect;
        let names = self.field_names();
        let own = self.name.trim().to_string();
        // precompute referenced scopes / tables / columns (needs `st` and the session)
        let dbs: Vec<String> = st.conn(&self.conn).map(|c| c.visible_databases()).unwrap_or_default();
        let schemas: Vec<String> = if d == Dialect::Postgres {
            if let Some(c) = st.conn_mut(&self.conn) {
                c.load_schemas(&self.scope.db);
                c.schemas.get(&self.scope.db).and_then(|l| l.ready()).cloned().unwrap_or_default()
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        let mut info: Vec<(Vec<String>, Option<Vec<String>>, bool)> = Vec::new();
        for fk in &self.fks {
            let rscope = match d {
                Dialect::MySql => Scope::new(if fk.ref_db.is_empty() { self.scope.db.clone() } else { fk.ref_db.clone() }, None),
                Dialect::Postgres => Scope::new(self.scope.db.clone(), fk.ref_schema.clone().or_else(|| self.scope.schema.clone())),
                Dialect::Sqlite => self.scope.clone(),
            };
            if let Some(c) = st.conn_mut(&self.conn) {
                c.load_objects(&rscope, ObjectKind::Table);
            }
            let mut tables = st.cached_tables(&self.conn, &rscope);
            if rscope == self.scope && !own.is_empty() && !tables.contains(&own) {
                tables.push(own.clone());
            }
            let (cols, loading) = if fk.ref_table.is_empty() {
                (None, false)
            } else if rscope == self.scope && fk.ref_table == own {
                (Some(names.clone()), false)
            } else {
                let p = rscope.path(fk.ref_table.clone());
                let e = self.ref_cols.entry(p.clone()).or_default();
                if e.is_not_loaded() {
                    if let Some(s) = &self.session {
                        *e = Loadable::Loading(s.run(move |drv| Ok(drv.columns(&p)?.into_iter().map(|c| c.name).collect())));
                    }
                }
                (e.ready().cloned(), e.is_loading())
            };
            info.push((tables, cols, loading));
        }
        let mut select = None;
        egui::ScrollArea::both().id_salt(("fk-scroll", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
            let ref_label = match d {
                Dialect::MySql => "Ref. Database",
                Dialect::Postgres => "Ref. Schema",
                Dialect::Sqlite => "",
            };
            egui::Grid::new(("fk-grid", &self.id)).num_columns(8).striped(true).spacing([8.0, 4.0]).min_row_height(22.0).show(ui, |ui| {
                for h in ["", "Name", "Fields", ref_label, "Ref. Table", "Ref. Fields", "On Delete", "On Update"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (i, fk) in self.fks.iter_mut().enumerate() {
                    let (tables, rcols, loading) = &info[i];
                    if ui.selectable_label(self.sel_fk == Some(i), RichText::new(format!("{:>2}", i + 1)).monospace()).clicked() {
                        select = Some(i);
                    }
                    if ui.add(egui::TextEdit::singleline(&mut fk.name).desired_width(150.0).hint_text("auto")).has_focus() {
                        select = Some(i);
                    }
                    ui.horizontal(|ui| {
                        columns_picker(ui, ("fk-cols", i), &mut fk.columns, &names, 140.0);
                    });
                    match d {
                        Dialect::MySql => {
                            let cur = if fk.ref_db.is_empty() { self.scope.db.clone() } else { fk.ref_db.clone() };
                            egui::ComboBox::from_id_salt(("fk-db", &self.id, i)).width(120.0).selected_text(&cur).show_ui(ui, |ui| {
                                for db in &dbs {
                                    if ui.selectable_label(&cur == db, db).clicked() {
                                        fk.ref_db = db.clone();
                                    }
                                }
                            });
                        }
                        Dialect::Postgres => {
                            let cur = fk.ref_schema.clone().or_else(|| self.scope.schema.clone()).unwrap_or_else(|| "public".into());
                            egui::ComboBox::from_id_salt(("fk-schema", &self.id, i)).width(120.0).selected_text(&cur).show_ui(ui, |ui| {
                                for s in &schemas {
                                    if ui.selectable_label(&cur == s, s).clicked() {
                                        fk.ref_schema = Some(s.clone());
                                    }
                                }
                            });
                        }
                        Dialect::Sqlite => {
                            ui.label("");
                        }
                    }
                    let tlabel = if fk.ref_table.is_empty() { "(select)".to_string() } else { fk.ref_table.clone() };
                    egui::ComboBox::from_id_salt(("fk-table", &self.id, i)).width(150.0).selected_text(tlabel).show_ui(ui, |ui| {
                        if tables.is_empty() {
                            ui.label(RichText::new("loading tables...").weak());
                        }
                        for t in tables {
                            if ui.selectable_label(&fk.ref_table == t, t).clicked() && &fk.ref_table != t {
                                fk.ref_table = t.clone();
                                fk.ref_columns.clear();
                            }
                        }
                    });
                    ui.horizontal(|ui| match rcols {
                        Some(c) => {
                            columns_picker(ui, ("fk-rcols", i), &mut fk.ref_columns, c, 140.0);
                        }
                        None if *loading => {
                            ui.spinner();
                        }
                        None => {
                            let mut s = fk.ref_columns.join(", ");
                            if ui.add(egui::TextEdit::singleline(&mut s).desired_width(160.0).hint_text("col1, col2")).changed() {
                                fk.ref_columns = s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                            }
                        }
                    });
                    combo(ui, ("fk-del", &self.id, i), &mut fk.on_delete, &FK_RULES, 100.0);
                    combo(ui, ("fk-upd", &self.id, i), &mut fk.on_update, &FK_RULES, 100.0);
                    ui.end_row();
                }
            });
            if self.fks.is_empty() {
                ui.label(RichText::new("No foreign keys.").weak());
            }
        });
        if let Some(i) = select {
            self.sel_fk = Some(i);
        }
    }

    fn checks_page(&mut self, ui: &mut Ui) {
        if self.dialect == Dialect::MySql {
            ui.label(RichText::new("CHECK constraints are enforced by MySQL 8.0.16+ / MariaDB 10.2+.").weak().small());
        }
        let mut select = None;
        egui::ScrollArea::both().id_salt(("chk-scroll", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new(("chk-grid", &self.id)).num_columns(3).striped(true).spacing([8.0, 4.0]).min_row_height(22.0).show(ui, |ui| {
                for h in ["", "Name", "Expression"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (i, c) in self.checks.iter_mut().enumerate() {
                    if ui.selectable_label(self.sel_check == Some(i), RichText::new(format!("{:>2}", i + 1)).monospace()).clicked() {
                        select = Some(i);
                    }
                    if ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(180.0).hint_text("optional")).has_focus() {
                        select = Some(i);
                    }
                    if ui.add(egui::TextEdit::singleline(&mut c.expression).desired_width(420.0).font(egui::TextStyle::Monospace).hint_text("price > 0")).has_focus() {
                        select = Some(i);
                    }
                    ui.end_row();
                }
            });
            if self.checks.is_empty() {
                ui.label(RichText::new("No check constraints.").weak());
            }
        });
        if let Some(i) = select {
            self.sel_check = Some(i);
        }
    }

    fn triggers_page(&mut self, ui: &mut Ui, st: &AppState) {
        let d = self.dialect;
        egui::SidePanel::left(egui::Id::new(("trg-list", &self.id))).resizable(true).default_width(200.0).show_inside(ui, |ui| {
            egui::ScrollArea::vertical().id_salt(("trg-list-scroll", &self.id)).show(ui, |ui| {
                for (i, t) in self.triggers.iter().enumerate() {
                    let label = if t.name.is_empty() { "(unnamed)".to_string() } else { t.name.clone() };
                    let sub = format!("{} {}", t.timing, t.event);
                    if ui.selectable_label(self.sel_trigger == Some(i), format!("⚡ {}  ", label)).on_hover_text(sub).clicked() {
                        self.sel_trigger = Some(i);
                    }
                }
                if self.triggers.is_empty() {
                    ui.label(RichText::new("No triggers.").weak());
                }
            });
        });
        let Some(i) = self.sel_trigger.filter(|&i| i < self.triggers.len()) else {
            ui.label(RichText::new("Select or add a trigger.").weak());
            return;
        };
        let timings: &[&str] = match d {
            Dialect::MySql => &["BEFORE", "AFTER"],
            _ => &["BEFORE", "AFTER", "INSTEAD OF"],
        };
        let events: &[&str] = match d {
            Dialect::Postgres => &["INSERT", "UPDATE", "DELETE", "INSERT OR UPDATE", "INSERT OR UPDATE OR DELETE", "TRUNCATE"],
            _ => &["INSERT", "UPDATE", "DELETE"],
        };
        let words = st.cached_tables(&self.conn, &self.scope);
        let t = &mut self.triggers[i];
        egui::Grid::new(("trg-props", &self.id)).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(240.0));
            ui.end_row();
            ui.label("Fires");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("trg-timing", i)).width(110.0).selected_text(&t.timing).show_ui(ui, |ui| {
                    for x in timings {
                        ui.selectable_value(&mut t.timing, x.to_string(), *x);
                    }
                });
                egui::ComboBox::from_id_salt(("trg-event", i)).width(200.0).selected_text(&t.event).show_ui(ui, |ui| {
                    for x in events {
                        ui.selectable_value(&mut t.event, x.to_string(), *x);
                    }
                });
                ui.label("FOR EACH ROW");
            });
            ui.end_row();
        });
        let hint = match d {
            Dialect::Postgres => "Body: the trigger function to call, e.g. my_trigger_fn() — or a complete CREATE TRIGGER statement.",
            Dialect::MySql => "Body: a single statement or BEGIN ... END block (NEW / OLD refer to the row).",
            Dialect::Sqlite => "Body: BEGIN ... END block (NEW / OLD refer to the row).",
        };
        ui.label(RichText::new(hint).weak().small());
        let size = ui.available_size();
        ui.allocate_ui(size, |ui| {
            t.editor.ui(ui, &format!("trg-body-{}-{}", self.id, i), &words);
        });
    }

    fn options_page(&mut self, ui: &mut Ui) {
        match self.dialect {
            Dialect::MySql => {
                if self.charsets.is_not_loaded() {
                    if let Some(s) = &self.session {
                        self.charsets = Loadable::Loading(s.run(|d| d.charsets()));
                    }
                }
                let cs = self.charset.clone();
                if !cs.is_empty() {
                    let e = self.collations.entry(cs.clone()).or_default();
                    if e.is_not_loaded() {
                        if let Some(s) = &self.session {
                            *e = Loadable::Loading(s.run(move |d| d.collations(&cs)));
                        }
                    }
                }
                let charsets = self.charsets.ready().cloned().unwrap_or_default();
                let collations = self.collations.get(&self.charset).and_then(|l| l.ready()).cloned().unwrap_or_default();
                egui::Grid::new(("opts", &self.id)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Engine");
                    egui::ComboBox::from_id_salt(("engine", &self.id)).width(200.0).selected_text(if self.engine.is_empty() { "(default)" } else { self.engine.as_str() }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.engine, String::new(), "(default)");
                        for e in MYSQL_ENGINES {
                            ui.selectable_value(&mut self.engine, e.to_string(), e);
                        }
                        if !self.engine.is_empty() && !MYSQL_ENGINES.iter().any(|e| e.eq_ignore_ascii_case(&self.engine)) {
                            let e = self.engine.clone();
                            ui.selectable_value(&mut self.engine, e.clone(), e);
                        }
                    });
                    ui.end_row();
                    ui.label("Character set");
                    let before = self.charset.clone();
                    egui::ComboBox::from_id_salt(("charset", &self.id)).width(200.0).selected_text(if self.charset.is_empty() { "(default)" } else { self.charset.as_str() }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.charset, String::new(), "(default)");
                        if self.charsets.is_loading() {
                            ui.spinner();
                        }
                        for c in &charsets {
                            ui.selectable_value(&mut self.charset, c.clone(), c);
                        }
                    });
                    if before != self.charset {
                        self.collation.clear();
                    }
                    ui.end_row();
                    ui.label("Collation");
                    egui::ComboBox::from_id_salt(("collation", &self.id)).width(200.0).selected_text(if self.collation.is_empty() { "(default)" } else { self.collation.as_str() }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.collation, String::new(), "(default)");
                        if self.charset.is_empty() {
                            ui.label(RichText::new("choose a character set first").weak());
                        }
                        for c in &collations {
                            ui.selectable_value(&mut self.collation, c.clone(), c);
                        }
                    });
                    ui.end_row();
                    ui.label("Auto increment");
                    ui.add(egui::TextEdit::singleline(&mut self.auto_inc).desired_width(120.0).hint_text("next value"));
                    ui.end_row();
                });
                if let Some(e) = self.charsets.error() {
                    ui::error_text(ui, e);
                }
            }
            Dialect::Postgres => {
                ui.label(RichText::new("PostgreSQL tables have no additional options in this designer. Identity columns are set with \"Auto Inc.\" on the Fields page.").weak());
            }
            Dialect::Sqlite => {
                ui.label(RichText::new("SQLite tables have no additional options. An INTEGER primary key with \"Auto Inc.\" becomes AUTOINCREMENT.").weak());
            }
        }
    }

    fn comment_page(&mut self, ui: &mut Ui) {
        if !self.dialect.supports_comments() {
            ui.label(RichText::new("SQLite does not support table comments.").weak());
            return;
        }
        ui.label("Table comment");
        ui.add(egui::TextEdit::multiline(&mut self.comment).desired_width(f32::INFINITY).desired_rows(8));
    }

    fn preview_page(&mut self, ui: &mut Ui) {
        match self.statements() {
            Err(e) => ui::error_text(ui, &e),
            Ok(stmts) if stmts.is_empty() => {
                ui.label(RichText::new("No changes.").weak());
            }
            Ok(stmts) => {
                let script = ddl::script(&stmts);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} statement(s) will be executed on Save", stmts.len())).weak());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(script.clone());
                    }
                });
                egui::ScrollArea::both().id_salt(("preview", &self.id)).auto_shrink([false, false]).show(ui, |ui| {
                    let mut s = script.as_str();
                    ui.add(egui::TextEdit::multiline(&mut s).code_editor().desired_width(f32::INFINITY));
                });
            }
        }
    }
}

impl Tab for TableDesignTab {
    fn title(&self) -> String {
        let n = self.table.clone().unwrap_or_else(|| if self.name.trim().is_empty() { "Untitled".into() } else { self.name.trim().to_string() });
        format!("{} {} (Design)", icon::DESIGN, n)
    }

    fn key(&self) -> String {
        match &self.table {
            Some(t) => format!("design:{}:{}", self.conn, self.scope.path(t.clone()).display()),
            None => format!("design:{}:new:{}", self.conn, self.id),
        }
    }

    fn dirty(&self) -> bool {
        if self.is_new {
            return !self.name.trim().is_empty();
        }
        match &self.orig {
            Some(o) => self.build() != *o || !self.renames().is_empty(),
            None => false,
        }
    }

    fn can_close(&mut self, _st: &mut AppState) -> bool {
        if self.dirty() && !self.confirm_close {
            self.confirm_close = true;
            self.message = Some(("Unsaved changes: Save them, or close again to discard.".into(), true));
            return false;
        }
        true
    }

    fn tick(&mut self, st: &mut AppState) {
        self.poll(st);
    }

    fn ui(&mut self, ui: &mut Ui, st: &mut AppState) {
        self.poll(st);
        let save_key = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));

        // header: save / name / location
        ui.horizontal(|ui| {
            let busy = self.saving.is_some() || self.loading.is_some();
            if ui.add_enabled(!busy, egui::Button::new(format!("{} Save", icon::SAVE))).on_hover_text("Save (⌘S)").clicked() {
                self.save();
            }
            if self.saving.is_some() {
                ui.spinner();
            }
            if !self.is_new && ui.add_enabled(!busy, egui::Button::new(icon::REFRESH)).on_hover_text("Reload (discards edits)").clicked() {
                self.reload();
            }
            ui.separator();
            ui.label("Table name:");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(200.0).hint_text("required"));
            if let Some(t) = &self.table {
                if t != self.name.trim() && !self.name.trim().is_empty() {
                    ui.label(RichText::new(format!("↻ renamed from {}", t)).color(ui.visuals().warn_fg_color).small());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} · {}", st.conn_name(&self.conn), self.scope.display())).weak());
            });
        });
        if save_key {
            self.save();
        }

        // page bar
        ui.horizontal(|ui| {
            for p in Page::ALL {
                let mut label = p.label().to_string();
                let n = match p {
                    Page::Fields => self.fields.len(),
                    Page::Indexes => self.indexes.len(),
                    Page::ForeignKeys => self.fks.len(),
                    Page::Checks => self.checks.len(),
                    Page::Triggers => self.triggers.len(),
                    _ => 0,
                };
                if n > 0 {
                    label = format!("{} ({})", label, n);
                }
                ui.selectable_value(&mut self.page, p, label);
            }
        });
        ui.separator();

        // status bar
        egui::TopBottomPanel::bottom(egui::Id::new(("design-status", &self.id))).show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if self.message.is_some() {
                    status_line(ui, &self.message);
                } else {
                    let pk = self.fields.iter().filter(|f| f.col.primary_key).map(|f| f.col.name.clone()).collect::<Vec<_>>();
                    ui.label(RichText::new(format!("{} field(s) · primary key: {}", self.fields.len(), join_cols(&pk))).weak().small());
                }
            });
        });

        if self.loading.is_some() {
            ui::loading(ui, "Loading table definition...");
            return;
        }
        if let Some(e) = self.load_error.clone() {
            ui::error_text(ui, &e);
            if ui.button("Retry").clicked() {
                self.reload();
            }
            return;
        }

        // page toolbar
        ui.horizontal(|ui| match self.page {
            Page::Fields => self.fields_toolbar(ui),
            Page::Indexes => {
                if ui::tool_button(ui, icon::ADD, "Add Index").clicked() {
                    let col = self.sel_field.and_then(|i| self.fields.get(i)).map(|f| f.col.name.trim().to_string()).filter(|n| !n.is_empty());
                    self.indexes.push(IndexDef { columns: col.into_iter().collect(), ..Default::default() });
                    self.sel_index = Some(self.indexes.len() - 1);
                }
                let has = self.sel_index.is_some_and(|i| i < self.indexes.len());
                if ui.add_enabled(has, egui::Button::new(format!("{} Delete Index", icon::DELETE)).small()).clicked() {
                    if let Some(i) = self.sel_index.take() {
                        self.indexes.remove(i);
                    }
                }
            }
            Page::ForeignKeys => {
                if ui::tool_button(ui, icon::ADD, "Add Foreign Key").clicked() {
                    let rule = if self.dialect == Dialect::MySql { "RESTRICT" } else { "NO ACTION" };
                    self.fks.push(ForeignKeyDef {
                        ref_db: self.scope.db.clone(),
                        ref_schema: self.scope.schema.clone(),
                        on_delete: rule.into(),
                        on_update: rule.into(),
                        ..Default::default()
                    });
                    self.sel_fk = Some(self.fks.len() - 1);
                }
                let has = self.sel_fk.is_some_and(|i| i < self.fks.len());
                if ui.add_enabled(has, egui::Button::new(format!("{} Delete Foreign Key", icon::DELETE)).small()).clicked() {
                    if let Some(i) = self.sel_fk.take() {
                        self.fks.remove(i);
                    }
                }
            }
            Page::Checks => {
                if ui::tool_button(ui, icon::ADD, "Add Check").clicked() {
                    self.checks.push(CheckDef::default());
                    self.sel_check = Some(self.checks.len() - 1);
                }
                let has = self.sel_check.is_some_and(|i| i < self.checks.len());
                if ui.add_enabled(has, egui::Button::new(format!("{} Delete Check", icon::DELETE)).small()).clicked() {
                    if let Some(i) = self.sel_check.take() {
                        self.checks.remove(i);
                    }
                }
            }
            Page::Triggers => {
                if ui::tool_button(ui, icon::ADD, "Add Trigger").clicked() {
                    let body = match self.dialect {
                        Dialect::Postgres => "my_trigger_fn()".to_string(),
                        Dialect::MySql => "BEGIN\n  -- SET NEW.updated_at = NOW();\nEND".to_string(),
                        Dialect::Sqlite => "BEGIN\n  SELECT 1;\nEND".to_string(),
                    };
                    let n = self.triggers.len() + 1;
                    let tname = if self.name.trim().is_empty() { "table".to_string() } else { self.name.trim().to_string() };
                    self.triggers.push(TriggerRow {
                        name: format!("trg_{}_{}", tname, n),
                        timing: "BEFORE".into(),
                        event: "INSERT".into(),
                        editor: new_editor(st, body),
                    });
                    self.sel_trigger = Some(self.triggers.len() - 1);
                }
                let has = self.sel_trigger.is_some_and(|i| i < self.triggers.len());
                if ui.add_enabled(has, egui::Button::new(format!("{} Delete Trigger", icon::DELETE)).small()).clicked() {
                    if let Some(i) = self.sel_trigger.take() {
                        self.triggers.remove(i);
                    }
                }
            }
            _ => {}
        });

        match self.page {
            Page::Fields => self.fields_page(ui),
            Page::Indexes => self.indexes_page(ui),
            Page::ForeignKeys => self.fks_page(ui, st),
            Page::Checks => self.checks_page(ui),
            Page::Triggers => self.triggers_page(ui, st),
            Page::Options => self.options_page(ui),
            Page::Comment => self.comment_page(ui),
            Page::SqlPreview => self.preview_page(ui),
        }
    }
}
