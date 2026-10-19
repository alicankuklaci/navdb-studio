use serde::{Deserialize, Serialize};

use crate::model::ObjPath;

/// SQL flavour used for quoting, literals and generated statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dialect {
    MySql,
    Postgres,
    Sqlite,
    /// T-SQL (SQL Server): `[ident]`, `OFFSET .. FETCH`.
    SqlServer,
    /// Oracle: `"IDENT"`, `OFFSET .. FETCH`, no `LIMIT`.
    Oracle,
    /// ANSI-style SQL used by DuckDB, ClickHouse, Trino, Vertica, CQL and the
    /// SQL subsets accepted by the MongoDB / OpenSearch drivers.
    Generic,
}

impl Dialect {
    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Dialect::MySql => format!("`{}`", name.replace('`', "``")),
            Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    /// Fully qualified object name.
    pub fn qualify(self, p: &ObjPath) -> String {
        match self {
            Dialect::MySql => {
                if p.db.is_empty() {
                    self.quote_ident(&p.name)
                } else {
                    format!("{}.{}", self.quote_ident(&p.db), self.quote_ident(&p.name))
                }
            }
            Dialect::Postgres => {
                let schema = p.schema.as_deref().unwrap_or("public");
                format!("{}.{}", self.quote_ident(schema), self.quote_ident(&p.name))
            }
            Dialect::Sqlite => {
                if p.db.is_empty() || p.db == "main" {
                    self.quote_ident(&p.name)
                } else {
                    format!("{}.{}", self.quote_ident(&p.db), self.quote_ident(&p.name))
                }
            }
            // `[db]..[t]`: two-part names would be read as schema.table
            Dialect::SqlServer if p.schema.is_none() && !p.db.is_empty() => {
                format!("{}..{}", self.quote_ident(&p.db), self.quote_ident(&p.name))
            }
            Dialect::SqlServer | Dialect::Generic => [Some(p.db.as_str()), p.schema.as_deref(), Some(p.name.as_str())]
                .into_iter()
                .flatten()
                .filter(|x| !x.is_empty())
                .map(|x| self.quote_ident(x))
                .collect::<Vec<_>>()
                .join("."),
            // Oracle: database = owning schema (user)
            Dialect::Oracle => {
                let owner = p.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or(&p.db);
                if owner.is_empty() {
                    self.quote_ident(&p.name)
                } else {
                    format!("{}.{}", self.quote_ident(owner), self.quote_ident(&p.name))
                }
            }
        }
    }

    pub fn string_literal(self, s: &str) -> String {
        match self {
            Dialect::MySql => {
                let mut out = String::with_capacity(s.len() + 2);
                out.push('\'');
                for ch in s.chars() {
                    match ch {
                        '\'' => out.push_str("''"),
                        '\\' => out.push_str("\\\\"),
                        '\0' => out.push_str("\\0"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\u{1a}' => out.push_str("\\Z"),
                        c => out.push(c),
                    }
                }
                out.push('\'');
                out
            }
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    pub fn blob_literal(self, b: &[u8]) -> String {
        let h = crate::value::hex(b);
        match self {
            Dialect::MySql => format!("0x{}", h),
            Dialect::Postgres => format!("'\\x{}'::bytea", h),
            Dialect::Sqlite | Dialect::Generic => format!("X'{}'", h),
            Dialect::SqlServer => format!("0x{}", h),
            Dialect::Oracle => format!("HEXTORAW('{}')", h),
        }
    }

    pub fn limit_clause(self, limit: usize, offset: usize) -> String {
        if matches!(self, Dialect::SqlServer | Dialect::Oracle) {
            return format!(" OFFSET {} ROWS FETCH NEXT {} ROWS ONLY", offset, limit);
        }
        if offset > 0 {
            format!(" LIMIT {} OFFSET {}", limit, offset)
        } else {
            format!(" LIMIT {}", limit)
        }
    }

    pub fn statement_terminator(self) -> &'static str {
        ";"
    }

    /// Data types offered in the table designer type combo.
    pub fn data_types(self) -> &'static [&'static str] {
        match self {
            Dialect::MySql => &[
                "int", "bigint", "tinyint", "smallint", "mediumint", "decimal", "float", "double",
                "bit", "boolean", "char", "varchar", "tinytext", "text", "mediumtext", "longtext",
                "binary", "varbinary", "tinyblob", "blob", "mediumblob", "longblob", "date",
                "datetime", "timestamp", "time", "year", "enum", "set", "json", "geometry",
                "point", "linestring", "polygon",
            ],
            Dialect::Postgres => &[
                "int4", "int8", "int2", "serial", "bigserial", "numeric", "float4", "float8",
                "bool", "varchar", "char", "text", "bytea", "date", "timestamp", "timestamptz",
                "time", "timetz", "interval", "uuid", "json", "jsonb", "inet", "cidr", "macaddr",
                "money", "xml", "point", "tsvector", "int4[]", "text[]",
            ],
            Dialect::Sqlite => &["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC", "BOOLEAN", "DATETIME", "VARCHAR"],
            Dialect::SqlServer => &[
                "int", "bigint", "smallint", "tinyint", "bit", "decimal", "numeric", "money", "float",
                "real", "char", "varchar", "nchar", "nvarchar", "text", "ntext", "binary", "varbinary",
                "date", "datetime", "datetime2", "datetimeoffset", "smalldatetime", "time",
                "uniqueidentifier", "xml", "image",
            ],
            Dialect::Oracle => &[
                "NUMBER", "INTEGER", "FLOAT", "BINARY_FLOAT", "BINARY_DOUBLE", "CHAR", "NCHAR",
                "VARCHAR2", "NVARCHAR2", "CLOB", "NCLOB", "BLOB", "RAW", "DATE", "TIMESTAMP",
                "TIMESTAMP WITH TIME ZONE", "INTERVAL DAY TO SECOND", "JSON",
            ],
            Dialect::Generic => &[
                "INTEGER", "BIGINT", "SMALLINT", "DOUBLE", "REAL", "DECIMAL", "BOOLEAN", "VARCHAR",
                "TEXT", "DATE", "TIME", "TIMESTAMP", "BLOB", "JSON", "UUID",
            ],
        }
    }

    pub fn supports_comments(self) -> bool {
        matches!(self, Dialect::MySql | Dialect::Postgres)
    }

    pub fn explain_prefix(self) -> &'static str {
        match self {
            Dialect::MySql => "EXPLAIN ",
            Dialect::Postgres => "EXPLAIN (FORMAT TEXT) ",
            Dialect::Sqlite => "EXPLAIN QUERY PLAN ",
            Dialect::Oracle => "EXPLAIN PLAN FOR ",
            Dialect::SqlServer | Dialect::Generic => "EXPLAIN ",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(Dialect::MySql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::MySql.string_literal("it's\\"), "'it''s\\\\'");
        assert_eq!(Dialect::Postgres.string_literal("it's"), "'it''s'");
    }

    #[test]
    fn qualify() {
        let p = ObjPath::new("db", None, "t");
        assert_eq!(Dialect::MySql.qualify(&p), "`db`.`t`");
        assert_eq!(Dialect::Postgres.qualify(&p), "\"public\".\"t\"");
        assert_eq!(Dialect::Sqlite.qualify(&ObjPath::new("main", None, "t")), "\"t\"");
        assert_eq!(Dialect::SqlServer.qualify(&ObjPath::new("db", Some("dbo".into()), "t")), "[db].[dbo].[t]");
        assert_eq!(Dialect::Generic.qualify(&ObjPath::new("ks", None, "t")), "\"ks\".\"t\"");
        assert_eq!(Dialect::Oracle.qualify(&ObjPath::new("HR", None, "EMP")), "\"HR\".\"EMP\"");
        assert_eq!(Dialect::Oracle.limit_clause(10, 20), " OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY");
    }
}
