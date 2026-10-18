//! SQL text utilities: statement splitting, beautifying, keyword lists.

use crate::dialect::Dialect;

pub const KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "AUTO_INCREMENT", "BEGIN", "BETWEEN", "BY",
    "CASCADE", "CASE", "CHECK", "COLUMN", "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "DATABASE",
    "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXISTS", "EXPLAIN", "FALSE",
    "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT", "GROUP", "HAVING", "IF", "IN", "INDEX",
    "INNER", "INSERT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "NOT", "NULL",
    "OFFSET", "ON", "OR", "ORDER", "OUTER", "PRIMARY", "PROCEDURE", "REFERENCES", "RENAME",
    "REPLACE", "RETURNING", "REVOKE", "RIGHT", "ROLLBACK", "SCHEMA", "SELECT", "SET", "SHOW",
    "TABLE", "THEN", "TO", "TRIGGER", "TRUE", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USE",
    "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH", "COUNT", "SUM", "AVG", "MIN", "MAX",
    "COALESCE", "CAST", "NOW", "CURRENT_TIMESTAMP", "INTERVAL", "OVER", "PARTITION",
];

/// Split a script into individual statements. Handles quotes, comments,
/// PostgreSQL dollar quoting and MySQL `DELIMITER` directives.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut delimiter = ";".to_string();
    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    // BEGIN...END depth for SQLite triggers (statements contain ';').
    let mut at_line_start = true;

    while i < n {
        let c = chars[i];

        // MySQL DELIMITER directive (must start a line).
        if dialect == Dialect::MySql && at_line_start && cur.trim().is_empty() {
            let rest: String = chars[i..].iter().take(10).collect();
            if rest.to_ascii_uppercase().starts_with("DELIMITER ") {
                let mut j = i + 10;
                let mut d = String::new();
                while j < n && chars[j] != '\n' {
                    d.push(chars[j]);
                    j += 1;
                }
                let d = d.trim().to_string();
                if !d.is_empty() {
                    delimiter = d;
                }
                cur.clear();
                i = j;
                continue;
            }
        }

        at_line_start = c == '\n' || (at_line_start && c.is_whitespace());

        // Comments
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                cur.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if c == '#' && dialect == Dialect::MySql {
            while i < n && chars[i] != '\n' {
                cur.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            cur.push_str("/*");
            i += 2;
            while i < n && !(chars[i] == '*' && i + 1 < n && chars[i + 1] == '/') {
                cur.push(chars[i]);
                i += 1;
            }
            if i < n {
                cur.push_str("*/");
                i += 2;
            }
            continue;
        }
        // Quoted strings / identifiers
        if c == '\'' || c == '"' || c == '`' {
            cur.push(c);
            i += 1;
            while i < n {
                let ch = chars[i];
                cur.push(ch);
                i += 1;
                if ch == '\\' && dialect == Dialect::MySql && c != '`' && i < n {
                    cur.push(chars[i]);
                    i += 1;
                    continue;
                }
                if ch == c {
                    if i < n && chars[i] == c {
                        cur.push(chars[i]);
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            continue;
        }
        // Dollar quoting $tag$ ... $tag$
        if c == '$' && dialect == Dialect::Postgres {
            let mut j = i + 1;
            while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j < n && chars[j] == '$' {
                let tag: String = chars[i..=j].iter().collect();
                cur.push_str(&tag);
                i = j + 1;
                let tag_chars: Vec<char> = tag.chars().collect();
                while i < n {
                    if chars[i] == '$' && chars[i..].starts_with(&tag_chars) {
                        cur.push_str(&tag);
                        i += tag_chars.len();
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
        }
        // SQLite: CREATE TRIGGER ... BEGIN ... END; contains semicolons.
        let dchars: Vec<char> = delimiter.chars().collect();
        if chars[i..].starts_with(&dchars) {
            if dialect == Dialect::Sqlite && delimiter == ";" && in_trigger_body(&cur) {
                cur.push(';');
                i += 1;
                continue;
            }
            let stmt = cur.trim().to_string();
            if !is_blank_or_comment(&stmt) {
                out.push(stmt);
            }
            cur.clear();
            i += dchars.len();
            continue;
        }
        cur.push(c);
        i += 1;
    }
    let stmt = cur.trim().to_string();
    if !is_blank_or_comment(&stmt) {
        out.push(stmt);
    }
    out
}

fn in_trigger_body(cur: &str) -> bool {
    let up = cur.to_ascii_uppercase();
    if !up.contains("CREATE") || !up.contains("TRIGGER") {
        return false;
    }
    let words: Vec<&str> = up.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
    let mut depth = 0i32;
    for w in words {
        match w {
            "BEGIN" | "CASE" => depth += 1,
            "END" => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

fn is_blank_or_comment(s: &str) -> bool {
    let mut rest = s.trim();
    loop {
        if rest.is_empty() {
            return true;
        }
        if rest.starts_with("--") || rest.starts_with('#') {
            match rest.find('\n') {
                Some(p) => rest = rest[p + 1..].trim(),
                None => return true,
            }
        } else if rest.starts_with("/*") && !rest.starts_with("/*!") {
            match rest.find("*/") {
                Some(p) => rest = rest[p + 2..].trim(),
                None => return true,
            }
        } else {
            return false;
        }
    }
}

/// Returns the statement containing the character offset `pos` (Run Current).
pub fn statement_at(sql: &str, pos: usize, dialect: Dialect) -> Option<String> {
    let stmts = split_statements(sql, dialect);
    let mut search_from = 0usize;
    for s in &stmts {
        if let Some(found) = sql[search_from..].find(s.as_str()) {
            let start = search_from + found;
            let end = start + s.len();
            if pos <= end + 1 {
                return Some(s.clone());
            }
            search_from = end;
        }
    }
    stmts.last().cloned()
}

/// Whether a statement returns rows (used to decide paging / limits).
pub fn is_select_like(stmt: &str) -> bool {
    let s = strip_leading_comments(stmt).to_ascii_uppercase();
    ["SELECT", "WITH", "SHOW", "EXPLAIN", "DESCRIBE", "DESC ", "PRAGMA", "VALUES", "TABLE "]
        .iter()
        .any(|k| s.starts_with(k))
}

pub fn strip_leading_comments(s: &str) -> &str {
    let mut rest = s.trim_start();
    loop {
        if rest.starts_with("--") || rest.starts_with('#') {
            match rest.find('\n') {
                Some(p) => rest = rest[p + 1..].trim_start(),
                None => return "",
            }
        } else if rest.starts_with("/*") {
            match rest.find("*/") {
                Some(p) => rest = rest[p + 2..].trim_start(),
                None => return "",
            }
        } else {
            return rest;
        }
    }
}

/// Simple SQL beautifier (Beautify SQL): upper-cases keywords and breaks
/// lines before major clauses. Strings, quoted identifiers and comments are
/// left untouched.
pub fn beautify(sql: &str, uppercase: bool) -> String {
    const BREAK_BEFORE: &[&str] = &[
        "SELECT", "FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "UNION", "VALUES",
        "SET", "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "FULL JOIN", "CROSS JOIN", "JOIN", "AND",
        "OR", "ON", "OFFSET", "RETURNING",
    ];
    let tokens = tokenize(sql);
    let mut out = String::new();
    let mut paren_depth: i32 = 0;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        match t {
            Tok::Word(w) => {
                let up = w.to_ascii_uppercase();
                // two-word clauses
                let mut clause = up.clone();
                let mut consumed = 1;
                if let Some(Tok::Space) = tokens.get(i + 1) {
                    if let Some(Tok::Word(w2)) = tokens.get(i + 2) {
                        let two = format!("{} {}", up, w2.to_ascii_uppercase());
                        if BREAK_BEFORE.contains(&two.as_str()) {
                            clause = two;
                            consumed = 3;
                        }
                    }
                }
                let is_kw = KEYWORDS.contains(&up.as_str()) || BREAK_BEFORE.contains(&clause.as_str());
                if BREAK_BEFORE.contains(&clause.as_str()) && paren_depth == 0 && !out.trim().is_empty() {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                    if matches!(clause.as_str(), "AND" | "OR" | "ON") {
                        out.push_str("  ");
                    }
                }
                let text = if consumed == 3 {
                    if uppercase { clause.clone() } else { format!("{} {}", w, word_of(&tokens[i + 2])) }
                } else if is_kw && uppercase {
                    up
                } else {
                    w.clone()
                };
                out.push_str(&text);
                i += consumed;
                continue;
            }
            Tok::Space => {
                if !out.ends_with(' ') && !out.ends_with('\n') {
                    out.push(' ');
                }
            }
            Tok::Punct(p) => {
                if *p == '(' {
                    paren_depth += 1;
                } else if *p == ')' {
                    paren_depth -= 1;
                }
                if *p == ',' {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str(", ");
                    if let Some(Tok::Space) = tokens.get(i + 1) {
                        i += 1;
                    }
                } else if *p == ';' {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str(";\n\n");
                    if let Some(Tok::Space) = tokens.get(i + 1) {
                        i += 1;
                    }
                } else {
                    out.push(*p);
                }
            }
            Tok::Raw(r) => out.push_str(r),
        }
        i += 1;
    }
    out.trim().to_string()
}

fn word_of(t: &Tok) -> String {
    match t {
        Tok::Word(w) => w.clone(),
        _ => String::new(),
    }
}

#[derive(Debug)]
enum Tok {
    Word(String),
    Space,
    Punct(char),
    Raw(String),
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            out.push(Tok::Space);
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '@' || c == '$' || c == '.' {
            let mut w = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || "_@$.".contains(chars[i])) {
                w.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Word(w));
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let mut s = String::from(c);
            i += 1;
            while i < chars.len() {
                s.push(chars[i]);
                if chars[i] == c {
                    if i + 1 < chars.len() && chars[i + 1] == c {
                        s.push(c);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Tok::Raw(s));
            continue;
        }
        if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            let mut s = String::new();
            while i < chars.len() && chars[i] != '\n' {
                s.push(chars[i]);
                i += 1;
            }
            s.push('\n');
            out.push(Tok::Raw(s));
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            let mut s = String::new();
            while i < chars.len() {
                s.push(chars[i]);
                if chars[i] == '/' && s.len() > 2 && s.ends_with("*/") {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Tok::Raw(s));
            continue;
        }
        out.push(Tok::Punct(c));
        i += 1;
    }
    out
}

/// Collapse whitespace into a single line (Minify SQL).
pub fn minify(sql: &str) -> String {
    let mut out = String::new();
    for t in tokenize(sql) {
        match t {
            Tok::Word(w) => out.push_str(&w),
            Tok::Space => {
                if !out.ends_with(' ') {
                    out.push(' ')
                }
            }
            Tok::Punct(p) => out.push(p),
            Tok::Raw(r) => {
                if r.starts_with("--") {
                    continue;
                }
                out.push_str(&r)
            }
        }
    }
    out.trim().to_string()
}

/// Toggle `-- ` line comments on each line of `text`.
pub fn toggle_comment(text: &str) -> String {
    let all_commented = text.lines().filter(|l| !l.trim().is_empty()).all(|l| l.trim_start().starts_with("--"));
    text.lines()
        .map(|l| {
            if all_commented {
                let idx = l.find("--").unwrap_or(0);
                let rest = &l[idx + 2..];
                format!("{}{}", &l[..idx], rest.strip_prefix(' ').unwrap_or(rest))
            } else if l.trim().is_empty() {
                l.to_string()
            } else {
                format!("-- {}", l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word immediately before the cursor (for auto-completion).
pub fn word_before(text: &str, cursor: usize) -> (usize, String) {
    let bytes: Vec<char> = text.chars().collect();
    let cursor = cursor.min(bytes.len());
    let mut start = cursor;
    while start > 0 && (bytes[start - 1].is_alphanumeric() || bytes[start - 1] == '_') {
        start -= 1;
    }
    (start, bytes[start..cursor].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_basic() {
        let v = split_statements("select 1; select ';'; -- c\nselect 2", Dialect::MySql);
        assert_eq!(v, vec!["select 1", "select ';'", "-- c\nselect 2"]);
    }

    #[test]
    fn split_delimiter() {
        let sql = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2;";
        let v = split_statements(sql, Dialect::MySql);
        assert_eq!(v.len(), 2);
        assert!(v[0].starts_with("CREATE PROCEDURE"));
        assert_eq!(v[1], "SELECT 2");
    }

    #[test]
    fn split_dollar() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END $$ LANGUAGE plpgsql; SELECT 1";
        let v = split_statements(sql, Dialect::Postgres);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn split_sqlite_trigger() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON a BEGIN UPDATE b SET x=1; DELETE FROM c; END; SELECT 1;";
        let v = split_statements(sql, Dialect::Sqlite);
        assert_eq!(v.len(), 2, "{:?}", v);
    }

    #[test]
    fn comments_only_dropped() {
        assert!(split_statements("-- nothing\n/* x */", Dialect::Postgres).is_empty());
    }

    #[test]
    fn beautify_breaks() {
        let b = beautify("select a, b from t where x = 1 and y = 'from' order by a", true);
        assert!(b.contains("\nFROM t"));
        assert!(b.contains("\nWHERE"));
        assert!(b.contains("'from'"));
        assert!(b.contains("\nORDER BY a"));
    }

    #[test]
    fn select_like() {
        assert!(is_select_like("  -- hi\n select 1"));
        assert!(!is_select_like("update t set a=1"));
    }

    #[test]
    fn comment_toggle() {
        assert_eq!(toggle_comment("a\nb"), "-- a\n-- b");
        assert_eq!(toggle_comment("-- a\n-- b"), "a\nb");
    }
}
