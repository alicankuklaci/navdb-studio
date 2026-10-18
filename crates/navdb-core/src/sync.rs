//! Structure Synchronization and Data Synchronization (pure comparison logic).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::ddl;
use crate::dialect::Dialect;
use crate::model::*;
use crate::value::{QueryResult, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatus {
    OnlyInSource,
    OnlyInTarget,
    Different,
    Identical,
}

impl DiffStatus {
    pub fn label(self) -> &'static str {
        match self {
            DiffStatus::OnlyInSource => "Objects to be created",
            DiffStatus::OnlyInTarget => "Objects to be dropped",
            DiffStatus::Different => "Objects to be modified",
            DiffStatus::Identical => "Identical objects",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StructDiff {
    pub name: String,
    pub status: DiffStatus,
    pub source: Option<TableDef>,
    pub target: Option<TableDef>,
    /// Statements that make target match source.
    pub statements: Vec<String>,
    pub selected: bool,
}

/// Normalise a definition so two databases can be compared regardless of location.
fn normalized(t: &TableDef) -> TableDef {
    let mut t = t.clone();
    t.path = ObjPath::default();
    t.auto_increment = None;
    for fk in t.foreign_keys.iter_mut() {
        fk.ref_db.clear();
        fk.ref_schema = None;
    }
    t
}

fn retarget(t: &TableDef, scope: &Scope) -> TableDef {
    let mut t = t.clone();
    t.path = scope.path(t.path.name.clone());
    for fk in t.foreign_keys.iter_mut() {
        fk.ref_db = if scope.schema.is_some() { scope.db.clone() } else { String::new() };
        fk.ref_schema = scope.schema.clone();
    }
    t
}

pub fn compare_structures(d: Dialect, source: &[TableDef], target: &[TableDef], target_scope: &Scope, drop_extra: bool) -> Vec<StructDiff> {
    let mut out = Vec::new();
    let tmap: HashMap<&str, &TableDef> = target.iter().map(|t| (t.path.name.as_str(), t)).collect();
    let smap: HashMap<&str, &TableDef> = source.iter().map(|t| (t.path.name.as_str(), t)).collect();
    for s in source {
        let wanted = retarget(s, target_scope);
        match tmap.get(s.path.name.as_str()) {
            None => out.push(StructDiff {
                name: s.path.name.clone(),
                status: DiffStatus::OnlyInSource,
                source: Some(s.clone()),
                target: None,
                statements: ddl::create_table(d, &wanted),
                selected: true,
            }),
            Some(t) => {
                if normalized(s) == normalized(t) {
                    out.push(StructDiff {
                        name: s.path.name.clone(),
                        status: DiffStatus::Identical,
                        source: Some(s.clone()),
                        target: Some((*t).clone()),
                        statements: vec![],
                        selected: false,
                    });
                } else {
                    let mut current = (*t).clone();
                    current.auto_increment = wanted.auto_increment;
                    let stmts = ddl::alter_table(d, &current, &wanted);
                    out.push(StructDiff {
                        name: s.path.name.clone(),
                        status: if stmts.is_empty() { DiffStatus::Identical } else { DiffStatus::Different },
                        source: Some(s.clone()),
                        target: Some((*t).clone()),
                        selected: !stmts.is_empty(),
                        statements: stmts,
                    });
                }
            }
        }
    }
    for t in target {
        if !smap.contains_key(t.path.name.as_str()) {
            out.push(StructDiff {
                name: t.path.name.clone(),
                status: DiffStatus::OnlyInTarget,
                source: None,
                target: Some(t.clone()),
                statements: vec![ddl::drop_object(d, ObjectKind::Table, &t.path, None)],
                selected: drop_extra,
            });
        }
    }
    // create tables first, then alters, then drops
    out.sort_by_key(|x| match x.status {
        DiffStatus::OnlyInSource => 0,
        DiffStatus::Different => 1,
        DiffStatus::OnlyInTarget => 2,
        DiffStatus::Identical => 3,
    });
    out
}

pub fn structure_script(diffs: &[StructDiff]) -> Vec<String> {
    diffs.iter().filter(|x| x.selected).flat_map(|x| x.statements.clone()).collect()
}

#[derive(Clone, Debug, Default)]
pub struct DataDiff {
    pub table: String,
    pub inserts: Vec<Vec<Value>>,
    pub updates: Vec<Vec<Value>>,
    pub deletes: Vec<Vec<Value>>,
    pub identical: usize,
    pub columns: Vec<String>,
    pub key: Vec<String>,
}

impl DataDiff {
    pub fn total_changes(&self) -> usize {
        self.inserts.len() + self.updates.len() + self.deletes.len()
    }
}

fn key_of(row: &[Value], idx: &[usize]) -> String {
    idx.iter().map(|&i| format!("{:?}", row[i].display())).collect::<Vec<_>>().join("\u{1f}")
}

/// Compare rows of the same table in two databases using `key` columns.
pub fn compare_data(table: &str, key: &[String], source: &QueryResult, target: &QueryResult) -> DataDiff {
    let cols: Vec<String> = source.columns.iter().map(|c| c.name.clone()).collect();
    let sidx: Vec<usize> = key.iter().filter_map(|k| source.column_index(k)).collect();
    let tidx: Vec<usize> = key.iter().filter_map(|k| target.column_index(k)).collect();
    // target rows reordered to source column order
    let tcol: Vec<Option<usize>> = cols.iter().map(|c| target.column_index(c)).collect();
    let mut tmap: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &target.rows {
        let aligned: Vec<Value> = tcol.iter().map(|i| i.map(|i| r[i].clone()).unwrap_or(Value::Null)).collect();
        tmap.insert(key_of(r, &tidx), aligned);
    }
    let mut diff = DataDiff { table: table.into(), columns: cols, key: key.to_vec(), ..Default::default() };
    for r in &source.rows {
        let k = key_of(r, &sidx);
        match tmap.remove(&k) {
            None => diff.inserts.push(r.clone()),
            Some(t) => {
                if t.iter().zip(r).all(|(a, b)| a.display() == b.display()) {
                    diff.identical += 1;
                } else {
                    diff.updates.push(r.clone());
                }
            }
        }
    }
    diff.deletes = tmap.into_values().collect();
    diff
}

pub fn data_script(d: Dialect, target: &ObjPath, diff: &DataDiff, do_insert: bool, do_update: bool, do_delete: bool) -> Vec<String> {
    let mut out = Vec::new();
    let key_vals = |row: &[Value]| -> Vec<(String, Value)> {
        diff.key
            .iter()
            .filter_map(|k| diff.columns.iter().position(|c| c == k).map(|i| (k.clone(), row[i].clone())))
            .collect()
    };
    if do_delete {
        for r in &diff.deletes {
            out.push(crate::dml::delete_row(d, target, &key_vals(r)));
        }
    }
    if do_update {
        for r in &diff.updates {
            let changes: Vec<(String, Value)> = diff
                .columns
                .iter()
                .zip(r)
                .filter(|(c, _)| !diff.key.contains(c))
                .map(|(c, v)| (c.clone(), v.clone()))
                .collect();
            if !changes.is_empty() {
                out.push(crate::dml::update_row(d, target, &changes, &key_vals(r)));
            }
        }
    }
    if do_insert {
        for chunk in diff.inserts.chunks(100) {
            out.push(crate::dml::insert_rows(d, target, &diff.columns, chunk));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ColumnMeta;

    fn t(name: &str, cols: &[(&str, &str)]) -> TableDef {
        TableDef {
            path: ObjPath::new("src", None, name),
            columns: cols.iter().map(|(n, ty)| ColumnDef::new(*n, *ty)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn structure() {
        let src = vec![t("a", &[("id", "int")]), t("b", &[("id", "int"), ("x", "int")])];
        let dst = vec![t("b", &[("id", "int")]), t("c", &[("id", "int")])];
        let diffs = compare_structures(Dialect::MySql, &src, &dst, &Scope::new("dst", None), true);
        assert_eq!(diffs[0].status, DiffStatus::OnlyInSource);
        assert!(diffs[0].statements[0].contains("`dst`.`a`"));
        assert_eq!(diffs[1].status, DiffStatus::Different);
        assert!(diffs[1].statements[0].contains("ADD COLUMN `x`"));
        assert_eq!(diffs[2].status, DiffStatus::OnlyInTarget);
    }

    #[test]
    fn data() {
        let cols = vec![ColumnMeta::new("id"), ColumnMeta::new("v")];
        let s = QueryResult {
            columns: cols.clone(),
            rows: vec![vec![Value::Int(1), Value::Text("a".into())], vec![Value::Int(2), Value::Text("b".into())], vec![Value::Int(3), Value::Text("c".into())]],
            ..Default::default()
        };
        let tg = QueryResult {
            columns: cols,
            rows: vec![vec![Value::Int(1), Value::Text("a".into())], vec![Value::Int(2), Value::Text("X".into())], vec![Value::Int(9), Value::Text("z".into())]],
            ..Default::default()
        };
        let d = compare_data("t", &["id".into()], &s, &tg);
        assert_eq!((d.inserts.len(), d.updates.len(), d.deletes.len(), d.identical), (1, 1, 1, 1));
        let sql = data_script(Dialect::Sqlite, &ObjPath::new("main", None, "t"), &d, true, true, true);
        assert_eq!(sql.len(), 3);
    }
}
