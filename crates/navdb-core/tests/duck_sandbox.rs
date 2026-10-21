//! The Mac App Store build must not download DuckDB extensions.
#![cfg(not(feature = "duckdb-extensions"))]

use navdb_core::{ConnectionConfig, DbKind};

#[test]
fn duckdb_refuses_extension_downloads() {
    let cfg = ConnectionConfig { kind: DbKind::DuckDb, database: ":memory:".into(), ..Default::default() };
    let mut db = navdb_core::driver::connect(&cfg).expect("in-memory DuckDB opens");
    assert!(db.execute(None, "SELECT 42").is_ok());
    for sql in ["INSTALL spatial", "/* x */ force install spatial", "LOAD httpfs"] {
        assert!(db.execute(None, sql).is_err(), "{sql} must fail");
    }
    assert!(db.execute(None, "SET autoinstall_known_extensions = true").is_err(), "configuration is locked");
    // compiled-in extensions still work without a download
    assert!(db.execute(None, "COPY (SELECT 1 AS a) TO '/dev/null' (FORMAT parquet)").is_ok(), "parquet is built in");
    assert!(!DbKind::Iceberg.available());
}
