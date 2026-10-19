//! The SQL subset the data grid generates ([`crate::sqlmini`]) -> query DSL.

use anyhow::{bail, Result};
use serde_json::{json, Map, Value as Json};

use super::tabulate::Fields;
use crate::sqlmini::{self, CmpOp, Expr, Statement};
use crate::value::Value;

/// Largest `from + size` a plain search accepts (`index.max_result_window`).
pub const MAX_WINDOW: u64 = 10_000;
/// Rows returned by a SELECT without LIMIT.
pub const ROW_CAP: u64 = 100_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// `body` holds query/sort/_source; paging is applied by the driver.
    Search { index: String, body: Json, columns: Vec<String>, offset: u64, limit: u64 },
    Count { index: String, body: Json },
    Bulk { index: String, body: String, rows: usize },
    UpdateDoc { index: String, id: String, body: Json },
    UpdateByQuery { index: String, body: Json },
    DeleteDoc { index: String, id: String },
    DeleteByQuery { index: String, body: Json },
}

impl Plan {
    pub fn index(&self) -> &str {
        match self {
            Plan::Search { index, .. }
            | Plan::Count { index, .. }
            | Plan::Bulk { index, .. }
            | Plan::UpdateDoc { index, .. }
            | Plan::UpdateByQuery { index, .. }
            | Plan::DeleteDoc { index, .. }
            | Plan::DeleteByQuery { index, .. } => index,
        }
    }

    /// `(method, path, body)` of the request (for a search: the first page).
    pub fn request(&self) -> (&'static str, String, Option<String>) {
        let ix = enc(self.index());
        match self {
            Plan::Search { body, offset, limit, .. } => {
                let mut b = body.clone();
                if offset + limit <= MAX_WINDOW {
                    b["from"] = json!(offset);
                    b["size"] = json!(limit);
                    ("POST", format!("/{}/_search", ix), Some(b.to_string()))
                } else {
                    b["size"] = json!(MAX_WINDOW.min(offset + limit));
                    ("POST", format!("/{}/_search?scroll=1m", ix), Some(b.to_string()))
                }
            }
            Plan::Count { body, .. } => ("POST", format!("/{}/_count", ix), Some(body.to_string())),
            Plan::Bulk { body, .. } => ("POST", "/_bulk?refresh=true".to_string(), Some(body.clone())),
            Plan::UpdateDoc { id, body, .. } => {
                ("POST", format!("/{}/_update/{}?refresh=true", ix, enc(id)), Some(body.to_string()))
            }
            Plan::UpdateByQuery { body, .. } => {
                ("POST", format!("/{}/_update_by_query?refresh=true&conflicts=proceed", ix), Some(body.to_string()))
            }
            Plan::DeleteDoc { id, .. } => ("DELETE", format!("/{}/_doc/{}?refresh=true", ix, enc(id)), None),
            Plan::DeleteByQuery { body, .. } => {
                ("POST", format!("/{}/_delete_by_query?refresh=true&conflicts=proceed", ix), Some(body.to_string()))
            }
        }
    }

    /// Console-style text of the request (EXPLAIN output).
    pub fn describe(&self) -> String {
        let (m, p, b) = self.request();
        let body = match b {
            Some(b) if matches!(self, Plan::Bulk { .. }) => b.trim_end().to_string(),
            Some(b) => serde_json::from_str::<Json>(&b).map(|j| pretty(&j)).unwrap_or(b),
            None => String::new(),
        };
        if body.is_empty() { format!("{} {}", m, p) } else { format!("{} {}\n{}", m, p, body) }
    }
}

pub fn pretty(j: &Json) -> String {
    serde_json::to_string_pretty(j).unwrap_or_else(|_| j.to_string())
}

/// Percent-encode a path segment (index names, document ids).
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' | b',' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Index (or alias) a statement targets.
pub fn target(stmt: &Statement) -> &str {
    match stmt {
        Statement::Select(s) => s.table_name(),
        Statement::Insert { table, .. } | Statement::Update { table, .. } | Statement::Delete { table, .. } => {
            sqlmini::last(table)
        }
    }
}

pub fn plan(stmt: &Statement, f: &Fields) -> Result<Plan> {
    let index = target(stmt).to_string();
    if index.is_empty() {
        bail!("missing index name");
    }
    Ok(match stmt {
        Statement::Select(s) => {
            let q = filter_query(s.filter.as_ref(), f)?;
            if s.count {
                return Ok(Plan::Count { index, body: json!({ "query": q }) });
            }
            let mut body = json!({ "query": q });
            if !s.order.is_empty() {
                body["sort"] = Json::Array(
                    s.order
                        .iter()
                        .map(|(c, desc)| json!({ f.exact(c): { "order": if *desc { "desc" } else { "asc" } } }))
                        .collect(),
                );
            }
            if !s.columns.is_empty() {
                let src: Vec<&String> = s.columns.iter().filter(|c| !c.starts_with('_')).collect();
                body["_source"] = if src.is_empty() { json!(false) } else { json!(src) };
            }
            Plan::Search { index, body, columns: s.columns.clone(), offset: s.offset.unwrap_or(0), limit: s.limit.unwrap_or(ROW_CAP) }
        }
        Statement::Insert { columns, rows, .. } => {
            let mut body = String::new();
            for r in rows {
                let mut doc = Map::new();
                let mut id = None;
                for (c, v) in columns.iter().zip(r) {
                    if c == "_id" {
                        if !v.is_null() {
                            id = Some(value_str(v));
                        }
                    } else if !c.starts_with('_') && !v.is_null() {
                        set_path(&mut doc, c, doc_value(f.type_of(c), v));
                    }
                }
                let mut meta = json!({ "_index": index });
                if let Some(id) = id {
                    meta["_id"] = json!(id);
                }
                body.push_str(&json!({ "index": meta }).to_string());
                body.push('\n');
                body.push_str(&Json::Object(doc).to_string());
                body.push('\n');
            }
            Plan::Bulk { index, body, rows: rows.len() }
        }
        Statement::Update { set, filter, .. } => {
            if set.iter().any(|(c, _)| c == "_id") {
                bail!("the document _id cannot be changed");
            }
            let set: Vec<(String, Json)> =
                set.iter().filter(|(c, _)| !c.starts_with('_')).map(|(c, v)| (c.clone(), doc_value(f.type_of(c), v))).collect();
            if set.is_empty() {
                bail!("nothing to update");
            }
            match filter.as_ref().and_then(id_eq) {
                Some(id) => {
                    let mut doc = Map::new();
                    for (c, v) in set {
                        set_path(&mut doc, &c, v);
                    }
                    Plan::UpdateDoc { index, id, body: json!({ "doc": doc }) }
                }
                None => Plan::UpdateByQuery {
                    index,
                    body: json!({ "query": filter_query(filter.as_ref(), f)?, "script": script(&set) }),
                },
            }
        }
        Statement::Delete { filter, .. } => match filter.as_ref().and_then(id_eq) {
            Some(id) => Plan::DeleteDoc { index, id },
            None => Plan::DeleteByQuery { index, body: json!({ "query": filter_query(filter.as_ref(), f)? }) },
        },
    })
}

fn value_str(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `_id = 'x'` (alone) -> `x`.
fn id_eq(e: &Expr) -> Option<String> {
    match e {
        Expr::Cmp { column, op: CmpOp::Eq, value } if column == "_id" && !value.is_null() => Some(value_str(value)),
        Expr::And(v) if v.len() == 1 => id_eq(&v[0]),
        _ => None,
    }
}

const NUMERIC: [&str; 9] = ["long", "integer", "short", "byte", "double", "float", "half_float", "scaled_float", "unsigned_long"];

/// JSON for a written value: JSON text becomes an embedded object, text in
/// numeric / boolean fields is converted.
pub fn doc_value(ty: Option<&str>, v: &Value) -> Json {
    if let Value::Text(s) = v {
        let t = s.trim();
        if (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')) {
            if let Ok(j) = serde_json::from_str::<Json>(t) {
                return j;
            }
        }
        match ty {
            Some(ty) if NUMERIC.contains(&ty) => {
                if let Ok(i) = t.parse::<i64>() {
                    return json!(i);
                }
                if let Some(n) = t.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                    return Json::Number(n);
                }
            }
            Some("boolean") => match t.to_ascii_lowercase().as_str() {
                "true" | "1" => return json!(true),
                "false" | "0" => return json!(false),
                _ => {}
            },
            _ => {}
        }
    }
    v.to_json()
}

/// Insert `a.b.c` as nested objects.
pub fn set_path(doc: &mut Map<String, Json>, key: &str, v: Json) {
    fn nestable(m: &Map<String, Json>, parts: &[&str]) -> bool {
        match parts {
            [] | [_] => true,
            [h, rest @ ..] => match m.get(*h) {
                None => true,
                Some(Json::Object(o)) => nestable(o, rest),
                _ => false,
            },
        }
    }
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) || !nestable(doc, &parts) {
        doc.insert(key.to_string(), v);
        return;
    }
    let mut cur = doc;
    for p in &parts[..parts.len() - 1] {
        match cur.entry(p.to_string()).or_insert_with(|| Json::Object(Map::new())) {
            Json::Object(m) => cur = m,
            _ => return,
        }
    }
    cur.insert(parts[parts.len() - 1].to_string(), v);
}

fn painless_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Painless script assigning `params.pN` to each (dotted) field.
pub fn script(set: &[(String, Json)]) -> Json {
    let mut src = String::new();
    let mut params = Map::new();
    for (i, (col, v)) in set.iter().enumerate() {
        let parts: Vec<&str> = col.split('.').collect();
        let mut path = "ctx._source".to_string();
        for p in &parts[..parts.len() - 1] {
            path = format!("{}[{}]", path, painless_str(p));
            src.push_str(&format!("if (!({p} instanceof Map)) {{ {p} = new HashMap(); }} ", p = path));
        }
        src.push_str(&format!("{}[{}] = params.p{}; ", path, painless_str(parts[parts.len() - 1]), i));
        params.insert(format!("p{}", i), v.clone());
    }
    json!({ "source": src.trim_end(), "lang": "painless", "params": params })
}

/// `%`/`_` LIKE pattern -> wildcard pattern.
pub fn like_to_wildcard(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push('*'),
            '_' => out.push('?'),
            '*' | '?' => {
                out.push('\\');
                out.push(c);
            }
            '\\' => match chars.next() {
                Some(n @ ('%' | '_')) => out.push(n),
                Some(n) => {
                    out.push_str("\\\\");
                    out.push(n);
                }
                None => out.push_str("\\\\"),
            },
            c => out.push(c),
        }
    }
    out
}

pub fn filter_query(e: Option<&Expr>, f: &Fields) -> Result<Json> {
    match e {
        None => Ok(json!({ "match_all": {} })),
        Some(e) => query(e, f),
    }
}

fn not(q: Json) -> Json {
    json!({ "bool": { "must_not": [q] } })
}

fn exists(col: &str) -> Json {
    json!({ "exists": { "field": col } })
}

fn eq(col: &str, v: &Value, f: &Fields) -> Json {
    if col == "_id" {
        return json!({ "ids": { "values": [value_str(v)] } });
    }
    let jv = doc_value(f.type_of(col), v);
    if f.is_text(col) && !f.keyword.contains_key(col) {
        return json!({ "match_phrase": { col: jv } });
    }
    json!({ "term": { f.exact(col): jv } })
}

pub fn query(e: &Expr, f: &Fields) -> Result<Json> {
    Ok(match e {
        Expr::And(v) => match v.as_slice() {
            [one] => query(one, f)?,
            _ => json!({ "bool": { "must": v.iter().map(|x| query(x, f)).collect::<Result<Vec<_>>>()? } }),
        },
        Expr::Or(v) => json!({ "bool": {
            "should": v.iter().map(|x| query(x, f)).collect::<Result<Vec<_>>>()?,
            "minimum_should_match": 1,
        } }),
        Expr::Not(x) => not(query(x, f)?),
        Expr::Cmp { column, op, value } => {
            if value.is_null() {
                return Ok(match op {
                    CmpOp::Eq => not(exists(column)),
                    CmpOp::Ne => exists(column),
                    _ => json!({ "match_none": {} }),
                });
            }
            let r = |k: &str| json!({ "range": { f.exact(column): { k: doc_value(f.type_of(column), value) } } });
            match op {
                CmpOp::Eq => eq(column, value, f),
                // SQL `<>` never matches NULL
                CmpOp::Ne => json!({ "bool": { "must_not": [eq(column, value, f)], "filter": [exists(column)] } }),
                CmpOp::Lt => r("lt"),
                CmpOp::Le => r("lte"),
                CmpOp::Gt => r("gt"),
                CmpOp::Ge => r("gte"),
            }
        }
        Expr::Like { column, pattern, negated } => {
            let q = json!({ "wildcard": { f.exact(column): { "value": like_to_wildcard(pattern), "case_insensitive": true } } });
            if *negated { not(q) } else { q }
        }
        Expr::In { column, values, negated } => {
            let q = if column == "_id" {
                json!({ "ids": { "values": values.iter().map(value_str).collect::<Vec<_>>() } })
            } else if f.is_text(column) && !f.keyword.contains_key(column) {
                json!({ "bool": { "should": values.iter().map(|v| eq(column, v, f)).collect::<Vec<_>>(), "minimum_should_match": 1 } })
            } else {
                let ty = f.type_of(column);
                json!({ "terms": { f.exact(column): values.iter().map(|v| doc_value(ty, v)).collect::<Vec<_>>() } })
            };
            if *negated { not(q) } else { q }
        }
        Expr::Between { column, low, high } => {
            let ty = f.type_of(column);
            json!({ "range": { f.exact(column): { "gte": doc_value(ty, low), "lte": doc_value(ty, high) } } })
        }
        Expr::IsNull { column, negated } => {
            if *negated { exists(column) } else { not(exists(column)) }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::dml::{self, FilterOp, FilterRule};
    use crate::model::ObjPath;

    fn fields() -> Fields {
        Fields::from_mapping(&json!({"idx": {"mappings": {"properties": {
            "name": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
            "body": {"type": "text"},
            "age": {"type": "long"},
            "ok": {"type": "boolean"},
        }}}}))
    }

    fn rule(c: &str, op: FilterOp, v: &str) -> FilterRule {
        FilterRule { op, value: v.into(), ..FilterRule::new(c) }
    }

    fn plan_sql(sql: &str) -> Plan {
        plan(&sqlmini::parse(sql).unwrap(), &fields()).unwrap()
    }

    #[test]
    fn select_page() {
        let d = Dialect::Generic;
        let p = ObjPath::new("cluster", None, "idx");
        let w = dml::filters_to_where(d, &[rule("name", FilterOp::Contains, "a_b"), rule("age", FilterOp::Ge, "3"), FilterRule { and: false, ..rule("body", FilterOp::Eq, "hi") }]);
        let sql = dml::select_page(d, &p, &w, &[dml::SortSpec { column: "name".into(), descending: true }], 50, 100);
        let Plan::Search { index, body, offset, limit, .. } = plan_sql(&sql) else { panic!() };
        assert_eq!((index.as_str(), offset, limit), ("idx", 100, 50));
        assert_eq!(body["sort"], json!([{"name.keyword": {"order": "desc"}}]));
        let should = &body["query"]["bool"]["should"];
        assert_eq!(should[0]["bool"]["must"][0], json!({"wildcard": {"name.keyword": {"value": "*a?b*", "case_insensitive": true}}}));
        assert_eq!(should[0]["bool"]["must"][1], json!({"range": {"age": {"gte": 3}}}));
        assert_eq!(should[1], json!({"match_phrase": {"body": "hi"}}));
        let (m, path, b) = plan_sql(&sql).request();
        assert_eq!((m, path.as_str()), ("POST", "/idx/_search"));
        let b: Json = serde_json::from_str(&b.unwrap()).unwrap();
        assert_eq!((b["from"].as_u64(), b["size"].as_u64()), (Some(100), Some(50)));
        let (_, path, _) = plan_sql("SELECT * FROM idx LIMIT 100 OFFSET 20000").request();
        assert_eq!(path, "/idx/_search?scroll=1m");
    }

    #[test]
    fn filters() {
        let f = fields();
        let q = |sql: &str| {
            let Statement::Select(s) = sqlmini::parse(sql).unwrap() else { panic!() };
            filter_query(s.filter.as_ref(), &f).unwrap()
        };
        assert_eq!(q("SELECT * FROM t WHERE _id = 'x'"), json!({"ids": {"values": ["x"]}}));
        assert_eq!(q("SELECT * FROM t WHERE name = 'x'"), json!({"term": {"name.keyword": "x"}}));
        assert_eq!(q("SELECT * FROM t WHERE age IN (1, '2')"), json!({"terms": {"age": [1, 2]}}));
        assert_eq!(q("SELECT * FROM t WHERE age BETWEEN 1 AND 5"), json!({"range": {"age": {"gte": 1, "lte": 5}}}));
        assert_eq!(q("SELECT * FROM t WHERE age IS NULL"), json!({"bool": {"must_not": [{"exists": {"field": "age"}}]}}));
        assert_eq!(q("SELECT * FROM t WHERE ok IS NOT NULL"), json!({"exists": {"field": "ok"}}));
        assert_eq!(q("SELECT * FROM t WHERE NOT (age < 2)"), json!({"bool": {"must_not": [{"range": {"age": {"lt": 2}}}]}}));
        assert_eq!(q("SELECT * FROM t WHERE age <> 2")["bool"]["filter"], json!([{"exists": {"field": "age"}}]));
        assert_eq!(q("SELECT * FROM t WHERE a = 1 OR b = 2")["bool"]["minimum_should_match"], json!(1));
        assert_eq!(q("SELECT * FROM t")["match_all"], json!({}));
        assert_eq!(like_to_wildcard("a*b%_\\%"), "a\\*b*?%");
    }

    #[test]
    fn writes() {
        let d = Dialect::Generic;
        let p = ObjPath::new("c", None, "idx");
        let ins = dml::insert_rows(d, &p, &["_id".into(), "age".into(), "addr.city".into(), "tags".into()], &[
            vec![Value::Text("k1".into()), Value::Text("42".into()), Value::Text("x".into()), Value::Text("[\"a\"]".into())],
            vec![Value::Null, Value::Int(1), Value::Null, Value::Null],
        ]);
        let Plan::Bulk { body, rows, .. } = plan_sql(&ins) else { panic!() };
        assert_eq!(rows, 2);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[0], r#"{"index":{"_index":"idx","_id":"k1"}}"#);
        assert_eq!(lines[1], r#"{"age":42,"addr":{"city":"x"},"tags":["a"]}"#);
        assert_eq!(lines[2], r#"{"index":{"_index":"idx"}}"#);
        assert_eq!(lines[3], r#"{"age":1}"#);

        let up = dml::update_row(d, &p, &[("addr.city".into(), Value::Text("y".into()))], &[("_id".into(), Value::Text("k1".into()))]);
        assert_eq!(plan_sql(&up), Plan::UpdateDoc { index: "idx".into(), id: "k1".into(), body: json!({"doc": {"addr": {"city": "y"}}}) });
        let Plan::UpdateByQuery { body, .. } = plan_sql("UPDATE idx SET a.b = 'v', age = '7' WHERE age = 1") else { panic!() };
        assert_eq!(body["script"]["params"], json!({"p0": "v", "p1": 7}));
        assert_eq!(
            body["script"]["source"],
            json!("if (!(ctx._source['a'] instanceof Map)) { ctx._source['a'] = new HashMap(); } ctx._source['a']['b'] = params.p0; ctx._source['age'] = params.p1;")
        );
        let del = dml::delete_row(d, &p, &[("_id".into(), Value::Text("a/b".into()))]);
        assert_eq!(plan_sql(&del).request().1, "/idx/_doc/a%2Fb?refresh=true");
        assert!(matches!(plan_sql("DELETE FROM idx WHERE age > 3"), Plan::DeleteByQuery { .. }));
        assert!(matches!(plan_sql("SELECT COUNT(*) FROM \"c\".\"idx\""), Plan::Count { .. }));
    }
}
