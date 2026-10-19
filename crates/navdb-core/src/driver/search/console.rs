//! Kibana / Dev Tools console syntax: `METHOD /path[?query]` followed by an
//! optional JSON body (several JSON documents for `_bulk` / `_msearch`).

use anyhow::{anyhow, bail, Result};

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleRequest {
    pub method: String,
    /// Path with leading `/` and optional query string.
    pub path: String,
    pub body: Option<String>,
    /// Body is newline-delimited JSON (`_bulk`, `_msearch`).
    pub ndjson: bool,
    /// The request as typed (for `QueryResult::sql`).
    pub text: String,
}

fn method_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let (m, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    let m = m.to_ascii_uppercase();
    if !METHODS.contains(&m.as_str()) {
        return None;
    }
    let rest = rest.trim();
    // the path is one token; `DELETE FROM t ...` is SQL
    if rest.is_empty() || rest.starts_with('{') || rest.contains(char::is_whitespace) {
        return None;
    }
    Some((m, rest.to_string()))
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

/// Whether `text` is console syntax (first meaningful line is `METHOD path`).
pub fn is_console(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty() && !is_comment(l))
        .and_then(method_line)
        .is_some()
}

/// Normalize a typed path: leading `/`, spaces encoded.
pub fn normalize_path(p: &str) -> String {
    let p = p.trim();
    let p = if p.starts_with('/') { p.to_string() } else { format!("/{}", p) };
    p.replace(' ', "%20")
}

fn is_ndjson_path(path: &str) -> bool {
    let p = path.split('?').next().unwrap_or("");
    p.ends_with("/_bulk") || p.ends_with("/_msearch") || p.ends_with("/_msearch/template") || p == "_bulk"
}

/// Split console text into requests.
pub fn parse(text: &str) -> Result<Vec<ConsoleRequest>> {
    let mut out = Vec::new();
    let mut cur: Option<(String, String, Vec<String>, Vec<String>)> = None;
    let flush = |cur: Option<(String, String, Vec<String>, Vec<String>)>, out: &mut Vec<ConsoleRequest>| -> Result<()> {
        if let Some((method, path, body, raw)) = cur {
            out.push(build(method, path, &body.join("\n"), raw.join("\n").trim().to_string())?);
        }
        Ok(())
    };
    for line in text.lines() {
        if let Some((m, p)) = method_line(line) {
            flush(cur.take(), &mut out)?;
            cur = Some((m, p, vec![], vec![line.to_string()]));
            continue;
        }
        if is_comment(line) {
            continue;
        }
        match &mut cur {
            Some((_, _, body, raw)) => {
                raw.push(line.to_string());
                if !line.trim().is_empty() {
                    body.push(line.to_string());
                }
            }
            None if line.trim().is_empty() => {}
            None => bail!("expected `METHOD /path`, found: {}", line.trim()),
        }
    }
    flush(cur, &mut out)?;
    Ok(out)
}

fn build(method: String, path: String, body: &str, text: String) -> Result<ConsoleRequest> {
    let path = normalize_path(&path);
    let ndjson = is_ndjson_path(&path);
    let body = body.trim();
    if body.is_empty() {
        return Ok(ConsoleRequest { method, path, body: None, ndjson, text });
    }
    let docs = serde_json::Deserializer::from_str(body)
        .into_iter::<serde_json::Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("invalid JSON body for {} {}: {}", method, path, e))?;
    let body = if ndjson {
        let mut s = String::new();
        for d in &docs {
            s.push_str(&d.to_string());
            s.push('\n');
        }
        s
    } else {
        match docs.as_slice() {
            [one] => one.to_string(),
            _ => bail!("{} {} expects a single JSON body, found {} documents", method, path, docs.len()),
        }
    };
    Ok(ConsoleRequest { method, path, body: Some(body), ndjson, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        let t = "# comment\nGET _cat/indices\n\nPOST /idx/_search?size=2\n{\n  \"query\": {\"match_all\": {}}\n}\nPOST _bulk\n{\"index\":{\"_index\":\"a\"}}\n{\"x\":1}\n\nDELETE /a";
        assert!(is_console(t));
        let r = parse(t).unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].path, "/_cat/indices");
        assert_eq!(r[0].body, None);
        assert_eq!(r[1].method, "POST");
        assert_eq!(r[1].path, "/idx/_search?size=2");
        assert_eq!(r[1].body.as_deref(), Some(r#"{"query":{"match_all":{}}}"#));
        assert!(r[2].ndjson);
        assert_eq!(r[2].body.as_deref(), Some("{\"index\":{\"_index\":\"a\"}}\n{\"x\":1}\n"));
        assert_eq!(r[3].method, "DELETE");
    }

    #[test]
    fn detects_sql() {
        assert!(!is_console("SELECT * FROM t"));
        assert!(!is_console("DELETE FROM t WHERE a = 1"));
        assert!(is_console("delete /t"));
        assert!(parse("GET /x\n{bad").is_err());
        assert!(parse("GET /x\n{}\n{}").is_err());
    }
}
