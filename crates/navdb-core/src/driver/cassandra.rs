//! Apache Cassandra / ScyllaDB driver (CQL native protocol via the `scylla` crate).
//! Database = keyspace. The SQL the data grid generates is rewritten to CQL
//! (typed literals, no OFFSET, client-side sorting and unsupported predicates).

mod cql;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use scylla::client::execution_profile::ExecutionProfile;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::response::PagingState;
use scylla::statement::unprepared::Statement;
use scylla::statement::Consistency;
use scylla::value::Row;

use crate::config::{ConnectionConfig, DbKind, SslConfig};
use crate::model::*;
use crate::sqlmini::{self, CmpOp, Expr, Select};
use crate::value::{ColumnMeta, QueryResult, Value};
use cql::{quote, quote_ident, Ty, Udts};

const PAGE_SIZE: i32 = 1000;
/// Most rows a single statement returns.
const MAX_ROWS: usize = 100_000;
/// Most rows scanned for client-side filtering / sorting / counting.
const SCAN_CAP: usize = 100_000;

#[derive(Clone, Debug)]
struct Col {
    name: String,
    type_text: String,
    ty: Ty,
    /// partition_key, clustering, static, regular
    kind: String,
    position: i64,
    clustering_order: String,
}

impl Col {
    fn is_key(&self) -> bool {
        self.kind == "partition_key" || self.kind == "clustering"
    }
}

pub struct CassandraDriver {
    kind: DbKind,
    rt: tokio::runtime::Runtime,
    session: Session,
    show_system: bool,
    cols: HashMap<(String, String), Arc<Vec<Col>>>,
    udts: HashMap<String, Arc<Udts>>,
}

fn parse_consistency(s: &str) -> Result<Consistency> {
    Ok(match s.trim().to_ascii_uppercase().replace(['_', '-', ' '], "").as_str() {
        "" | "LOCALONE" => Consistency::LocalOne,
        "ONE" => Consistency::One,
        "TWO" => Consistency::Two,
        "THREE" => Consistency::Three,
        "QUORUM" => Consistency::Quorum,
        "ALL" => Consistency::All,
        "LOCALQUORUM" => Consistency::LocalQuorum,
        "EACHQUORUM" => Consistency::EachQuorum,
        "ANY" => Consistency::Any,
        "SERIAL" => Consistency::Serial,
        "LOCALSERIAL" => Consistency::LocalSerial,
        other => bail!("unknown consistency level '{}'", other),
    })
}

fn tls_config(ssl: &SslConfig) -> Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let builder = if !ssl.verify_server {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        if ssl.ca_cert.trim().is_empty() {
            for c in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(c);
            }
        } else {
            for c in CertificateDer::pem_file_iter(ssl.ca_cert.trim()).with_context(|| format!("reading CA certificate {}", ssl.ca_cert))? {
                roots.add(c.context("invalid CA certificate")?)?;
            }
        }
        builder.with_root_certificates(roots)
    };
    let cfg = if !ssl.client_cert.trim().is_empty() {
        let certs = CertificateDer::pem_file_iter(ssl.client_cert.trim())
            .with_context(|| format!("reading client certificate {}", ssl.client_cert))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("invalid client certificate")?;
        let key = PrivateKeyDer::from_pem_file(ssl.client_key.trim()).with_context(|| format!("reading client key {}", ssl.client_key))?;
        builder.with_client_auth_cert(certs, key)?
    } else {
        builder.with_no_client_auth()
    };
    Ok(Arc::new(cfg))
}

/// Certificate verifier used when "verify server certificate" is off.
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Maps node addresses the client cannot reach (e.g. container IPs behind a
/// port mapping or NAT) to the contact point.
struct ContactPointTranslator {
    contact: std::net::SocketAddr,
    probe: Duration,
    cache: std::sync::Mutex<HashMap<std::net::SocketAddr, std::net::SocketAddr>>,
}

#[async_trait::async_trait]
impl scylla::policies::address_translator::AddressTranslator for ContactPointTranslator {
    async fn translate_address(
        &self,
        peer: &scylla::policies::address_translator::UntranslatedPeer,
    ) -> std::result::Result<std::net::SocketAddr, scylla::errors::TranslationError> {
        let addr = peer.untranslated_address();
        if addr == self.contact {
            return Ok(addr);
        }
        if let Some(a) = self.cache.lock().ok().and_then(|c| c.get(&addr).copied()) {
            return Ok(a);
        }
        let reachable = matches!(tokio::time::timeout(self.probe, tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)));
        let out = if reachable { addr } else { self.contact };
        if let Ok(mut c) = self.cache.lock() {
            c.insert(addr, out);
        }
        Ok(out)
    }
}

fn text(r: &QueryResult, row: usize, col: &str) -> String {
    match r.get(row, col) {
        None | Some(Value::Null) => String::new(),
        Some(v) => v.to_string(),
    }
}

fn int(r: &QueryResult, row: usize, col: &str) -> i64 {
    r.get(row, col).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64
}

/// JSON text of a `list<text>` column -> items.
fn json_list(s: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        _ => vec![],
    }
}

fn json_map(s: &str) -> Vec<(String, String)> {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(o)) => {
            o.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()
        }
        _ => vec![],
    }
}

fn starts_with_kw(s: &str, kw: &str) -> bool {
    s.trim_start().get(..kw.len()).is_some_and(|p| p.eq_ignore_ascii_case(kw))
}

fn count_result(n: i64) -> QueryResult {
    QueryResult {
        columns: vec![ColumnMeta { name: "count".into(), type_name: "bigint".into() }],
        rows: vec![vec![Value::Int(n)]],
        is_query: true,
        ..Default::default()
    }
}

impl CassandraDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
        let consistency = parse_consistency(cfg.option("consistency"))?;
        let timeout = Duration::from_secs(cfg.connect_timeout_secs.max(1));
        let request_timeout = cfg.option("request_timeout").trim().parse::<u64>().unwrap_or(60);
        let profile = ExecutionProfile::builder()
            .consistency(consistency)
            .request_timeout(Some(Duration::from_secs(request_timeout.max(1))))
            .build()
            .into_handle();
        let host = if cfg.host.trim().is_empty() { "127.0.0.1" } else { cfg.host.trim() };
        let port = if cfg.port == 0 { 9042 } else { cfg.port };
        let node = if host.contains(':') && !host.starts_with('[') { format!("[{}]:{}", host, port) } else { format!("{}:{}", host, port) };
        let mut b = SessionBuilder::new()
            .known_node(node)
            .connection_timeout(timeout)
            .default_execution_profile_handle(profile)
            .fetch_schema_metadata(false);
        if cfg.option("address_translation") != "none" {
            use std::net::ToSocketAddrs;
            if let Some(contact) = (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
                b = b.address_translator(Arc::new(ContactPointTranslator {
                    contact,
                    probe: Duration::from_secs(2).min(timeout),
                    cache: Default::default(),
                }));
            }
        }
        if !cfg.user.is_empty() {
            b = b.user(cfg.user.clone(), cfg.password.clone());
        }
        let dc = cfg.option("datacenter").trim();
        if !dc.is_empty() {
            b = b.prefer_datacenter(dc.to_string());
        }
        if cfg.ssl.enabled {
            b = b.tls_context(Some(tls_config(&cfg.ssl)?));
        }
        if !cfg.database.trim().is_empty() {
            b = b.use_keyspace(cfg.database.trim(), true);
        }
        let session = rt
            .block_on(async { tokio::time::timeout(timeout + Duration::from_secs(5), b.build()).await })
            .map_err(|_| anyhow!("connection timed out after {} s", timeout.as_secs()))?
            .map_err(|e| anyhow!("{}", e))?;
        Ok(Self {
            kind: cfg.kind,
            rt,
            session,
            show_system: matches!(cfg.option("show_system_keyspaces"), "1" | "true" | "yes"),
            cols: HashMap::new(),
            udts: HashMap::new(),
        })
    }

    fn current_keyspace(&self) -> Option<String> {
        self.session.get_keyspace().map(|k| k.to_string())
    }

    fn use_keyspace(&mut self, ks: &str, case_sensitive: bool) -> Result<()> {
        if self.current_keyspace().as_deref() == Some(ks) {
            return Ok(());
        }
        self.rt.block_on(self.session.use_keyspace(ks, case_sensitive)).map_err(|e| anyhow!("{}", e))
    }

    /// Run one CQL statement, following pages up to `cap` rows.
    fn run_cql(&self, cql: &str, cap: usize) -> Result<QueryResult> {
        let start = Instant::now();
        let mut st = Statement::new(cql.to_string());
        st.set_page_size(PAGE_SIZE.min(cap.max(1) as i32));
        let mut paging = PagingState::start();
        let mut out = QueryResult { sql: cql.to_string(), ..Default::default() };
        'pages: loop {
            let (res, next) = self.rt.block_on(self.session.query_single_page(st.clone(), (), paging)).map_err(|e| anyhow!("{}", e))?;
            if !res.is_rows() {
                if ["INSERT", "UPDATE", "DELETE", "BEGIN"].iter().any(|k| starts_with_kw(cql, k)) {
                    out.affected_rows = 1;
                }
                break;
            }
            let rows = res.into_rows_result().map_err(|e| anyhow!("{}", e))?;
            if out.columns.is_empty() {
                out.columns = rows.column_specs().iter().map(|c| ColumnMeta { name: c.name().to_string(), type_name: cql::type_name(c.typ()) }).collect();
            }
            out.is_query = true;
            for r in rows.rows::<Row>().map_err(|e| anyhow!("{}", e))? {
                let r = r.map_err(|e| anyhow!("{}", e))?;
                out.rows.push(r.columns.into_iter().map(|c| c.map(cql::to_value).unwrap_or(Value::Null)).collect());
                if out.rows.len() >= cap {
                    break 'pages;
                }
            }
            match next.into_paging_control_flow() {
                std::ops::ControlFlow::Break(()) => break,
                std::ops::ControlFlow::Continue(p) => paging = p,
            }
        }
        out.elapsed_ms = start.elapsed().as_millis();
        Ok(out)
    }

    fn q(&self, cql: &str) -> Result<QueryResult> {
        self.run_cql(cql, MAX_ROWS)
    }

    fn columns_of(&mut self, ks: &str, table: &str) -> Result<Arc<Vec<Col>>> {
        let key = (ks.to_string(), table.to_string());
        if let Some(c) = self.cols.get(&key) {
            return Ok(c.clone());
        }
        let sel = |schema: &str| {
            format!(
                "SELECT column_name, type, kind, position, clustering_order FROM {}.columns WHERE keyspace_name = {} AND table_name = {}",
                schema,
                quote(ks),
                quote(table)
            )
        };
        let mut r = self.q(&sel("system_schema"))?;
        if r.rows.is_empty() {
            if let Ok(v) = self.q(&sel("system_virtual_schema")) {
                r = v;
            }
        }
        let mut cols: Vec<Col> = (0..r.rows.len())
            .map(|i| {
                let type_text = text(&r, i, "type");
                Col {
                    name: text(&r, i, "column_name"),
                    ty: cql::parse_type(&type_text),
                    type_text,
                    kind: text(&r, i, "kind"),
                    position: int(&r, i, "position"),
                    clustering_order: text(&r, i, "clustering_order"),
                }
            })
            .collect();
        let rank = |k: &str| match k {
            "partition_key" => 0,
            "clustering" => 1,
            _ => 2,
        };
        cols.sort_by(|a, b| {
            rank(&a.kind).cmp(&rank(&b.kind)).then(if rank(&a.kind) < 2 { a.position.cmp(&b.position) } else { a.name.cmp(&b.name) })
        });
        let cols = Arc::new(cols);
        if !cols.is_empty() {
            self.cols.insert(key, cols.clone());
        }
        Ok(cols)
    }

    fn udts_of(&mut self, ks: &str) -> Arc<Udts> {
        if let Some(u) = self.udts.get(ks) {
            return u.clone();
        }
        let mut map = Udts::new();
        if let Ok(r) = self.q(&format!("SELECT type_name, field_names, field_types FROM system_schema.types WHERE keyspace_name = {}", quote(ks))) {
            for i in 0..r.rows.len() {
                let names = json_list(&text(&r, i, "field_names"));
                let types = json_list(&text(&r, i, "field_types"));
                map.insert(text(&r, i, "type_name"), names.into_iter().zip(types.iter().map(|t| cql::parse_type(t))).collect());
            }
        }
        let u = Arc::new(map);
        self.udts.insert(ks.to_string(), u.clone());
        u
    }

    fn resolve(&self, parts: &[String]) -> Result<(String, String)> {
        match parts {
            [t] => match self.current_keyspace() {
                Some(ks) => Ok((ks, t.clone())),
                None => bail!("no keyspace selected (use a qualified name or USE <keyspace>)"),
            },
            [ks, .., t] => Ok((ks.clone(), t.clone())),
            [] => bail!("missing table name"),
        }
    }

    fn table_ref(ks: &str, t: &str) -> String {
        format!("{}.{}", quote_ident(ks), quote_ident(t))
    }

    // ------------------------------------------------------------ statements

    fn run_statement(&mut self, stmt: &str) -> Result<QueryResult> {
        if let Some((ks, cs)) = cql::parse_use(stmt) {
            self.use_keyspace(&ks, cs)?;
            return Ok(QueryResult { sql: stmt.to_string(), ..Default::default() });
        }
        match sqlmini::parse(stmt) {
            Ok(sqlmini::Statement::Select(s)) => self.grid_select(s),
            Ok(sqlmini::Statement::Insert { table, columns, rows }) => self.grid_insert(&table, columns, rows),
            Ok(sqlmini::Statement::Update { table, set, filter }) => self.grid_update(&table, set, filter),
            Ok(sqlmini::Statement::Delete { table, filter }) => self.grid_delete(&table, filter),
            Err(_) => {
                let r = self.q(stmt);
                if !starts_with_kw(stmt, "SELECT") {
                    self.cols.clear();
                    self.udts.clear();
                }
                r
            }
        }
    }

    fn grid_select(&mut self, s: Select) -> Result<QueryResult> {
        let (ks, t) = self.resolve(&s.table)?;
        let cols = self.columns_of(&ks, &t)?;
        let udts = self.udts_of(&ks);
        let conj = cql::conjuncts(s.filter.clone());
        if !cols.is_empty() {
            let mut used = vec![];
            conj.iter().for_each(|e| cql::columns_of(e, &mut used));
            used.extend(s.order.iter().map(|(c, _)| c.clone()));
            used.extend(s.columns.iter().cloned());
            if let Some(c) = used.iter().find(|c| !cols.iter().any(|x| &x.name == *c)) {
                bail!("Undefined column name {} in table {}.{}", c, ks, t);
            }
        }
        let (server, client): (Vec<Expr>, Vec<Expr>) = conj.iter().cloned().partition(cql::server_side);
        match self.select_with(&ks, &t, &cols, &udts, &s, &server, &client) {
            Ok(r) => Ok(r),
            Err(e) if !server.is_empty() => self.select_with(&ks, &t, &cols, &udts, &s, &[], &conj).map_err(|_| e),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn select_with(&self, ks: &str, t: &str, cols: &[Col], udts: &Udts, s: &Select, server: &[Expr], client: &[Expr]) -> Result<QueryResult> {
        let ty = |c: &str| cols.iter().find(|x| x.name == c).map(|x| x.ty.clone()).unwrap_or_default();
        let preds = server.iter().map(|e| cql::render_pred(e, &ty, udts)).collect::<Result<Vec<_>>>()?;
        let post = !client.is_empty() || !s.order.is_empty();
        let offset = s.offset.unwrap_or(0) as usize;
        let projection = if s.count && client.is_empty() {
            "COUNT(*)".to_string()
        } else if !post && !s.columns.is_empty() {
            s.columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ")
        } else {
            "*".to_string()
        };
        let mut q = format!("SELECT {} FROM {}", projection, Self::table_ref(ks, t));
        if !preds.is_empty() {
            q.push_str(&format!(" WHERE {}", preds.join(" AND ")));
        }
        let cap = if s.count || post {
            SCAN_CAP
        } else {
            let n = s.limit.map(|l| l as usize + offset).unwrap_or(MAX_ROWS).min(MAX_ROWS);
            q.push_str(&format!(" LIMIT {}", n.max(1)));
            n.max(1)
        };
        if !preds.is_empty() {
            q.push_str(" ALLOW FILTERING");
        }
        let mut r = self.run_cql(&q, cap)?;
        if s.count && client.is_empty() {
            let n = r.rows.first().and_then(|x| x.first()).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
            let mut c = count_result(n);
            c.elapsed_ms = r.elapsed_ms;
            return Ok(c);
        }
        let idx: HashMap<String, usize> = r.columns.iter().enumerate().map(|(i, c)| (c.name.clone(), i)).collect();
        if !client.is_empty() {
            r.rows.retain(|row| {
                let get = |c: &str| idx.get(c).and_then(|&i| row.get(i)).cloned();
                client.iter().all(|e| cql::eval(e, &get))
            });
        }
        if s.count {
            let mut c = count_result(r.rows.len() as i64);
            c.elapsed_ms = r.elapsed_ms;
            return Ok(c);
        }
        if !s.order.is_empty() {
            let keys: Vec<(usize, bool)> = s.order.iter().filter_map(|(c, d)| idx.get(c).map(|&i| (i, *d))).collect();
            r.rows.sort_by(|a, b| {
                for &(i, desc) in &keys {
                    let o = a[i].cmp_loose(&b[i]);
                    if o != std::cmp::Ordering::Equal {
                        return if desc { o.reverse() } else { o };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        let limit = s.limit.map(|l| l as usize).unwrap_or(usize::MAX);
        r.rows = r.rows.into_iter().skip(offset).take(limit).collect();
        if post && !s.columns.is_empty() {
            let pick: Vec<usize> = s.columns.iter().filter_map(|c| idx.get(c).copied()).collect();
            r.columns = pick.iter().map(|&i| r.columns[i].clone()).collect();
            r.rows = r.rows.into_iter().map(|row| pick.iter().map(|&i| row[i].clone()).collect()).collect();
        }
        Ok(r)
    }

    fn grid_insert(&mut self, table: &[String], columns: Vec<String>, rows: Vec<Vec<Value>>) -> Result<QueryResult> {
        let (ks, t) = self.resolve(table)?;
        let cols = self.columns_of(&ks, &t)?;
        let udts = self.udts_of(&ks);
        let columns = if columns.is_empty() { cols.iter().map(|c| c.name.clone()).collect() } else { columns };
        let start = Instant::now();
        let mut n = 0;
        for row in rows {
            if row.len() != columns.len() {
                bail!("INSERT has {} columns but {} values", columns.len(), row.len());
            }
            let mut names = vec![];
            let mut lits = vec![];
            for (c, v) in columns.iter().zip(row.iter()) {
                let col = cols.iter().find(|x| &x.name == c);
                if cols.is_empty() || col.is_some() {
                    if v.is_null() && !col.is_some_and(|x| x.is_key()) {
                        continue;
                    }
                } else {
                    bail!("Undefined column name {} in table {}.{}", c, ks, t);
                }
                names.push(quote_ident(c));
                lits.push(cql::literal(v, &col.map(|x| x.ty.clone()).unwrap_or_default(), &udts).with_context(|| format!("column {}", c))?);
            }
            self.q(&format!("INSERT INTO {} ({}) VALUES ({})", Self::table_ref(&ks, &t), names.join(", "), lits.join(", ")))?;
            n += 1;
        }
        Ok(QueryResult { affected_rows: n, elapsed_ms: start.elapsed().as_millis(), ..Default::default() })
    }

    /// WHERE for UPDATE/DELETE: only predicates on primary key columns are kept.
    fn key_where(cols: &[Col], udts: &Udts, filter: Option<Expr>, full_key: bool) -> Result<(String, HashMap<String, Value>)> {
        let ty = |c: &str| cols.iter().find(|x| x.name == c).map(|x| x.ty.clone()).unwrap_or_default();
        let is_key = |c: &str| cols.is_empty() || cols.iter().any(|x| x.name == c && x.is_key());
        let mut parts = vec![];
        let mut eq = HashMap::new();
        for e in cql::conjuncts(filter) {
            let mut used = vec![];
            cql::columns_of(&e, &mut used);
            if !cols.is_empty() && !used.iter().any(|c| is_key(c)) {
                continue;
            }
            match &e {
                Expr::Cmp { column, op, value } if is_key(column) => {
                    if *op == CmpOp::Eq {
                        eq.insert(column.clone(), value.clone());
                    } else if full_key || *op == CmpOp::Ne {
                        bail!("only '=' conditions on primary key columns are supported (column {})", column);
                    }
                    parts.push(cql::render_pred(&e, &ty, udts)?);
                }
                Expr::In { column, negated: false, values } if is_key(column) => {
                    if values.len() == 1 {
                        eq.insert(column.clone(), values[0].clone());
                    }
                    parts.push(cql::render_pred(&e, &ty, udts)?);
                }
                Expr::IsNull { column, .. } if is_key(column) => bail!("primary key column {} cannot be null", column),
                _ => bail!("unsupported condition on primary key columns for CQL"),
            }
        }
        let partition: Vec<&Col> = cols.iter().filter(|c| c.kind == "partition_key").collect();
        let required: Vec<&Col> = if full_key { cols.iter().filter(|c| c.is_key()).collect() } else { partition };
        let missing: Vec<&str> = required.iter().filter(|c| !Self::restricted(&c.name, &eq, &parts)).map(|c| c.name.as_str()).collect();
        if !missing.is_empty() {
            bail!("CQL needs the primary key in the WHERE clause (missing: {})", missing.join(", "));
        }
        if parts.is_empty() {
            bail!("CQL UPDATE/DELETE requires a WHERE clause on the primary key");
        }
        Ok((parts.join(" AND "), eq))
    }

    fn restricted(col: &str, eq: &HashMap<String, Value>, parts: &[String]) -> bool {
        eq.contains_key(col) || parts.iter().any(|p| p.starts_with(&format!("{} ", quote_ident(col))))
    }

    fn grid_update(&mut self, table: &[String], set: Vec<(String, Value)>, filter: Option<Expr>) -> Result<QueryResult> {
        let (ks, t) = self.resolve(table)?;
        let cols = self.columns_of(&ks, &t)?;
        let udts = self.udts_of(&ks);
        let (w, eq) = Self::key_where(&cols, &udts, filter, true)?;
        let mut assigns = vec![];
        for (c, v) in &set {
            let col = cols.iter().find(|x| &x.name == c);
            if !cols.is_empty() && col.is_none() {
                bail!("Undefined column name {} in table {}.{}", c, ks, t);
            }
            if col.is_some_and(|x| x.is_key()) {
                if eq.get(c).is_some_and(|k| k.cmp_loose(v) == std::cmp::Ordering::Equal) {
                    continue;
                }
                bail!("primary key column {} cannot be changed in CQL (delete and re-insert the row)", c);
            }
            let lit = cql::literal(v, &col.map(|x| x.ty.clone()).unwrap_or_default(), &udts).with_context(|| format!("column {}", c))?;
            assigns.push(format!("{} = {}", quote_ident(c), lit));
        }
        if assigns.is_empty() {
            return Ok(QueryResult::default());
        }
        let mut r = self.q(&format!("UPDATE {} SET {} WHERE {}", Self::table_ref(&ks, &t), assigns.join(", "), w))?;
        r.affected_rows = 1;
        Ok(r)
    }

    fn grid_delete(&mut self, table: &[String], filter: Option<Expr>) -> Result<QueryResult> {
        let (ks, t) = self.resolve(table)?;
        let cols = self.columns_of(&ks, &t)?;
        let udts = self.udts_of(&ks);
        let (w, _) = Self::key_where(&cols, &udts, filter, false)?;
        let mut r = self.q(&format!("DELETE FROM {} WHERE {}", Self::table_ref(&ks, &t), w))?;
        r.affected_rows = 1;
        Ok(r)
    }

    // ------------------------------------------------------------ DDL

    fn describe(&self, what: &str, ks: &str, name: &str) -> Option<String> {
        let r = self.q(&format!("DESCRIBE {} {}", what, Self::table_ref(ks, name))).ok()?;
        let col = r.column_index("create_statement")?;
        let s = r.rows.iter().filter_map(|row| row.get(col)).map(|v| v.to_string()).collect::<Vec<_>>().join("\n\n");
        (!s.trim().is_empty()).then_some(s)
    }

    fn primary_key_clause(cols: &[Col]) -> String {
        let pk: Vec<String> = cols.iter().filter(|c| c.kind == "partition_key").map(|c| quote_ident(&c.name)).collect();
        let ck: Vec<String> = cols.iter().filter(|c| c.kind == "clustering").map(|c| quote_ident(&c.name)).collect();
        let mut parts = vec![if pk.len() == 1 { pk[0].clone() } else { format!("({})", pk.join(", ")) }];
        parts.extend(ck);
        format!("PRIMARY KEY ({})", parts.join(", "))
    }

    fn clustering_order(cols: &[Col]) -> Option<String> {
        let ck: Vec<&Col> = cols.iter().filter(|c| c.kind == "clustering").collect();
        if ck.is_empty() {
            return None;
        }
        Some(format!(
            "CLUSTERING ORDER BY ({})",
            ck.iter().map(|c| format!("{} {}", quote_ident(&c.name), if c.clustering_order.eq_ignore_ascii_case("desc") { "DESC" } else { "ASC" })).collect::<Vec<_>>().join(", ")
        ))
    }

    fn synth_table(&mut self, p: &ObjPath) -> Result<String> {
        let cols = self.columns_of(&p.db, &p.name)?;
        if cols.is_empty() {
            bail!("table {}.{} not found", p.db, p.name);
        }
        let mut lines: Vec<String> = cols
            .iter()
            .map(|c| format!("    {} {}{}", quote_ident(&c.name), c.type_text, if c.kind == "static" { " static" } else { "" }))
            .collect();
        lines.push(format!("    {}", Self::primary_key_clause(&cols)));
        let mut s = format!("CREATE TABLE {} (\n{}\n)", Self::table_ref(&p.db, &p.name), lines.join(",\n"));
        let mut with = vec![];
        with.extend(Self::clustering_order(&cols));
        if let Ok(r) = self.q(&format!(
            "SELECT comment, default_time_to_live, gc_grace_seconds FROM system_schema.tables WHERE keyspace_name = {} AND table_name = {}",
            quote(&p.db),
            quote(&p.name)
        )) {
            if !r.rows.is_empty() {
                with.push(format!("comment = {}", quote(&text(&r, 0, "comment"))));
                with.push(format!("default_time_to_live = {}", int(&r, 0, "default_time_to_live")));
                with.push(format!("gc_grace_seconds = {}", int(&r, 0, "gc_grace_seconds")));
            }
        }
        if !with.is_empty() {
            s.push_str(&format!("\nWITH {}", with.join("\n    AND ")));
        }
        s.push(';');
        Ok(s)
    }

    fn synth_view(&mut self, p: &ObjPath) -> Result<String> {
        let r = self.q(&format!(
            "SELECT base_table_name, where_clause, include_all_columns FROM system_schema.views WHERE keyspace_name = {} AND view_name = {}",
            quote(&p.db),
            quote(&p.name)
        ))?;
        if r.rows.is_empty() {
            bail!("materialized view {}.{} not found", p.db, p.name);
        }
        let cols = self.columns_of(&p.db, &p.name)?;
        let base = text(&r, 0, "base_table_name");
        let projection = if text(&r, 0, "include_all_columns") == "true" {
            "*".to_string()
        } else {
            cols.iter().map(|c| quote_ident(&c.name)).collect::<Vec<_>>().join(", ")
        };
        let mut s = format!(
            "CREATE MATERIALIZED VIEW {} AS\n    SELECT {}\n    FROM {}\n    WHERE {}\n    {}",
            Self::table_ref(&p.db, &p.name),
            projection,
            Self::table_ref(&p.db, &base),
            text(&r, 0, "where_clause"),
            Self::primary_key_clause(&cols)
        );
        if let Some(o) = Self::clustering_order(&cols) {
            s.push_str(&format!("\nWITH {}", o));
        }
        s.push(';');
        Ok(s)
    }

    fn synth_index(&mut self, p: &ObjPath) -> Result<String> {
        let r = self.q(&format!("SELECT table_name, index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = {}", quote(&p.db)))?;
        let i = (0..r.rows.len()).find(|&i| text(&r, i, "index_name") == p.name).ok_or_else(|| anyhow!("index {}.{} not found", p.db, p.name))?;
        let opts = json_map(&text(&r, i, "options"));
        let target = opts.iter().find(|(k, _)| k == "target").map(|(_, v)| v.clone()).unwrap_or_default();
        let table = Self::table_ref(&p.db, &text(&r, i, "table_name"));
        Ok(match opts.iter().find(|(k, _)| k == "class_name") {
            Some((_, class)) => {
                let rest: Vec<String> = opts.iter().filter(|(k, _)| k != "target" && k != "class_name").map(|(k, v)| format!("{}: {}", quote(k), quote(v))).collect();
                let mut s = format!("CREATE CUSTOM INDEX {} ON {} ({}) USING {}", quote_ident(&p.name), table, target, quote(class));
                if !rest.is_empty() {
                    s.push_str(&format!(" WITH OPTIONS = {{{}}}", rest.join(", ")));
                }
                s + ";"
            }
            None => format!("CREATE INDEX {} ON {} ({});", quote_ident(&p.name), table, target),
        })
    }

    fn synth_function(&mut self, p: &ObjPath) -> Result<String> {
        let ks = quote(&p.db);
        let name = quote(&p.name);
        let mut out = vec![];
        if let Ok(r) = self.q(&format!(
            "SELECT argument_names, argument_types, body, called_on_null_input, language, return_type FROM system_schema.functions WHERE keyspace_name = {} AND function_name = {}",
            ks, name
        )) {
            for i in 0..r.rows.len() {
                let args: Vec<String> = json_list(&text(&r, i, "argument_names"))
                    .iter()
                    .zip(json_list(&text(&r, i, "argument_types")))
                    .map(|(n, t)| format!("{} {}", n, t))
                    .collect();
                out.push(format!(
                    "CREATE FUNCTION {} ({})\n    {} ON NULL INPUT\n    RETURNS {}\n    LANGUAGE {}\n    AS $${}$$;",
                    Self::table_ref(&p.db, &p.name),
                    args.join(", "),
                    if text(&r, i, "called_on_null_input") == "true" { "CALLED" } else { "RETURNS NULL" },
                    text(&r, i, "return_type"),
                    text(&r, i, "language"),
                    text(&r, i, "body")
                ));
            }
        }
        if let Ok(r) = self.q(&format!(
            "SELECT argument_types, state_func, state_type, final_func, initcond FROM system_schema.aggregates WHERE keyspace_name = {} AND aggregate_name = {}",
            ks, name
        )) {
            for i in 0..r.rows.len() {
                let mut s = format!(
                    "CREATE AGGREGATE {} ({})\n    SFUNC {}\n    STYPE {}",
                    Self::table_ref(&p.db, &p.name),
                    json_list(&text(&r, i, "argument_types")).join(", "),
                    text(&r, i, "state_func"),
                    text(&r, i, "state_type")
                );
                let ff = text(&r, i, "final_func");
                if !ff.is_empty() {
                    s.push_str(&format!("\n    FINALFUNC {}", ff));
                }
                let ic = text(&r, i, "initcond");
                if !ic.is_empty() {
                    s.push_str(&format!("\n    INITCOND {}", ic));
                }
                out.push(s + ";");
            }
        }
        if out.is_empty() {
            bail!("function {}.{} not found", p.db, p.name);
        }
        Ok(out.join("\n\n"))
    }

    fn try_q(&self, cqls: &[&str]) -> QueryResult {
        cqls.iter().find_map(|c| self.q(c).ok().filter(|r| r.is_query)).unwrap_or_default()
    }
}

impl super::Driver for CassandraDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    fn server_version(&mut self) -> Result<String> {
        let r = self.q("SELECT release_version FROM system.local")?;
        let rel = text(&r, 0, "release_version");
        if let Ok(v) = self.q("SELECT version FROM system.versions") {
            let sv = text(&v, 0, "version");
            if !sv.is_empty() {
                return Ok(format!("ScyllaDB {} (CQL {})", sv, rel));
            }
        }
        Ok(format!("Cassandra {}", rel))
    }

    fn list_databases(&mut self) -> Result<Vec<String>> {
        let r = self.q("SELECT keyspace_name FROM system_schema.keyspaces")?;
        let mut v: Vec<String> = (0..r.rows.len()).map(|i| text(&r, i, "keyspace_name")).filter(|k| self.show_system || !k.starts_with("system")).collect();
        v.sort();
        Ok(v)
    }

    fn object_kinds(&self) -> Vec<ObjectKind> {
        vec![ObjectKind::Table, ObjectKind::MaterializedView, ObjectKind::Function, ObjectKind::Index]
    }

    fn list_objects(&mut self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectInfo>> {
        let ks = quote(&scope.db);
        let mut out = vec![];
        match kind {
            ObjectKind::Table => {
                let r = self.q(&format!("SELECT * FROM system_schema.tables WHERE keyspace_name = {}", ks))?;
                let mut sizes: HashMap<String, i64> = HashMap::new();
                if let Ok(s) = self.q(&format!("SELECT table_name, mean_partition_size, partitions_count FROM system.size_estimates WHERE keyspace_name = {}", ks)) {
                    for i in 0..s.rows.len() {
                        *sizes.entry(text(&s, i, "table_name")).or_default() += int(&s, i, "mean_partition_size") * int(&s, i, "partitions_count");
                    }
                }
                for i in 0..r.rows.len() {
                    let name = text(&r, i, "table_name");
                    let mut o = ObjectInfo::named(name.clone());
                    o.comment = text(&r, i, "comment");
                    o.data_length = sizes.get(&name).copied();
                    o.engine = json_map(&text(&r, i, "compaction"))
                        .into_iter()
                        .find(|(k, _)| k == "class")
                        .map(|(_, v)| v.rsplit('.').next().unwrap_or(&v).to_string())
                        .unwrap_or_default();
                    for k in ["id", "default_time_to_live", "gc_grace_seconds", "bloom_filter_fp_chance", "caching", "compaction", "compression"] {
                        if r.column_index(k).is_some() {
                            o.extra.push((k.to_string(), text(&r, i, k)));
                        }
                    }
                    out.push(o);
                }
            }
            ObjectKind::MaterializedView => {
                let r = self.q(&format!("SELECT view_name, base_table_name, where_clause, comment FROM system_schema.views WHERE keyspace_name = {}", ks))?;
                for i in 0..r.rows.len() {
                    let mut o = ObjectInfo::named(text(&r, i, "view_name"));
                    o.comment = text(&r, i, "comment");
                    o.extra.push(("base table".into(), text(&r, i, "base_table_name")));
                    o.extra.push(("where".into(), text(&r, i, "where_clause")));
                    out.push(o);
                }
            }
            ObjectKind::Function => {
                let mut seen = std::collections::HashSet::new();
                let f = self.q(&format!("SELECT function_name, argument_types, return_type, language FROM system_schema.functions WHERE keyspace_name = {}", ks))?;
                for i in 0..f.rows.len() {
                    let name = text(&f, i, "function_name");
                    if seen.insert(name.clone()) {
                        let mut o = ObjectInfo::named(name.clone());
                        o.engine = text(&f, i, "language");
                        o.comment = format!("{}({}) -> {}", name, json_list(&text(&f, i, "argument_types")).join(", "), text(&f, i, "return_type"));
                        out.push(o);
                    }
                }
                let a = self.q(&format!("SELECT aggregate_name, argument_types, return_type FROM system_schema.aggregates WHERE keyspace_name = {}", ks))?;
                for i in 0..a.rows.len() {
                    let name = text(&a, i, "aggregate_name");
                    if seen.insert(name.clone()) {
                        let mut o = ObjectInfo::named(name.clone());
                        o.engine = "aggregate".into();
                        o.comment = format!("{}({}) -> {}", name, json_list(&text(&a, i, "argument_types")).join(", "), text(&a, i, "return_type"));
                        out.push(o);
                    }
                }
            }
            ObjectKind::Index => {
                let r = self.q(&format!("SELECT table_name, index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = {}", ks))?;
                for i in 0..r.rows.len() {
                    let mut o = ObjectInfo::named(text(&r, i, "index_name"));
                    o.engine = text(&r, i, "kind");
                    o.extra.push(("table".into(), text(&r, i, "table_name")));
                    for (k, v) in json_map(&text(&r, i, "options")) {
                        o.extra.push((k, v));
                    }
                    out.push(o);
                }
            }
            _ => {}
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn table_def(&mut self, path: &ObjPath) -> Result<TableDef> {
        self.cols.remove(&(path.db.clone(), path.name.clone()));
        let cols = self.columns_of(&path.db, &path.name)?;
        if cols.is_empty() {
            bail!("table {}.{} not found", path.db, path.name);
        }
        let mut def = TableDef { path: path.clone(), ..Default::default() };
        def.columns = cols
            .iter()
            .map(|c| ColumnDef {
                nullable: !c.is_key(),
                primary_key: c.is_key(),
                extra: match c.kind.as_str() {
                    "partition_key" => "partition key".into(),
                    "clustering" => format!("clustering ({})", c.clustering_order.to_lowercase()),
                    "static" => "static".into(),
                    _ => String::new(),
                },
                ..ColumnDef::new(c.name.clone(), c.type_text.clone())
            })
            .collect();
        def.indexes.push(IndexDef {
            name: "PRIMARY".into(),
            columns: cols.iter().filter(|c| c.is_key()).map(|c| c.name.clone()).collect(),
            unique: true,
            primary: true,
            ..Default::default()
        });
        if let Ok(r) = self.q(&format!("SELECT table_name, index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = {}", quote(&path.db))) {
            for i in (0..r.rows.len()).filter(|&i| text(&r, i, "table_name") == path.name) {
                let opts = json_map(&text(&r, i, "options"));
                let target = opts.iter().find(|(k, _)| k == "target").map(|(_, v)| v.clone()).unwrap_or_default();
                def.indexes.push(IndexDef {
                    name: text(&r, i, "index_name"),
                    columns: cql::parse_type(&target).name.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect(),
                    method: opts.iter().find(|(k, _)| k == "class_name").map(|(_, v)| v.clone()).unwrap_or_else(|| text(&r, i, "kind")),
                    ..Default::default()
                });
            }
        }
        if let Ok(r) = self.q(&format!(
            "SELECT comment FROM system_schema.tables WHERE keyspace_name = {} AND table_name = {}",
            quote(&path.db),
            quote(&path.name)
        )) {
            def.comment = text(&r, 0, "comment");
        }
        Ok(def)
    }

    fn object_ddl(&mut self, path: &ObjPath, kind: ObjectKind) -> Result<String> {
        match kind {
            ObjectKind::Table => match self.describe("TABLE", &path.db, &path.name) {
                Some(s) => Ok(s),
                None => self.synth_table(path),
            },
            ObjectKind::MaterializedView | ObjectKind::View => match self.describe("MATERIALIZED VIEW", &path.db, &path.name) {
                Some(s) => Ok(s),
                None => self.synth_view(path),
            },
            ObjectKind::Index => match self.describe("INDEX", &path.db, &path.name) {
                Some(s) => Ok(s),
                None => self.synth_index(path),
            },
            ObjectKind::Function => {
                let parts: Vec<String> =
                    ["FUNCTION", "AGGREGATE"].iter().filter_map(|w| self.describe(w, &path.db, &path.name)).collect();
                if parts.is_empty() {
                    self.synth_function(path)
                } else {
                    Ok(parts.join("\n\n"))
                }
            }
            k => bail!("{} is not supported by {}", k.singular(), self.kind.label()),
        }
    }

    fn execute(&mut self, scope: Option<&Scope>, sql: &str) -> Result<Vec<QueryResult>> {
        if let Some(s) = scope {
            if !s.db.is_empty() {
                self.use_keyspace(&s.db, true)?;
            }
        }
        let mut out = vec![];
        for stmt in cql::split(sql) {
            let start = Instant::now();
            let mut r = self.run_statement(&stmt)?;
            r.sql = stmt;
            r.elapsed_ms = start.elapsed().as_millis();
            out.push(r);
        }
        Ok(out)
    }

    fn explain(&mut self, scope: Option<&Scope>, sql: &str) -> Result<QueryResult> {
        let stmt = sql.trim().trim_end_matches(';').to_string();
        if !starts_with_kw(&stmt, "SELECT") {
            bail!("EXPLAIN is not supported by CQL; only SELECT statements can be traced");
        }
        if let Some(s) = scope.filter(|s| !s.db.is_empty()) {
            self.use_keyspace(&s.db, true)?;
        }
        let mut st = Statement::new(stmt.clone());
        st.set_tracing(true);
        st.set_page_size(PAGE_SIZE);
        let res = self.rt.block_on(self.session.query_single_page(st, (), PagingState::start())).map_err(|e| anyhow!("{}", e))?.0;
        let id = res.tracing_id().ok_or_else(|| anyhow!("the server returned no tracing information"))?;
        let info = self.rt.block_on(self.session.get_tracing_info(&id)).map_err(|e| anyhow!("{}", e))?;
        let mut r = QueryResult {
            sql: stmt,
            is_query: true,
            columns: ["activity", "source", "source_elapsed_us", "thread"].iter().map(|c| ColumnMeta::new(*c)).collect(),
            ..Default::default()
        };
        for e in info.events {
            r.rows.push(vec![
                e.activity.map(Value::Text).unwrap_or_default(),
                e.source.map(|s| Value::Text(s.to_string())).unwrap_or_default(),
                e.source_elapsed.map(|v| Value::Int(v as i64)).unwrap_or_default(),
                e.thread.map(Value::Text).unwrap_or_default(),
            ]);
        }
        Ok(r)
    }

    fn server_snapshot(&mut self) -> Result<ServerSnapshot> {
        let local = self.q("SELECT * FROM system.local")?;
        let mut status = QueryResult { is_query: true, columns: vec![ColumnMeta::new("name"), ColumnMeta::new("value")], ..Default::default() };
        if let Some(row) = local.rows.first() {
            for (c, v) in local.columns.iter().zip(row) {
                let v = match v {
                    Value::Text(s) if s.len() > 200 => Value::Text(format!("{}…", s.chars().take(200).collect::<String>())),
                    v => v.clone(),
                };
                status.rows.push(vec![Value::Text(c.name.clone()), v]);
            }
        }
        if let Ok(p) = self.q("SELECT peer, data_center, rack, release_version FROM system.peers") {
            for i in 0..p.rows.len() {
                status.rows.push(vec![
                    Value::Text(format!("peer {}", text(&p, i, "peer"))),
                    Value::Text(format!("{} / {} / {}", text(&p, i, "data_center"), text(&p, i, "rack"), text(&p, i, "release_version"))),
                ]);
            }
        }
        Ok(ServerSnapshot {
            processes: self.try_q(&["SELECT * FROM system_views.clients", "SELECT * FROM system.clients"]),
            variables: self.try_q(&["SELECT name, value FROM system_views.settings", "SELECT name, value, source FROM system.config"]),
            status,
        })
    }
}
