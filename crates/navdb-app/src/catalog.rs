//! Auto-completion metadata backed by a connection's navigator caches, so
//! names loaded once are shared by the tree and every query tab.

use navdb_core::complete::{Catalog, TableEntry};
use navdb_core::{ColumnDef, ForeignKeyDef, ObjectKind, Scope};

use crate::state::{AppState, OpenConn};

pub struct AppCatalog<'a> {
    pub st: &'a mut AppState,
    pub conn: &'a str,
    pub scope: &'a Scope,
}

impl AppCatalog<'_> {
    fn oc(&mut self) -> Option<&mut OpenConn> {
        self.st.session(self.conn)?;
        self.st.conn_mut(self.conn)
    }

    /// Scope named by a `db`, `schema` or `db.schema` qualifier.
    fn resolve(&mut self, q: Option<&str>) -> Option<Scope> {
        let Some(q) = q else { return Some(self.scope.clone()) };
        if let Some((db, schema)) = q.split_once('.') {
            return Some(Scope::new(db, Some(schema.to_string())));
        }
        let cur = self.scope.clone();
        let oc = self.oc()?;
        let kind = oc.cfg.kind;
        if !kind.has_schemas() {
            return Some(Scope::new(q, None));
        }
        let is_schema = oc.schemas.get(&cur.db).and_then(|l| l.ready()).is_some_and(|v| v.iter().any(|s| s == q));
        let is_db = oc.databases.ready().is_some_and(|v| v.iter().any(|d| d == q));
        if is_db && !is_schema {
            Some(Scope::new(q, kind.default_schema().map(String::from)))
        } else {
            Some(Scope::new(cur.db, Some(q.to_string())))
        }
    }

    /// The catalog's spelling of a table name (`users` typed, `Users` stored).
    fn spelling(&mut self, qualifier: Option<&str>, table: &str) -> String {
        self.tables(qualifier)
            .and_then(|v| v.into_iter().find(|t| t.name.eq_ignore_ascii_case(table)).map(|t| t.name))
            .unwrap_or_else(|| table.to_string())
    }

    fn names(oc: &mut OpenConn, scope: &Scope, kind: ObjectKind) -> Option<Vec<String>> {
        oc.load_objects(scope, kind);
        let l = oc.objects.get_mut(&(scope.clone(), kind))?;
        l.poll();
        if l.error().is_some() {
            return Some(vec![]);
        }
        l.ready().map(|v| v.iter().map(|o| o.name.clone()).collect())
    }
}

impl Catalog for AppCatalog<'_> {
    fn databases(&mut self) -> Option<Vec<String>> {
        let oc = self.oc()?;
        oc.databases.poll();
        if oc.databases.error().is_some() {
            return Some(vec![]);
        }
        oc.databases.ready()?;
        Some(oc.visible_databases())
    }

    fn schemas(&mut self) -> Option<Vec<String>> {
        let db = self.scope.db.clone();
        let oc = self.oc()?;
        if !oc.cfg.kind.has_schemas() {
            return Some(vec![]);
        }
        oc.load_schemas(&db);
        let l = oc.schemas.get_mut(&db)?;
        l.poll();
        if l.error().is_some() {
            return Some(vec![]);
        }
        l.ready().cloned()
    }

    fn tables(&mut self, qualifier: Option<&str>) -> Option<Vec<TableEntry>> {
        let scope = self.resolve(qualifier)?;
        let oc = self.oc()?;
        let tables = Self::names(oc, &scope, ObjectKind::Table)?;
        let mut out: Vec<TableEntry> = tables.into_iter().map(|name| TableEntry { name, view: false }).collect();
        // views are a bonus: don't hold the list back while they load
        if oc.object_kinds().contains(&ObjectKind::View) {
            if let Some(v) = Self::names(oc, &scope, ObjectKind::View) {
                out.extend(v.into_iter().map(|name| TableEntry { name, view: true }));
            }
        }
        Some(out)
    }

    fn columns(&mut self, qualifier: Option<&str>, table: &str) -> Option<Vec<ColumnDef>> {
        let scope = self.resolve(qualifier)?;
        let name = self.spelling(qualifier, table);
        let oc = self.oc()?;
        oc.columns(&scope, &name).map(|c| c.to_vec())
    }

    fn foreign_keys(&mut self, qualifier: Option<&str>, table: &str) -> Option<Vec<ForeignKeyDef>> {
        let scope = self.resolve(qualifier)?;
        let name = self.spelling(qualifier, table);
        let oc = self.oc()?;
        oc.foreign_keys(&scope, &name).map(|f| f.to_vec())
    }
}
