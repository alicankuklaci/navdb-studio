//! Parser for the subset of mongo shell syntax the query editor accepts:
//! `db.coll.method(args).modifier(args)`, `db.method(args)`, `use x`,
//! `show dbs`, plain SQL statements (passed through), separated by `;` or
//! newlines. Arguments are relaxed JSON with the usual shell helpers
//! (`ObjectId()`, `ISODate()`, `NumberLong()`, regex literals, ...).

use anyhow::{anyhow, bail, Result};
use mongodb::bson::{self, oid::ObjectId, Bson, Document};

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Use(String),
    Show(String),
    /// A SQL statement (SELECT / INSERT / UPDATE / DELETE) handled by `sqlmini`.
    Sql(String),
    Chain { db: Option<String>, coll: Option<String>, calls: Vec<Call> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Vec<Bson>,
}

enum Elem {
    Prop(String),
    Call(Call),
}

struct P {
    c: Vec<char>,
    i: usize,
}

const SQL_WORDS: &[&str] = &["select", "insert", "update", "delete"];

/// Parse a whole script into statements, each with its source text.
pub fn parse_script(text: &str) -> Result<Vec<(String, Stmt)>> {
    let mut p = P { c: text.chars().collect(), i: 0 };
    let mut out = vec![];
    loop {
        p.skip_ws();
        while p.eat(';') {
            p.skip_ws();
        }
        if p.eof() {
            break;
        }
        let start = p.i;
        let word = p.peek_word();
        let lw = word.to_ascii_lowercase();
        let stmt = if (lw == "use" || lw == "show") && p.c.get(p.i + word.len()).is_some_and(|c| c.is_whitespace()) {
            p.i += word.len();
            let mut arg = String::new();
            while let Some(&ch) = p.c.get(p.i) {
                if ch == '\n' || ch == ';' {
                    break;
                }
                arg.push(ch);
                p.i += 1;
            }
            let arg = arg.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            if arg.is_empty() {
                bail!("{} needs an argument", lw);
            }
            if lw == "use" {
                Stmt::Use(arg)
            } else {
                Stmt::Show(arg.to_ascii_lowercase())
            }
        } else if SQL_WORDS.contains(&lw.as_str()) {
            Stmt::Sql(p.sql_text())
        } else if word == "db" {
            p.i += 2;
            p.chain()?
        } else {
            bail!("unsupported statement near '{}' (expected db.<collection>.<method>(...), use, show or SQL)", p.snippet());
        };
        let src: String = p.c[start..p.i].iter().collect();
        out.push((src.trim().to_string(), stmt));
    }
    Ok(out)
}

/// Parse one relaxed-JSON value.
pub fn parse_value(text: &str) -> Result<Bson> {
    let mut p = P { c: text.chars().collect(), i: 0 };
    let v = p.value()?;
    p.skip_ws();
    if !p.eof() {
        bail!("unexpected text near '{}'", p.snippet());
    }
    Ok(v)
}

impl P {
    fn eof(&self) -> bool {
        self.i >= self.c.len()
    }

    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        self.skip_ws();
        if self.eat(ch) {
            Ok(())
        } else {
            Err(anyhow!("expected '{}' near '{}'", ch, self.snippet()))
        }
    }

    fn snippet(&self) -> String {
        let s: String = self.c[self.i.min(self.c.len())..].iter().take(30).collect();
        if s.is_empty() {
            "end of input".into()
        } else {
            s
        }
    }

    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(ch) if ch.is_whitespace() => self.i += 1,
                Some('/') if self.c.get(self.i + 1) == Some(&'/') => {
                    while let Some(ch) = self.peek() {
                        if ch == '\n' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                Some('/') if self.c.get(self.i + 1) == Some(&'*') => {
                    self.i += 2;
                    while !self.eof() && !(self.peek() == Some('*') && self.c.get(self.i + 1) == Some(&'/')) {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.c.len());
                }
                _ => break,
            }
        }
    }

    fn peek_word(&self) -> String {
        self.c[self.i..].iter().take_while(|c| c.is_alphanumeric() || **c == '_' || **c == '$').collect()
    }

    fn ident(&mut self) -> Result<String> {
        self.skip_ws();
        let w = self.peek_word();
        if w.is_empty() {
            bail!("expected a name near '{}'", self.snippet());
        }
        self.i += w.chars().count();
        Ok(w)
    }

    /// SQL text up to an unquoted `;` or the end of input.
    fn sql_text(&mut self) -> String {
        let mut s = String::new();
        let mut quote: Option<char> = None;
        while let Some(ch) = self.peek() {
            match quote {
                Some(q) if ch == q => quote = None,
                Some(_) => {}
                None if ch == '\'' || ch == '"' || ch == '`' => quote = Some(ch),
                None if ch == ';' => break,
                None => {}
            }
            s.push(ch);
            self.i += 1;
        }
        s.trim().to_string()
    }

    fn chain(&mut self) -> Result<Stmt> {
        let mut elems = vec![];
        loop {
            let save = self.i;
            self.skip_ws();
            let name = if self.eat('.') {
                self.ident()?
            } else if self.eat('[') {
                self.skip_ws();
                let n = match self.value()? {
                    Bson::String(s) => s,
                    other => bail!("expected a collection name, got {}", other),
                };
                self.expect(']')?;
                elems.push(Elem::Prop(n));
                continue;
            } else {
                self.i = save;
                break;
            };
            let save2 = self.i;
            self.skip_ws();
            if self.peek() == Some('(') {
                let args = self.args()?;
                elems.push(Elem::Call(Call { name, args }));
            } else {
                self.i = save2;
                elems.push(Elem::Prop(name));
            }
        }
        let mut db = None;
        let mut coll: Option<String> = None;
        let mut parts: Vec<String> = vec![];
        let mut calls = vec![];
        for e in elems {
            match e {
                Elem::Prop(p) if calls.is_empty() => parts.push(p),
                Elem::Prop(p) => bail!("unexpected property '{}' after a method call", p),
                Elem::Call(c) if calls.is_empty() && parts.is_empty() && coll.is_none() && matches!(c.name.as_str(), "getSiblingDB" | "getDB") => {
                    db = Some(str_arg(&c.args, 0)?);
                }
                Elem::Call(c) if calls.is_empty() && parts.is_empty() && coll.is_none() && c.name == "getCollection" => {
                    coll = Some(str_arg(&c.args, 0)?);
                }
                Elem::Call(c) => {
                    if !parts.is_empty() {
                        if coll.is_some() {
                            bail!("unexpected property after getCollection()");
                        }
                        coll = Some(std::mem::take(&mut parts).join("."));
                    }
                    calls.push(c);
                }
            }
        }
        if !parts.is_empty() {
            bail!("missing method call after db.{}", parts.join("."));
        }
        if calls.is_empty() {
            bail!("missing method call");
        }
        Ok(Stmt::Chain { db, coll, calls })
    }

    fn args(&mut self) -> Result<Vec<Bson>> {
        self.expect('(')?;
        let mut out = vec![];
        loop {
            self.skip_ws();
            if self.eat(')') {
                break;
            }
            out.push(self.value()?);
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            self.expect(')')?;
            break;
        }
        Ok(out)
    }

    fn value(&mut self) -> Result<Bson> {
        self.skip_ws();
        match self.peek() {
            None => bail!("expected a value at end of input"),
            Some('{') => {
                self.i += 1;
                let mut d = Document::new();
                loop {
                    self.skip_ws();
                    if self.eat('}') {
                        break;
                    }
                    let key = match self.peek() {
                        Some('"') | Some('\'') => self.string()?,
                        _ => {
                            let k: String = self.c[self.i..]
                                .iter()
                                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '-'))
                                .collect();
                            if k.is_empty() {
                                bail!("expected a key near '{}'", self.snippet());
                            }
                            self.i += k.chars().count();
                            k
                        }
                    };
                    self.expect(':')?;
                    let v = self.value()?;
                    d.insert(key, v);
                    self.skip_ws();
                    if self.eat(',') {
                        continue;
                    }
                    self.expect('}')?;
                    break;
                }
                Ok(extjson(d))
            }
            Some('[') => {
                self.i += 1;
                let mut a = vec![];
                loop {
                    self.skip_ws();
                    if self.eat(']') {
                        break;
                    }
                    a.push(self.value()?);
                    self.skip_ws();
                    if self.eat(',') {
                        continue;
                    }
                    self.expect(']')?;
                    break;
                }
                Ok(Bson::Array(a))
            }
            Some('"') | Some('\'') => Ok(Bson::String(self.string()?)),
            Some('/') => self.regex(),
            Some(ch) if ch.is_ascii_digit() || ch == '-' || ch == '+' || ch == '.' => self.number(),
            Some(_) => {
                let w = self.ident()?;
                self.word_value(&w)
            }
        }
    }

    fn word_value(&mut self, w: &str) -> Result<Bson> {
        match w {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" | "undefined" => return Ok(Bson::Null),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "MinKey" | "MaxKey" => {
                self.skip_ws();
                if self.peek() == Some('(') {
                    self.args()?;
                }
                return Ok(if w == "MinKey" { Bson::MinKey } else { Bson::MaxKey });
            }
            "new" => {
                let f = self.ident()?;
                return self.word_value(&f);
            }
            _ => {}
        }
        self.skip_ws();
        if self.peek() != Some('(') {
            bail!("unknown identifier '{}'", w);
        }
        let args = self.args()?;
        let a0 = args.first();
        Ok(match w {
            "ObjectId" | "ObjectID" => match a0 {
                None => Bson::ObjectId(ObjectId::new()),
                Some(Bson::String(s)) => Bson::ObjectId(ObjectId::parse_str(s).map_err(|e| anyhow!("ObjectId(\"{}\"): {}", s, e))?),
                Some(o) => bail!("ObjectId() expects a hex string, got {}", o),
            },
            "ISODate" | "Date" => match a0 {
                None => Bson::DateTime(bson::DateTime::now()),
                Some(Bson::String(s)) => Bson::DateTime(parse_date(s)?),
                Some(b) => match num(b) {
                    Some(n) => Bson::DateTime(bson::DateTime::from_millis(n as i64)),
                    None => bail!("{}() expects a string or milliseconds", w),
                },
            },
            "NumberLong" | "Long" => Bson::Int64(int_arg(a0, w)?),
            "NumberInt" | "Int32" => Bson::Int32(int_arg(a0, w)? as i32),
            "NumberDecimal" | "Decimal128" => {
                let s = match a0 {
                    Some(Bson::String(s)) => s.clone(),
                    Some(b) => b.to_string(),
                    None => "0".into(),
                };
                Bson::Decimal128(s.parse().map_err(|e| anyhow!("{}(\"{}\"): {:?}", w, s, e))?)
            }
            "Double" => Bson::Double(a0.and_then(num).ok_or_else(|| anyhow!("Double() expects a number"))?),
            "Timestamp" => {
                let (t, i) = match (a0, args.get(1)) {
                    (Some(Bson::Document(d)), _) => (d.get("t").and_then(num).unwrap_or(0.0), d.get("i").and_then(num).unwrap_or(0.0)),
                    (a, b) => (a.and_then(num).unwrap_or(0.0), b.and_then(num).unwrap_or(0.0)),
                };
                Bson::Timestamp(bson::Timestamp { time: t as u32, increment: i as u32 })
            }
            "UUID" => match a0 {
                Some(Bson::String(s)) => Bson::Binary(bson::Binary::from_uuid(bson::Uuid::parse_str(s).map_err(|e| anyhow!("UUID(\"{}\"): {}", s, e))?)),
                None => Bson::Binary(bson::Binary::from_uuid(bson::Uuid::new())),
                Some(o) => bail!("UUID() expects a string, got {}", o),
            },
            "BinData" => {
                use base64::Engine;
                let sub = a0.and_then(num).unwrap_or(0.0) as u8;
                let data = match args.get(1) {
                    Some(Bson::String(s)) => base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| anyhow!("BinData: {}", e))?,
                    _ => bail!("BinData(subtype, base64) expects a base64 string"),
                };
                Bson::Binary(bson::Binary { subtype: sub.into(), bytes: data })
            }
            "RegExp" => {
                let pat = str_arg(&args, 0)?;
                let opts = if args.len() > 1 { str_arg(&args, 1)? } else { String::new() };
                regex(pat, &opts)
            }
            _ => bail!("unknown function {}()", w),
        })
    }

    fn string(&mut self) -> Result<String> {
        let q = self.peek().ok_or_else(|| anyhow!("expected a string"))?;
        self.i += 1;
        let mut s = String::new();
        loop {
            let ch = self.peek().ok_or_else(|| anyhow!("unterminated string"))?;
            self.i += 1;
            if ch == q {
                return Ok(s);
            }
            if ch != '\\' {
                s.push(ch);
                continue;
            }
            let e = self.peek().ok_or_else(|| anyhow!("unterminated string"))?;
            self.i += 1;
            match e {
                'n' => s.push('\n'),
                't' => s.push('\t'),
                'r' => s.push('\r'),
                'b' => s.push('\u{8}'),
                'f' => s.push('\u{c}'),
                'v' => s.push('\u{b}'),
                '0' => s.push('\0'),
                'u' | 'x' => {
                    let n = if e == 'u' { 4 } else { 2 };
                    let hex: String = self.c[self.i..(self.i + n).min(self.c.len())].iter().collect();
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| anyhow!("bad escape \\{}{}", e, hex))?;
                    self.i += n;
                    s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                '\n' => {}
                o => s.push(o),
            }
        }
    }

    fn regex(&mut self) -> Result<Bson> {
        self.i += 1;
        let mut pat = String::new();
        let mut class = false;
        loop {
            let ch = self.peek().ok_or_else(|| anyhow!("unterminated regular expression"))?;
            self.i += 1;
            match ch {
                '\\' => {
                    pat.push(ch);
                    if let Some(n) = self.peek() {
                        pat.push(n);
                        self.i += 1;
                    }
                }
                '[' => {
                    class = true;
                    pat.push(ch);
                }
                ']' => {
                    class = false;
                    pat.push(ch);
                }
                '/' if !class => break,
                '\n' => bail!("unterminated regular expression"),
                o => pat.push(o),
            }
        }
        let flags: String = self.c[self.i..].iter().take_while(|c| c.is_ascii_alphabetic()).collect();
        self.i += flags.len();
        Ok(regex(pat, &flags))
    }

    fn number(&mut self) -> Result<Bson> {
        let start = self.i;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.i += 1;
        }
        if self.c[self.i..].starts_with(&['I', 'n', 'f']) {
            let w = self.ident()?;
            if w != "Infinity" {
                bail!("bad number");
            }
            return Ok(Bson::Double(if self.c[start] == '-' { f64::NEG_INFINITY } else { f64::INFINITY }));
        }
        while let Some(ch) = self.peek() {
            let prev = if self.i > start { self.c[self.i - 1] } else { ' ' };
            if ch.is_ascii_alphanumeric() || ch == '.' || ((ch == '-' || ch == '+') && matches!(prev, 'e' | 'E')) {
                self.i += 1;
            } else {
                break;
            }
        }
        let t: String = self.c[start..self.i].iter().collect();
        let t = t.trim_start_matches('+');
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            let v = i64::from_str_radix(h, 16).map_err(|_| anyhow!("bad number {}", t))?;
            return Ok(int_bson(v));
        }
        if !t.contains(['.', 'e', 'E']) {
            if let Ok(v) = t.parse::<i64>() {
                return Ok(int_bson(v));
            }
        }
        t.parse::<f64>().map(Bson::Double).map_err(|_| anyhow!("bad number {}", t))
    }
}

fn int_bson(v: i64) -> Bson {
    match i32::try_from(v) {
        Ok(i) => Bson::Int32(i),
        Err(_) => Bson::Int64(v),
    }
}

pub fn regex(pattern: String, flags: &str) -> Bson {
    let mut o: Vec<char> = flags.chars().collect();
    o.sort_unstable();
    o.dedup();
    Bson::RegularExpression(bson::Regex { pattern, options: o.into_iter().collect() })
}

fn num(b: &Bson) -> Option<f64> {
    match b {
        Bson::Int32(v) => Some(*v as f64),
        Bson::Int64(v) => Some(*v as f64),
        Bson::Double(v) => Some(*v),
        Bson::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn int_arg(b: Option<&Bson>, f: &str) -> Result<i64> {
    match b {
        None => Ok(0),
        Some(Bson::String(s)) => s.trim().parse().map_err(|_| anyhow!("{}(\"{}\"): not an integer", f, s)),
        Some(o) => num(o).map(|v| v as i64).ok_or_else(|| anyhow!("{}() expects a number", f)),
    }
}

pub fn str_arg(args: &[Bson], i: usize) -> Result<String> {
    match args.get(i) {
        Some(Bson::String(s)) => Ok(s.clone()),
        Some(o) => bail!("argument {} must be a string, got {}", i + 1, o),
        None => bail!("missing argument {}", i + 1),
    }
}

/// Accepts RFC 3339 plus the shorter forms the shell accepts.
pub fn parse_date(s: &str) -> Result<bson::DateTime> {
    let t = s.trim();
    if let Ok(d) = bson::DateTime::parse_rfc3339_str(t) {
        return Ok(d);
    }
    let fmts = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"];
    for f in fmts {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(t.trim_end_matches('Z'), f) {
            return Ok(bson::DateTime::from_millis(n.and_utc().timestamp_millis()));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        if let Some(n) = d.and_hms_opt(0, 0, 0) {
            return Ok(bson::DateTime::from_millis(n.and_utc().timestamp_millis()));
        }
    }
    bail!("invalid date \"{}\"", s)
}

/// Turn `{"$oid": ".."}`-style extended JSON wrappers into real BSON values.
fn extjson(d: Document) -> Bson {
    const KEYS: &[&str] = &[
        "$oid", "$date", "$numberLong", "$numberInt", "$numberDouble", "$numberDecimal", "$binary", "$uuid", "$timestamp",
        "$regularExpression", "$minKey", "$maxKey",
    ];
    if d.len() == 1 && d.keys().next().is_some_and(|k| KEYS.contains(&k.as_str())) {
        if let Ok(j) = serde_json::to_value(&d) {
            if let Ok(b) = Bson::try_from(j) {
                if !matches!(b, Bson::Document(_)) {
                    return b;
                }
            }
        }
        // `{$date: "2024-01-01"}` with a non-RFC 3339 string
        if let Some(Bson::String(s)) = d.get("$date") {
            if let Ok(dt) = parse_date(s) {
                return Bson::DateTime(dt);
            }
        }
    }
    Bson::Document(d)
}

/// `{a: 1}` -> `a_1` (default index name).
pub fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                Bson::Double(f) if f.fract() == 0.0 => format!("{}", *f as i64),
                o => o.to_string(),
            };
            format!("{}_{}", k, v)
        })
        .collect::<Vec<_>>()
        .join("_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    fn one(s: &str) -> Stmt {
        let v = parse_script(s).unwrap();
        assert_eq!(v.len(), 1, "{:?}", v);
        v.into_iter().next().unwrap().1
    }

    #[test]
    fn relaxed_json() {
        let v = parse_value("{a: 1, 'b': \"x\\n\", c: [1, 2.5, -3,], d: {e: null, $gt: true}, big: 9999999999, }").unwrap();
        assert_eq!(
            v,
            Bson::Document(doc! { "a": 1, "b": "x\n", "c": [1, 2.5, -3], "d": { "e": Bson::Null, "$gt": true }, "big": 9_999_999_999i64 })
        );
        assert_eq!(parse_value("/ab\\/c[/]/ig").unwrap(), regex("ab\\/c[/]".into(), "gi"));
        assert_eq!(parse_value("'it\\'s'").unwrap(), Bson::String("it's".into()));
        assert_eq!(parse_value("1e3").unwrap(), Bson::Double(1000.0));
        assert_eq!(parse_value("-Infinity").unwrap(), Bson::Double(f64::NEG_INFINITY));
        assert!(parse_value("{a: }").is_err());
        assert!(parse_value("{a: 1").is_err());
        assert!(parse_value("foo").is_err());
    }

    #[test]
    fn helpers() {
        let hex = "64b7f0c2a1b2c3d4e5f60718";
        assert_eq!(parse_value(&format!("ObjectId('{}')", hex)).unwrap(), Bson::ObjectId(ObjectId::parse_str(hex).unwrap()));
        assert_eq!(parse_value(&format!("{{\"$oid\": \"{}\"}}", hex)).unwrap(), Bson::ObjectId(ObjectId::parse_str(hex).unwrap()));
        let d = bson::DateTime::parse_rfc3339_str("2024-01-02T03:04:05Z").unwrap();
        assert_eq!(parse_value("ISODate(\"2024-01-02T03:04:05Z\")").unwrap(), Bson::DateTime(d));
        assert_eq!(parse_value("new Date('2024-01-02T03:04:05')").unwrap(), Bson::DateTime(d));
        assert_eq!(parse_value("{$date: '2024-01-02T03:04:05Z'}").unwrap(), Bson::DateTime(d));
        assert!(matches!(parse_value("ISODate('2024-01-02')").unwrap(), Bson::DateTime(_)));
        assert_eq!(parse_value("NumberLong(\"5\")").unwrap(), Bson::Int64(5));
        assert_eq!(parse_value("NumberInt(7)").unwrap(), Bson::Int32(7));
        assert_eq!(parse_value("NumberDecimal(\"1.50\")").unwrap().to_string(), "1.50");
        assert!(matches!(parse_value("UUID('3b241101-e2bb-4255-8caf-4136c566a962')").unwrap(), Bson::Binary(_)));
        assert!(matches!(parse_value("BinData(0, 'AQID')").unwrap(), Bson::Binary(b) if b.bytes == vec![1, 2, 3]));
        assert!(matches!(parse_value("Timestamp(5, 1)").unwrap(), Bson::Timestamp(t) if t.time == 5 && t.increment == 1));
        assert!(matches!(parse_value("ObjectId()").unwrap(), Bson::ObjectId(_)));
        assert_eq!(parse_value("MinKey").unwrap(), Bson::MinKey);
    }

    #[test]
    fn statements() {
        let v = parse_script("use shop\nshow collections; db.users.find({age: {$gt: 3}}, {name: 1})\n  .sort({name: -1})\n  .limit(5)\ndb.getCollection('a.b').countDocuments() // c\n/* x */ db.system.users.drop();").unwrap();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].1, Stmt::Use("shop".into()));
        assert_eq!(v[1].1, Stmt::Show("collections".into()));
        let Stmt::Chain { coll, calls, db } = &v[2].1 else { panic!() };
        assert_eq!(coll.as_deref(), Some("users"));
        assert!(db.is_none());
        assert_eq!(calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["find", "sort", "limit"]);
        assert_eq!(calls[0].args[0], Bson::Document(doc! { "age": { "$gt": 3 } }));
        assert_eq!(v[2].0, "db.users.find({age: {$gt: 3}}, {name: 1})\n  .sort({name: -1})\n  .limit(5)");
        let Stmt::Chain { coll, .. } = &v[3].1 else { panic!() };
        assert_eq!(coll.as_deref(), Some("a.b"));
        let Stmt::Chain { coll, calls, .. } = &v[4].1 else { panic!() };
        assert_eq!(coll.as_deref(), Some("system.users"));
        assert_eq!(calls[0].name, "drop");
    }

    #[test]
    fn db_level_and_sql() {
        let Stmt::Chain { db, coll, calls } = one("db.runCommand({ping: 1})") else { panic!() };
        assert!(db.is_none() && coll.is_none());
        assert_eq!(calls[0].name, "runCommand");
        let Stmt::Chain { db, coll, .. } = one("db.getSiblingDB('admin').getCollection('x').find()") else { panic!() };
        assert_eq!((db.as_deref(), coll.as_deref()), (Some("admin"), Some("x")));
        let Stmt::Chain { coll, .. } = one("db['my coll'].find()") else { panic!() };
        assert_eq!(coll.as_deref(), Some("my coll"));
        let v = parse_script("SELECT * FROM \"d\".\"c\" WHERE a = 'x;y';\nINSERT INTO c (a) VALUES\n(1),\n(2)").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].1, Stmt::Sql("SELECT * FROM \"d\".\"c\" WHERE a = 'x;y'".into()));
        assert!(matches!(&v[1].1, Stmt::Sql(s) if s.ends_with("(2)")));
        assert!(parse_script("db.users").is_err());
        assert!(parse_script("foo()").is_err());
        assert!(parse_script("db.c.find({a: 1}").is_err());
    }

    #[test]
    fn names() {
        assert_eq!(index_name(&doc! { "a": 1, "b": -1, "t": "text" }), "a_1_b_-1_t_text");
        assert_eq!(index_name(&doc! { "a": 1.0 }), "a_1");
    }
}
