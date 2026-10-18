//! DML generation used by the table viewer, data transfer and import.

use crate::dialect::Dialect;
use crate::model::ObjPath;
use crate::value::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SortSpec {
    pub column: String,
    pub descending: bool,
}

/// Filter row of the Filter builder: `column op value`.
#[derive(Clone, Debug, PartialEq)]
pub struct FilterRule {
    pub enabled: bool,
    pub column: String,
    pub op: FilterOp,
    pub value: String,
    /// true = AND with previous, false = OR
    pub and: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    NotContains,
    BeginsWith,
    EndsWith,
    IsNull,
    IsNotNull,
    IsEmpty,
    IsNotEmpty,
    In,
    Between,
}

impl FilterOp {
    pub const ALL: [FilterOp; 16] = [
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Le,
        FilterOp::Gt,
        FilterOp::Ge,
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::BeginsWith,
        FilterOp::EndsWith,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
        FilterOp::IsEmpty,
        FilterOp::IsNotEmpty,
        FilterOp::In,
        FilterOp::Between,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::Ne => "<>",
            FilterOp::Lt => "<",
            FilterOp::Le => "<=",
            FilterOp::Gt => ">",
            FilterOp::Ge => ">=",
            FilterOp::Contains => "contains",
            FilterOp::NotContains => "does not contain",
            FilterOp::BeginsWith => "begins with",
            FilterOp::EndsWith => "ends with",
            FilterOp::IsNull => "is null",
            FilterOp::IsNotNull => "is not null",
            FilterOp::IsEmpty => "is empty",
            FilterOp::IsNotEmpty => "is not empty",
            FilterOp::In => "is in list",
            FilterOp::Between => "is between",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, FilterOp::IsNull | FilterOp::IsNotNull | FilterOp::IsEmpty | FilterOp::IsNotEmpty)
    }
}

impl FilterRule {
    pub fn new(column: impl Into<String>) -> Self {
        Self { enabled: true, column: column.into(), op: FilterOp::Eq, value: String::new(), and: true }
    }

    pub fn to_sql(&self, d: Dialect) -> String {
        let c = d.quote_ident(&self.column);
        let lit = |s: &str| literal_guess(d, s);
        let like = |pat: String| {
            if d == Dialect::Postgres {
                format!("{}::text LIKE {}", c, d.string_literal(&pat))
            } else {
                format!("{} LIKE {}", c, d.string_literal(&pat))
            }
        };
        match self.op {
            FilterOp::Eq => format!("{} = {}", c, lit(&self.value)),
            FilterOp::Ne => format!("{} <> {}", c, lit(&self.value)),
            FilterOp::Lt => format!("{} < {}", c, lit(&self.value)),
            FilterOp::Le => format!("{} <= {}", c, lit(&self.value)),
            FilterOp::Gt => format!("{} > {}", c, lit(&self.value)),
            FilterOp::Ge => format!("{} >= {}", c, lit(&self.value)),
            FilterOp::Contains => like(format!("%{}%", self.value)),
            FilterOp::NotContains => format!("NOT ({})", like(format!("%{}%", self.value))),
            FilterOp::BeginsWith => like(format!("{}%", self.value)),
            FilterOp::EndsWith => like(format!("%{}", self.value)),
            FilterOp::IsNull => format!("{} IS NULL", c),
            FilterOp::IsNotNull => format!("{} IS NOT NULL", c),
            FilterOp::IsEmpty => format!("{} = ''", c),
            FilterOp::IsNotEmpty => format!("{} <> ''", c),
            FilterOp::In => format!(
                "{} IN ({})",
                c,
                self.value.split(',').map(|v| lit(v.trim())).collect::<Vec<_>>().join(", ")
            ),
            FilterOp::Between => {
                let mut it = self.value.splitn(2, ',');
                let a = it.next().unwrap_or("").trim();
                let b = it.next().unwrap_or("").trim();
                format!("{} BETWEEN {} AND {}", c, lit(a), lit(b))
            }
        }
    }
}

/// Numbers stay bare, everything else becomes a string literal.
pub fn literal_guess(d: Dialect, s: &str) -> String {
    let t = s.trim();
    if !t.is_empty() && t.parse::<f64>().is_ok() && !t.starts_with('0') || t == "0" {
        t.to_string()
    } else if t.eq_ignore_ascii_case("null") {
        "NULL".into()
    } else {
        d.string_literal(s)
    }
}

pub fn filters_to_where(d: Dialect, rules: &[FilterRule]) -> String {
    let mut out = String::new();
    for r in rules.iter().filter(|r| r.enabled && !r.column.is_empty()) {
        if !out.is_empty() {
            out.push_str(if r.and { " AND " } else { " OR " });
        }
        out.push_str(&r.to_sql(d));
    }
    out
}

pub fn select_page(
    d: Dialect,
    table: &ObjPath,
    where_clause: &str,
    sort: &[SortSpec],
    limit: usize,
    offset: usize,
) -> String {
    let mut s = format!("SELECT * FROM {}", d.qualify(table));
    if !where_clause.trim().is_empty() {
        s.push_str(&format!(" WHERE {}", where_clause.trim()));
    }
    if !sort.is_empty() {
        s.push_str(" ORDER BY ");
        s.push_str(
            &sort
                .iter()
                .map(|o| format!("{}{}", d.quote_ident(&o.column), if o.descending { " DESC" } else { "" }))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if limit > 0 {
        s.push_str(&d.limit_clause(limit, offset));
    }
    s
}

pub fn count_rows(d: Dialect, table: &ObjPath, where_clause: &str) -> String {
    let mut s = format!("SELECT COUNT(*) FROM {}", d.qualify(table));
    if !where_clause.trim().is_empty() {
        s.push_str(&format!(" WHERE {}", where_clause.trim()));
    }
    s
}

fn where_for_row(d: Dialect, key_cols: &[(String, Value)]) -> String {
    key_cols
        .iter()
        .map(|(c, v)| {
            if v.is_null() {
                format!("{} IS NULL", d.quote_ident(c))
            } else {
                format!("{} = {}", d.quote_ident(c), v.to_sql_literal(d))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// UPDATE for an edited grid row. `key_cols` identify the row (primary key or,
/// lacking one, all original values).
pub fn update_row(d: Dialect, table: &ObjPath, changes: &[(String, Value)], key_cols: &[(String, Value)]) -> String {
    let set = changes
        .iter()
        .map(|(c, v)| format!("{} = {}", d.quote_ident(c), v.to_sql_literal(d)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!("UPDATE {} SET {} WHERE {}", d.qualify(table), set, where_for_row(d, key_cols));
    if d == Dialect::MySql {
        s.push_str(" LIMIT 1");
    }
    s
}

pub fn delete_row(d: Dialect, table: &ObjPath, key_cols: &[(String, Value)]) -> String {
    let mut s = format!("DELETE FROM {} WHERE {}", d.qualify(table), where_for_row(d, key_cols));
    if d == Dialect::MySql {
        s.push_str(" LIMIT 1");
    }
    s
}

pub fn insert_row(d: Dialect, table: &ObjPath, cols: &[String], values: &[Value]) -> String {
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        d.qualify(table),
        cols.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", "),
        values.iter().map(|v| v.to_sql_literal(d)).collect::<Vec<_>>().join(", ")
    )
}

/// Multi-row INSERT (used by dump, data transfer and import).
pub fn insert_rows(d: Dialect, table: &ObjPath, cols: &[String], rows: &[Vec<Value>]) -> String {
    let values = rows
        .iter()
        .map(|r| format!("({})", r.iter().map(|v| v.to_sql_literal(d)).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "INSERT INTO {} ({}) VALUES\n{}",
        d.qualify(table),
        cols.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", "),
        values
    )
}

/// UPSERT for data synchronization / "Update or insert" import mode.
pub fn upsert_row(d: Dialect, table: &ObjPath, cols: &[String], values: &[Value], key: &[String]) -> String {
    let base = insert_row(d, table, cols, values);
    let non_key: Vec<&String> = cols.iter().filter(|c| !key.contains(c)).collect();
    match d {
        Dialect::MySql => {
            if non_key.is_empty() {
                return base.replacen("INSERT", "INSERT IGNORE", 1);
            }
            format!(
                "{} ON DUPLICATE KEY UPDATE {}",
                base,
                non_key.iter().map(|c| format!("{0} = VALUES({0})", d.quote_ident(c))).collect::<Vec<_>>().join(", ")
            )
        }
        _ => {
            let conflict = key.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", ");
            if non_key.is_empty() {
                format!("{} ON CONFLICT ({}) DO NOTHING", base, conflict)
            } else {
                format!(
                    "{} ON CONFLICT ({}) DO UPDATE SET {}",
                    base,
                    conflict,
                    non_key.iter().map(|c| format!("{0} = excluded.{0}", d.quote_ident(c))).collect::<Vec<_>>().join(", ")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        let mut r = FilterRule::new("name");
        r.op = FilterOp::Contains;
        r.value = "ab".into();
        let mut r2 = FilterRule::new("age");
        r2.op = FilterOp::Gt;
        r2.value = "30".into();
        r2.and = false;
        assert_eq!(filters_to_where(Dialect::MySql, &[r, r2]), "`name` LIKE '%ab%' OR `age` > 30");
    }

    #[test]
    fn page() {
        let p = ObjPath::new("d", None, "t");
        let s = select_page(Dialect::MySql, &p, "a=1", &[SortSpec { column: "b".into(), descending: true }], 100, 200);
        assert_eq!(s, "SELECT * FROM `d`.`t` WHERE a=1 ORDER BY `b` DESC LIMIT 100 OFFSET 200");
    }

    #[test]
    fn update_and_null_keys() {
        let p = ObjPath::new("main", None, "t");
        let s = update_row(
            Dialect::Sqlite,
            &p,
            &[("a".into(), Value::Text("x".into()))],
            &[("id".into(), Value::Int(3)), ("z".into(), Value::Null)],
        );
        assert_eq!(s, "UPDATE \"t\" SET \"a\" = 'x' WHERE \"id\" = 3 AND \"z\" IS NULL");
    }

    #[test]
    fn upsert_pg() {
        let p = ObjPath::new("db", Some("public".into()), "t");
        let s = upsert_row(Dialect::Postgres, &p, &["id".into(), "v".into()], &[Value::Int(1), Value::Text("a".into())], &["id".into()]);
        assert!(s.ends_with("ON CONFLICT (\"id\") DO UPDATE SET \"v\" = excluded.\"v\""));
    }

    #[test]
    fn literal_guessing() {
        assert_eq!(literal_guess(Dialect::MySql, "42"), "42");
        assert_eq!(literal_guess(Dialect::MySql, "007"), "'007'");
        assert_eq!(literal_guess(Dialect::MySql, "abc"), "'abc'");
    }
}
