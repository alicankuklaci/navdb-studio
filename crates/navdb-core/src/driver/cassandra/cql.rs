//! CQL helpers: type parsing, value conversion, typed literals, statement
//! splitting and client-side evaluation of predicates CQL cannot run.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::value::{CqlDecimal, CqlDuration, CqlValue};
use serde_json::Value as J;

use crate::sqlmini::{CmpOp, Expr};
use crate::value::{hex, unhex, Value};

/// Parsed CQL type, e.g. `map<text, frozen<list<int>>>` (frozen is dropped).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Ty {
    pub name: String,
    pub args: Vec<Ty>,
}

/// User-defined types of a keyspace: name -> fields.
pub type Udts = HashMap<String, Vec<(String, Ty)>>;

pub fn parse_type(s: &str) -> Ty {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    parse_ty(&c, &mut i)
}

fn parse_ty(c: &[char], i: &mut usize) -> Ty {
    let ws = |i: &mut usize| {
        while *i < c.len() && c[*i].is_whitespace() {
            *i += 1;
        }
    };
    ws(i);
    let mut name = String::new();
    if *i < c.len() && c[*i] == '"' {
        *i += 1;
        while *i < c.len() {
            if c[*i] == '"' {
                if *i + 1 < c.len() && c[*i + 1] == '"' {
                    name.push('"');
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            name.push(c[*i]);
            *i += 1;
        }
    } else {
        while *i < c.len() && !matches!(c[*i], '<' | '>' | ',') && !c[*i].is_whitespace() {
            name.push(c[*i]);
            *i += 1;
        }
        name = name.to_lowercase();
    }
    ws(i);
    let mut args = vec![];
    if *i < c.len() && c[*i] == '<' {
        *i += 1;
        loop {
            args.push(parse_ty(c, i));
            ws(i);
            if *i < c.len() && c[*i] == ',' {
                *i += 1;
                continue;
            }
            if *i < c.len() && c[*i] == '>' {
                *i += 1;
            }
            break;
        }
    }
    if name == "frozen" && args.len() == 1 {
        return args.remove(0);
    }
    Ty { name, args }
}

#[allow(unreachable_patterns)]
pub fn type_name(t: &ColumnType) -> String {
    match t {
        ColumnType::Native(n) => match n {
            NativeType::Ascii => "ascii",
            NativeType::Boolean => "boolean",
            NativeType::Blob => "blob",
            NativeType::Counter => "counter",
            NativeType::Date => "date",
            NativeType::Decimal => "decimal",
            NativeType::Double => "double",
            NativeType::Duration => "duration",
            NativeType::Float => "float",
            NativeType::Int => "int",
            NativeType::BigInt => "bigint",
            NativeType::Text => "text",
            NativeType::Timestamp => "timestamp",
            NativeType::Inet => "inet",
            NativeType::SmallInt => "smallint",
            NativeType::TinyInt => "tinyint",
            NativeType::Time => "time",
            NativeType::Timeuuid => "timeuuid",
            NativeType::Uuid => "uuid",
            NativeType::Varint => "varint",
            _ => "unknown",
        }
        .to_string(),
        ColumnType::Collection { frozen, typ } => {
            let s = match typ {
                CollectionType::List(a) => format!("list<{}>", type_name(a)),
                CollectionType::Set(a) => format!("set<{}>", type_name(a)),
                CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
                _ => "collection".to_string(),
            };
            if *frozen {
                format!("frozen<{}>", s)
            } else {
                s
            }
        }
        ColumnType::Vector { typ, dimensions } => format!("vector<{}, {}>", type_name(typ), dimensions),
        ColumnType::UserDefinedType { frozen, definition } => {
            if *frozen {
                format!("frozen<{}>", definition.name)
            } else {
                definition.name.to_string()
            }
        }
        ColumnType::Tuple(v) => format!("tuple<{}>", v.iter().map(type_name).collect::<Vec<_>>().join(", ")),
        _ => "unknown".to_string(),
    }
}

// ---------------------------------------------------------------- values

pub fn to_value(v: CqlValue) -> Value {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Value::Text(s),
        CqlValue::Boolean(b) => Value::Bool(b),
        CqlValue::Blob(b) => Value::Bytes(b),
        CqlValue::Counter(c) => Value::Int(c.0),
        CqlValue::Double(f) => Value::Float(f),
        CqlValue::Float(f) => Value::Float(f.to_string().parse().unwrap_or(f as f64)),
        CqlValue::Int(i) => Value::Int(i as i64),
        CqlValue::BigInt(i) => Value::Int(i),
        CqlValue::SmallInt(i) => Value::Int(i as i64),
        CqlValue::TinyInt(i) => Value::Int(i as i64),
        CqlValue::Empty => Value::Text(String::new()),
        v @ (CqlValue::List(_)
        | CqlValue::Set(_)
        | CqlValue::Map(_)
        | CqlValue::Tuple(_)
        | CqlValue::UserDefinedType { .. }
        | CqlValue::Vector(_)) => Value::Text(to_json(&v).to_string()),
        other => Value::Text(scalar_text(&other)),
    }
}

/// Canonical text of a scalar CQL value.
pub fn scalar_text(v: &CqlValue) -> String {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => s.clone(),
        CqlValue::Boolean(b) => b.to_string(),
        CqlValue::Blob(b) => format!("0x{}", hex(b).to_lowercase()),
        CqlValue::Counter(c) => c.0.to_string(),
        CqlValue::Decimal(d) => decimal_str(d),
        CqlValue::Date(d) => {
            let days = d.0 as i64 - (1i64 << 31);
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .and_then(|e| e.checked_add_signed(chrono::Duration::days(days)))
                .map(|x| x.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| days.to_string())
        }
        CqlValue::Double(f) => f.to_string(),
        CqlValue::Float(f) => f.to_string(),
        CqlValue::Duration(d) => duration_str(d),
        CqlValue::Empty => String::new(),
        CqlValue::Int(i) => i.to_string(),
        CqlValue::BigInt(i) => i.to_string(),
        CqlValue::SmallInt(i) => i.to_string(),
        CqlValue::TinyInt(i) => i.to_string(),
        CqlValue::Timestamp(ts) => chrono::DateTime::from_timestamp_millis(ts.0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
            .unwrap_or_else(|| ts.0.to_string()),
        CqlValue::Inet(ip) => ip.to_string(),
        CqlValue::Time(t) => {
            let t = t.0;
            format!(
                "{:02}:{:02}:{:02}.{:09}",
                t / 3_600_000_000_000,
                t / 60_000_000_000 % 60,
                t / 1_000_000_000 % 60,
                t % 1_000_000_000
            )
        }
        CqlValue::Timeuuid(u) => u.to_string(),
        CqlValue::Uuid(u) => u.to_string(),
        CqlValue::Varint(v) => varint_str(v.as_signed_bytes_be_slice()),
        other => to_json(other).to_string(),
    }
}

pub fn to_json(v: &CqlValue) -> J {
    match v {
        CqlValue::Boolean(b) => J::Bool(*b),
        CqlValue::Int(i) => (*i).into(),
        CqlValue::BigInt(i) => (*i).into(),
        CqlValue::SmallInt(i) => (*i).into(),
        CqlValue::TinyInt(i) => (*i).into(),
        CqlValue::Counter(c) => c.0.into(),
        CqlValue::Double(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or_else(|| J::String(f.to_string())),
        CqlValue::Float(f) => {
            let f: f64 = f.to_string().parse().unwrap_or(*f as f64);
            serde_json::Number::from_f64(f).map(J::Number).unwrap_or_else(|| J::String(f.to_string()))
        }
        CqlValue::List(l) | CqlValue::Set(l) | CqlValue::Vector(l) => J::Array(l.iter().map(to_json).collect()),
        CqlValue::Tuple(t) => J::Array(t.iter().map(|x| x.as_ref().map(to_json).unwrap_or(J::Null)).collect()),
        CqlValue::Map(m) => {
            if m.iter().all(|(k, _)| !matches!(k, CqlValue::List(_) | CqlValue::Set(_) | CqlValue::Map(_) | CqlValue::Tuple(_) | CqlValue::UserDefinedType { .. })) {
                J::Object(m.iter().map(|(k, v)| (scalar_text(k), to_json(v))).collect())
            } else {
                J::Array(m.iter().map(|(k, v)| J::Array(vec![to_json(k), to_json(v)])).collect())
            }
        }
        CqlValue::UserDefinedType { fields, .. } => {
            J::Object(fields.iter().map(|(k, v)| (k.clone(), v.as_ref().map(to_json).unwrap_or(J::Null))).collect())
        }
        other => J::String(scalar_text(other)),
    }
}

/// Signed big-endian two's complement bytes -> decimal string.
pub fn varint_str(b: &[u8]) -> String {
    if b.is_empty() {
        return "0".into();
    }
    let neg = b[0] & 0x80 != 0;
    let mut mag: Vec<u8> = b.to_vec();
    if neg {
        for x in mag.iter_mut() {
            *x = !*x;
        }
        for x in mag.iter_mut().rev() {
            let (v, carry) = x.overflowing_add(1);
            *x = v;
            if !carry {
                break;
            }
        }
    }
    let mut digits = vec![];
    while mag.iter().any(|&x| x != 0) {
        let mut rem = 0u32;
        for x in mag.iter_mut() {
            let cur = (rem << 8) | *x as u32;
            *x = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    if neg {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

pub fn decimal_str(d: &CqlDecimal) -> String {
    let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
    scale_digits(&varint_str(bytes), scale)
}

fn scale_digits(unscaled: &str, scale: i32) -> String {
    let (sign, digits) = match unscaled.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", unscaled),
    };
    if scale <= 0 {
        if digits == "0" {
            return "0".into();
        }
        return format!("{}{}{}", sign, digits, "0".repeat((-scale) as usize));
    }
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{}", "0".repeat(scale + 1 - digits.len()), digits) } else { digits.to_string() };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{}.{}", sign, int, frac)
}

pub fn duration_str(d: &CqlDuration) -> String {
    let neg = d.months < 0 || d.days < 0 || d.nanoseconds < 0;
    let months = (d.months as i64).abs();
    let days = (d.days as i64).abs();
    let mut ns = d.nanoseconds.unsigned_abs();
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    let mut part = |v: u64, unit: &str| {
        if v > 0 {
            s.push_str(&format!("{}{}", v, unit));
        }
    };
    part(months as u64 / 12, "y");
    part(months as u64 % 12, "mo");
    part(days as u64, "d");
    for (unit, size) in [("h", 3_600_000_000_000u64), ("m", 60_000_000_000), ("s", 1_000_000_000), ("ms", 1_000_000), ("us", 1_000), ("ns", 1)] {
        part(ns / size, unit);
        ns %= size;
    }
    if s.is_empty() || s == "-" {
        return "0s".into();
    }
    s
}

// ---------------------------------------------------------------- literals

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

fn is_integer(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}

fn is_number(s: &str) -> bool {
    s.parse::<f64>().is_ok() && s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'))
}

fn float_lit(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        f.to_string()
    }
}

/// Parse a timestamp literal into milliseconds since the epoch (UTC when no zone is given).
pub fn parse_ts_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    for f in ["%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%z", "%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%d %H:%M:%S%.f %z"] {
        if let Ok(d) = chrono::DateTime::parse_from_str(s, f) {
            return Some(d.timestamp_millis());
        }
    }
    let s = s.trim_end_matches(['Z', 'z']);
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return Some(d.and_utc().timestamp_millis());
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| d.and_utc().timestamp_millis())
}

const SCALARS: &[&str] = &[
    "ascii", "text", "varchar", "tinyint", "smallint", "int", "bigint", "varint", "counter", "float", "double", "decimal",
    "boolean", "uuid", "timeuuid", "timestamp", "date", "time", "inet", "duration", "blob",
];

/// Render `v` as a CQL literal of type `ty`. An empty type name means "unknown":
/// numbers and booleans stay bare, everything else is quoted.
pub fn literal(v: &Value, ty: &Ty, udts: &Udts) -> Result<String> {
    if v.is_null() {
        return Ok("null".into());
    }
    let t = text_of(v);
    let tt = t.trim();
    Ok(match ty.name.as_str() {
        "" => match v {
            Value::Int(_) | Value::UInt(_) | Value::Bool(_) => v.to_string(),
            Value::Float(f) => float_lit(*f),
            Value::Bytes(b) if std::str::from_utf8(b).is_err() => format!("0x{}", hex(b)),
            _ => quote(&t),
        },
        "ascii" | "text" | "varchar" => quote(&t),
        "tinyint" | "smallint" | "int" | "bigint" | "varint" | "counter" => match v {
            Value::Int(_) | Value::UInt(_) => v.to_string(),
            Value::Float(f) if f.fract() == 0.0 && f.is_finite() => format!("{}", *f as i64),
            Value::Bool(b) => if *b { "1" } else { "0" }.into(),
            _ if is_integer(tt) => tt.to_string(),
            _ => bail!("invalid {} value '{}'", ty.name, t),
        },
        "float" | "double" | "decimal" => match v {
            Value::Int(_) | Value::UInt(_) => v.to_string(),
            Value::Float(f) => float_lit(*f),
            _ if is_number(tt) => tt.to_string(),
            _ if ["nan", "infinity", "-infinity"].contains(&tt.to_lowercase().as_str()) && ty.name != "decimal" => tt.to_string(),
            _ => bail!("invalid {} value '{}'", ty.name, t),
        },
        "boolean" => match v {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => (*i != 0).to_string(),
            _ => match tt.to_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "y" => "true".into(),
                "false" | "f" | "0" | "no" | "n" => "false".into(),
                _ => bail!("invalid boolean value '{}'", t),
            },
        },
        "uuid" | "timeuuid" => uuid::Uuid::parse_str(tt).map(|u| u.to_string()).map_err(|_| anyhow!("invalid {} value '{}'", ty.name, t))?,
        "timestamp" => match v {
            Value::Int(i) => i.to_string(),
            _ => parse_ts_millis(tt).map(|m| m.to_string()).unwrap_or_else(|| quote(tt)),
        },
        "date" | "time" | "inet" => match v {
            Value::Int(_) if ty.name == "date" => v.to_string(),
            _ => quote(tt),
        },
        "duration" => {
            if !tt.is_empty() && tt.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | 'µ' | ':' | '.')) {
                tt.to_string()
            } else {
                bail!("invalid duration value '{}'", t)
            }
        }
        "blob" => match v {
            Value::Bytes(b) => format!("0x{}", hex(b)),
            _ => match tt.strip_prefix("0x").or_else(|| tt.strip_prefix("0X")) {
                Some(h) if unhex(h).is_some() => format!("0x{}", h),
                _ => format!("0x{}", hex(t.as_bytes())),
            },
        },
        "list" | "set" | "map" | "tuple" | "vector" => {
            let j: J = serde_json::from_str(tt).map_err(|_| anyhow!("invalid {} value '{}' (expected JSON)", ty.name, t))?;
            json_literal(&j, ty, udts)?
        }
        _ => {
            if tt.starts_with('{') {
                if let Ok(j) = serde_json::from_str::<J>(tt) {
                    return json_literal(&j, ty, udts);
                }
            }
            quote(&t)
        }
    })
}

fn json_literal(j: &J, ty: &Ty, udts: &Udts) -> Result<String> {
    let unknown = Ty::default();
    let arg = |i: usize| ty.args.get(i).unwrap_or(&unknown);
    let join = |items: Vec<String>| items.join(", ");
    Ok(match (ty.name.as_str(), j) {
        (_, J::Null) => "null".into(),
        ("list" | "vector", J::Array(a)) => format!("[{}]", join(a.iter().map(|x| json_literal(x, arg(0), udts)).collect::<Result<_>>()?)),
        ("set", J::Array(a)) => format!("{{{}}}", join(a.iter().map(|x| json_literal(x, arg(0), udts)).collect::<Result<_>>()?)),
        ("map", J::Object(o)) => format!(
            "{{{}}}",
            join(o.iter().map(|(k, v)| Ok(format!("{}: {}", literal(&Value::Text(k.clone()), arg(0), udts)?, json_literal(v, arg(1), udts)?))).collect::<Result<_>>()?)
        ),
        ("map", J::Array(a)) => {
            let mut items = vec![];
            for p in a {
                match p {
                    J::Array(kv) if kv.len() == 2 => items.push(format!("{}: {}", json_literal(&kv[0], arg(0), udts)?, json_literal(&kv[1], arg(1), udts)?)),
                    _ => bail!("invalid map entry {}", p),
                }
            }
            format!("{{{}}}", join(items))
        }
        ("tuple", J::Array(a)) => {
            format!("({})", join(a.iter().enumerate().map(|(i, x)| json_literal(x, arg(i), udts)).collect::<Result<_>>()?))
        }
        ("", J::Array(a)) => format!("[{}]", join(a.iter().map(|x| json_literal(x, &unknown, udts)).collect::<Result<_>>()?)),
        (name, J::Object(o)) if !SCALARS.contains(&name) && !matches!(name, "list" | "set" | "map" | "tuple" | "vector") => {
            let fields = udts.get(name);
            let mut items = vec![];
            for (k, v) in o {
                let fty = fields.and_then(|f| f.iter().find(|(n, _)| n == k)).map(|(_, t)| t).unwrap_or(&unknown);
                items.push(format!("{}: {}", quote_ident(k), json_literal(v, fty, udts)?));
            }
            format!("{{{}}}", join(items))
        }
        (_, J::Array(_) | J::Object(_)) if SCALARS.contains(&ty.name.as_str()) => literal(&Value::Text(j.to_string()), ty, udts)?,
        (_, J::Array(_) | J::Object(_)) => bail!("value {} does not match type {}", j, ty.name),
        _ => literal(&Value::from_json(j), ty, udts)?,
    })
}

// ---------------------------------------------------------------- statements

/// Split a CQL script on `;`, honouring strings, quoted identifiers, `$$` bodies,
/// comments and `BEGIN BATCH ... APPLY BATCH` blocks.
pub fn split(sql: &str) -> Vec<String> {
    let c: Vec<char> = sql.chars().collect();
    let mut parts = vec![];
    let mut cur = String::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch == '\'' || ch == '"' {
            cur.push(ch);
            i += 1;
            while i < c.len() {
                cur.push(c[i]);
                if c[i] == ch {
                    if c.get(i + 1) == Some(&ch) {
                        cur.push(ch);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if ch == '$' && next == Some('$') {
            cur.push_str("$$");
            i += 2;
            while i < c.len() && !(c[i] == '$' && c.get(i + 1) == Some(&'$')) {
                cur.push(c[i]);
                i += 1;
            }
            if i < c.len() {
                cur.push_str("$$");
                i += 2;
            }
            continue;
        }
        if (ch == '-' && next == Some('-')) || (ch == '/' && next == Some('/')) {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && next == Some('*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
            continue;
        }
        if ch == ';' {
            parts.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    parts.push(cur);
    let mut out: Vec<String> = vec![];
    let mut batch: Option<String> = None;
    for p in parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        let up = p.to_uppercase();
        let ends = up.split_whitespace().collect::<Vec<_>>().ends_with(&["APPLY", "BATCH"]);
        match batch.as_mut() {
            Some(b) => {
                b.push_str(";\n");
                b.push_str(&p);
                if ends {
                    out.extend(batch.take());
                }
            }
            None => {
                let words: Vec<&str> = up.split_whitespace().take(3).collect();
                if words.first() == Some(&"BEGIN") && words.contains(&"BATCH") && !ends {
                    batch = Some(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out.extend(batch);
    out
}

/// `USE ks` / `USE "Ks"` -> (name, case_sensitive).
pub fn parse_use(stmt: &str) -> Option<(String, bool)> {
    let s = stmt.trim();
    if s.len() < 4 || !s[..4].eq_ignore_ascii_case("use ") {
        return None;
    }
    let name = s[4..].trim();
    if let Some(q) = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        return Some((q.replace("\"\"", "\""), true));
    }
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_lowercase(), false))
}

// ---------------------------------------------------------------- filters

/// Top-level AND conjuncts of a filter.
pub fn conjuncts(e: Option<Expr>) -> Vec<Expr> {
    match e {
        None => vec![],
        Some(Expr::And(v)) => v.into_iter().flat_map(|x| conjuncts(Some(x))).collect(),
        Some(x) => vec![x],
    }
}

/// Whether CQL can evaluate the predicate (possibly with ALLOW FILTERING).
pub fn server_side(e: &Expr) -> bool {
    match e {
        Expr::Cmp { op, .. } => *op != CmpOp::Ne,
        Expr::In { negated, .. } => !negated,
        Expr::Between { .. } => true,
        _ => false,
    }
}

pub fn columns_of(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::And(v) | Expr::Or(v) => v.iter().for_each(|x| columns_of(x, out)),
        Expr::Not(x) => columns_of(x, out),
        Expr::Cmp { column, .. } | Expr::Like { column, .. } | Expr::In { column, .. } | Expr::Between { column, .. } | Expr::IsNull { column, .. } => {
            out.push(column.clone())
        }
    }
}

pub fn op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

/// CQL text of a server-side predicate.
pub fn render_pred(e: &Expr, ty: &dyn Fn(&str) -> Ty, udts: &Udts) -> Result<String> {
    Ok(match e {
        Expr::Cmp { column, op, value } => format!("{} {} {}", quote_ident(column), op_str(*op), literal(value, &ty(column), udts)?),
        Expr::In { column, values, .. } => format!(
            "{} IN ({})",
            quote_ident(column),
            values.iter().map(|v| literal(v, &ty(column), udts)).collect::<Result<Vec<_>>>()?.join(", ")
        ),
        Expr::Between { column, low, high } => {
            let t = ty(column);
            format!("{0} >= {1} AND {0} <= {2}", quote_ident(column), literal(low, &t, udts)?, literal(high, &t, udts)?)
        }
        _ => bail!("predicate cannot be evaluated by CQL"),
    })
}

fn same(a: &Value, b: &Value) -> bool {
    a.cmp_loose(b) == std::cmp::Ordering::Equal || a.display().eq_ignore_ascii_case(&b.display())
}

/// Evaluate a predicate against a row (`get` looks up a column value).
pub fn eval(e: &Expr, get: &dyn Fn(&str) -> Option<Value>) -> bool {
    use std::cmp::Ordering::*;
    let val = |c: &str| get(c).filter(|v| !v.is_null());
    match e {
        Expr::And(v) => v.iter().all(|x| eval(x, get)),
        Expr::Or(v) => v.iter().any(|x| eval(x, get)),
        Expr::Not(x) => !eval(x, get),
        Expr::Cmp { column, op, value } => match val(column) {
            None => false,
            Some(v) => {
                let o = v.cmp_loose(value);
                match op {
                    CmpOp::Eq => same(&v, value),
                    CmpOp::Ne => !same(&v, value),
                    CmpOp::Lt => o == Less,
                    CmpOp::Le => o != Greater,
                    CmpOp::Gt => o == Greater,
                    CmpOp::Ge => o != Less,
                }
            }
        },
        Expr::Like { column, pattern, negated } => match val(column) {
            None => false,
            Some(v) => like_match(pattern, &v.display()) != *negated,
        },
        Expr::In { column, values, negated } => match val(column) {
            None => false,
            Some(v) => values.iter().any(|x| same(&v, x)) != *negated,
        },
        Expr::Between { column, low, high } => match val(column) {
            None => false,
            Some(v) => v.cmp_loose(low) != Less && v.cmp_loose(high) != Greater,
        },
        Expr::IsNull { column, negated } => val(column).is_none() != *negated,
    }
}

/// SQL LIKE matching (`%` any run, `_` one char), case-sensitive.
pub fn like_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || (p[pi] != '%' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> Ty {
        parse_type(s)
    }

    #[test]
    fn types() {
        let t = ty("frozen<map<text, frozen<list<int>>>>");
        assert_eq!(t.name, "map");
        assert_eq!(t.args[0].name, "text");
        assert_eq!(t.args[1], Ty { name: "list".into(), args: vec![Ty { name: "int".into(), args: vec![] }] });
        assert_eq!(ty("\"MyType\"").name, "MyType");
        assert_eq!(ty("vector<float, 3>").args.len(), 2);
    }

    #[test]
    fn varints() {
        assert_eq!(varint_str(&[0x00]), "0");
        assert_eq!(varint_str(&[0xff]), "-1");
        assert_eq!(varint_str(&[0x80]), "-128");
        assert_eq!(varint_str(&[0x01, 0x00]), "256");
        assert_eq!(varint_str(&[0x00, 0x8a, 0xc7, 0x23, 0x04, 0x89, 0xe8, 0x00, 0x00]), "10000000000000000000");
        assert_eq!(scale_digits("12345", 2), "123.45");
        assert_eq!(scale_digits("-5", 3), "-0.005");
        assert_eq!(scale_digits("7", -2), "700");
        assert_eq!(duration_str(&CqlDuration { months: 14, days: 3, nanoseconds: 3_723_000_000_000 }), "1y2mo3d1h2m3s");
    }

    #[test]
    fn literals() {
        let u = Udts::new();
        let l = |v: Value, t: &str| literal(&v, &ty(t), &u).unwrap();
        assert_eq!(l(Value::Int(42), "text"), "'42'");
        assert_eq!(l(Value::Text("it's".into()), "varchar"), "'it''s'");
        assert_eq!(l(Value::Text("7".into()), "int"), "7");
        assert!(literal(&Value::Text("x".into()), &ty("int"), &u).is_err());
        assert_eq!(l(Value::Text("TRUE".into()), "boolean"), "true");
        assert_eq!(l(Value::Text("123E4567-E89B-12D3-A456-426614174000".into()), "uuid"), "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(l(Value::Text("2024-01-02T03:04:05.006Z".into()), "timestamp"), "1704164645006");
        assert_eq!(l(Value::Text("2024-01-02".into()), "date"), "'2024-01-02'");
        assert_eq!(l(Value::Text("10.0.0.1".into()), "inet"), "'10.0.0.1'");
        assert_eq!(l(Value::Bytes(vec![1, 0xab]), "blob"), "0x01AB");
        assert_eq!(l(Value::Text("0xcafe".into()), "blob"), "0xcafe");
        assert_eq!(l(Value::Text("[\"a\",\"b\"]".into()), "list<text>"), "['a', 'b']");
        assert_eq!(l(Value::Text("[1,2]".into()), "frozen<set<int>>"), "{1, 2}");
        assert_eq!(l(Value::Text("{\"1\":\"x\"}".into()), "map<int, text>"), "{1: 'x'}");
        assert_eq!(l(Value::Text("[1,\"a\"]".into()), "tuple<int, text>"), "(1, 'a')");
        assert_eq!(l(Value::Null, "int"), "null");
        let mut udts = Udts::new();
        udts.insert("addr".into(), vec![("zip".into(), ty("int")), ("city".into(), ty("text"))]);
        assert_eq!(literal(&Value::Text("{\"city\":\"X\",\"zip\":\"5\"}".into()), &ty("frozen<addr>"), &udts).unwrap(), "{\"city\": 'X', \"zip\": 5}");
    }

    #[test]
    fn json_values() {
        let v = CqlValue::Map(vec![(CqlValue::Int(1), CqlValue::List(vec![CqlValue::Text("a".into())]))]);
        assert_eq!(to_value(v), Value::Text("{\"1\":[\"a\"]}".into()));
        let ts = CqlValue::Timestamp(scylla::value::CqlTimestamp(1704164645006));
        assert_eq!(to_value(ts), Value::Text("2024-01-02T03:04:05.006Z".into()));
        assert_eq!(to_value(CqlValue::Date(scylla::value::CqlDate((1u32 << 31) + 1))), Value::Text("1970-01-02".into()));
        assert_eq!(to_value(CqlValue::Float(1.1)), Value::Float(1.1));
    }

    #[test]
    fn splitting() {
        let s = split("USE ks; -- c;\nCREATE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$;\nBEGIN BATCH INSERT INTO t (a) VALUES ('x;y'); INSERT INTO t (a) VALUES (2); APPLY BATCH; SELECT 1");
        assert_eq!(s.len(), 4, "{:?}", s);
        assert!(s[1].contains("return 1;"));
        assert!(s[2].starts_with("BEGIN BATCH") && s[2].ends_with("APPLY BATCH"));
        assert_eq!(parse_use("use MyKs"), Some(("myks".into(), false)));
        assert_eq!(parse_use("USE \"MyKs\""), Some(("MyKs".into(), true)));
        assert_eq!(parse_use("user"), None);
    }

    #[test]
    fn client_eval() {
        let row = |c: &str| match c {
            "name" => Some(Value::Text("alpha".into())),
            "n" => Some(Value::Int(5)),
            _ => Some(Value::Null),
        };
        assert!(eval(&Expr::Like { column: "name".into(), pattern: "%ph_".into(), negated: false }, &row));
        assert!(!eval(&Expr::Like { column: "name".into(), pattern: "b%".into(), negated: false }, &row));
        assert!(eval(&Expr::Cmp { column: "n".into(), op: CmpOp::Ne, value: Value::Int(4) }, &row));
        assert!(eval(&Expr::IsNull { column: "z".into(), negated: false }, &row));
        assert!(eval(&Expr::Between { column: "n".into(), low: Value::Int(1), high: Value::Text("5".into()) }, &row));
        assert!(like_match("a%c", "abbc") && !like_match("a%c", "abcd") && like_match("%", ""));
    }
}
