//! Context-aware SQL auto-completion.
//!
//! A tolerant token scan of the statement under the cursor works out what
//! belongs there (tables after `FROM`, the columns of `o` after `o.`, the
//! target table's columns inside `INSERT INTO t (`, ...). Names come from a
//! [`Catalog`] that loads metadata lazily, and candidates are ranked with
//! fuzzy matching (`usrnm` finds `user_name`).

use crate::dialect::Dialect;
use crate::model::{ColumnDef, ForeignKeyDef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Keyword,
    Function,
    Database,
    Schema,
    Table,
    View,
    Column,
    Alias,
    Snippet,
    Word,
    /// `ON` condition joining two tables.
    Join,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub label: String,
    /// Text that replaces the word being typed.
    pub insert: String,
    /// Characters to move the cursor back after inserting (to land inside `()`).
    pub cursor_back: usize,
    pub kind: ItemKind,
    /// Right-hand hint: column type, owning table, function signature...
    pub detail: String,
    pub score: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableEntry {
    pub name: String,
    pub view: bool,
}

/// Metadata source. `None` means "not loaded yet": implementations start
/// loading in the background and the caller asks again later.
pub trait Catalog {
    fn databases(&mut self) -> Option<Vec<String>>;
    /// Schemas of the current database (empty for engines without schemas).
    fn schemas(&mut self) -> Option<Vec<String>>;
    /// Tables of the current scope, or of the database/schema named by
    /// `qualifier` (`"db"`, `"schema"` or `"db.schema"`).
    fn tables(&mut self, qualifier: Option<&str>) -> Option<Vec<TableEntry>>;
    fn columns(&mut self, qualifier: Option<&str>, table: &str) -> Option<Vec<ColumnDef>>;
    /// Foreign keys declared on `table` (engines without them return none).
    fn foreign_keys(&mut self, _qualifier: Option<&str>, _table: &str) -> Option<Vec<ForeignKeyDef>> {
        Some(vec![])
    }
}

/// Catalog with nothing in it (editors without a connection).
pub struct NoCatalog;

impl Catalog for NoCatalog {
    fn databases(&mut self) -> Option<Vec<String>> {
        Some(vec![])
    }
    fn schemas(&mut self) -> Option<Vec<String>> {
        Some(vec![])
    }
    fn tables(&mut self, _: Option<&str>) -> Option<Vec<TableEntry>> {
        Some(vec![])
    }
    fn columns(&mut self, _: Option<&str>, _: &str) -> Option<Vec<ColumnDef>> {
        Some(vec![])
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// Char range replaced on accept.
    pub start: usize,
    pub end: usize,
    pub items: Vec<Item>,
    /// Some metadata was still loading; ask again to get the full list.
    pub incomplete: bool,
}

const MAX_ITEMS: usize = 60;

/// Completions at `cursor` (a char index). `explicit` is set for Ctrl+Space:
/// the list is shown even before anything is typed. `extra` items (snippets)
/// are merged in when they match.
pub fn complete(text: &str, cursor: usize, dialect: Dialect, cat: &mut dyn Catalog, explicit: bool, extra: &[Item]) -> Option<Completion> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let toks = tokenize(&chars, dialect);
    if toks.iter().any(|t| t.contains_cursor(cursor)) {
        return None;
    }
    let toks: Vec<Tok> = toks.into_iter().filter(|t| t.t != Tk::Comment).collect();
    // current statement
    let seg_start = toks.iter().rposition(|t| t.t == Tk::Punct(';') && t.end <= cursor).map(|i| i + 1).unwrap_or(0);
    let seg_end = toks[seg_start..].iter().position(|t| t.t == Tk::Punct(';') && t.start >= cursor).map(|i| i + seg_start).unwrap_or(toks.len());
    let seg = &toks[seg_start..seg_end];

    // word being typed
    let word_idx = seg.iter().position(|t| t.start < cursor && t.end >= cursor && matches!(t.t, Tk::Word(_) | Tk::Quoted(..)));
    let (start, end, prefix, quoted) = match word_idx.map(|i| &seg[i]) {
        Some(Tok { t: Tk::Word(_), start, end, .. }) => (*start, *end, chars[*start..cursor].iter().collect::<String>(), false),
        Some(Tok { t: Tk::Quoted(s, _), start, end, .. }) => {
            let typed = (cursor - start).saturating_sub(1);
            (*start, *end, s.chars().take(typed).collect(), true)
        }
        _ => (cursor, cursor, String::new(), false),
    };
    let before: Vec<&Tok> = seg.iter().filter(|t| t.end <= start).collect();

    // `a.b.` qualifier chain
    let mut qual: Vec<String> = Vec::new();
    let mut k = before.len();
    while k >= 2 && before[k - 1].t == Tk::Punct('.') && before[k - 1].end == before[k - 1].start + 1 {
        match before[k - 2].name() {
            Some(n) => qual.insert(0, n),
            None => break,
        }
        k -= 2;
    }
    let before = &before[..k];

    // right after `JOIN t x ON` the join condition is offered unprompted
    let after_on = qual.is_empty() && before.last().is_some_and(|t| t.is_kw("ON"));
    let short = prefix.chars().count() < 2;
    if !explicit && qual.is_empty() && short && !after_on {
        return None;
    }
    // `AS name`: the user is naming something new
    if qual.is_empty() && before.last().is_some_and(|t| t.is_kw("AS")) {
        return None;
    }

    let refs_toks: Vec<Tok> = seg.iter().enumerate().filter(|(i, _)| Some(*i) != word_idx).map(|(_, t)| t.clone()).collect();
    let ctes = parse_ctes(&refs_toks, cat);
    let refs = parse_refs(&refs_toks, &ctes, cat);
    let pos = position(before);

    let mut b = Builder { prefix: prefix.clone(), dialect, quoted, items: Vec::new(), incomplete: false, exact: false };

    if after_on && pos == Pos::Expr(Clause::On) {
        add_join_conditions(&mut b, &refs, cursor, &ctes, cat);
    }
    if after_on && short && !explicit {
        // only the conditions: columns and keywords wait for typing
    } else if !qual.is_empty() {
        complete_qualified(&mut b, &qual, pos, &refs, &ctes, cat);
    } else {
        match pos {
            Pos::Tables => {
                add_tables(&mut b, None, cat, 200);
                for c in &ctes {
                    b.add(&c.name, ItemKind::Table, "CTE", 250);
                }
                add_containers(&mut b, cat, 40);
            }
            Pos::Databases => {
                match cat.databases() {
                    Some(v) => v.iter().for_each(|d| b.add(d, ItemKind::Database, "", 200)),
                    None => b.incomplete = true,
                }
                match cat.schemas() {
                    Some(v) => v.iter().for_each(|s| b.add(s, ItemKind::Schema, "", 150)),
                    None => b.incomplete = true,
                }
            }
            Pos::TargetColumns => {
                if let Some(r) = refs.first() {
                    add_ref_columns(&mut b, r, &ctes, cat, 300, false);
                }
            }
            Pos::AfterTable(clause) => {
                for kw in after_table_keywords(clause) {
                    b.add_kw(kw, 150);
                }
            }
            Pos::Expr(clause) => {
                let many = refs.len() > 1;
                for r in &refs {
                    add_ref_columns(&mut b, r, &ctes, cat, 250, many);
                }
                for r in &refs {
                    let label = r.alias.clone().unwrap_or_else(|| r.name.clone());
                    if !label.is_empty() {
                        let detail = if r.alias.is_some() { r.name.clone() } else { String::new() };
                        b.add(&label, ItemKind::Alias, &detail, 180);
                    }
                }
                if refs.is_empty() && matches!(clause, Clause::Select) {
                    add_tables(&mut b, None, cat, 60);
                }
                add_functions(&mut b, dialect, 80);
                for kw in expr_keywords(clause) {
                    b.add_kw(kw, 60);
                }
                add_keywords(&mut b, 0);
            }
            Pos::Values => {
                add_functions(&mut b, dialect, 80);
                add_keywords(&mut b, 0);
            }
            Pos::Statement => {
                for kw in STATEMENT_STARTS {
                    b.add_kw(kw, 120);
                }
                add_keywords(&mut b, 0);
            }
            Pos::Other => {
                add_keywords(&mut b, 0);
                add_functions(&mut b, dialect, -10);
            }
        }
    }
    // a complete name was typed: Enter must not swap it for a longer fuzzy match
    if b.exact && !explicit {
        return None;
    }
    for it in extra {
        if let Some(s) = fuzzy_score(&it.label, &b.prefix) {
            b.items.push(Item { score: s + it.score, ..it.clone() });
        }
    }

    let mut items = b.items;
    // stable: equal scores keep catalog order (columns in table order)
    items.sort_by(|a, b| b.score.cmp(&a.score));
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert((i.label.to_lowercase(), i.kind == ItemKind::Keyword)));
    items.truncate(MAX_ITEMS);
    if items.is_empty() && !b.incomplete {
        return None;
    }
    Some(Completion { start, end: end.max(cursor), items, incomplete: b.incomplete })
}

/// Plain word completion for languages the SQL engine doesn't understand
/// (MongoDB shell, OpenSearch console): prefix/fuzzy match over `words`.
pub fn complete_words(text: &str, cursor: usize, words: &[String], keywords: &[&str], explicit: bool) -> Option<Completion> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    let prefix: String = chars[start..cursor].iter().collect();
    if !explicit && prefix.chars().count() < 2 {
        return None;
    }
    let mut items = Vec::new();
    let mut push = |w: &str, kind: ItemKind, boost: i32| {
        if w.eq_ignore_ascii_case(&prefix) {
            return;
        }
        if let Some(s) = fuzzy_score(w, &prefix) {
            items.push(Item { label: w.to_string(), insert: w.to_string(), cursor_back: 0, kind, detail: String::new(), score: s + boost });
        }
    };
    for w in words {
        push(w, ItemKind::Word, 100);
    }
    for k in keywords {
        push(k, ItemKind::Keyword, 0);
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then(a.label.len().cmp(&b.label.len())).then_with(|| a.label.cmp(&b.label)));
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.label.clone()));
    items.truncate(MAX_ITEMS);
    if items.is_empty() {
        return None;
    }
    Some(Completion { start, end: cursor, items, incomplete: false })
}

// ---------------------------------------------------------------------------
// Fuzzy matching

/// Match score of `pattern` against `cand`, or `None` when it doesn't match.
/// Prefix > word-start initials (`usrnm` → `user_name`, `oi` → `orderId`) >
/// substring > scattered subsequence.
pub fn fuzzy_score(cand: &str, pattern: &str) -> Option<i32> {
    if pattern.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = cand.chars().flat_map(|c| c.to_lowercase()).collect();
    let p: Vec<char> = pattern.chars().flat_map(|c| c.to_lowercase()).collect();
    if c.len() < p.len() {
        return None;
    }
    if c.starts_with(&p) {
        return Some(1000 - (c.len() - p.len()).min(200) as i32);
    }
    if let Some(s) = boundary_score(cand, &p) {
        return Some(700 + s);
    }
    let cs: String = c.iter().collect();
    let ps: String = p.iter().collect();
    if let Some(i) = cs.find(&ps) {
        return Some(500 - (i.min(100) as i32));
    }
    // subsequence with gap penalty
    let mut gaps = 0;
    let mut j = 0;
    let mut last: Option<usize> = None;
    for (i, ch) in c.iter().enumerate() {
        if j < p.len() && *ch == p[j] {
            if let Some(l) = last {
                gaps += (i - l - 1) as i32;
            }
            last = Some(i);
            j += 1;
        }
    }
    if j == p.len() && c.first() == p.first() {
        return Some((300 - gaps * 10).max(50));
    }
    None
}

/// Every pattern char is consumed from word segments in order, starting each
/// segment at its first letter (`_`, `-`, `.` and camelCase split segments).
fn boundary_score(cand: &str, p: &[char]) -> Option<i32> {
    let mut segs: Vec<Vec<char>> = Vec::new();
    let mut prev_lower = false;
    for ch in cand.chars() {
        if ch == '_' || ch == '-' || ch == '.' || ch == ' ' {
            segs.push(Vec::new());
            prev_lower = false;
            continue;
        }
        if segs.is_empty() || (ch.is_uppercase() && prev_lower) {
            segs.push(Vec::new());
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        segs.last_mut().unwrap().extend(ch.to_lowercase());
    }
    segs.retain(|s| !s.is_empty());
    if segs.len() < 2 {
        return None;
    }
    // greedy: consume as many chars of each segment as possible, backtrack-free
    fn go(segs: &[Vec<char>], p: &[char]) -> bool {
        if p.is_empty() {
            return true;
        }
        let Some((first, rest)) = segs.split_first() else { return false };
        if first.first() == p.first() {
            let mut n = 1;
            while n < first.len() && n < p.len() && first[n] == p[n] {
                n += 1;
            }
            for take in (1..=n).rev() {
                if go(rest, &p[take..]) {
                    return true;
                }
            }
        }
        go(rest, p)
    }
    if segs[0].first() != p.first() || !go(&segs, p) {
        return None;
    }
    Some(100 - segs.len().min(20) as i32)
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Clone, Debug, PartialEq)]
enum Tk {
    Word(String),
    /// Quoted identifier: contents and whether the closing quote was found.
    Quoted(String, bool),
    Str,
    Num,
    Comment,
    Punct(char),
}

#[derive(Clone, Debug)]
struct Tok {
    t: Tk,
    start: usize,
    end: usize,
    closed: bool,
}

impl Tok {
    /// Cursor is inside a string literal or comment (no completion there).
    fn contains_cursor(&self, cursor: usize) -> bool {
        matches!(self.t, Tk::Str | Tk::Comment) && self.start < cursor && (cursor < self.end || !self.closed)
    }

    fn word(&self) -> Option<String> {
        match &self.t {
            Tk::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(&self.t, Tk::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    /// Identifier text: unquoted word that isn't a reserved word, or a quoted identifier.
    fn name(&self) -> Option<String> {
        match &self.t {
            Tk::Word(w) if !is_reserved(w) => Some(w.clone()),
            Tk::Quoted(s, _) => Some(s.clone()),
            _ => None,
        }
    }
}

fn tokenize(c: &[char], dialect: Dialect) -> Vec<Tok> {
    let n = c.len();
    let mut out = Vec::new();
    let mut i = 0;
    let ident_start = |ch: char| ch.is_alphabetic() || ch == '_' || ch == '@' || ch == '#' && dialect == Dialect::SqlServer;
    while i < n {
        let ch = c[i];
        let s = i;
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '-' && i + 1 < n && c[i + 1] == '-' || ch == '#' && dialect == Dialect::MySql {
            while i < n && c[i] != '\n' {
                i += 1;
            }
            out.push(Tok { t: Tk::Comment, start: s, end: i, closed: i < n });
            continue;
        }
        if ch == '/' && i + 1 < n && c[i + 1] == '*' {
            i += 2;
            let mut closed = false;
            while i < n {
                if c[i] == '*' && i + 1 < n && c[i + 1] == '/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            out.push(Tok { t: Tk::Comment, start: s, end: i, closed });
            continue;
        }
        let ident_quote = match ch {
            '`' if dialect == Dialect::MySql => Some('`'),
            '"' if dialect != Dialect::MySql => Some('"'),
            '[' if dialect == Dialect::SqlServer => Some(']'),
            _ => None,
        };
        if ch == '\'' || (ch == '"' && dialect == Dialect::MySql) || ident_quote.is_some() {
            let close = ident_quote.unwrap_or(ch);
            i += 1;
            let mut body = String::new();
            let mut closed = false;
            while i < n {
                if c[i] == '\\' && dialect == Dialect::MySql && ident_quote.is_none() && i + 1 < n {
                    i += 2;
                    continue;
                }
                if c[i] == close {
                    if i + 1 < n && c[i + 1] == close {
                        body.push(close);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                body.push(c[i]);
                i += 1;
            }
            let t = if ident_quote.is_some() { Tk::Quoted(body, closed) } else { Tk::Str };
            out.push(Tok { t, start: s, end: i, closed });
            continue;
        }
        if ch.is_ascii_digit() {
            while i < n && (c[i].is_ascii_alphanumeric() || c[i] == '.' || c[i] == '_') {
                i += 1;
            }
            out.push(Tok { t: Tk::Num, start: s, end: i, closed: true });
            continue;
        }
        if ident_start(ch) {
            i += 1;
            while i < n && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok { t: Tk::Word(c[s..i].iter().collect()), start: s, end: i, closed: true });
            continue;
        }
        if ch == '$' && dialect == Dialect::Postgres {
            // $tag$ ... $tag$ body is a string
            let mut j = i + 1;
            while j < n && (c[j].is_alphanumeric() || c[j] == '_') {
                j += 1;
            }
            if j < n && c[j] == '$' {
                let tag: Vec<char> = c[i..=j].to_vec();
                i = j + 1;
                let mut closed = false;
                while i < n {
                    if c[i..].starts_with(&tag) {
                        i += tag.len();
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                out.push(Tok { t: Tk::Str, start: s, end: i, closed });
                continue;
            }
        }
        out.push(Tok { t: Tk::Punct(ch), start: s, end: i + 1, closed: true });
        i += 1;
    }
    out
}

/// Words that never act as a table alias or bare identifier.
fn is_reserved(w: &str) -> bool {
    const R: &[&str] = &[
        "ALL", "AND", "ANTI", "ANY", "ARRAY", "AS", "ASC", "ASOF", "BETWEEN", "BY", "CASE", "CROSS", "DEFAULT", "DELETE", "DESC",
        "DISTINCT", "ELSE", "END", "EXCEPT", "EXISTS", "FETCH", "FINAL", "FOR", "FORCE", "FROM", "FULL", "GLOBAL", "GROUP",
        "HAVING", "IGNORE", "IN", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "LATERAL", "LEFT", "LIKE", "LIMIT",
        "NATURAL", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "PARTITION", "POSITIONAL", "PREWHERE", "QUALIFY",
        "RETURNING", "RIGHT", "SAMPLE", "SELECT", "SEMI", "SET", "STRAIGHT_JOIN", "TABLESAMPLE", "THEN", "TOP", "UNION",
        "UPDATE", "USE", "USING", "VALUES", "WHEN", "WHERE", "WINDOW", "WITH",
    ];
    R.iter().any(|r| r.eq_ignore_ascii_case(w))
}

// ---------------------------------------------------------------------------
// Cursor position

#[derive(Clone, Copy, Debug, PartialEq)]
enum Clause {
    Start,
    Select,
    From,
    Join,
    On,
    Where,
    GroupBy,
    OrderBy,
    Having,
    Set,
    Into,
    InsertCols,
    Values,
    Update,
    Use,
    TableKw,
    Limit,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Pos {
    Statement,
    Tables,
    Databases,
    /// Column list of `INSERT INTO t (`.
    TargetColumns,
    AfterTable(Clause),
    Expr(Clause),
    Values,
    Other,
}

/// Clause the cursor sits in, tracking parentheses so `IN (SELECT ...` or
/// `COUNT(` see the right clause.
fn position(before: &[&Tok]) -> Pos {
    let mut stack: Vec<Clause> = vec![Clause::Start];
    let mut prev: Option<&Tok> = None;
    for t in before {
        let cur = *stack.last().unwrap();
        match &t.t {
            Tk::Punct('(') => {
                let next = if cur == Clause::Into && prev.is_some_and(|p| p.name().is_some()) {
                    Clause::InsertCols
                } else {
                    cur
                };
                stack.push(next);
            }
            Tk::Punct(')') => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            Tk::Word(_) => {
                let w = t.word().unwrap();
                let pw = prev.and_then(|p| p.word());
                let new = match w.as_str() {
                    "SELECT" => Some(Clause::Select),
                    "RETURNING" | "OUTPUT" => Some(Clause::Select),
                    "FROM" if cur != Clause::Other || pw.as_deref() == Some("DELETE") => Some(Clause::From),
                    "FROM" => Some(Clause::From),
                    "JOIN" | "STRAIGHT_JOIN" => Some(Clause::Join),
                    "ON" if !matches!(pw.as_deref(), Some("DELETE" | "UPDATE")) && cur != Clause::Other => Some(Clause::On),
                    "USING" if matches!(cur, Clause::Join | Clause::From) => Some(Clause::On),
                    "WHERE" | "PREWHERE" | "QUALIFY" => Some(Clause::Where),
                    "BY" => match pw.as_deref() {
                        Some("GROUP") => Some(Clause::GroupBy),
                        Some("ORDER") | Some("PARTITION") | Some("SORT") | Some("CLUSTER") => Some(Clause::OrderBy),
                        _ => None,
                    },
                    "HAVING" => Some(Clause::Having),
                    "SET" if cur == Clause::Update || cur == Clause::Start || pw.as_deref() == Some("UPDATE") => Some(Clause::Set),
                    "INTO" => Some(Clause::Into),
                    "UPDATE" if !matches!(pw.as_deref(), Some("ON" | "FOR" | "KEY")) => Some(Clause::Update),
                    "VALUES" | "VALUE" => Some(Clause::Values),
                    "USE" if cur == Clause::Start => Some(Clause::Use),
                    "TABLE" if matches!(pw.as_deref(), Some("ALTER" | "DROP" | "TRUNCATE" | "LOCK" | "ANALYZE" | "OPTIMIZE" | "REPAIR")) => {
                        Some(Clause::TableKw)
                    }
                    "DESCRIBE" | "DESC" | "EXPLAIN" if cur == Clause::Start && prev.is_none() => Some(Clause::TableKw),
                    "LIMIT" | "OFFSET" | "TOP" if cur != Clause::Select || w != "TOP" => Some(Clause::Limit),
                    "UNION" | "EXCEPT" | "INTERSECT" => Some(Clause::Start),
                    "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE" | "SHOW" | "WITH" | "INSERT" | "DELETE"
                    | "REPLACE" | "MERGE" | "CALL" | "EXEC" | "BEGIN" | "COMMIT" | "ROLLBACK"
                        if cur == Clause::Start || prev.is_none() =>
                    {
                        Some(Clause::Other)
                    }
                    _ => None,
                };
                if let Some(c) = new {
                    // `EXPLAIN SELECT ...` keeps going as a SELECT
                    *stack.last_mut().unwrap() = c;
                }
            }
            _ => {}
        }
        prev = Some(t);
    }
    let cur = *stack.last().unwrap();
    let Some(prev) = prev else { return Pos::Statement };
    let pw = prev.word();
    match cur {
        Clause::Start => Pos::Statement,
        Clause::Use => Pos::Databases,
        Clause::From | Clause::Join | Clause::Update | Clause::Into | Clause::TableKw => {
            let after_kw = matches!(pw.as_deref(), Some("FROM" | "JOIN" | "UPDATE" | "INTO" | "TABLE" | "DESCRIBE" | "DESC" | "EXPLAIN" | "STRAIGHT_JOIN" | "ONLY" | "LATERAL"));
            let after_comma = prev.t == Tk::Punct(',') && cur == Clause::From;
            if after_kw || after_comma {
                Pos::Tables
            } else if prev.t == Tk::Punct('(') {
                Pos::Statement
            } else {
                Pos::AfterTable(cur)
            }
        }
        Clause::InsertCols => Pos::TargetColumns,
        Clause::Values => Pos::Values,
        Clause::Limit => Pos::Other,
        Clause::Other => {
            if prev.t == Tk::Punct('(') {
                Pos::Statement
            } else {
                Pos::Other
            }
        }
        c => Pos::Expr(c),
    }
}

// ---------------------------------------------------------------------------
// Table references

#[derive(Clone, Debug, PartialEq)]
struct Ref {
    qualifier: Option<String>,
    name: String,
    alias: Option<String>,
    /// Output columns of a derived table (`(SELECT ...) x`).
    derived: Option<Vec<String>>,
    /// Char offset where the reference starts.
    at: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Cte {
    name: String,
    columns: Vec<String>,
}

fn matching_paren(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.t {
            Tk::Punct('(') => depth += 1,
            Tk::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// `WITH name [(cols)] AS (...) [, ...]` definitions.
fn parse_ctes(toks: &[Tok], cat: &mut dyn Catalog) -> Vec<Cte> {
    let mut out: Vec<Cte> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if !toks[i].is_kw("WITH") {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if toks.get(j).is_some_and(|t| t.is_kw("RECURSIVE")) {
            j += 1;
        }
        loop {
            let Some(name) = toks.get(j).and_then(|t| t.name()) else { break };
            j += 1;
            let mut cols = Vec::new();
            if toks.get(j).is_some_and(|t| t.t == Tk::Punct('(')) {
                let close = matching_paren(toks, j);
                cols = toks[j + 1..close.min(toks.len())].iter().filter_map(|t| t.name()).collect();
                j = close + 1;
            }
            if !toks.get(j).is_some_and(|t| t.is_kw("AS")) {
                break;
            }
            j += 1;
            if toks.get(j).is_some_and(|t| t.is_kw("NOT") || t.is_kw("MATERIALIZED")) {
                while toks.get(j).is_some_and(|t| t.t != Tk::Punct('(')) {
                    j += 1;
                }
            }
            if !toks.get(j).is_some_and(|t| t.t == Tk::Punct('(')) {
                break;
            }
            let close = matching_paren(toks, j);
            if cols.is_empty() {
                let inner = &toks[j + 1..close.min(toks.len())];
                cols = select_outputs(inner, &out, cat);
            }
            out.push(Cte { name, columns: cols });
            j = close + 1;
            if toks.get(j).is_some_and(|t| t.t == Tk::Punct(',')) {
                j += 1;
            } else {
                break;
            }
        }
        i = j.max(i + 1);
    }
    out
}

/// Tables named after FROM / JOIN / UPDATE / INTO (plus comma-separated FROM
/// lists), with their aliases. Derived tables carry their output columns.
fn parse_refs(toks: &[Tok], ctes: &[Cte], cat: &mut dyn Catalog) -> Vec<Ref> {
    let mut out = Vec::new();
    // per paren level: are we inside a FROM list?
    let mut in_from: Vec<bool> = vec![false];
    let mut expect = false;
    // `INTO t (cols)` is a column list, not a table function call
    let mut after_into = false;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if expect {
            if t.t == Tk::Punct('(') {
                let close = matching_paren(toks, i);
                let inner = &toks[i + 1..close.min(toks.len())];
                if inner.first().is_some_and(|t| t.is_kw("SELECT") || t.is_kw("WITH") || t.is_kw("VALUES")) {
                    let cols = select_outputs(inner, ctes, cat);
                    let mut j = close + 1;
                    if toks.get(j).is_some_and(|t| t.is_kw("AS")) {
                        j += 1;
                    }
                    let alias = toks.get(j).and_then(|t| t.name());
                    out.push(Ref { qualifier: None, name: alias.clone().unwrap_or_default(), alias: None, derived: Some(cols), at: t.start });
                    expect = false;
                }
                in_from.push(false);
                i += 1;
                continue;
            }
            if t.is_kw("ONLY") || t.is_kw("LATERAL") || t.is_kw("IGNORE") {
                i += 1;
                continue;
            }
            if let Some(first) = t.name() {
                let mut parts = vec![first];
                let mut j = i + 1;
                while j + 1 < toks.len() && toks[j].t == Tk::Punct('.') {
                    match toks[j + 1].name() {
                        Some(n) => parts.push(n),
                        None => break,
                    }
                    j += 2;
                }
                // function call (`FROM read_parquet('x')`, `generate_series(...)`)
                let is_call = !after_into && toks.get(j).is_some_and(|t| t.t == Tk::Punct('('));
                if is_call {
                    j = matching_paren(toks, j) + 1;
                }
                if toks.get(j).is_some_and(|t| t.is_kw("AS")) {
                    j += 1;
                }
                let alias = toks.get(j).and_then(|t| t.name());
                if alias.is_some() {
                    j += 1;
                }
                let name = parts.pop().unwrap();
                let qualifier = if parts.is_empty() { None } else { Some(parts.join(".")) };
                if is_call {
                    if let Some(a) = alias {
                        out.push(Ref { qualifier: None, name: a, alias: None, derived: Some(vec![]), at: t.start });
                    }
                } else {
                    out.push(Ref { qualifier, name, alias, derived: None, at: t.start });
                }
                expect = false;
                i = j;
                continue;
            }
            expect = false;
        }
        match &t.t {
            Tk::Punct('(') => in_from.push(false),
            Tk::Punct(')') => {
                if in_from.len() > 1 {
                    in_from.pop();
                }
            }
            Tk::Punct(',') if *in_from.last().unwrap() => expect = true,
            Tk::Word(_) => {
                let w = t.word().unwrap();
                match w.as_str() {
                    "FROM" | "JOIN" | "STRAIGHT_JOIN" => {
                        after_into = false;
                        *in_from.last_mut().unwrap() = true;
                        expect = true;
                    }
                    "UPDATE" => {
                        let pw = i.checked_sub(1).and_then(|p| toks[p].word());
                        if !matches!(pw.as_deref(), Some("ON" | "FOR" | "KEY")) {
                            expect = true;
                        }
                    }
                    "INTO" => {
                        expect = true;
                        after_into = true;
                    }
                    "TABLE" => {
                        let pw = i.checked_sub(1).and_then(|p| toks[p].word());
                        if matches!(pw.as_deref(), Some("ALTER" | "DROP" | "TRUNCATE")) {
                            expect = true;
                        }
                    }
                    "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "ON" | "USING" | "SET" | "VALUES" | "UNION" | "WINDOW"
                    | "SELECT" | "QUALIFY" | "RETURNING" => {
                        *in_from.last_mut().unwrap() = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Output column names of a query: aliases, bare/qualified column names and
/// `*` expanded from its own FROM list.
fn select_outputs(toks: &[Tok], ctes: &[Cte], cat: &mut dyn Catalog) -> Vec<String> {
    // first top-level SELECT
    let mut depth = 0;
    let mut sel = None;
    for (i, t) in toks.iter().enumerate() {
        match t.t {
            Tk::Punct('(') => depth += 1,
            Tk::Punct(')') => depth -= 1,
            _ if depth == 0 && t.is_kw("SELECT") => {
                sel = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(sel) = sel else { return vec![] };
    let mut items: Vec<Vec<&Tok>> = vec![vec![]];
    let mut depth = 0;
    for t in &toks[sel + 1..] {
        match t.t {
            Tk::Punct('(') => depth += 1,
            Tk::Punct(')') => depth -= 1,
            _ => {}
        }
        if depth == 0 && (t.is_kw("FROM") || t.is_kw("INTO") || t.is_kw("WHERE") || t.is_kw("UNION")) {
            break;
        }
        if depth == 0 && t.t == Tk::Punct(',') {
            items.push(vec![]);
        } else {
            items.last_mut().unwrap().push(t);
        }
    }
    let mut out = Vec::new();
    let mut refs: Option<Vec<Ref>> = None;
    for mut it in items {
        while it.first().is_some_and(|t| t.is_kw("DISTINCT") || t.is_kw("ALL")) {
            it.remove(0);
        }
        let n = it.len();
        if n == 0 {
            continue;
        }
        let last = it[n - 1];
        if last.t == Tk::Punct('*') {
            let refs = refs.get_or_insert_with(|| parse_refs(toks, ctes, cat));
            let only = if n >= 3 && it[n - 2].t == Tk::Punct('.') { it[n - 3].name() } else { None };
            for r in refs.iter() {
                if let Some(o) = &only {
                    let label = r.alias.as_ref().unwrap_or(&r.name);
                    if !label.eq_ignore_ascii_case(o) {
                        continue;
                    }
                }
                if let Some(cols) = ref_columns(r, ctes, cat) {
                    out.extend(cols.into_iter().map(|c| c.0));
                }
            }
            continue;
        }
        let Some(name) = last.name() else { continue };
        if n == 1 || (n >= 2 && it[n - 2].t == Tk::Punct('.')) {
            out.push(name);
        } else if n >= 2 && (it[n - 2].is_kw("AS") || matches!(it[n - 2].t, Tk::Punct(')') | Tk::Word(_) | Tk::Quoted(..) | Tk::Num | Tk::Str)) {
            out.push(name);
        }
    }
    out
}

/// (name, detail) columns of a reference; `None` while loading.
fn ref_columns(r: &Ref, ctes: &[Cte], cat: &mut dyn Catalog) -> Option<Vec<(String, String)>> {
    if let Some(d) = &r.derived {
        return Some(d.iter().map(|c| (c.clone(), String::new())).collect());
    }
    if r.qualifier.is_none() {
        if let Some(c) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(&r.name)) {
            return Some(c.columns.iter().map(|c| (c.clone(), String::new())).collect());
        }
    }
    let cols = cat.columns(r.qualifier.as_deref(), &r.name)?;
    Some(cols.into_iter().map(|c| (c.name.clone(), column_detail(&c))).collect())
}

fn column_detail(c: &ColumnDef) -> String {
    let mut d = c.data_type.clone();
    if c.primary_key {
        d.push_str(" · PK");
    } else if !c.nullable {
        d.push_str(" · NOT NULL");
    }
    d
}

// ---------------------------------------------------------------------------
// Item building

struct Builder {
    prefix: String,
    dialect: Dialect,
    /// The user opened a quoted identifier: always insert quoted.
    quoted: bool,
    items: Vec<Item>,
    incomplete: bool,
    /// Some candidate equals the typed word (it is never offered itself).
    exact: bool,
}

impl Builder {
    fn add(&mut self, name: &str, kind: ItemKind, detail: &str, boost: i32) {
        if name.is_empty() {
            return;
        }
        if !self.quoted && name.eq_ignore_ascii_case(&self.prefix) {
            self.exact = true;
            return;
        }
        let Some(s) = fuzzy_score(name, &self.prefix) else { return };
        let insert = if self.quoted || needs_quote(name, self.dialect) { self.dialect.quote_ident(name) } else { name.to_string() };
        self.items.push(Item { label: name.into(), insert, cursor_back: 0, kind, detail: detail.into(), score: s + boost });
    }

    fn add_join(&mut self, cond: &str, detail: &str, boost: i32) {
        if self.quoted {
            return;
        }
        let Some(s) = fuzzy_score(cond, &self.prefix) else { return };
        self.items.push(Item { label: cond.into(), insert: cond.into(), cursor_back: 0, kind: ItemKind::Join, detail: detail.into(), score: s + boost });
    }

    fn add_kw(&mut self, kw: &str, boost: i32) {
        if self.quoted {
            return;
        }
        if kw.eq_ignore_ascii_case(&self.prefix) {
            self.exact = true;
            return;
        }
        if let Some(s) = fuzzy_score(kw, &self.prefix) {
            // multi-word keywords only on prefix match: `or` shouldn't offer `ORDER BY` fuzzily over `OR`
            if kw.contains(' ') && s < 900 {
                return;
            }
            self.items.push(Item { label: kw.into(), insert: kw.into(), cursor_back: 0, kind: ItemKind::Keyword, detail: String::new(), score: s + boost });
        }
    }
}

/// Identifier must be quoted to be read back as written.
pub fn needs_quote(name: &str, dialect: Dialect) -> bool {
    let mut cs = name.chars();
    let simple = cs.next().is_some_and(|c| c.is_alphabetic() || c == '_') && cs.all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    const ALSO: &[&str] = &[
        "ALTER", "CHECK", "COLUMN", "CONSTRAINT", "CREATE", "DROP", "FOREIGN", "INDEX", "KEY", "PRIMARY", "REFERENCES", "TABLE", "TO",
        "UNIQUE", "USER",
    ];
    if !simple || is_reserved(name) || ALSO.iter().any(|k| k.eq_ignore_ascii_case(name)) {
        return true;
    }
    match dialect {
        Dialect::Postgres => name.chars().any(|c| c.is_uppercase()),
        Dialect::Oracle => name.chars().any(|c| c.is_lowercase()),
        _ => false,
    }
}

fn add_tables(b: &mut Builder, qualifier: Option<&str>, cat: &mut dyn Catalog, boost: i32) {
    match cat.tables(qualifier) {
        Some(v) => {
            for t in v {
                let kind = if t.view { ItemKind::View } else { ItemKind::Table };
                b.add(&t.name, kind, qualifier.unwrap_or(""), boost);
            }
        }
        None => b.incomplete = true,
    }
}

/// Databases and schemas, so `FROM other_db.` can be started from the list.
fn add_containers(b: &mut Builder, cat: &mut dyn Catalog, boost: i32) {
    if let Some(v) = cat.schemas() {
        v.iter().for_each(|s| b.add(s, ItemKind::Schema, "schema", boost));
    }
    if let Some(v) = cat.databases() {
        v.iter().for_each(|d| b.add(d, ItemKind::Database, "database", boost - 10));
    }
}

fn add_ref_columns(b: &mut Builder, r: &Ref, ctes: &[Cte], cat: &mut dyn Catalog, boost: i32, show_owner: bool) {
    match ref_columns(r, ctes, cat) {
        Some(cols) => {
            let owner = r.alias.as_deref().unwrap_or(&r.name).to_string();
            for (c, d) in cols {
                let detail = if show_owner && !owner.is_empty() { format!("{} · {}", d, owner).trim_start_matches(" · ").to_string() } else { d };
                b.add(&c, ItemKind::Column, &detail, boost);
            }
        }
        None => b.incomplete = true,
    }
}

/// The statement's table that `a` / `t` / `db.t` in front of a dot names.
fn find_ref<'r>(refs: &'r [Ref], qual: &[String]) -> Option<&'r Ref> {
    let last = qual.last()?;
    let parent = (qual.len() > 1).then(|| qual[..qual.len() - 1].join("."));
    refs.iter().find(|r| match &r.alias {
        Some(a) => qual.len() == 1 && a.eq_ignore_ascii_case(last),
        None => {
            r.name.eq_ignore_ascii_case(last)
                && match (&parent, &r.qualifier) {
                    (None, _) => true,
                    (Some(p), Some(rq)) => p.eq_ignore_ascii_case(rq),
                    (Some(_), None) => false,
                }
        }
    })
}

fn complete_qualified(b: &mut Builder, qual: &[String], pos: Pos, refs: &[Ref], ctes: &[Cte], cat: &mut dyn Catalog) {
    let q = qual.join(".");
    if pos == Pos::Tables {
        let canon = canonical_container(&q, cat, &mut b.incomplete);
        add_tables(b, Some(canon.as_deref().unwrap_or(&q)), cat, 200);
        return;
    }
    // alias / table of this statement
    let last = qual.last().unwrap();
    let parent = (qual.len() > 1).then(|| qual[..qual.len() - 1].join("."));
    if let Some(r) = find_ref(refs, qual) {
        add_ref_columns(b, r, ctes, cat, 300, false);
        b.add_kw("*", 0);
        return;
    }
    if qual.len() == 1 {
        if let Some(c) = ctes.iter().find(|c| c.name.eq_ignore_ascii_case(last)) {
            for col in &c.columns {
                b.add(col, ItemKind::Column, "", 300);
            }
            return;
        }
    }
    // a table that isn't in FROM yet
    let tables = match &parent {
        None => cat.tables(None),
        Some(p) => canonical_container(p, cat, &mut b.incomplete).and_then(|p| cat.tables(Some(&p))),
    };
    if let Some(t) = tables.as_ref().and_then(|v| v.iter().find(|t| t.name.eq_ignore_ascii_case(last))) {
        let r = Ref { qualifier: parent.clone(), name: t.name.clone(), alias: None, derived: None, at: 0 };
        add_ref_columns(b, &r, ctes, cat, 300, false);
        return;
    }
    // database / schema
    if let Some(c) = canonical_container(&q, cat, &mut b.incomplete) {
        add_tables(b, Some(&c), cat, 200);
    } else if tables.is_none() {
        b.incomplete = true;
    }
}

/// Real spelling of a database or schema name, if `q` names one.
fn canonical_container(q: &str, cat: &mut dyn Catalog, incomplete: &mut bool) -> Option<String> {
    if q.contains('.') {
        return Some(q.to_string());
    }
    let mut found = None;
    match cat.schemas() {
        Some(v) => found = v.into_iter().find(|s| s.eq_ignore_ascii_case(q)),
        None => *incomplete = true,
    }
    if found.is_none() {
        match cat.databases() {
            Some(v) => found = v.into_iter().find(|s| s.eq_ignore_ascii_case(q)),
            None => *incomplete = true,
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Keywords and functions

const STATEMENT_STARTS: &[&str] = &[
    "SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "WITH", "CREATE TABLE", "CREATE VIEW", "CREATE INDEX", "ALTER TABLE",
    "DROP TABLE", "TRUNCATE TABLE", "EXPLAIN", "SHOW", "USE", "BEGIN", "COMMIT", "ROLLBACK",
];

const MULTI_WORD: &[&str] = &[
    "GROUP BY", "ORDER BY", "PARTITION BY", "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "FULL JOIN", "CROSS JOIN", "IS NULL",
    "IS NOT NULL", "NOT NULL", "PRIMARY KEY", "FOREIGN KEY", "INSERT INTO", "DELETE FROM", "UNION ALL", "NOT IN",
    "NOT EXISTS",
];

fn after_table_keywords(c: Clause) -> &'static [&'static str] {
    match c {
        Clause::Into => &["VALUES", "SELECT", "DEFAULT VALUES"],
        Clause::Update => &["SET", "AS"],
        Clause::TableKw => &["ADD", "ADD COLUMN", "DROP COLUMN", "RENAME TO", "MODIFY", "ALTER COLUMN"],
        _ => &[
            "WHERE", "JOIN", "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "FULL JOIN", "CROSS JOIN", "ON", "USING", "AS", "GROUP BY",
            "ORDER BY", "HAVING", "LIMIT", "UNION", "UNION ALL",
        ],
    }
}

fn expr_keywords(c: Clause) -> &'static [&'static str] {
    match c {
        Clause::Select => &["FROM", "AS", "DISTINCT", "CASE", "WHEN", "THEN", "ELSE", "END"],
        Clause::Where | Clause::On | Clause::Having => {
            &["AND", "OR", "NOT", "IN", "IS NULL", "IS NOT NULL", "LIKE", "BETWEEN", "EXISTS", "GROUP BY", "ORDER BY", "LIMIT"]
        }
        Clause::GroupBy => &["HAVING", "ORDER BY", "LIMIT"],
        Clause::OrderBy => &["ASC", "DESC", "NULLS FIRST", "NULLS LAST", "LIMIT"],
        Clause::Set => &["WHERE", "DEFAULT"],
        _ => &[],
    }
}

fn add_keywords(b: &mut Builder, boost: i32) {
    for k in crate::sqlutil::KEYWORDS {
        if FUNCTIONS.iter().any(|f| f.0 == *k) {
            continue;
        }
        b.add_kw(k, boost);
    }
    for k in MULTI_WORD {
        b.add_kw(k, boost);
    }
}

/// (name, signature). Name followed by `(` in the signature inserts `name()`.
const FUNCTIONS: &[(&str, &str)] = &[
    ("COUNT", "COUNT(expr)"),
    ("SUM", "SUM(expr)"),
    ("AVG", "AVG(expr)"),
    ("MIN", "MIN(expr)"),
    ("MAX", "MAX(expr)"),
    ("COALESCE", "COALESCE(a, b, ...)"),
    ("NULLIF", "NULLIF(a, b)"),
    ("CAST", "CAST(expr AS type)"),
    ("LOWER", "LOWER(text)"),
    ("UPPER", "UPPER(text)"),
    ("TRIM", "TRIM(text)"),
    ("LENGTH", "LENGTH(text)"),
    ("SUBSTRING", "SUBSTRING(text, start, len)"),
    ("REPLACE", "REPLACE(text, from, to)"),
    ("CONCAT", "CONCAT(a, b, ...)"),
    ("ROUND", "ROUND(x, digits)"),
    ("ABS", "ABS(x)"),
    ("FLOOR", "FLOOR(x)"),
    ("CEIL", "CEIL(x)"),
    ("NOW", "NOW()"),
    ("CURRENT_DATE", "CURRENT_DATE"),
    ("CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP"),
    ("ROW_NUMBER", "ROW_NUMBER() OVER (...)"),
    ("RANK", "RANK() OVER (...)"),
    ("DENSE_RANK", "DENSE_RANK() OVER (...)"),
    ("LAG", "LAG(expr, offset)"),
    ("LEAD", "LEAD(expr, offset)"),
    ("EXTRACT", "EXTRACT(field FROM date)"),
];

fn dialect_functions(d: Dialect) -> &'static [(&'static str, &'static str)] {
    match d {
        Dialect::MySql => &[
            ("IFNULL", "IFNULL(a, b)"),
            ("IF", "IF(cond, a, b)"),
            ("DATE_FORMAT", "DATE_FORMAT(date, format)"),
            ("DATE_ADD", "DATE_ADD(date, INTERVAL n unit)"),
            ("DATEDIFF", "DATEDIFF(a, b)"),
            ("GROUP_CONCAT", "GROUP_CONCAT(expr SEPARATOR ',')"),
            ("JSON_EXTRACT", "JSON_EXTRACT(doc, path)"),
            ("CURDATE", "CURDATE()"),
            ("UNIX_TIMESTAMP", "UNIX_TIMESTAMP(date)"),
        ],
        Dialect::Postgres => &[
            ("STRING_AGG", "STRING_AGG(expr, sep)"),
            ("ARRAY_AGG", "ARRAY_AGG(expr)"),
            ("TO_CHAR", "TO_CHAR(value, format)"),
            ("DATE_TRUNC", "DATE_TRUNC(field, ts)"),
            ("GENERATE_SERIES", "GENERATE_SERIES(start, stop, step)"),
            ("JSONB_BUILD_OBJECT", "JSONB_BUILD_OBJECT(k, v, ...)"),
            ("JSONB_AGG", "JSONB_AGG(expr)"),
            ("AGE", "AGE(ts)"),
        ],
        Dialect::Sqlite => &[
            ("IFNULL", "IFNULL(a, b)"),
            ("STRFTIME", "STRFTIME(format, time)"),
            ("DATETIME", "DATETIME(time)"),
            ("GROUP_CONCAT", "GROUP_CONCAT(expr, sep)"),
            ("JSON_EXTRACT", "JSON_EXTRACT(doc, path)"),
        ],
        Dialect::SqlServer => &[
            ("ISNULL", "ISNULL(a, b)"),
            ("GETDATE", "GETDATE()"),
            ("DATEADD", "DATEADD(part, n, date)"),
            ("DATEDIFF", "DATEDIFF(part, a, b)"),
            ("FORMAT", "FORMAT(value, format)"),
            ("STRING_AGG", "STRING_AGG(expr, sep)"),
            ("LEN", "LEN(text)"),
            ("IIF", "IIF(cond, a, b)"),
        ],
        Dialect::Oracle => &[
            ("NVL", "NVL(a, b)"),
            ("NVL2", "NVL2(x, a, b)"),
            ("DECODE", "DECODE(x, k1, v1, ..., default)"),
            ("TO_CHAR", "TO_CHAR(value, format)"),
            ("TO_DATE", "TO_DATE(text, format)"),
            ("LISTAGG", "LISTAGG(expr, sep)"),
            ("SYSDATE", "SYSDATE"),
            ("TRUNC", "TRUNC(x)"),
        ],
        Dialect::Generic => &[
            ("DATE_TRUNC", "DATE_TRUNC(part, ts)"),
            ("STRING_AGG", "STRING_AGG(expr, sep)"),
            ("LIST", "LIST(expr)"),
            ("STRFTIME", "STRFTIME(ts, format)"),
            ("READ_PARQUET", "READ_PARQUET('file.parquet')"),
            ("READ_CSV", "READ_CSV('file.csv')"),
            ("ARRAY_JOIN", "arrayJoin(arr)"),
            ("TO_DATE", "toDate(x)"),
        ],
    }
}

fn add_functions(b: &mut Builder, d: Dialect, boost: i32) {
    if b.quoted {
        return;
    }
    for (name, sig) in FUNCTIONS.iter().chain(dialect_functions(d)) {
        if name.eq_ignore_ascii_case(&b.prefix) {
            continue;
        }
        // ClickHouse-style camelCase names come from the signature
        let label = sig.split('(').next().unwrap_or(name).trim().to_string();
        let Some(s) = fuzzy_score(&label, &b.prefix) else { continue };
        let call = sig.contains('(') && !sig.starts_with(&format!("{}'", label));
        let (insert, back) = if call { (format!("{}()", label), 1) } else { (label.clone(), 0) };
        b.items.push(Item { label, insert, cursor_back: back, kind: ItemKind::Function, detail: sig.to_string(), score: s + boost });
    }
}

// ---------------------------------------------------------------------------
// Join conditions

/// Name a reference is written with in expressions (its alias, else its name).
fn ref_label(r: &Ref, d: Dialect) -> String {
    let l = r.alias.as_deref().unwrap_or(&r.name);
    if needs_quote(l, d) { d.quote_ident(l) } else { l.to_string() }
}

/// `user_id` style names a table's rows are referred to by: `users` → `user`, `categories` → `category`.
fn singular_stems(table: &str) -> Vec<String> {
    let t = table.to_lowercase();
    let mut v = vec![t.clone()];
    if let Some(s) = t.strip_suffix("ies") {
        v.push(format!("{}y", s));
    } else if let Some(s) = t.strip_suffix('s') {
        v.push(s.to_string());
    }
    v
}

/// `ON` conditions linking the table joined last to each table before it:
/// declared foreign keys (either direction), else `<table>_id = id` naming.
fn add_join_conditions(b: &mut Builder, refs: &[Ref], cursor: usize, ctes: &[Cte], cat: &mut dyn Catalog) {
    let Some(ni) = refs.iter().rposition(|r| r.at < cursor) else { return };
    let new = &refs[ni];
    let d = b.dialect;
    let col = |c: &str| if needs_quote(c, d) { d.quote_ident(c) } else { c.to_string() };
    let is_table = |r: &Ref| r.derived.is_none() && !(r.qualifier.is_none() && ctes.iter().any(|c| c.name.eq_ignore_ascii_case(&r.name)));
    for (k, old) in refs[..ni].iter().enumerate().rev() {
        let (nl, ol) = (ref_label(new, d), ref_label(old, d));
        let mut found = false;
        // (table holding the key, referenced table, key on the new side)
        for (from, to, from_new) in [(new, old, true), (old, new, false)] {
            if !is_table(from) || !is_table(to) {
                continue;
            }
            let Some(fks) = cat.foreign_keys(from.qualifier.as_deref(), &from.name) else {
                b.incomplete = true;
                continue;
            };
            for fk in fks.iter().filter(|fk| fk.ref_table.eq_ignore_ascii_case(&to.name) && !fk.columns.is_empty()) {
                let cond = fk
                    .columns
                    .iter()
                    .zip(&fk.ref_columns)
                    .map(|(c, rc)| if from_new { format!("{}.{} = {}.{}", nl, col(c), ol, col(rc)) } else { format!("{}.{} = {}.{}", nl, col(rc), ol, col(c)) })
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let detail = if fk.name.is_empty() { "foreign key".to_string() } else { format!("FK {}", fk.name) };
                b.add_join(&cond, &detail, 500 - k as i32);
                found = true;
            }
        }
        if found {
            continue;
        }
        let (Some(nc), Some(oc)) = (ref_columns(new, ctes, cat), ref_columns(old, ctes, cat)) else {
            b.incomplete = true;
            continue;
        };
        let has = |cols: &[(String, String)], n: &str| cols.iter().find(|(c, _)| c.eq_ignore_ascii_case(n)).map(|(c, _)| c.clone());
        for (from_cols, to_cols, to, from_new) in [(&nc, &oc, old, true), (&oc, &nc, new, false)] {
            let Some(id) = has(to_cols, "id") else { continue };
            for stem in singular_stems(&to.name) {
                if let Some(fc) = has(from_cols, &format!("{}_id", stem)) {
                    let cond = if from_new { format!("{}.{} = {}.{}", nl, col(&fc), ol, col(&id)) } else { format!("{}.{} = {}.{}", nl, col(&id), ol, col(&fc)) };
                    b.add_join(&cond, "by name", 400 - k as i32);
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Signature help

#[derive(Clone, Debug, PartialEq)]
pub struct SignatureHelp {
    /// e.g. `DATE_FORMAT(date, format)`.
    pub signature: String,
    /// Char range of each parameter inside `signature`.
    pub params: Vec<(usize, usize)>,
    /// Parameter the cursor is in (may be past the end for unknown arity).
    pub active: usize,
}

fn lookup_function(name: &str, d: Dialect) -> Option<&'static str> {
    FUNCTIONS
        .iter()
        .chain(dialect_functions(d))
        .find(|(n, sig)| n.eq_ignore_ascii_case(name) || sig.split('(').next().is_some_and(|l| l.trim().eq_ignore_ascii_case(name)))
        .map(|(_, sig)| *sig)
}

fn signature_params(sig: &[char]) -> Vec<(usize, usize)> {
    let Some(open) = sig.iter().position(|&c| c == '(') else { return vec![] };
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = open + 1;
    for (i, &c) in sig.iter().enumerate().skip(open + 1) {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            ',' | ')' if depth == 0 => {
                let (mut a, mut e) = (start, i);
                while a < e && sig[a] == ' ' {
                    a += 1;
                }
                while e > a && sig[e - 1] == ' ' {
                    e -= 1;
                }
                if a < e {
                    out.push((a, e));
                }
                start = i + 1;
                if c == ')' {
                    break;
                }
            }
            _ => {}
        }
    }
    out
}

/// Signature of the function call the cursor is inside, with the argument
/// being typed.
pub fn signature_help(text: &str, cursor: usize, dialect: Dialect) -> Option<SignatureHelp> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let toks = tokenize(&chars, dialect);
    if toks.iter().any(|t| t.t == Tk::Comment && t.contains_cursor(cursor)) {
        return None;
    }
    let before: Vec<&Tok> = toks.iter().filter(|t| t.t != Tk::Comment && t.end <= cursor).collect();
    let (mut depth, mut commas) = (0, 0);
    for i in (0..before.len()).rev() {
        match before[i].t {
            Tk::Punct(')') => depth += 1,
            Tk::Punct('(') if depth > 0 => depth -= 1,
            Tk::Punct('(') => {
                let Tk::Word(name) = &before.get(i.checked_sub(1)?)?.t else { return None };
                let sig = lookup_function(name, dialect)?;
                let sc: Vec<char> = sig.chars().collect();
                let params = signature_params(&sc);
                let mut active = commas;
                if active >= params.len() && params.last().is_some_and(|&(a, e)| sc[a..e].iter().collect::<String>().contains("...")) {
                    active = params.len() - 1;
                }
                return Some(SignatureHelp { signature: sig.to_string(), params, active });
            }
            Tk::Punct(',') if depth == 0 => commas += 1,
            Tk::Punct(';') => return None,
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Hover

#[derive(Clone, Debug, PartialEq)]
pub struct Hover {
    pub kind: ItemKind,
    pub title: String,
    pub detail: String,
    /// (name, detail) rows: a table's columns.
    pub rows: Vec<(String, String)>,
    /// Some metadata was still loading; ask again for the full picture.
    pub incomplete: bool,
}

/// What the word at char index `at` refers to: a table of the statement (by
/// name or alias), a column, a catalog table or a function.
pub fn hover(text: &str, at: usize, dialect: Dialect, cat: &mut dyn Catalog) -> Option<Hover> {
    let chars: Vec<char> = text.chars().collect();
    let toks: Vec<Tok> = tokenize(&chars, dialect).into_iter().filter(|t| t.t != Tk::Comment).collect();
    let wi = toks.iter().position(|t| t.start <= at && at < t.end && matches!(t.t, Tk::Word(_) | Tk::Quoted(..)))?;
    let seg_start = toks[..wi].iter().rposition(|t| t.t == Tk::Punct(';')).map(|i| i + 1).unwrap_or(0);
    let seg_end = toks[wi..].iter().position(|t| t.t == Tk::Punct(';')).map(|i| i + wi).unwrap_or(toks.len());
    let seg = &toks[seg_start..seg_end];
    let wi = wi - seg_start;
    let tok = &seg[wi];
    let next = seg.get(wi + 1).map(|t| &t.t);

    if let Tk::Word(w) = &tok.t {
        if next == Some(&Tk::Punct('(')) {
            if let Some(sig) = lookup_function(w, dialect) {
                return Some(Hover { kind: ItemKind::Function, title: sig.to_string(), detail: "function".into(), rows: vec![], incomplete: false });
            }
        }
    }
    let name = tok.name()?;
    let mut qual: Vec<String> = Vec::new();
    let mut k = wi;
    while k >= 2 && seg[k - 1].t == Tk::Punct('.') {
        match seg[k - 2].name() {
            Some(n) => qual.insert(0, n),
            None => break,
        }
        k -= 2;
    }
    let ctes = parse_ctes(seg, cat);
    let refs = parse_refs(seg, &ctes, cat);
    let mut incomplete = false;

    // a table: `o` in `o.id`, `orders` / `o` in `FROM orders o`
    let table_hover = |r: &Ref, cat: &mut dyn Catalog, incomplete: &mut bool| -> Hover {
        let cte = r.qualifier.is_none() && ctes.iter().any(|c| c.name.eq_ignore_ascii_case(&r.name));
        let detail = if r.derived.is_some() {
            "derived table".to_string()
        } else if cte {
            "CTE".to_string()
        } else {
            match &r.qualifier {
                Some(q) => format!("table · {}", q),
                None => "table".to_string(),
            }
        };
        let fks = if r.derived.is_none() && !cte { cat.foreign_keys(r.qualifier.as_deref(), &r.name).unwrap_or_default() } else { vec![] };
        let rows = match ref_columns(r, &ctes, cat) {
            Some(cols) => cols.into_iter().map(|(c, d)| { let d = with_fk(d, &c, &fks); (c, d) }).collect(),
            None => {
                *incomplete = true;
                vec![]
            }
        };
        let title = match &r.alias {
            Some(a) => format!("{} {}", r.name, a),
            None => r.name.clone(),
        };
        Hover { kind: ItemKind::Table, title, detail, rows, incomplete: *incomplete }
    };
    let column_hover = |r: &Ref, col: &str, cat: &mut dyn Catalog, incomplete: &mut bool| -> Option<Hover> {
        let Some(cols) = ref_columns(r, &ctes, cat) else {
            *incomplete = true;
            return None;
        };
        let (c, d) = cols.into_iter().find(|(c, _)| c.eq_ignore_ascii_case(col))?;
        let fks = if r.derived.is_none() { cat.foreign_keys(r.qualifier.as_deref(), &r.name).unwrap_or_default() } else { vec![] };
        Some(Hover { kind: ItemKind::Column, title: format!("{}.{}", r.alias.as_deref().unwrap_or(&r.name), c), detail: with_fk(d, &c, &fks), rows: vec![], incomplete: false })
    };
    let by_label = |n: &str| refs.iter().find(|r| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(n))).or_else(|| refs.iter().find(|r| r.name.eq_ignore_ascii_case(n)));
    let unreferenced = |qualifier: Option<&str>, n: &str, cat: &mut dyn Catalog, incomplete: &mut bool| -> Option<Ref> {
        match cat.tables(qualifier) {
            Some(v) => v.into_iter().find(|t| t.name.eq_ignore_ascii_case(n)).map(|t| Ref { qualifier: qualifier.map(String::from), name: t.name, alias: None, derived: None, at: 0 }),
            None => {
                *incomplete = true;
                None
            }
        }
    };

    if next == Some(&Tk::Punct('.')) || qual.is_empty() {
        if let Some(r) = if qual.is_empty() { by_label(&name) } else { None } {
            return Some(table_hover(r, cat, &mut incomplete));
        }
        if qual.is_empty() && next != Some(&Tk::Punct('.')) {
            for r in &refs {
                if let Some(h) = column_hover(r, &name, cat, &mut incomplete) {
                    return Some(h);
                }
            }
        }
        let q = (!qual.is_empty()).then(|| qual.join("."));
        if let Some(r) = unreferenced(q.as_deref(), &name, cat, &mut incomplete) {
            return Some(table_hover(&r, cat, &mut incomplete));
        }
    } else {
        let owner = match find_ref(&refs, &qual) {
            Some(r) => Some(r.clone()),
            None => {
                let parent = (qual.len() > 1).then(|| qual[..qual.len() - 1].join("."));
                unreferenced(parent.as_deref(), qual.last().unwrap(), cat, &mut incomplete)
            }
        };
        if let Some(h) = owner.and_then(|r| column_hover(&r, &name, cat, &mut incomplete)) {
            return Some(h);
        }
    }
    incomplete.then(|| Hover { kind: ItemKind::Word, title: name, detail: "loading…".into(), rows: vec![], incomplete: true })
}

/// Column detail plus the column it references, if it's a foreign key.
fn with_fk(detail: String, col: &str, fks: &[ForeignKeyDef]) -> String {
    for fk in fks {
        if let Some(i) = fk.columns.iter().position(|c| c.eq_ignore_ascii_case(col)) {
            let rc = fk.ref_columns.get(i).map(String::as_str).unwrap_or("");
            return format!("{} → {}.{}", detail, fk.ref_table, rc);
        }
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Mem {
        dbs: Vec<String>,
        schemas: Vec<String>,
        tables: HashMap<String, Vec<(String, Vec<&'static str>)>>,
        fks: HashMap<String, Vec<ForeignKeyDef>>,
        asked: Vec<String>,
    }

    impl Mem {
        fn shop() -> Self {
            let mut m = Mem { dbs: vec!["shop".into(), "analytics".into()], ..Default::default() };
            m.tables.insert(
                String::new(),
                vec![
                    ("users".into(), vec!["id", "user_name", "email", "created_at"]),
                    ("orders".into(), vec!["id", "user_id", "total", "status"]),
                    ("order_items".into(), vec!["order_id", "product_id", "qty"]),
                ],
            );
            m.tables.insert("analytics".into(), vec![("events".into(), vec!["event_id", "kind", "ts"])]);
            let fk = ForeignKeyDef { name: "fk_item_order".into(), columns: vec!["order_id".into()], ref_table: "orders".into(), ref_columns: vec!["id".into()], ..Default::default() };
            m.fks.insert("order_items".into(), vec![fk]);
            m
        }
    }

    impl Catalog for Mem {
        fn databases(&mut self) -> Option<Vec<String>> {
            Some(self.dbs.clone())
        }
        fn schemas(&mut self) -> Option<Vec<String>> {
            Some(self.schemas.clone())
        }
        fn tables(&mut self, q: Option<&str>) -> Option<Vec<TableEntry>> {
            let v = self.tables.get(q.unwrap_or(""))?;
            Some(v.iter().map(|(n, _)| TableEntry { name: n.clone(), view: false }).collect())
        }
        fn columns(&mut self, q: Option<&str>, t: &str) -> Option<Vec<ColumnDef>> {
            self.asked.push(format!("{}.{}", q.unwrap_or(""), t));
            let v = self.tables.get(q.unwrap_or(""))?;
            let (_, cols) = v.iter().find(|(n, _)| n.eq_ignore_ascii_case(t))?;
            Some(cols.iter().map(|c| ColumnDef { primary_key: *c == "id", ..ColumnDef::new(*c, "int") }).collect())
        }
        fn foreign_keys(&mut self, _: Option<&str>, t: &str) -> Option<Vec<ForeignKeyDef>> {
            Some(self.fks.get(t).cloned().unwrap_or_default())
        }
    }

    /// `|` marks the cursor.
    fn run(sql: &str, explicit: bool) -> Vec<Item> {
        run_d(sql, explicit, Dialect::MySql)
    }

    fn run_d(sql: &str, explicit: bool, d: Dialect) -> Vec<Item> {
        let cur = sql.chars().position(|c| c == '|').unwrap();
        let text: String = sql.replacen('|', "", 1);
        complete(&text, cur, d, &mut Mem::shop(), explicit, &[]).map(|c| c.items).unwrap_or_default()
    }

    fn labels(items: &[Item], kind: ItemKind) -> Vec<String> {
        items.iter().filter(|i| i.kind == kind).map(|i| i.label.clone()).collect()
    }

    #[test]
    fn tables_after_from_and_join() {
        let v = run("SELECT * FROM |", true);
        assert_eq!(v[0].kind, ItemKind::Table);
        assert!(labels(&v, ItemKind::Column).is_empty());
        assert!(labels(&v, ItemKind::Table).contains(&"orders".to_string()));
        let v = run("select * from users u join or|", false);
        assert_eq!(v[0].label, "orders");
        let v = run("select * from users, ord|", false);
        assert_eq!(v[0].label, "orders");
        let v = run("UPDATE us|", false);
        assert_eq!(v[0].label, "users");
        let v = run("insert into ord|", false);
        assert_eq!(v[0].label, "orders");
    }

    #[test]
    fn keywords_after_table_name() {
        let v = run("SELECT * FROM users u wh|", false);
        assert_eq!(v[0].label, "WHERE");
        assert!(labels(&v, ItemKind::Table).is_empty());
    }

    #[test]
    fn alias_dot_columns() {
        let v = run("SELECT o.| FROM orders o JOIN users u ON u.id = o.user_id", false);
        let cols = labels(&v, ItemKind::Column);
        assert_eq!(cols.len(), 4);
        assert!(!cols.contains(&"email".to_string()));
        let v = run("SELECT u.em| FROM orders o JOIN users u ON u.id = o.user_id", false);
        assert_eq!(v[0].label, "email");
    }

    #[test]
    fn self_join_aliases_resolve_independently() {
        let v = run("SELECT a.id, b.| FROM users a JOIN users b ON a.id = b.id", false);
        assert_eq!(labels(&v, ItemKind::Column).len(), 4);
    }

    #[test]
    fn table_name_without_alias_and_unreferenced_table() {
        let v = run("SELECT * FROM users WHERE users.cr|", false);
        assert_eq!(v[0].label, "created_at");
        let v = run("SELECT order_items.q|", false);
        assert_eq!(v[0].label, "qty");
    }

    #[test]
    fn database_qualifier() {
        let v = run("SELECT * FROM analytics.|", false);
        assert_eq!(labels(&v, ItemKind::Table), vec!["events"]);
        let v = run("SELECT e.| FROM analytics.events e", false);
        assert_eq!(labels(&v, ItemKind::Column), vec!["event_id", "kind", "ts"]);
        let v = run("SELECT analytics.events.k|", false);
        assert_eq!(v[0].label, "kind");
    }

    #[test]
    fn columns_in_expressions_from_all_refs() {
        let v = run("SELECT * FROM orders o JOIN users u ON u.id = o.user_id WHERE tot|", false);
        assert_eq!(v[0].label, "total");
        let v = run("SELECT usrnm| FROM users", false);
        assert_eq!(v[0].label, "user_name");
        // select list sees the FROM that comes after it
        let v = run("SELECT st| FROM orders", false);
        assert_eq!(v[0].label, "status");
    }

    #[test]
    fn subquery_and_derived_table() {
        let v = run("SELECT t.| FROM (SELECT id, total AS amount, user_id uid FROM orders) t", false);
        let cols = labels(&v, ItemKind::Column);
        assert!(cols.contains(&"amount".to_string()) && cols.contains(&"uid".to_string()) && cols.contains(&"id".to_string()));
        assert_eq!(cols.len(), 3);
        // inside a subquery the inner table is visible
        let v = run("SELECT * FROM users WHERE id IN (SELECT user_| FROM orders)", false);
        assert_eq!(v[0].label, "user_id");
        let v = run("SELECT x.| FROM (SELECT * FROM users) x", false);
        assert_eq!(labels(&v, ItemKind::Column).len(), 4);
    }

    #[test]
    fn cte_names_and_columns() {
        let sql = "WITH big AS (SELECT user_id, SUM(total) AS spent FROM orders GROUP BY user_id) SELECT b.| FROM big b";
        let v = run(sql, false);
        let cols = labels(&v, ItemKind::Column);
        assert_eq!(cols.len(), 2);
        assert!(cols.contains(&"spent".to_string()));
        let v = run("WITH big(a, b) AS (SELECT 1, 2) SELECT * FROM bi|", false);
        assert_eq!(v[0].label, "big");
        let v = run("WITH big AS (SELECT 1 AS one) SELECT big.| FROM big", false);
        assert_eq!(labels(&v, ItemKind::Column), vec!["one"]);
    }

    #[test]
    fn insert_column_list() {
        let v = run("INSERT INTO users (|", true);
        assert_eq!(labels(&v, ItemKind::Column).len(), 4);
        let v = run("INSERT INTO users (id, em|", false);
        assert_eq!(v[0].label, "email");
    }

    #[test]
    fn update_set_columns() {
        let v = run("UPDATE users SET em|", false);
        assert_eq!(v[0].label, "email");
    }

    #[test]
    fn use_database() {
        let v = run("USE an|", false);
        assert_eq!(v[0].label, "analytics");
        assert_eq!(v[0].kind, ItemKind::Database);
    }

    #[test]
    fn statement_start_keywords() {
        let v = run("sel|", false);
        assert_eq!(v[0].label, "SELECT");
        let v = run("SELECT 1; ins|", false);
        assert_eq!(v[0].label, "INSERT INTO");
    }

    #[test]
    fn functions_insert_parens() {
        let v = run("SELECT cou| FROM users", false);
        let f = v.iter().find(|i| i.kind == ItemKind::Function && i.label == "COUNT").unwrap();
        assert_eq!(f.insert, "COUNT()");
        assert_eq!(f.cursor_back, 1);
        let v = run_d("SELECT nv| FROM users", false, Dialect::Oracle);
        assert!(v.iter().any(|i| i.label == "NVL"));
    }

    #[test]
    fn no_completion_in_strings_and_comments() {
        assert!(run("SELECT 'us|", false).is_empty());
        assert!(run("SELECT 1 -- fro|", false).is_empty());
        assert!(run("SELECT /* fro| */ 1", false).is_empty());
    }

    #[test]
    fn exact_word_is_not_offered() {
        // Enter after a complete word must insert a newline, not "accept" the same word
        let v = run("SELECT * FROM users|", false);
        assert!(!v.iter().any(|i| i.label == "users"));
        // ...nor a longer fuzzy match of it (`id` must not become `user_id` on Enter)
        assert!(run("SELECT o.id| FROM orders o", false).is_empty());
        assert!(!run("SELECT o.id| FROM orders o", true).is_empty());
    }

    #[test]
    fn nothing_after_as() {
        assert!(run("SELECT COUNT(*) AS total|", false).is_empty());
        assert!(run("SELECT * FROM users AS us|", false).is_empty());
    }

    #[test]
    fn only_current_statement_counts() {
        let v = run("SELECT * FROM users u; SELECT u.| FROM orders", false);
        assert!(!labels(&v, ItemKind::Column).contains(&"email".to_string()));
    }

    #[test]
    fn quoting_rules() {
        assert!(needs_quote("order", Dialect::MySql));
        assert!(needs_quote("my table", Dialect::MySql));
        assert!(!needs_quote("users", Dialect::MySql));
        assert!(needs_quote("Users", Dialect::Postgres));
        assert!(!needs_quote("Users", Dialect::MySql));
        assert!(needs_quote("users", Dialect::Oracle));
        let mut m = Mem::shop();
        m.tables.get_mut("").unwrap().push(("Line Items".into(), vec!["x"]));
        let c = complete("SELECT * FROM lin", 17, Dialect::Postgres, &mut m, false, &[]).unwrap();
        assert_eq!(c.items[0].insert, "\"Line Items\"");
        // typed quote: replace including the quote and close it
        let c = complete("SELECT * FROM \"use", 18, Dialect::Postgres, &mut m, false, &[]).unwrap();
        assert_eq!(c.start, 14);
        assert_eq!(c.items[0].insert, "\"users\"");
    }

    #[test]
    fn replacement_range_covers_whole_word() {
        let c = complete("SELECT * FROM ordxyz", 17, Dialect::MySql, &mut Mem::shop(), false, &[]).unwrap();
        assert_eq!((c.start, c.end), (14, 20));
    }

    #[test]
    fn columns_only_requested_for_referenced_tables() {
        let mut m = Mem::shop();
        complete("SELECT na FROM users", 9, Dialect::MySql, &mut m, false, &[]);
        assert_eq!(m.asked, vec![".users"]);
    }

    #[test]
    fn incomplete_while_loading() {
        struct Slow;
        impl Catalog for Slow {
            fn databases(&mut self) -> Option<Vec<String>> {
                None
            }
            fn schemas(&mut self) -> Option<Vec<String>> {
                None
            }
            fn tables(&mut self, _: Option<&str>) -> Option<Vec<TableEntry>> {
                None
            }
            fn columns(&mut self, _: Option<&str>, _: &str) -> Option<Vec<ColumnDef>> {
                None
            }
        }
        let c = complete("SELECT * FROM us", 16, Dialect::MySql, &mut Slow, false, &[]).unwrap();
        assert!(c.incomplete);
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("user_name", "usrnm").is_some());
        assert!(fuzzy_score("orderId", "oi").is_some());
        assert!(fuzzy_score("users", "us").unwrap() > fuzzy_score("status", "us").unwrap_or(0));
        assert!(fuzzy_score("user_name", "un").unwrap() > fuzzy_score("unused", "xn").unwrap_or(0));
        assert!(fuzzy_score("email", "zz").is_none());
        // subsequence must start at the first letter
        assert!(fuzzy_score("status", "ts").is_none() || fuzzy_score("status", "ts").unwrap() < 600);
    }

    #[test]
    fn snippets_are_merged() {
        let snip = Item { label: "monthly_report".into(), insert: "SELECT ...".into(), cursor_back: 0, kind: ItemKind::Snippet, detail: "snippet".into(), score: 100 };
        let c = complete("mon", 3, Dialect::MySql, &mut Mem::shop(), false, &[snip]).unwrap();
        assert_eq!(c.items[0].kind, ItemKind::Snippet);
    }

    #[test]
    fn join_conditions_after_on() {
        // foreign key, offered as soon as `ON ` is typed
        let v = run("SELECT * FROM orders o JOIN order_items oi ON |", false);
        assert_eq!(v[0].label, "oi.order_id = o.id");
        assert_eq!(v[0].kind, ItemKind::Join);
        assert!(v.iter().all(|i| i.kind == ItemKind::Join));
        // reverse direction: key held by the earlier table
        let v = run("SELECT * FROM order_items oi JOIN orders o ON |", false);
        assert_eq!(v[0].label, "o.id = oi.order_id");
        // naming convention without a declared key
        let v = run("SELECT * FROM users u JOIN orders o ON |", false);
        assert_eq!(v[0].label, "o.user_id = u.id");
        assert_eq!(v[0].detail, "by name");
        // typing narrows to columns again, conditions stay on top when they match
        let v = run("SELECT * FROM users u JOIN orders o ON o.|", false);
        assert!(labels(&v, ItemKind::Join).is_empty());
        let v = run("SELECT * FROM users u JOIN orders ON |", false);
        assert_eq!(v[0].label, "orders.user_id = u.id");
        // nothing to join to
        assert!(run("SELECT * FROM users u JOIN users ON |", false).is_empty());
    }

    #[test]
    fn signature_help_tracks_argument() {
        let sig = |sql: &str| {
            let cur = sql.chars().position(|c| c == '|').unwrap();
            signature_help(&sql.replacen('|', "", 1), cur, Dialect::MySql)
        };
        let h = sig("SELECT DATE_FORMAT(created_at, |) FROM users").unwrap();
        assert_eq!(h.signature, "DATE_FORMAT(date, format)");
        assert_eq!(h.active, 1);
        let p: Vec<String> = h.params.iter().map(|&(a, e)| h.signature.chars().skip(a).take(e - a).collect()).collect();
        assert_eq!(p, vec!["date", "format"]);
        // nested call: the inner one wins, parens inside args don't count commas
        let h = sig("SELECT IFNULL(COALESCE(a, (b), |").unwrap();
        assert!(h.signature.starts_with("COALESCE"));
        assert_eq!(h.active, 2);
        let h = sig("SELECT IFNULL(COALESCE(a, b), |").unwrap();
        assert_eq!((h.signature.as_str(), h.active), ("IFNULL(a, b)", 1));
        assert!(sig("SELECT * FROM t WHERE id IN (|").is_none());
        assert!(sig("SELECT IFNULL(a, b)|").is_none());
    }

    #[test]
    fn hover_tables_columns_functions() {
        let hov = |sql: &str, word: &str| {
            let at = sql.find(word).unwrap();
            hover(sql, at, Dialect::MySql, &mut Mem::shop())
        };
        let sql = "SELECT o.total, user_name, COUNT(*) FROM orders o JOIN order_items oi ON oi.order_id = o.id JOIN users u";
        let h = hov(sql, "o.total").unwrap();
        assert_eq!((h.kind, h.title.as_str()), (ItemKind::Table, "orders o"));
        assert_eq!(h.rows.len(), 4);
        let h = hov(sql, "total").unwrap();
        assert_eq!((h.kind, h.title.as_str(), h.detail.as_str()), (ItemKind::Column, "o.total", "int"));
        let h = hov(sql, "user_name").unwrap();
        assert_eq!(h.title, "u.user_name");
        let h = hov(sql, "order_id").unwrap();
        assert_eq!(h.detail, "int → orders.id");
        let h = hov(sql, "COUNT").unwrap();
        assert_eq!(h.kind, ItemKind::Function);
        let h = hov("SELECT 1 FROM users", "users").unwrap();
        assert_eq!(h.rows[0], ("id".to_string(), "int · PK".to_string()));
        // table not in FROM yet
        let h = hov("SELECT order_items.qty", "qty").unwrap();
        assert_eq!(h.title, "order_items.qty");
        assert!(hov("SELECT 1 FROM users", "SELECT").is_none());
        assert!(hov("SELECT nope FROM users", "nope").is_none());
    }
}
