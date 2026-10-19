//! A small SQL parser for the statements the data grid, filters, import and
//! data transfer generate (see [`crate::dml`]). Document and search stores
//! (MongoDB, OpenSearch) translate the parsed form into their native query
//! languages, so every generic tool keeps working against them.

use anyhow::{anyhow, bail, Result};

use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cmp { column: String, op: CmpOp, value: Value },
    Like { column: String, pattern: String, negated: bool },
    In { column: String, values: Vec<Value>, negated: bool },
    Between { column: String, low: Value, high: Value },
    IsNull { column: String, negated: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Select(Select),
    Insert { table: Vec<String>, columns: Vec<String>, rows: Vec<Vec<Value>> },
    Update { table: Vec<String>, set: Vec<(String, Value)>, filter: Option<Expr> },
    Delete { table: Vec<String>, filter: Option<Expr> },
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Select {
    /// Qualified name parts, e.g. `["db", "coll"]`.
    pub table: Vec<String>,
    /// Empty = `*`.
    pub columns: Vec<String>,
    pub count: bool,
    pub filter: Option<Expr>,
    /// (column, descending)
    pub order: Vec<(String, bool)>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl Select {
    /// Last part of the qualified table name.
    pub fn table_name(&self) -> &str {
        self.table.last().map(|s| s.as_str()).unwrap_or("")
    }
}

/// Last part of a qualified name.
pub fn last(parts: &[String]) -> &str {
    parts.last().map(|s| s.as_str()).unwrap_or("")
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Ident(String),
    Str(String),
    Num(String),
    Sym(String),
}

fn tokenize(sql: &str) -> Result<Vec<Tok>> {
    let c: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut out = vec![];
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                if i >= c.len() {
                    bail!("unterminated string literal");
                }
                if c[i] == '\'' {
                    if i + 1 < c.len() && c[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if c[i] == '\\' && i + 1 < c.len() {
                    let n = c[i + 1];
                    s.push(match n {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '0' => '\0',
                        'Z' => '\u{1a}',
                        o => o,
                    });
                    i += 2;
                    continue;
                }
                s.push(c[i]);
                i += 1;
            }
            out.push(Tok::Str(s));
        } else if ch == '"' || ch == '`' || ch == '[' {
            let close = if ch == '[' { ']' } else { ch };
            let mut s = String::new();
            i += 1;
            loop {
                if i >= c.len() {
                    bail!("unterminated identifier");
                }
                if c[i] == close {
                    if i + 1 < c.len() && c[i + 1] == close {
                        s.push(close);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(c[i]);
                i += 1;
            }
            out.push(Tok::Ident(s));
        } else if ch.is_ascii_digit() || (ch == '-' && i + 1 < c.len() && c[i + 1].is_ascii_digit() && prev_allows_sign(&out)) {
            let start = i;
            i += 1;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.' || ((c[i] == '-' || c[i] == '+') && matches!(c[i - 1], 'e' | 'E'))) {
                i += 1;
            }
            out.push(Tok::Num(c[start..i].iter().collect()));
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(c[start..i].iter().collect()));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            if ["<=", ">=", "<>", "!=", "::"].contains(&two.as_str()) {
                out.push(Tok::Sym(two));
                i += 2;
            } else {
                out.push(Tok::Sym(ch.to_string()));
                i += 1;
            }
        }
    }
    Ok(out)
}

fn prev_allows_sign(out: &[Tok]) -> bool {
    match out.last() {
        None => true,
        Some(Tok::Sym(s)) => s != ")",
        Some(Tok::Word(w)) => is_kw(w),
        _ => false,
    }
}

fn is_kw(w: &str) -> bool {
    const KW: &[&str] = &[
        "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "ORDER", "BY", "LIMIT", "OFFSET", "IN", "IS", "NULL", "LIKE",
        "BETWEEN", "VALUES", "SET", "UPDATE", "DELETE", "INSERT", "INTO", "ASC", "DESC", "ILIKE",
    ];
    KW.iter().any(|k| k.eq_ignore_ascii_case(w))
}

struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i)
    }

    fn kw(&mut self, k: &str) -> bool {
        if let Some(Tok::Word(w)) = self.peek() {
            if w.eq_ignore_ascii_case(k) {
                self.i += 1;
                return true;
            }
        }
        false
    }

    fn expect_kw(&mut self, k: &str) -> Result<()> {
        if self.kw(k) {
            Ok(())
        } else {
            Err(anyhow!("expected {} near {}", k, self.near()))
        }
    }

    fn sym(&mut self, s: &str) -> bool {
        if let Some(Tok::Sym(x)) = self.peek() {
            if x == s {
                self.i += 1;
                return true;
            }
        }
        false
    }

    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.sym(s) {
            Ok(())
        } else {
            Err(anyhow!("expected '{}' near {}", s, self.near()))
        }
    }

    fn near(&self) -> String {
        match self.peek() {
            None => "end of statement".into(),
            Some(Tok::Word(w) | Tok::Ident(w) | Tok::Num(w) | Tok::Sym(w)) => format!("'{}'", w),
            Some(Tok::Str(s)) => format!("'{}'", s),
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.t.get(self.i).cloned() {
            Some(Tok::Ident(s)) => {
                self.i += 1;
                Ok(s)
            }
            Some(Tok::Word(w)) if !is_kw(&w) => {
                self.i += 1;
                Ok(w)
            }
            _ => Err(anyhow!("expected a name near {}", self.near())),
        }
    }

    /// `a.b.c` -> ["a", "b", "c"]
    fn qname(&mut self) -> Result<Vec<String>> {
        let mut v = vec![self.ident()?];
        while self.sym(".") {
            v.push(self.ident()?);
        }
        Ok(v)
    }

    /// Column reference; a dotted path is joined back (`"a"."b"` -> `a.b`)
    /// so nested document fields can be addressed.
    fn column(&mut self) -> Result<String> {
        let parts = self.qname()?;
        // skip casts such as `col::text`
        if self.sym("::") {
            self.ident()?;
        }
        Ok(parts.join("."))
    }

    fn value(&mut self) -> Result<Value> {
        let neg = self.sym("-");
        let v = match self.t.get(self.i).cloned() {
            Some(Tok::Str(s)) => Value::Text(s),
            Some(Tok::Num(n)) => {
                let n = if neg { format!("-{}", n) } else { n };
                if let Ok(i) = n.parse::<i64>() {
                    Value::Int(i)
                } else if let Ok(f) = n.parse::<f64>() {
                    Value::Float(f)
                } else if let Some(h) = n.strip_prefix("0x") {
                    Value::Bytes(crate::value::unhex(h).unwrap_or_default())
                } else {
                    bail!("bad number {}", n)
                }
            }
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("NULL") => Value::Null,
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("TRUE") => Value::Bool(true),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("FALSE") => Value::Bool(false),
            // X'ABCD'
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("X") => {
                self.i += 1;
                match self.t.get(self.i).cloned() {
                    Some(Tok::Str(h)) => Value::Bytes(crate::value::unhex(&h).unwrap_or_default()),
                    _ => bail!("expected hex string"),
                }
            }
            _ => bail!("expected a literal value near {}", self.near()),
        };
        self.i += 1;
        // `'..'::bytea` style casts
        if self.sym("::") {
            self.ident()?;
        }
        Ok(v)
    }

    fn expr(&mut self) -> Result<Expr> {
        let mut parts = vec![self.and_expr()?];
        while self.kw("OR") {
            parts.push(self.and_expr()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::Or(parts) })
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut parts = vec![self.unary()?];
        while self.kw("AND") {
            parts.push(self.unary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::And(parts) })
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.kw("NOT") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.sym("(") {
            let e = self.expr()?;
            self.expect_sym(")")?;
            return Ok(e);
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expr> {
        let column = self.column()?;
        if self.kw("IS") {
            let negated = self.kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull { column, negated });
        }
        let negated = self.kw("NOT");
        if self.kw("LIKE") || self.kw("ILIKE") {
            let pattern = match self.value()? {
                Value::Text(s) => s,
                v => v.to_string(),
            };
            return Ok(Expr::Like { column, pattern, negated });
        }
        if self.kw("IN") {
            self.expect_sym("(")?;
            let mut values = vec![self.value()?];
            while self.sym(",") {
                values.push(self.value()?);
            }
            self.expect_sym(")")?;
            return Ok(Expr::In { column, values, negated });
        }
        if self.kw("BETWEEN") {
            let low = self.value()?;
            self.expect_kw("AND")?;
            let high = self.value()?;
            let e = Expr::Between { column, low, high };
            return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
        }
        if negated {
            bail!("expected LIKE, IN or BETWEEN after NOT");
        }
        let op = match self.t.get(self.i).cloned() {
            Some(Tok::Sym(s)) => match s.as_str() {
                "=" => CmpOp::Eq,
                "<>" | "!=" => CmpOp::Ne,
                "<" => CmpOp::Lt,
                "<=" => CmpOp::Le,
                ">" => CmpOp::Gt,
                ">=" => CmpOp::Ge,
                _ => bail!("unsupported operator {}", s),
            },
            _ => bail!("expected an operator near {}", self.near()),
        };
        self.i += 1;
        let value = self.value()?;
        if value.is_null() {
            return Ok(Expr::IsNull { column, negated: op != CmpOp::Eq });
        }
        Ok(Expr::Cmp { column, op, value })
    }

    fn u64(&mut self) -> Result<u64> {
        match self.t.get(self.i).cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                n.parse().map_err(|_| anyhow!("bad number {}", n))
            }
            _ => Err(anyhow!("expected a number near {}", self.near())),
        }
    }

    fn end(&mut self) -> Result<()> {
        self.sym(";");
        if self.i < self.t.len() {
            bail!("unsupported syntax near {}", self.near());
        }
        Ok(())
    }

    fn select(&mut self) -> Result<Select> {
        let mut s = Select::default();
        if self.kw("COUNT") {
            self.expect_sym("(")?;
            if !self.sym("*") {
                self.column()?;
            }
            self.expect_sym(")")?;
            s.count = true;
        } else if !self.sym("*") {
            s.columns.push(self.column()?);
            while self.sym(",") {
                s.columns.push(self.column()?);
            }
        }
        self.expect_kw("FROM")?;
        s.table = self.qname()?;
        if self.kw("WHERE") {
            s.filter = Some(self.expr()?);
        }
        if self.kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let c = self.column()?;
                let desc = if self.kw("DESC") {
                    true
                } else {
                    self.kw("ASC");
                    false
                };
                s.order.push((c, desc));
                if !self.sym(",") {
                    break;
                }
            }
        }
        loop {
            if self.kw("LIMIT") {
                s.limit = Some(self.u64()?);
            } else if self.kw("OFFSET") {
                s.offset = Some(self.u64()?);
                let _ = self.kw("ROWS") || self.kw("ROW");
            } else {
                break;
            }
        }
        self.end()?;
        Ok(s)
    }
}

/// Parse one statement of the supported subset.
pub fn parse(sql: &str) -> Result<Statement> {
    let mut p = P { t: tokenize(sql.trim())?, i: 0 };
    if p.kw("SELECT") {
        return Ok(Statement::Select(p.select()?));
    }
    if p.kw("INSERT") {
        p.expect_kw("INTO")?;
        let table = p.qname()?;
        let mut columns = vec![];
        if p.sym("(") {
            columns.push(p.ident()?);
            while p.sym(",") {
                columns.push(p.ident()?);
            }
            p.expect_sym(")")?;
        }
        p.expect_kw("VALUES")?;
        let mut rows = vec![];
        loop {
            p.expect_sym("(")?;
            let mut r = vec![p.value()?];
            while p.sym(",") {
                r.push(p.value()?);
            }
            p.expect_sym(")")?;
            rows.push(r);
            if !p.sym(",") {
                break;
            }
        }
        p.end()?;
        return Ok(Statement::Insert { table, columns, rows });
    }
    if p.kw("UPDATE") {
        let table = p.qname()?;
        p.expect_kw("SET")?;
        let mut set = vec![];
        loop {
            let c = p.column()?;
            p.expect_sym("=")?;
            set.push((c, p.value()?));
            if !p.sym(",") {
                break;
            }
        }
        let filter = if p.kw("WHERE") { Some(p.expr()?) } else { None };
        if p.kw("LIMIT") {
            p.u64()?;
        }
        p.end()?;
        return Ok(Statement::Update { table, set, filter });
    }
    if p.kw("DELETE") {
        p.expect_kw("FROM")?;
        let table = p.qname()?;
        let filter = if p.kw("WHERE") { Some(p.expr()?) } else { None };
        if p.kw("LIMIT") {
            p.u64()?;
        }
        p.end()?;
        return Ok(Statement::Delete { table, filter });
    }
    bail!("unsupported statement (supported: SELECT, INSERT, UPDATE, DELETE)")
}

/// Convert a SQL LIKE pattern into an anchored regular expression.
pub fn like_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;
    use crate::dml;
    use crate::model::ObjPath;

    #[test]
    fn parses_grid_select() {
        let p = ObjPath::new("shop", None, "users");
        let mut r = dml::FilterRule::new("name");
        r.op = dml::FilterOp::Contains;
        r.value = "ab".into();
        let mut r2 = dml::FilterRule::new("age");
        r2.op = dml::FilterOp::Between;
        r2.value = "1, 5".into();
        let w = dml::filters_to_where(Dialect::Generic, &[r, r2]);
        let sql = dml::select_page(Dialect::Generic, &p, &w, &[dml::SortSpec { column: "age".into(), descending: true }], 100, 200);
        let Statement::Select(s) = parse(&sql).unwrap() else { panic!() };
        assert_eq!(s.table, vec!["shop", "users"]);
        assert_eq!(s.limit, Some(100));
        assert_eq!(s.offset, Some(200));
        assert_eq!(s.order, vec![("age".to_string(), true)]);
        assert_eq!(
            s.filter,
            Some(Expr::And(vec![
                Expr::Like { column: "name".into(), pattern: "%ab%".into(), negated: false },
                Expr::Between { column: "age".into(), low: Value::Int(1), high: Value::Int(5) },
            ]))
        );
    }

    #[test]
    fn parses_writes() {
        let p = ObjPath::new("db", None, "t");
        let u = dml::update_row(Dialect::Generic, &p, &[("a".into(), Value::Text("it's".into()))], &[("_id".into(), Value::Int(3))]);
        assert_eq!(
            parse(&u).unwrap(),
            Statement::Update {
                table: vec!["db".into(), "t".into()],
                set: vec![("a".into(), Value::Text("it's".into()))],
                filter: Some(Expr::Cmp { column: "_id".into(), op: CmpOp::Eq, value: Value::Int(3) }),
            }
        );
        let i = dml::insert_rows(Dialect::Generic, &p, &["a".into(), "b".into()], &[vec![Value::Int(-1), Value::Null], vec![Value::Float(2.5), Value::Bool(true)]]);
        let Statement::Insert { rows, columns, .. } = parse(&i).unwrap() else { panic!() };
        assert_eq!(columns, vec!["a", "b"]);
        assert_eq!(rows, vec![vec![Value::Int(-1), Value::Null], vec![Value::Float(2.5), Value::Bool(true)]]);
        let d = dml::delete_row(Dialect::Generic, &p, &[("k".into(), Value::Null)]);
        assert_eq!(parse(&d).unwrap(), Statement::Delete { table: vec!["db".into(), "t".into()], filter: Some(Expr::IsNull { column: "k".into(), negated: false }) });
    }

    #[test]
    fn count_and_in() {
        let Statement::Select(s) = parse("SELECT COUNT(*) FROM \"t\" WHERE x IN (1, 'a') OR NOT (y <> 2)").unwrap() else { panic!() };
        assert!(s.count);
        assert!(matches!(s.filter, Some(Expr::Or(_))));
        assert_eq!(like_to_regex("a%b_"), "^a.*b.$");
    }
}
