//! Vertica frontend/backend protocol (a PostgreSQL v3 derivative), simple
//! query flow only, text result format.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Fixed startup version; servers >= 3.7 negotiate via the `protocol_version` parameter.
const FIXED_PROTOCOL: u32 = 3 << 16 | 5;
const REQUESTED_PROTOCOL: u32 = 3 << 16 | 16;
const SSL_REQUEST: u32 = 80877103;

const AUTH_OK: u32 = 0;
const AUTH_CLEARTEXT: u32 = 3;
const AUTH_MD5: u32 = 5;
const AUTH_CHANGE_PASSWORD: u32 = 9;
const AUTH_PASSWORD_GRACE: u32 = 11;
const AUTH_HASH: u32 = 65536;
const AUTH_HASH_MD5: u32 = 65536 + 5;
const AUTH_HASH_SHA512: u32 = 65536 + 512;

pub struct Tls {
    pub verify: bool,
    pub ca_cert: String,
}

pub struct Options {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub label: String,
    pub autocommit: bool,
    pub workload: String,
    pub timeout: Duration,
    pub tls: Option<Tls>,
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub name: String,
    pub type_oid: u32,
    /// Name from the type pool for user-defined types (GEOMETRY, ...).
    pub user_type: Option<String>,
    pub type_mod: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    /// `Some` when the statement returned a result set.
    pub fields: Option<Vec<Field>>,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    pub tag: String,
}

/// ErrorResponse / NoticeResponse fields keyed by their one-byte code.
#[derive(Clone, Debug, Default)]
pub struct Notice(pub BTreeMap<u8, String>);

impl Notice {
    fn parse(body: &[u8]) -> Self {
        let mut m = BTreeMap::new();
        let mut pos = 0;
        while pos < body.len() && body[pos] != 0 {
            let code = body[pos];
            let end = body[pos + 1..].iter().position(|b| *b == 0).map(|p| pos + 1 + p).unwrap_or(body.len());
            m.insert(code, String::from_utf8_lossy(&body[pos + 1..end]).into_owned());
            pos = end + 1;
        }
        Notice(m)
    }

    pub fn message(&self) -> String {
        let get = |c: u8| self.0.get(&c).cloned().unwrap_or_default();
        let mut s = format!("{} {}: {}", get(b'S'), get(b'C'), get(b'M'));
        if let Some(d) = self.0.get(&b'D') {
            s.push_str(&format!("\nDETAIL: {}", d));
        }
        if let Some(h) = self.0.get(&b'H') {
            s.push_str(&format!("\nHINT: {}", h));
        }
        s.trim_start().to_string()
    }
}

pub struct Conn {
    io: BufReader<Stream>,
    pub params: BTreeMap<String, String>,
    pub backend_pid: u32,
    pub backend_key: u32,
    /// Transaction status from the last ReadyForQuery (`I`, `T` or `E`).
    pub tx_status: u8,
    pub notices: Vec<String>,
    broken: bool,
}

fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let mut last = None;
    for a in (host, port).to_socket_addrs().with_context(|| format!("resolving {}", host))? {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => anyhow!(e),
        None => anyhow!("no address for {}", host),
    })
    .with_context(|| format!("connecting to {}:{}", host, port))
}

fn cstr(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(s);
    out.push(0);
}

fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 5);
    v.push(tag);
    v.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    v.extend_from_slice(body);
    v
}

pub fn startup_message(o: &Options) -> Vec<u8> {
    let mut body = FIXED_PROTOCOL.to_be_bytes().to_vec();
    cstr(&mut body, b"protocol_version");
    body.extend_from_slice(&REQUESTED_PROTOCOL.to_be_bytes());
    body.push(0);
    let pid = std::process::id().to_string();
    let params: [(&str, &str); 11] = [
        ("user", &o.user),
        ("database", &o.database),
        ("client_label", &o.label),
        ("client_type", "NavDB Studio"),
        ("client_version", env!("CARGO_PKG_VERSION")),
        ("client_os", std::env::consts::OS),
        ("client_pid", &pid),
        ("autocommit", if o.autocommit { "on" } else { "off" }),
        ("binary_data_protocol", "0"),
        ("protocol_features", "{\"request_complex_types\":false}"),
        ("protocol_compat", "VER"),
    ];
    for (k, v) in params {
        cstr(&mut body, k.as_bytes());
        cstr(&mut body, v.as_bytes());
    }
    if !o.workload.is_empty() {
        cstr(&mut body, b"workload");
        cstr(&mut body, o.workload.as_bytes());
    }
    cstr(&mut body, b"auth_category");
    cstr(&mut body, if o.password.is_empty() { b"" } else { b"User" });
    body.push(0);
    let mut msg = (body.len() as u32 + 4).to_be_bytes().to_vec();
    msg.extend_from_slice(&body);
    msg
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Password payload for an authentication request (without the trailing NUL).
pub fn password_payload(code: u32, data: &[u8], user: &str, password: &str) -> Result<Vec<u8>> {
    use md5::Digest as _;
    match code {
        AUTH_CLEARTEXT => Ok(password.as_bytes().to_vec()),
        AUTH_MD5 | AUTH_HASH_MD5 => {
            let salt = data.get(..4).ok_or_else(|| anyhow!("short MD5 salt"))?;
            let first = hex_lower(&md5::Md5::digest([password.as_bytes(), user.as_bytes()].concat()));
            let second = hex_lower(&md5::Md5::digest([first.as_bytes(), salt].concat()));
            Ok(format!("md5{}", second).into_bytes())
        }
        AUTH_HASH | AUTH_HASH_SHA512 => {
            let salt = data.get(..4).ok_or_else(|| anyhow!("short SHA512 salt"))?;
            let len = data.get(4..8).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0) as usize;
            let user_salt = data.get(8..8 + len).ok_or_else(|| anyhow!("short SHA512 user salt"))?;
            let first = hex_lower(&sha2::Sha512::digest([password.as_bytes(), user_salt].concat()));
            let second = hex_lower(&sha2::Sha512::digest([first.as_bytes(), salt].concat()));
            Ok(format!("sha512{}", second).into_bytes())
        }
        c => bail!("unsupported Vertica authentication method {}", c),
    }
}

fn be16(b: &[u8], pos: &mut usize) -> Result<u16> {
    let v = b.get(*pos..*pos + 2).ok_or_else(|| anyhow!("truncated message"))?;
    *pos += 2;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}

fn be32(b: &[u8], pos: &mut usize) -> Result<u32> {
    let v = b.get(*pos..*pos + 4).ok_or_else(|| anyhow!("truncated message"))?;
    *pos += 4;
    Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}

fn be64(b: &[u8], pos: &mut usize) -> Result<u64> {
    let v = b.get(*pos..*pos + 8).ok_or_else(|| anyhow!("truncated message"))?;
    *pos += 8;
    let mut a = [0u8; 8];
    a.copy_from_slice(v);
    Ok(u64::from_be_bytes(a))
}

fn read_cstr(b: &[u8], pos: &mut usize) -> Result<String> {
    let rest = b.get(*pos..).ok_or_else(|| anyhow!("truncated message"))?;
    let end = rest.iter().position(|x| *x == 0).ok_or_else(|| anyhow!("unterminated string"))?;
    let s = String::from_utf8_lossy(&rest[..end]).into_owned();
    *pos += end + 1;
    Ok(s)
}

/// Vertica RowDescription: field count, a pool of user types, then per field
/// name, table oid (+ schema/table names when non-zero), attribute number,
/// (parent attribute number when complex types are enabled), user-type flag,
/// type oid/pool index, size, nullable, identity, type modifier, format.
pub fn parse_row_description(b: &[u8], complex_types: bool) -> Result<Vec<Field>> {
    let mut pos = 0;
    let n = be16(b, &mut pos)? as usize;
    if n == 0 {
        return Ok(vec![]);
    }
    let pool_n = be32(b, &mut pos)? as usize;
    let mut pool = Vec::with_capacity(pool_n);
    for _ in 0..pool_n {
        let oid = be32(b, &mut pos)?;
        pool.push((oid, read_cstr(b, &mut pos)?));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let name = read_cstr(b, &mut pos)?;
        let table_oid = be64(b, &mut pos)?;
        if table_oid != 0 {
            read_cstr(b, &mut pos)?;
            read_cstr(b, &mut pos)?;
        }
        be16(b, &mut pos)?;
        let parent = if complex_types { be16(b, &mut pos)? } else { 0 };
        let user = *b.get(pos).ok_or_else(|| anyhow!("truncated message"))?;
        pos += 1;
        let oid = be32(b, &mut pos)?;
        be16(b, &mut pos)?; // size
        be16(b, &mut pos)?; // nullable
        be16(b, &mut pos)?; // identity
        let type_mod = be32(b, &mut pos)? as i32;
        be16(b, &mut pos)?; // format
        if parent != 0 {
            continue;
        }
        let (type_oid, user_type) = if user == 1 {
            let (o, n) = pool.get(oid as usize).cloned().ok_or_else(|| anyhow!("bad type pool index"))?;
            (o, Some(n))
        } else {
            (oid, None)
        };
        out.push(Field { name, type_oid, user_type, type_mod });
    }
    Ok(out)
}

pub fn parse_data_row(b: &[u8]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut pos = 0;
    let n = be16(b, &mut pos)? as usize;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        let len = be32(b, &mut pos)? as i32;
        if len < 0 {
            row.push(None);
        } else {
            let v = b.get(pos..pos + len as usize).ok_or_else(|| anyhow!("truncated data row"))?;
            pos += len as usize;
            row.push(Some(v.to_vec()));
        }
    }
    Ok(row)
}

impl Conn {
    pub fn connect(o: &Options) -> Result<Conn> {
        let tcp = connect_tcp(&o.host, o.port, o.timeout)?;
        tcp.set_read_timeout(Some(o.timeout.max(Duration::from_secs(1)))).ok();
        let stream = match &o.tls {
            None => Stream::Plain(tcp),
            Some(t) => {
                let mut tcp = tcp;
                let mut req = 8u32.to_be_bytes().to_vec();
                req.extend_from_slice(&SSL_REQUEST.to_be_bytes());
                tcp.write_all(&req)?;
                let mut ans = [0u8; 1];
                tcp.read_exact(&mut ans).context("reading SSL response")?;
                if ans[0] != b'S' {
                    bail!("the Vertica server does not accept SSL connections");
                }
                let mut b = native_tls::TlsConnector::builder();
                b.danger_accept_invalid_certs(!t.verify);
                b.danger_accept_invalid_hostnames(!t.verify);
                if !t.ca_cert.is_empty() {
                    let pem = std::fs::read(&t.ca_cert).context("reading CA certificate")?;
                    b.add_root_certificate(native_tls::Certificate::from_pem(&pem)?);
                }
                let tls = b.build()?.connect(&o.host, tcp).map_err(|e| anyhow!("TLS handshake failed: {}", e))?;
                Stream::Tls(Box::new(tls))
            }
        };
        let mut c = Conn {
            io: BufReader::with_capacity(64 * 1024, stream),
            params: BTreeMap::new(),
            backend_pid: 0,
            backend_key: 0,
            tx_status: b'I',
            notices: vec![],
            broken: false,
        };
        c.send(&startup_message(o))?;
        loop {
            let (tag, body) = c.read_msg()?;
            match tag {
                b'R' => {
                    let mut pos = 0;
                    let code = be32(&body, &mut pos)?;
                    match code {
                        AUTH_OK | AUTH_PASSWORD_GRACE => {}
                        AUTH_CHANGE_PASSWORD => bail!("the password for user {} has expired", o.user),
                        _ => {
                            let mut p = password_payload(code, &body[4..], &o.user, &o.password)?;
                            p.push(0);
                            c.send(&frame(b'p', &p))?;
                        }
                    }
                }
                b'K' => {
                    let mut pos = 0;
                    c.backend_pid = be32(&body, &mut pos)?;
                    c.backend_key = be32(&body, &mut pos)?;
                }
                b'Z' => {
                    c.tx_status = body.first().copied().unwrap_or(b'I');
                    break;
                }
                b'E' => bail!("{}", Notice::parse(&body).message()),
                _ => {}
            }
        }
        if let Stream::Plain(s) = c.io.get_ref() {
            s.set_read_timeout(None).ok();
        }
        if let Stream::Tls(s) = c.io.get_ref() {
            s.get_ref().set_read_timeout(None).ok();
        }
        Ok(c)
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    fn send(&mut self, data: &[u8]) -> Result<()> {
        let r = self.io.get_mut().write_all(data).and_then(|_| self.io.get_mut().flush());
        if r.is_err() {
            self.broken = true;
        }
        r.context("sending to Vertica")
    }

    /// Next message; ParameterStatus and NoticeResponse are consumed here.
    fn read_msg(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            let mut head = [0u8; 5];
            if let Err(e) = self.io.read_exact(&mut head) {
                self.broken = true;
                return Err(anyhow!(e).context("connection to Vertica lost"));
            }
            let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if len < 4 {
                self.broken = true;
                bail!("bad message size {}", len);
            }
            let mut body = vec![0u8; len - 4];
            if let Err(e) = self.io.read_exact(&mut body) {
                self.broken = true;
                return Err(anyhow!(e).context("connection to Vertica lost"));
            }
            match head[0] {
                b'S' => {
                    let mut pos = 0;
                    let k = read_cstr(&body, &mut pos)?;
                    let v = read_cstr(&body, &mut pos).unwrap_or_default();
                    self.params.insert(k, v);
                }
                b'N' => self.notices.push(Notice::parse(&body).message()),
                t => return Ok((t, body)),
            }
        }
    }

    fn complex_types(&self) -> bool {
        let v: u32 = self.params.get("protocol_version").and_then(|v| v.parse().ok()).unwrap_or(0);
        v >= (3 << 16 | 12) && self.params.get("request_complex_types").map(|s| s == "on").unwrap_or(false)
    }

    /// Run a simple query (possibly several statements); one `Reply` per statement.
    pub fn simple_query(&mut self, sql: &str) -> Result<Vec<Reply>> {
        let mut body = Vec::with_capacity(sql.len() + 1);
        cstr(&mut body, sql.as_bytes());
        self.send(&frame(b'Q', &body))?;
        let mut out = Vec::new();
        let mut cur = Reply::default();
        let mut err: Option<String> = None;
        loop {
            let (tag, body) = self.read_msg()?;
            match tag {
                b'T' => cur.fields = Some(parse_row_description(&body, self.complex_types())?),
                b'D' => cur.rows.push(parse_data_row(&body)?),
                b'C' => {
                    let mut pos = 0;
                    cur.tag = read_cstr(&body, &mut pos).unwrap_or_default();
                    out.push(std::mem::take(&mut cur));
                }
                b'I' => out.push(std::mem::take(&mut cur)),
                b'E' => {
                    if err.is_none() {
                        err = Some(Notice::parse(&body).message());
                    }
                }
                b'G' => {
                    // COPY ... FROM STDIN: no data stream available here
                    let mut b = Vec::new();
                    cstr(&mut b, b"COPY FROM STDIN is not supported by this client");
                    self.send(&frame(b'f', &b))?;
                }
                b'H' | b'F' | b'O' => {
                    self.broken = true;
                    bail!("COPY FROM LOCAL / RETURNREJECTED is not supported by this client");
                }
                b'Z' => {
                    self.tx_status = body.first().copied().unwrap_or(b'I');
                    break;
                }
                _ => {}
            }
        }
        match err {
            Some(e) => Err(anyhow!(e)),
            None => Ok(out),
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if !self.broken {
            let _ = self.io.get_mut().write_all(&frame(b'X', &[]));
            let _ = self.io.get_mut().flush();
        }
    }
}

/// Decode the text form of BINARY / VARBINARY / LONG VARBINARY (`\ooo` octal escapes, `\\`).
pub fn decode_binary(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            if s.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if let Some(oct) = s.get(i + 1..i + 4).and_then(|o| std::str::from_utf8(o).ok()).and_then(|o| u8::from_str_radix(o, 8).ok()) {
                out.push(oct);
                i += 4;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;

    pub fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
        frame(tag, body)
    }

    pub fn row_description(cols: &[(&str, u32, i32)]) -> Vec<u8> {
        let mut b = (cols.len() as u16).to_be_bytes().to_vec();
        b.extend_from_slice(&0u32.to_be_bytes());
        for (i, (name, oid, tmod)) in cols.iter().enumerate() {
            cstr(&mut b, name.as_bytes());
            if i == 0 {
                b.extend_from_slice(&42u64.to_be_bytes());
                cstr(&mut b, b"public");
                cstr(&mut b, b"t");
            } else {
                b.extend_from_slice(&0u64.to_be_bytes());
            }
            b.extend_from_slice(&(i as u16 + 1).to_be_bytes());
            b.push(0);
            b.extend_from_slice(&oid.to_be_bytes());
            b.extend_from_slice(&8u16.to_be_bytes());
            b.extend_from_slice(&1u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&tmod.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
        }
        msg(b'T', &b)
    }

    pub fn data_row(vals: &[Option<&[u8]>]) -> Vec<u8> {
        let mut b = (vals.len() as u16).to_be_bytes().to_vec();
        for v in vals {
            match v {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(v) => {
                    b.extend_from_slice(&(v.len() as u32).to_be_bytes());
                    b.extend_from_slice(v);
                }
            }
        }
        msg(b'D', &b)
    }

    pub fn error(code: &str, text: &str) -> Vec<u8> {
        let mut b = Vec::new();
        for (k, v) in [(b'S', "ERROR"), (b'C', code), (b'M', text)] {
            b.push(k);
            cstr(&mut b, v.as_bytes());
        }
        b.push(0);
        msg(b'E', &b)
    }

    fn read_frame(s: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut h = [0u8; 5];
        s.read_exact(&mut h).unwrap();
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut b = vec![0u8; len - 4];
        s.read_exact(&mut b).unwrap();
        (h[0], b)
    }

    /// Minimal in-process Vertica server: SHA512 auth, then answers each query
    /// with `answer(sql)`.
    pub fn mock_server(password: &'static str, answer: fn(&str) -> Vec<u8>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut len = [0u8; 4];
            s.read_exact(&mut len).unwrap();
            let mut startup = vec![0u8; u32::from_be_bytes(len) as usize - 4];
            s.read_exact(&mut startup).unwrap();
            assert_eq!(&startup[..4], &FIXED_PROTOCOL.to_be_bytes());
            let text = String::from_utf8_lossy(&startup);
            assert!(text.contains("client_label\0NavDB Studio\0"), "{:?}", text);
            let user_salt = [7u8; 16];
            let mut auth = AUTH_HASH_SHA512.to_be_bytes().to_vec();
            auth.extend_from_slice(b"salt");
            auth.extend_from_slice(&16u32.to_be_bytes());
            auth.extend_from_slice(&user_salt);
            s.write_all(&msg(b'R', &auth)).unwrap();
            let (tag, body) = read_frame(&mut s);
            assert_eq!(tag, b'p');
            let mut data = b"salt".to_vec();
            data.extend_from_slice(&16u32.to_be_bytes());
            data.extend_from_slice(&user_salt);
            let mut want = password_payload(AUTH_HASH_SHA512, &data, "dbadmin", password).unwrap();
            want.push(0);
            if body != want {
                s.write_all(&error("28000", "Invalid username or password")).unwrap();
                return;
            }
            let mut out = msg(b'R', &0u32.to_be_bytes());
            let mut ps = Vec::new();
            cstr(&mut ps, b"server_version");
            cstr(&mut ps, b"v24.1.0-0");
            out.extend(msg(b'S', &ps));
            out.extend(msg(b'K', &[0, 0, 0, 9, 0, 0, 0, 1]));
            out.extend(msg(b'Z', b"I"));
            s.write_all(&out).unwrap();
            loop {
                let (tag, body) = read_frame(&mut s);
                if tag == b'X' {
                    return;
                }
                assert_eq!(tag, b'Q');
                let sql = String::from_utf8_lossy(&body[..body.len() - 1]).into_owned();
                let mut out = answer(&sql);
                out.extend(msg(b'Z', b"I"));
                s.write_all(&out).unwrap();
            }
        });
        port
    }

    pub fn opts(port: u16, password: &str) -> Options {
        Options {
            host: "127.0.0.1".into(),
            port,
            user: "dbadmin".into(),
            password: password.into(),
            database: "VMart".into(),
            label: "NavDB Studio".into(),
            autocommit: true,
            workload: String::new(),
            timeout: Duration::from_secs(5),
            tls: None,
        }
    }

    fn answer(sql: &str) -> Vec<u8> {
        match sql {
            "SELECT 1" => {
                let mut v = row_description(&[("a", 6, -1), ("b", 9, 14)]);
                v.extend(data_row(&[Some(b"1"), None]));
                v.extend(msg(b'C', b"SELECT\0"));
                v
            }
            "" => msg(b'I', &[]),
            _ => error("42601", "Syntax error at or near \"bad\""),
        }
    }

    #[test]
    fn mock_roundtrip() {
        let port = mock_server("secret", answer);
        let mut c = Conn::connect(&opts(port, "secret")).unwrap();
        assert_eq!(c.backend_pid, 9);
        assert_eq!(c.params.get("server_version").map(|s| s.as_str()), Some("v24.1.0-0"));
        let r = c.simple_query("SELECT 1").unwrap();
        assert_eq!(r.len(), 1);
        let f = r[0].fields.as_ref().unwrap();
        assert_eq!(f[0], Field { name: "a".into(), type_oid: 6, user_type: None, type_mod: -1 });
        assert_eq!(f[1].type_mod, 14);
        assert_eq!(r[0].rows, vec![vec![Some(b"1".to_vec()), None]]);
        let e = c.simple_query("bad").unwrap_err().to_string();
        assert!(e.contains("42601") && e.contains("Syntax error"), "{}", e);
        assert_eq!(c.simple_query("").unwrap().len(), 1);
        assert_eq!(c.simple_query("SELECT 1").unwrap()[0].rows.len(), 1);
    }

    #[test]
    fn mock_bad_password() {
        let port = mock_server("secret", answer);
        let e = Conn::connect(&opts(port, "nope")).err().unwrap().to_string();
        assert!(e.contains("Invalid username or password"), "{}", e);
    }

    #[test]
    fn hashes() {
        // md5(md5(password + user) + salt)
        let p = password_payload(AUTH_MD5, b"abcd", "u", "p").unwrap();
        let inner = hex_lower(&<md5::Md5 as md5::Digest>::digest(b"pu"));
        let outer = hex_lower(&<md5::Md5 as md5::Digest>::digest([inner.as_bytes(), b"abcd"].concat()));
        assert_eq!(String::from_utf8(p).unwrap(), format!("md5{}", outer));
        assert_eq!(password_payload(AUTH_CLEARTEXT, b"", "u", "pw").unwrap(), b"pw");
        assert!(password_payload(7, b"", "u", "pw").is_err());
    }

    #[test]
    fn binary_text() {
        assert_eq!(decode_binary(b"a\\001\\\\b\\377"), vec![b'a', 1, b'\\', b'b', 255]);
        assert_eq!(decode_binary(b"plain"), b"plain".to_vec());
    }

    #[test]
    fn notice_fields() {
        let e = error("42V01", "Table \"x\" does not exist");
        let n = Notice::parse(&e[5..]);
        assert_eq!(n.message(), "ERROR 42V01: Table \"x\" does not exist");
    }
}
