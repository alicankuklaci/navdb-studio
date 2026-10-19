//! Import Wizard: parse source files into rows and turn them into statements.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::model::{ColumnDef, ObjPath, TableDef};
use crate::value::{ColumnMeta, QueryResult, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportFormat {
    Csv,
    Tsv,
    Json,
    Xml,
}

impl ImportFormat {
    pub const ALL: [ImportFormat; 4] = [ImportFormat::Csv, ImportFormat::Tsv, ImportFormat::Json, ImportFormat::Xml];

    pub fn label(self) -> &'static str {
        match self {
            ImportFormat::Csv => "CSV file (*.csv)",
            ImportFormat::Tsv => "Text / TSV file (*.txt, *.tsv)",
            ImportFormat::Json => "JSON file (*.json)",
            ImportFormat::Xml => "XML file (*.xml)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportMode {
    /// Append records to the destination table.
    Append,
    /// Update existing records matched by key, ignore others.
    Update,
    /// Insert or update by key.
    AppendUpdate,
    /// Delete records in destination that match source keys.
    Delete,
    /// Empty destination then append.
    Copy,
}

impl ImportMode {
    pub const ALL: [ImportMode; 5] = [ImportMode::Append, ImportMode::Update, ImportMode::AppendUpdate, ImportMode::Delete, ImportMode::Copy];

    pub fn label(self) -> &'static str {
        match self {
            ImportMode::Append => "Append: add records to the destination table",
            ImportMode::Update => "Update: update destination records matching source",
            ImportMode::AppendUpdate => "Append/Update: update if exists, otherwise add",
            ImportMode::Delete => "Delete: delete destination records matching source",
            ImportMode::Copy => "Copy: delete all destination records, then add",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportOptions {
    pub format: ImportFormat,
    pub delimiter: char,
    pub quote: char,
    pub has_header: bool,
    /// 1-based line that holds the field names.
    pub header_row: usize,
    /// 1-based first data row.
    pub first_data_row: usize,
    pub last_data_row: Option<usize>,
    pub empty_as_null: bool,
    /// XML element that represents one record.
    pub xml_record_tag: String,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            format: ImportFormat::Csv,
            delimiter: ',',
            quote: '"',
            has_header: true,
            header_row: 1,
            first_data_row: 2,
            last_data_row: None,
            empty_as_null: true,
            xml_record_tag: "RECORD".into(),
        }
    }
}

fn text_value(s: &str, empty_as_null: bool) -> Value {
    if s.is_empty() && empty_as_null {
        Value::Null
    } else {
        Value::Text(s.to_string())
    }
}

pub fn parse(content: &str, o: &ImportOptions) -> Result<QueryResult> {
    match o.format {
        ImportFormat::Csv | ImportFormat::Tsv => parse_delimited(content, o),
        ImportFormat::Json => parse_json(content, o),
        ImportFormat::Xml => parse_xml(content, o),
    }
}

fn parse_delimited(content: &str, o: &ImportOptions) -> Result<QueryResult> {
    let delim = if o.format == ImportFormat::Tsv { b'\t' } else { o.delimiter as u8 };
    let mut rd = csv::ReaderBuilder::new()
        .delimiter(delim)
        .quote(o.quote as u8)
        .has_headers(false)
        .flexible(true)
        .from_reader(content.as_bytes());
    let records: Vec<csv::StringRecord> = rd.records().collect::<std::result::Result<_, _>>()?;
    let mut out = QueryResult { is_query: true, ..Default::default() };
    let width = records.iter().map(|r| r.len()).max().unwrap_or(0);
    if o.has_header && o.header_row >= 1 && o.header_row <= records.len() {
        let h = &records[o.header_row - 1];
        out.columns = (0..width)
            .map(|i| ColumnMeta::new(h.get(i).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("F{}", i + 1))))
            .collect();
    } else {
        out.columns = (0..width).map(|i| ColumnMeta::new(format!("F{}", i + 1))).collect();
    }
    let first = o.first_data_row.max(1) - 1;
    let last = o.last_data_row.unwrap_or(records.len()).min(records.len());
    for r in records.iter().take(last).skip(first) {
        out.rows.push((0..width).map(|i| text_value(r.get(i).unwrap_or(""), o.empty_as_null)).collect());
    }
    Ok(out)
}

fn parse_json(content: &str, o: &ImportOptions) -> Result<QueryResult> {
    let v: serde_json::Value = serde_json::from_str(content)?;
    // Accept `[...]`, `{"name": [...]}` or newline-delimited objects.
    let arr: Vec<serde_json::Value> = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(m) => match m.into_iter().find(|(_, v)| v.is_array()) {
            Some((_, serde_json::Value::Array(a))) => a,
            _ => bail!("JSON object does not contain an array of records"),
        },
        _ => bail!("unsupported JSON layout"),
    };
    let mut cols: Vec<String> = Vec::new();
    for item in &arr {
        if let serde_json::Value::Object(m) = item {
            for k in m.keys() {
                if !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    let mut out = QueryResult { is_query: true, columns: cols.iter().map(ColumnMeta::new).collect(), ..Default::default() };
    for item in arr {
        if let serde_json::Value::Object(m) = item {
            out.rows.push(
                cols.iter()
                    .map(|c| match m.get(c) {
                        None => Value::Null,
                        Some(serde_json::Value::String(s)) => text_value(s, o.empty_as_null),
                        Some(v) => Value::from_json(v),
                    })
                    .collect(),
            );
        }
    }
    Ok(out)
}

/// Minimal XML reader for `<RECORDS><RECORD><field>value</field>...` layouts.
fn parse_xml(content: &str, o: &ImportOptions) -> Result<QueryResult> {
    let tag = if o.xml_record_tag.is_empty() { "RECORD" } else { &o.xml_record_tag };
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut cols: Vec<String> = Vec::new();
    let mut recs: Vec<Vec<(String, Option<String>)>> = Vec::new();
    let mut rest = content;
    let field_re = regex::Regex::new(r"(?s)<([A-Za-z_][\w\-.]*)\s*(/>|>(.*?)</([A-Za-z_][\w\-.]*)>)")?;
    while let Some(p) = rest.find(&open) {
        let after = &rest[p + open.len()..];
        let Some(e) = after.find(&close) else { break };
        let body = &after[..e];
        let mut rec = Vec::new();
        for cap in field_re.captures_iter(body) {
            let name = cap[1].to_string();
            let val = if &cap[2] == "/>" { None } else { cap.get(3).map(|m| unescape_xml(m.as_str())) };
            if !cols.contains(&name) {
                cols.push(name.clone());
            }
            rec.push((name, val));
        }
        recs.push(rec);
        rest = &after[e + close.len()..];
    }
    let mut out = QueryResult { is_query: true, columns: cols.iter().map(ColumnMeta::new).collect(), ..Default::default() };
    for rec in recs {
        out.rows.push(
            cols.iter()
                .map(|c| match rec.iter().find(|(n, _)| n == c) {
                    Some((_, Some(v))) => text_value(v, o.empty_as_null),
                    _ => Value::Null,
                })
                .collect(),
        );
    }
    Ok(out)
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Mapping of source field -> destination column (None = skip).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldMapping {
    pub source: String,
    pub target: Option<String>,
    pub is_key: bool,
}

pub fn auto_map(source: &QueryResult, target_cols: &[String]) -> Vec<FieldMapping> {
    source
        .columns
        .iter()
        .map(|c| {
            let t = target_cols.iter().find(|t| t.eq_ignore_ascii_case(&c.name)).cloned();
            FieldMapping { source: c.name.clone(), target: t, is_key: false }
        })
        .collect()
}

/// Destination table definition for "create new table" imports.
/// `YYYY-MM-DD`
fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// `YYYY-MM-DD HH:MM:SS` (optionally `T` separator and fractional seconds)
fn looks_like_datetime(s: &str) -> bool {
    s.is_ascii() && s.len() >= 19 && looks_like_date(&s[..10]) && matches!(s.as_bytes()[10], b' ' | b'T') && {
        let t = s[11..19].as_bytes();
        t[2] == b':' && t[5] == b':' && t.iter().enumerate().all(|(i, c)| i == 2 || i == 5 || c.is_ascii_digit())
    }
}

pub fn infer_table(source: &QueryResult, path: ObjPath, d: Dialect) -> TableDef {
    let columns = source
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut all_int = true;
            let mut all_num = true;
            let mut max_len = 1usize;
            let mut all_date = true;
            let mut all_datetime = true;
            for r in &source.rows {
                let s = r[i].edit_text();
                if r[i].is_null() || s.is_empty() {
                    continue;
                }
                max_len = max_len.max(s.chars().count());
                if s.parse::<i64>().is_err() {
                    all_int = false;
                }
                if s.parse::<f64>().is_err() {
                    all_num = false;
                }
                if !looks_like_date(&s) {
                    all_date = false;
                }
                if !looks_like_datetime(&s) {
                    all_datetime = false;
                }
            }
            let any = !source.rows.is_empty() && !all_num;
            let (all_date, all_datetime) = (any && all_date, any && all_datetime);
            let ty = match d {
                Dialect::Sqlite => if all_int { "INTEGER" } else if all_num { "REAL" } else if all_date { "DATE" } else if all_datetime { "DATETIME" } else { "TEXT" }.to_string(),
                Dialect::MySql => {
                    if all_int && !source.rows.is_empty() {
                        "bigint".into()
                    } else if all_num && !source.rows.is_empty() {
                        "double".into()
                    } else if all_date {
                        "date".into()
                    } else if all_datetime {
                        "datetime".into()
                    } else if max_len > 255 {
                        "text".into()
                    } else {
                        "varchar(255)".into()
                    }
                }
                Dialect::Postgres => {
                    if all_int && !source.rows.is_empty() {
                        "int8".into()
                    } else if all_num && !source.rows.is_empty() {
                        "float8".into()
                    } else if all_date {
                        "date".into()
                    } else if all_datetime {
                        "timestamp".into()
                    } else {
                        "text".into()
                    }
                }
            };
            ColumnDef::new(c.name.clone(), ty)
        })
        .collect();
    TableDef { path, columns, ..Default::default() }
}

/// Statements that apply the parsed rows to the destination table.
pub fn build_statements(
    d: Dialect,
    table: &ObjPath,
    source: &QueryResult,
    mapping: &[FieldMapping],
    mode: ImportMode,
    batch: usize,
) -> Vec<String> {
    let used: Vec<(usize, String, bool)> = mapping
        .iter()
        .filter_map(|m| {
            let target = m.target.clone()?;
            let idx = source.column_index(&m.source)?;
            Some((idx, target, m.is_key))
        })
        .collect();
    let cols: Vec<String> = used.iter().map(|(_, t, _)| t.clone()).collect();
    let keys: Vec<String> = used.iter().filter(|(_, _, k)| *k).map(|(_, t, _)| t.clone()).collect();
    let rows: Vec<Vec<Value>> = source.rows.iter().map(|r| used.iter().map(|(i, _, _)| r[*i].clone()).collect()).collect();
    let mut out = Vec::new();
    match mode {
        ImportMode::Append | ImportMode::Copy => {
            if mode == ImportMode::Copy {
                out.push(crate::ddl::empty_table(d, table));
            }
            for chunk in rows.chunks(batch.max(1)) {
                out.push(crate::dml::insert_rows(d, table, &cols, chunk));
            }
        }
        ImportMode::AppendUpdate => {
            for r in &rows {
                out.push(crate::dml::upsert_row(d, table, &cols, r, &keys));
            }
        }
        ImportMode::Update => {
            for r in &rows {
                let changes: Vec<(String, Value)> =
                    cols.iter().zip(r).filter(|(c, _)| !keys.contains(c)).map(|(c, v)| (c.clone(), v.clone())).collect();
                let key_vals: Vec<(String, Value)> =
                    cols.iter().zip(r).filter(|(c, _)| keys.contains(c)).map(|(c, v)| (c.clone(), v.clone())).collect();
                if !changes.is_empty() && !key_vals.is_empty() {
                    let mut s = crate::dml::update_row(d, table, &changes, &key_vals);
                    if d == Dialect::MySql {
                        s = s.trim_end_matches(" LIMIT 1").to_string();
                    }
                    out.push(s);
                }
            }
        }
        ImportMode::Delete => {
            for r in &rows {
                let key_vals: Vec<(String, Value)> =
                    cols.iter().zip(r).filter(|(c, _)| keys.contains(c)).map(|(c, v)| (c.clone(), v.clone())).collect();
                if !key_vals.is_empty() {
                    let s = crate::dml::delete_row(d, table, &key_vals);
                    out.push(s.trim_end_matches(" LIMIT 1").to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_parse() {
        let r = parse("id,name\n1,ali\n2,\n", &ImportOptions::default()).unwrap();
        assert_eq!(r.columns.len(), 2);
        assert_eq!(r.rows.len(), 2);
        assert!(r.rows[1][1].is_null());
    }

    #[test]
    fn json_parse_wrapped() {
        let o = ImportOptions { format: ImportFormat::Json, ..Default::default() };
        let r = parse(r#"{"t":[{"a":1,"b":"x"},{"a":2}]}"#, &o).unwrap();
        assert_eq!(r.columns.len(), 2);
        assert_eq!(r.rows[1][1], Value::Null);
    }

    #[test]
    fn xml_parse() {
        let o = ImportOptions { format: ImportFormat::Xml, ..Default::default() };
        let r = parse("<RECORDS><RECORD><a>1</a><b/></RECORD><RECORD><a>x &amp; y</a><b>2</b></RECORD></RECORDS>", &o).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[1][0], Value::Text("x & y".into()));
        assert!(r.rows[0][1].is_null());
    }

    #[test]
    fn infer_and_statements() {
        let r = parse("id,name\n1,ali\n2,veli\n", &ImportOptions::default()).unwrap();
        let t = infer_table(&r, ObjPath::new("main", None, "p"), Dialect::Sqlite);
        assert_eq!(t.columns[0].data_type, "INTEGER");
        let m = auto_map(&r, &["id".into(), "name".into()]);
        let s = build_statements(Dialect::Sqlite, &t.path, &r, &m, ImportMode::Append, 100);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("('1', 'ali')"));
    }

    #[test]
    fn infer_dates() {
        let r = parse("d,ts,t\n2024-01-10,2024-01-10 10:00:00,İstanbul şehri ve çevresi\n", &ImportOptions::default()).unwrap();
        let t = infer_table(&r, ObjPath::new("db", None, "p"), Dialect::MySql);
        let types: Vec<&str> = t.columns.iter().map(|c| c.data_type.as_str()).collect();
        assert_eq!(types, ["date", "datetime", "varchar(255)"]);
    }
}
