//! Export Wizard formats.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;

use crate::dialect::Dialect;
use crate::model::ObjPath;
use crate::value::{QueryResult, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Txt,
    Json,
    Xml,
    Html,
    Markdown,
    SqlInsert,
    Excel,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 9] = [
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Txt,
        ExportFormat::Json,
        ExportFormat::Xml,
        ExportFormat::Html,
        ExportFormat::Markdown,
        ExportFormat::SqlInsert,
        ExportFormat::Excel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV file (*.csv)",
            ExportFormat::Tsv => "Tab separated (*.tsv)",
            ExportFormat::Txt => "Text file (*.txt)",
            ExportFormat::Json => "JSON file (*.json)",
            ExportFormat::Xml => "XML file (*.xml)",
            ExportFormat::Html => "HTML file (*.html)",
            ExportFormat::Markdown => "Markdown (*.md)",
            ExportFormat::SqlInsert => "SQL script (*.sql)",
            ExportFormat::Excel => "Excel 2003 XML (*.xls)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Txt => "txt",
            ExportFormat::Json => "json",
            ExportFormat::Xml => "xml",
            ExportFormat::Html => "html",
            ExportFormat::Markdown => "md",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::Excel => "xls",
        }
    }

    pub fn from_extension(ext: &str) -> Option<ExportFormat> {
        Self::ALL.iter().copied().find(|f| f.extension().eq_ignore_ascii_case(ext))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportOptions {
    pub include_headers: bool,
    pub delimiter: char,
    pub quote: char,
    pub null_text: String,
    pub line_ending: String,
    /// Columns to export (empty = all).
    pub columns: Vec<String>,
    /// Target table for SQL INSERT export.
    pub table: ObjPath,
    pub dialect: Dialect,
    pub rows_per_insert: usize,
    pub append: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_headers: true,
            delimiter: ',',
            quote: '"',
            null_text: String::new(),
            line_ending: "\n".into(),
            columns: vec![],
            table: ObjPath::default(),
            dialect: Dialect::MySql,
            rows_per_insert: 100,
            append: false,
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn cell(v: &Value, o: &ExportOptions) -> String {
    if v.is_null() {
        o.null_text.clone()
    } else {
        v.edit_text()
    }
}

/// Column indices to export after applying `options.columns`.
fn selected(r: &QueryResult, o: &ExportOptions) -> Vec<usize> {
    if o.columns.is_empty() {
        (0..r.columns.len()).collect()
    } else {
        o.columns.iter().filter_map(|c| r.column_index(c)).collect()
    }
}

pub fn export_to_string(r: &QueryResult, fmt: ExportFormat, o: &ExportOptions) -> Result<String> {
    let mut buf = Vec::new();
    export(r, fmt, o, &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

pub fn export(r: &QueryResult, fmt: ExportFormat, o: &ExportOptions, w: &mut dyn Write) -> Result<()> {
    let cols = selected(r, o);
    let names: Vec<&str> = cols.iter().map(|&i| r.columns[i].name.as_str()).collect();
    let nl = &o.line_ending;
    match fmt {
        ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::Txt => {
            let delim = match fmt {
                ExportFormat::Tsv => b'\t',
                ExportFormat::Txt if o.delimiter == ',' => b'\t',
                _ => o.delimiter as u8,
            };
            let mut wr = csv::WriterBuilder::new()
                .delimiter(delim)
                .quote(o.quote as u8)
                .terminator(if nl == "\r\n" { csv::Terminator::CRLF } else { csv::Terminator::Any(b'\n') })
                .from_writer(w);
            if o.include_headers {
                wr.write_record(&names)?;
            }
            for row in &r.rows {
                wr.write_record(cols.iter().map(|&i| cell(&row[i], o)))?;
            }
            wr.flush()?;
        }
        ExportFormat::Json => {
            let arr: Vec<serde_json::Value> = r
                .rows
                .iter()
                .map(|row| {
                    let mut m = serde_json::Map::new();
                    for &i in &cols {
                        m.insert(r.columns[i].name.clone(), row[i].to_json());
                    }
                    serde_json::Value::Object(m)
                })
                .collect();
            let name = if o.table.name.is_empty() { "RECORDS".to_string() } else { o.table.name.clone() };
            let mut root = serde_json::Map::new();
            root.insert(name, serde_json::Value::Array(arr));
            serde_json::to_writer_pretty(&mut *w, &serde_json::Value::Object(root))?;
        }
        ExportFormat::Xml => {
            write!(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>{nl}<RECORDS>{nl}")?;
            for row in &r.rows {
                write!(w, "  <RECORD>{nl}")?;
                for &i in &cols {
                    let tag = xml_tag(&r.columns[i].name);
                    if row[i].is_null() {
                        write!(w, "    <{tag}/>{nl}")?;
                    } else {
                        write!(w, "    <{tag}>{}</{tag}>{nl}", xml_escape(&row[i].edit_text()))?;
                    }
                }
                write!(w, "  </RECORD>{nl}")?;
            }
            write!(w, "</RECORDS>{nl}")?;
        }
        ExportFormat::Html => {
            write!(
                w,
                "<!DOCTYPE html>{nl}<html><head><meta charset=\"utf-8\"><title>{}</title>\
                 <style>table{{border-collapse:collapse;font-family:sans-serif;font-size:13px}}th,td{{border:1px solid #ccc;padding:4px 8px}}th{{background:#eee}}</style>\
                 </head><body>{nl}<table>{nl}",
                xml_escape(&o.table.name)
            )?;
            if o.include_headers {
                write!(w, "<tr>{}</tr>{nl}", names.iter().map(|n| format!("<th>{}</th>", xml_escape(n))).collect::<String>())?;
            }
            for row in &r.rows {
                write!(
                    w,
                    "<tr>{}</tr>{nl}",
                    cols.iter().map(|&i| format!("<td>{}</td>", xml_escape(&cell(&row[i], o)))).collect::<String>()
                )?;
            }
            write!(w, "</table>{nl}</body></html>{nl}")?;
        }
        ExportFormat::Markdown => {
            write!(w, "| {} |{nl}", names.join(" | "))?;
            write!(w, "|{}{nl}", names.iter().map(|_| " --- |").collect::<String>())?;
            for row in &r.rows {
                write!(
                    w,
                    "| {} |{nl}",
                    cols.iter().map(|&i| cell(&row[i], o).replace('|', "\\|").replace('\n', " ")).collect::<Vec<_>>().join(" | ")
                )?;
            }
        }
        ExportFormat::SqlInsert => {
            let names_owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let per = o.rows_per_insert.max(1);
            for chunk in r.rows.chunks(per) {
                let rows: Vec<Vec<Value>> = chunk.iter().map(|row| cols.iter().map(|&i| row[i].clone()).collect()).collect();
                write!(w, "{};{nl}", crate::dml::insert_rows(o.dialect, &o.table, &names_owned, &rows))?;
            }
        }
        ExportFormat::Excel => {
            // SpreadsheetML 2003 – opens in Excel/Numbers/LibreOffice without extra crates
            write!(
                w,
                "<?xml version=\"1.0\"?>{nl}<?mso-application progid=\"Excel.Sheet\"?>{nl}\
                 <Workbook xmlns=\"urn:schemas-microsoft-com:office:spreadsheet\" xmlns:ss=\"urn:schemas-microsoft-com:office:spreadsheet\">{nl}\
                 <Worksheet ss:Name=\"{}\"><Table>{nl}",
                xml_escape(if o.table.name.is_empty() { "Sheet1" } else { &o.table.name })
            )?;
            if o.include_headers {
                write!(
                    w,
                    "<Row>{}</Row>{nl}",
                    names.iter().map(|n| format!("<Cell><Data ss:Type=\"String\">{}</Data></Cell>", xml_escape(n))).collect::<String>()
                )?;
            }
            for row in &r.rows {
                let cells: String = cols
                    .iter()
                    .map(|&i| {
                        let v = &row[i];
                        if v.is_numeric() {
                            format!("<Cell><Data ss:Type=\"Number\">{}</Data></Cell>", v)
                        } else {
                            format!("<Cell><Data ss:Type=\"String\">{}</Data></Cell>", xml_escape(&cell(v, o)))
                        }
                    })
                    .collect();
                write!(w, "<Row>{}</Row>{nl}", cells)?;
            }
            write!(w, "</Table></Worksheet>{nl}</Workbook>{nl}")?;
        }
    }
    Ok(())
}

fn xml_tag(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() || s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        s.insert(0, '_');
    }
    s
}

pub fn export_to_file(r: &QueryResult, fmt: ExportFormat, o: &ExportOptions, path: &std::path::Path) -> Result<()> {
    let f = std::fs::OpenOptions::new().create(true).write(true).append(o.append).truncate(!o.append).open(path)?;
    let mut w = std::io::BufWriter::new(f);
    export(r, fmt, o, &mut w)?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ColumnMeta;

    fn sample() -> QueryResult {
        QueryResult {
            columns: vec![ColumnMeta::new("id"), ColumnMeta::new("name")],
            rows: vec![vec![Value::Int(1), Value::Text("a,b".into())], vec![Value::Int(2), Value::Null]],
            is_query: true,
            ..Default::default()
        }
    }

    #[test]
    fn csv_quotes() {
        let s = export_to_string(&sample(), ExportFormat::Csv, &ExportOptions::default()).unwrap();
        assert_eq!(s, "id,name\n1,\"a,b\"\n2,\n");
    }

    #[test]
    fn json_shape() {
        let o = ExportOptions { table: ObjPath::new("", None, "t"), ..Default::default() };
        let s = export_to_string(&sample(), ExportFormat::Json, &o).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["t"][0]["id"], 1);
        assert!(v["t"][1]["name"].is_null());
    }

    #[test]
    fn sql_insert() {
        let o = ExportOptions { table: ObjPath::new("d", None, "t"), ..Default::default() };
        let s = export_to_string(&sample(), ExportFormat::SqlInsert, &o).unwrap();
        assert!(s.starts_with("INSERT INTO `d`.`t` (`id`, `name`) VALUES\n(1, 'a,b'),\n(2, NULL);"));
    }

    #[test]
    fn markdown_and_selected_columns() {
        let o = ExportOptions { columns: vec!["name".into()], ..Default::default() };
        let s = export_to_string(&sample(), ExportFormat::Markdown, &o).unwrap();
        assert_eq!(s, "| name |\n| --- |\n| a,b |\n|  |\n");
    }
}
