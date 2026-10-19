//! Index mappings and JSON responses -> grid rows.

use std::collections::HashMap;

use serde_json::{Map, Value as Json};

use crate::value::{ColumnMeta, QueryResult, Value};

/// Flattened view of an index mapping (`GET /<index>/_mapping`).
#[derive(Clone, Debug, Default)]
pub struct Fields {
    /// Leaf columns in mapping order: (dotted name, mapping type).
    pub columns: Vec<(String, String)>,
    /// text field -> keyword sub-field (`name.keyword`).
    pub keyword: HashMap<String, String>,
    /// Multi-fields per column: (full name, type).
    pub subfields: HashMap<String, Vec<(String, String)>>,
}

impl Fields {
    /// Merge the mappings of every index in a `_mapping` response.
    pub fn from_mapping(resp: &Json) -> Fields {
        let mut f = Fields::default();
        if let Some(obj) = resp.as_object() {
            for idx in obj.values() {
                let m = &idx["mappings"];
                let props = if m.get("properties").is_some() {
                    &m["properties"]
                } else {
                    // pre-7 type level mapping
                    m.as_object().and_then(|o| o.values().find(|v| v.get("properties").is_some())).map(|v| &v["properties"]).unwrap_or(&Json::Null)
                };
                if let Some(p) = props.as_object() {
                    f.add(p, "");
                }
            }
        }
        f
    }

    fn add(&mut self, props: &Map<String, Json>, prefix: &str) {
        for (k, v) in props {
            let full = format!("{}{}", prefix, k);
            let ty = v.get("type").and_then(|t| t.as_str());
            if let (Some(p), None | Some("object")) = (v.get("properties").and_then(|p| p.as_object()), ty) {
                self.add(p, &format!("{}.", full));
                continue;
            }
            if self.type_of(&full).is_some() {
                continue;
            }
            self.columns.push((full.clone(), ty.unwrap_or("object").to_string()));
            if let Some(subs) = v.get("fields").and_then(|f| f.as_object()) {
                let mut list = vec![];
                for (sk, sv) in subs {
                    let st = sv.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    let sf = format!("{}.{}", full, sk);
                    if st == "keyword" && !self.keyword.contains_key(&full) {
                        self.keyword.insert(full.clone(), sf.clone());
                    }
                    list.push((sf, st));
                }
                self.subfields.insert(full.clone(), list);
            }
        }
    }

    pub fn type_of(&self, col: &str) -> Option<&str> {
        self.columns.iter().find(|(c, _)| c == col).map(|(_, t)| t.as_str())
    }

    pub fn is_text(&self, col: &str) -> bool {
        matches!(self.type_of(col), Some("text" | "match_only_text"))
    }

    /// Field to use for exact matching / sorting / ranges.
    pub fn exact(&self, col: &str) -> String {
        if self.is_text(col) {
            if let Some(k) = self.keyword.get(col) {
                return k.clone();
            }
        }
        col.to_string()
    }
}

/// A cell for a JSON value: scalars map to [`Value`], objects/arrays to compact JSON.
pub fn cell(v: &Json) -> Value {
    match v {
        Json::Object(_) | Json::Array(_) => Value::Text(v.to_string()),
        other => Value::from_json(other),
    }
}

/// Flatten a `_source` document into dotted columns. Objects that are mapped
/// leaves (nested, geo_point, flattened, ...) stay as JSON text.
pub fn flatten(src: &Map<String, Json>, prefix: &str, fields: Option<&Fields>, out: &mut Vec<(String, Value)>) {
    for (k, v) in src {
        let full = format!("{}{}", prefix, k);
        match v {
            Json::Object(o) if fields.is_none_or(|f| f.type_of(&full).is_none()) => {
                flatten(o, &format!("{}.", full), fields, out)
            }
            _ => out.push((full, cell(v))),
        }
    }
}

/// Rows with a growing column set.
#[derive(Default)]
pub struct Table {
    cols: Vec<ColumnMeta>,
    idx: HashMap<String, usize>,
    rows: Vec<Vec<Value>>,
}

impl Table {
    pub fn with_columns(cols: impl IntoIterator<Item = (String, String)>) -> Table {
        let mut t = Table::default();
        for (c, ty) in cols {
            t.col(&c, &ty);
        }
        t
    }

    fn col(&mut self, name: &str, ty: &str) -> usize {
        if let Some(&i) = self.idx.get(name) {
            return i;
        }
        self.cols.push(ColumnMeta { name: name.to_string(), type_name: ty.to_string() });
        self.idx.insert(name.to_string(), self.cols.len() - 1);
        self.cols.len() - 1
    }

    pub fn push(&mut self, pairs: Vec<(String, Value)>) {
        let mut row = vec![Value::Null; self.cols.len()];
        for (k, v) in pairs {
            let i = self.col(&k, "");
            if i >= row.len() {
                row.resize(i + 1, Value::Null);
            }
            row[i] = v;
        }
        self.rows.push(row);
    }

    /// Drop pushed values whose column is not in `keep` (projection).
    pub fn finish(mut self, sql: &str) -> QueryResult {
        let n = self.cols.len();
        for r in &mut self.rows {
            r.resize(n, Value::Null);
        }
        QueryResult { sql: sql.to_string(), columns: self.cols, rows: self.rows, is_query: true, ..Default::default() }
    }
}

/// Rows for `hits.hits`. With `fields`, columns follow the mapping (`_id` first).
pub fn hits_table(resp: &Json, fields: Option<&Fields>, with_meta: bool) -> Table {
    let hits = resp["hits"]["hits"].as_array().cloned().unwrap_or_default();
    let multi_index = {
        let mut names = hits.iter().filter_map(|h| h["_index"].as_str());
        let first = names.next();
        names.any(|n| Some(n) != first)
    };
    let mut head = vec![("_id".to_string(), "keyword".to_string())];
    if multi_index {
        head.insert(0, ("_index".into(), "keyword".into()));
    }
    if with_meta {
        head.push(("_score".into(), "float".into()));
    }
    let mut t = Table::with_columns(head.into_iter().chain(fields.map(|f| f.columns.clone()).unwrap_or_default()));
    for h in &hits {
        t.push(hit_row(h, fields, multi_index, with_meta));
    }
    t
}

pub fn hit_row(h: &Json, fields: Option<&Fields>, with_index: bool, with_score: bool) -> Vec<(String, Value)> {
    let mut row = vec![];
    if with_index {
        row.push(("_index".to_string(), cell(&h["_index"])));
    }
    row.push(("_id".to_string(), cell(&h["_id"])));
    if with_score {
        row.push(("_score".to_string(), cell(&h["_score"])));
    }
    if let Some(src) = h["_source"].as_object() {
        flatten(src, "", fields, &mut row);
    }
    row
}

/// Bucket / metric aggregations as rows (one row per leaf bucket).
pub fn agg_rows(aggs: &Map<String, Json>) -> Vec<Vec<(String, Value)>> {
    let mut metrics: Vec<(String, Value)> = vec![];
    let mut bucket_rows: Vec<Vec<(String, Value)>> = vec![];
    let mut has_buckets = false;
    for (name, a) in aggs {
        let Some(o) = a.as_object() else {
            continue;
        };
        if let Some(b) = o.get("buckets") {
            has_buckets = true;
            let list: Vec<(Option<String>, &Json)> = match b {
                Json::Array(v) => v.iter().map(|x| (None, x)).collect(),
                Json::Object(m) => m.iter().map(|(k, x)| (Some(k.clone()), x)).collect(),
                _ => vec![],
            };
            for (k, bucket) in list {
                let Some(bo) = bucket.as_object() else { continue };
                let key = match k {
                    Some(k) => Value::Text(k),
                    None => cell(bo.get("key_as_string").or_else(|| bo.get("key")).unwrap_or(&Json::Null)),
                };
                let base = vec![(name.clone(), key), (format!("{}.doc_count", name), cell(&bucket["doc_count"]))];
                let mut sub = bo.clone();
                for k in ["key", "key_as_string", "doc_count", "from", "to", "from_as_string", "to_as_string"] {
                    sub.remove(k);
                }
                let subs = agg_rows(&sub);
                if subs.is_empty() {
                    bucket_rows.push(base);
                } else {
                    for s in subs {
                        bucket_rows.push(base.iter().cloned().chain(s).collect());
                    }
                }
            }
        } else if let Some(v) = o.get("value") {
            metrics.push((name.clone(), cell(o.get("value_as_string").unwrap_or(v))));
        } else if o.contains_key("doc_count") {
            // single bucket (filter, global, nested ...)
            metrics.push((format!("{}.doc_count", name), cell(&o["doc_count"])));
            let mut sub = o.clone();
            sub.remove("doc_count");
            for r in agg_rows(&sub) {
                metrics.extend(r.into_iter().map(|(k, v)| (format!("{}.{}", name, k), v)));
            }
        } else {
            let mut flat = vec![];
            flatten(o, &format!("{}.", name), None, &mut flat);
            metrics.extend(flat.into_iter().filter(|(k, _)| !k.ends_with(".meta")));
        }
    }
    if !has_buckets {
        return if metrics.is_empty() { vec![] } else { vec![metrics] };
    }
    bucket_rows.into_iter().map(|r| r.into_iter().chain(metrics.iter().cloned()).collect()).collect()
}

/// A JSON array of objects (`_cat/*`) as a table.
pub fn array_table(arr: &[Json], sql: &str) -> QueryResult {
    let mut t = Table::default();
    for item in arr {
        match item.as_object() {
            Some(o) => {
                let mut row = vec![];
                for (k, v) in o {
                    row.push((k.clone(), cell(v)));
                }
                t.push(row);
            }
            None => t.push(vec![("value".into(), cell(item))]),
        }
    }
    t.finish(sql)
}

/// Generic object response as `Key` / `Value` rows with dotted keys.
pub fn key_values(obj: &Map<String, Json>, sql: &str) -> QueryResult {
    let mut pairs = vec![];
    flatten(obj, "", None, &mut pairs);
    QueryResult {
        sql: sql.to_string(),
        columns: vec![ColumnMeta::new("Key"), ColumnMeta::new("Value")],
        rows: pairs.into_iter().map(|(k, v)| vec![Value::Text(k), v]).collect(),
        is_query: true,
        ..Default::default()
    }
}

fn bulk_items(resp: &Json, sql: &str) -> QueryResult {
    let mut t = Table::with_columns(
        ["action", "_index", "_id", "result", "status", "error"].iter().map(|c| (c.to_string(), String::new())),
    );
    for item in resp["items"].as_array().into_iter().flatten() {
        if let Some((action, r)) = item.as_object().and_then(|o| o.iter().next()) {
            t.push(vec![
                ("action".into(), Value::Text(action.clone())),
                ("_index".into(), cell(&r["_index"])),
                ("_id".into(), cell(&r["_id"])),
                ("result".into(), cell(&r["result"])),
                ("status".into(), cell(&r["status"])),
                ("error".into(), if r["error"].is_null() { Value::Null } else { Value::Text(super::error_text(&r["error"])) }),
            ]);
        }
    }
    let mut q = t.finish(sql);
    q.affected_rows = q.rows.len() as u64;
    q
}

fn search_results(resp: &Json, sql: &str, out: &mut Vec<QueryResult>) {
    let hits = hits_table(resp, None, true).finish(sql);
    let aggs = resp["aggregations"].as_object().map(|a| {
        let mut t = Table::default();
        for r in agg_rows(a) {
            t.push(r);
        }
        t.finish(sql)
    });
    match aggs {
        Some(a) if hits.rows.is_empty() => out.push(a),
        Some(a) => {
            out.push(hits);
            out.push(a);
        }
        None => out.push(hits),
    }
}

/// Results for a console request's JSON response.
pub fn response_results(resp: &Json, sql: &str) -> Vec<QueryResult> {
    let mut out = vec![];
    match resp {
        Json::Array(a) => out.push(array_table(a, sql)),
        Json::Object(o) => {
            if o.get("hits").and_then(|h| h.get("hits")).is_some() {
                search_results(resp, sql, &mut out);
            } else if let Some(rs) = o.get("responses").and_then(|r| r.as_array()) {
                for r in rs {
                    if r.get("error").is_some() {
                        out.push(key_values(r.as_object().unwrap_or(&Map::new()), sql));
                    } else {
                        search_results(r, sql, &mut out);
                    }
                }
            } else if o.contains_key("items") && o.contains_key("errors") {
                out.push(bulk_items(resp, sql));
            } else {
                out.push(key_values(o, sql));
            }
        }
        Json::Null => out.push(QueryResult { sql: sql.to_string(), ..Default::default() }),
        Json::String(s) => out.push(QueryResult {
            sql: sql.to_string(),
            columns: vec![ColumnMeta::new("response")],
            rows: s.lines().map(|l| vec![Value::Text(l.to_string())]).collect(),
            is_query: true,
            ..Default::default()
        }),
        other => out.push(QueryResult {
            sql: sql.to_string(),
            columns: vec![ColumnMeta::new("response")],
            rows: vec![vec![cell(other)]],
            is_query: true,
            ..Default::default()
        }),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields() -> Fields {
        Fields::from_mapping(&json!({"idx": {"mappings": {"properties": {
            "name": {"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}},
            "age": {"type": "long"},
            "addr": {"properties": {"city": {"type": "keyword"}, "geo": {"type": "geo_point"}}},
            "tags": {"type": "nested", "properties": {"t": {"type": "keyword"}}},
        }}}}))
    }

    #[test]
    fn mapping_flatten() {
        let f = fields();
        let names: Vec<_> = f.columns.iter().map(|(c, t)| format!("{}:{}", c, t)).collect();
        assert_eq!(names, vec!["name:text", "age:long", "addr.city:keyword", "addr.geo:geo_point", "tags:nested"]);
        assert_eq!(f.exact("name"), "name.keyword");
        assert_eq!(f.exact("age"), "age");
    }

    #[test]
    fn hits_rows() {
        let f = fields();
        let resp = json!({"hits": {"hits": [
            {"_index": "idx", "_id": "1", "_score": 1.0, "_source": {"name": "a", "addr": {"city": "x", "geo": {"lat": 1, "lon": 2}}, "tags": [{"t": "q"}], "extra": {"k": true}}},
        ]}});
        let q = hits_table(&resp, Some(&f), false).finish("");
        let cols: Vec<_> = q.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["_id", "name", "age", "addr.city", "addr.geo", "tags", "extra.k"]);
        assert_eq!(q.rows[0][0], Value::Text("1".into()));
        assert_eq!(q.rows[0][2], Value::Null);
        assert_eq!(q.rows[0][4], Value::Text(r#"{"lat":1,"lon":2}"#.into()));
        assert_eq!(q.rows[0][5], Value::Text(r#"[{"t":"q"}]"#.into()));
        assert_eq!(q.rows[0][6], Value::Bool(true));
    }

    #[test]
    fn aggregations() {
        let a = json!({
            "by_city": {"buckets": [
                {"key": "x", "doc_count": 2, "avg_age": {"value": 30.0}},
                {"key": "y", "doc_count": 1, "avg_age": {"value": null}},
            ]},
            "total": {"value": 3},
        });
        let rows = agg_rows(a.as_object().unwrap());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![
            ("by_city".to_string(), Value::Text("x".into())),
            ("by_city.doc_count".to_string(), Value::Int(2)),
            ("avg_age".to_string(), Value::Float(30.0)),
            ("total".to_string(), Value::Int(3)),
        ]);
        let r = response_results(&json!({"hits": {"hits": []}, "aggregations": a}), "q");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].columns[0].name, "by_city");
    }

    #[test]
    fn generic_responses() {
        let r = response_results(&json!([{"health": "green", "index": "a"}, {"index": "b", "x": 1}]), "");
        assert_eq!(r[0].columns.len(), 3);
        assert_eq!(r[0].rows[1], vec![Value::Null, Value::Text("b".into()), Value::Int(1)]);
        let r = response_results(&json!({"acknowledged": true, "shards": {"total": 2}}), "");
        assert_eq!(r[0].rows[1], vec![Value::Text("shards.total".into()), Value::Int(2)]);
    }
}
