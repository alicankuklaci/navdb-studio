//! Dump SQL File / Backup: write structure and data of a database as SQL.

use anyhow::Result;
use std::io::Write;

use crate::driver::Driver;
use crate::model::*;

#[derive(Clone, Debug)]
pub struct DumpOptions {
    pub structure: bool,
    pub data: bool,
    pub drop_before_create: bool,
    pub include_views: bool,
    pub include_routines: bool,
    pub include_triggers: bool,
    /// Tables to include (empty = all).
    pub tables: Vec<String>,
    pub rows_per_insert: usize,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            structure: true,
            data: true,
            drop_before_create: true,
            include_views: true,
            include_routines: true,
            include_triggers: true,
            tables: vec![],
            rows_per_insert: 200,
        }
    }
}

/// Write a dump of `scope` to `w`. `progress` receives (done, total, current object).
pub fn dump(
    drv: &mut dyn Driver,
    scope: &Scope,
    o: &DumpOptions,
    w: &mut dyn Write,
    progress: &mut dyn FnMut(usize, usize, &str),
) -> Result<()> {
    let d = drv.dialect();
    let version = drv.server_version().unwrap_or_default();
    writeln!(w, "-- NavDB Studio SQL dump")?;
    writeln!(w, "-- Server: {}", version)?;
    writeln!(w, "-- Database: {}", scope.display())?;
    writeln!(w, "-- Date: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"))?;
    match d {
        crate::Dialect::MySql => {
            writeln!(w, "SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS = 0;\n")?;
        }
        crate::Dialect::Sqlite => {
            writeln!(w, "PRAGMA foreign_keys = OFF;\nBEGIN TRANSACTION;\n")?;
        }
        crate::Dialect::Postgres => {
            if let Some(s) = &scope.schema {
                writeln!(w, "SET search_path TO {};\n", d.quote_ident(s))?;
            }
        }
    }
    let mut tables: Vec<String> = drv.list_objects(scope, ObjectKind::Table)?.into_iter().map(|o| o.name).collect();
    if !o.tables.is_empty() {
        tables.retain(|t| o.tables.contains(t));
    }
    let views: Vec<String> = if o.include_views && drv.object_kinds().contains(&ObjectKind::View) {
        drv.list_objects(scope, ObjectKind::View)?.into_iter().map(|o| o.name).collect()
    } else {
        vec![]
    };
    let total = tables.len() + views.len();
    let mut done = 0;
    for t in &tables {
        progress(done, total, t);
        let path = scope.path(t.clone());
        let def = drv.table_def(&path)?;
        if o.structure {
            writeln!(w, "-- ----------------------------\n-- Table structure for {}\n-- ----------------------------", t)?;
            if o.drop_before_create {
                writeln!(w, "DROP TABLE IF EXISTS {};", d.qualify(&path))?;
            }
            let mut def_no_triggers = def.clone();
            if !o.include_triggers {
                def_no_triggers.triggers.clear();
            }
            let ddl = if d == crate::Dialect::Postgres {
                crate::ddl::script(&crate::ddl::create_table(d, &def_no_triggers))
            } else {
                let raw = drv.object_ddl(&path, ObjectKind::Table)?;
                let raw = raw.trim().trim_end_matches(';').to_string();
                let mut s = format!("{};", raw);
                if o.include_triggers && d == crate::Dialect::Sqlite {
                    for tr in &def.triggers {
                        s.push_str(&format!("\n{};", crate::ddl::create_trigger(d, &path, tr)));
                    }
                    for ix in def.indexes.iter().filter(|i| !i.primary) {
                        if let Ok(x) = drv.object_ddl(&scope.path(ix.name.clone()), ObjectKind::Index) {
                            s.push_str(&format!("\n{}", x));
                        }
                    }
                }
                s
            };
            writeln!(w, "{}\n", ddl)?;
        }
        if o.data {
            let cols: Vec<String> = def.columns.iter().map(|c| c.name.clone()).collect();
            let mut offset = 0usize;
            let page = 5000usize;
            let mut wrote_header = false;
            loop {
                let sql = crate::dml::select_page(d, &path, "", &[], page, offset);
                let r = drv.query(Some(scope), &sql)?;
                if r.rows.is_empty() {
                    break;
                }
                if !wrote_header {
                    writeln!(w, "-- ----------------------------\n-- Records of {}\n-- ----------------------------", t)?;
                    wrote_header = true;
                }
                for chunk in r.rows.chunks(o.rows_per_insert.max(1)) {
                    writeln!(w, "{};", crate::dml::insert_rows(d, &path, &cols, chunk))?;
                }
                if r.rows.len() < page {
                    break;
                }
                offset += page;
            }
            if wrote_header {
                writeln!(w)?;
            }
        }
        if o.structure && o.include_triggers && d == crate::Dialect::MySql {
            for tr in &def.triggers {
                writeln!(w, "DELIMITER ;;\n{};;\nDELIMITER ;\n", crate::ddl::create_trigger(d, &path, tr))?;
            }
        }
        done += 1;
    }
    if o.structure {
        for v in &views {
            progress(done, total, v);
            let path = scope.path(v.clone());
            if let Ok(ddl) = drv.object_ddl(&path, ObjectKind::View) {
                writeln!(w, "-- ----------------------------\n-- View structure for {}\n-- ----------------------------", v)?;
                if o.drop_before_create {
                    writeln!(w, "DROP VIEW IF EXISTS {};", d.qualify(&path))?;
                }
                writeln!(w, "{};\n", ddl.trim().trim_end_matches(';'))?;
            }
            done += 1;
        }
        if o.include_routines {
            for kind in [ObjectKind::Function, ObjectKind::Procedure] {
                if !drv.object_kinds().contains(&kind) {
                    continue;
                }
                for r in drv.list_objects(scope, kind)? {
                    let path = scope.path(r.name.clone());
                    if let Ok(ddl) = drv.object_ddl(&path, kind) {
                        writeln!(w, "-- ----------------------------\n-- {} structure for {}\n-- ----------------------------", kind.singular(), r.name)?;
                        if d == crate::Dialect::MySql {
                            writeln!(w, "DROP {} IF EXISTS {};", kind.sql_keyword(), d.qualify(&path))?;
                            writeln!(w, "DELIMITER ;;\n{};;\nDELIMITER ;\n", ddl.trim().trim_end_matches(';'))?;
                        } else {
                            writeln!(w, "{};\n", ddl.trim().trim_end_matches(';'))?;
                        }
                    }
                }
            }
        }
    }
    match d {
        crate::Dialect::MySql => writeln!(w, "SET FOREIGN_KEY_CHECKS = 1;")?,
        crate::Dialect::Sqlite => writeln!(w, "COMMIT;\nPRAGMA foreign_keys = ON;")?,
        _ => {}
    }
    progress(total, total, "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionConfig, DbKind};

    #[test]
    fn sqlite_dump_restores() {
        let cfg = ConnectionConfig { database: ":memory:".into(), ..ConnectionConfig::new(DbKind::Sqlite) };
        let mut a = crate::driver::connect(&cfg).unwrap();
        a.execute(None, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t VALUES (1,'x'),(2,'it''s'); CREATE VIEW vv AS SELECT * FROM t;")
            .unwrap();
        let mut buf = Vec::new();
        dump(a.as_mut(), &Scope::new("main", None), &DumpOptions::default(), &mut buf, &mut |_, _, _| {}).unwrap();
        let sql = String::from_utf8(buf).unwrap();
        let mut b = crate::driver::connect(&cfg).unwrap();
        b.execute(None, &sql).unwrap();
        let r = b.query(None, "SELECT v FROM vv WHERE id = 2").unwrap();
        assert_eq!(r.text(0, 0), "it's");
    }
}
