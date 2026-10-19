use serde::{Deserialize, Serialize};
use std::fmt;

use crate::dialect::Dialect;

/// A single cell value returned by (or sent to) a database.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::UInt(_) | Value::Float(_))
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::UInt(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// Text shown in a grid cell. NULL is rendered as `(Null)` like most GUI clients.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "(Null)".into(),
            Value::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => s.to_string(),
                Err(_) => format!("(BLOB) {} bytes", b.len()),
            },
            other => other.to_string(),
        }
    }

    /// Text used when editing a cell (NULL edits as empty string).
    pub fn edit_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => s.to_string(),
                Err(_) => hex(b),
            },
            other => other.to_string(),
        }
    }

    pub fn to_sql_literal(&self, d: Dialect) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bool(b) => match d {
                Dialect::Postgres | Dialect::Generic => if *b { "TRUE" } else { "FALSE" }.into(),
                _ => if *b { "1" } else { "0" }.into(),
            },
            Value::Int(v) => v.to_string(),
            Value::UInt(v) => v.to_string(),
            Value::Float(v) => {
                if v.is_finite() {
                    v.to_string()
                } else {
                    "NULL".into()
                }
            }
            Value::Text(s) => d.string_literal(s),
            Value::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => d.string_literal(s),
                Err(_) => d.blob_literal(b),
            },
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => (*b).into(),
            Value::Int(v) => (*v).into(),
            Value::UInt(v) => (*v).into(),
            Value::Float(v) => serde_json::Number::from_f64(*v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Text(s) => s.clone().into(),
            Value::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => s.into(),
                Err(_) => hex(b).into(),
            },
        }
    }

    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Value::UInt(u)
                } else {
                    Value::Float(n.as_f64().unwrap_or_default())
                }
            }
            serde_json::Value::String(s) => Value::Text(s.clone()),
            other => Value::Text(other.to_string()),
        }
    }

    /// Ordering used for client-side sorting of result grids.
    pub fn cmp_loose(&self, other: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) if self.is_numeric() || other.is_numeric() => {
                    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
                }
                _ => self.display().cmp(&other.display()),
            },
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(v) => write!(f, "{}", v),
            Value::UInt(v) => write!(f, "{}", v),
            Value::Float(v) => write!(f, "{}", v),
            Value::Text(s) => write!(f, "{}", s),
            Value::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02X}", x)).collect()
}

/// Inverse of [`hex`]; `None` for odd length or non-hex input.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), type_name: String::new() }
    }
}

/// Result of a single executed statement.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryResult {
    pub sql: String,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub affected_rows: u64,
    pub elapsed_ms: u128,
    /// True when the statement produced a result set (even an empty one).
    pub is_query: bool,
}

impl QueryResult {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn get(&self, row: usize, col: &str) -> Option<&Value> {
        let i = self.column_index(col)?;
        self.rows.get(row).and_then(|r| r.get(i))
    }

    pub fn text(&self, row: usize, col: usize) -> String {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(|v| if v.is_null() { String::new() } else { v.to_string() })
            .unwrap_or_default()
    }

    pub fn summary(&self) -> String {
        if self.is_query {
            format!("{} row(s) returned, {} ms", self.rows.len(), self.elapsed_ms)
        } else {
            format!("Affected rows: {}, {} ms", self.affected_rows, self.elapsed_ms)
        }
    }
}
