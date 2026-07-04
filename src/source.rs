//! Check data sources.
//!
//! All check queries are read-only and go through **connectorx** (Postgres,
//! MySQL, BigQuery, Trino; Redshift via the Postgres wire), which returns
//! Apache Arrow that [`crate::cx`] coerces to `hcl::Value` through one path.
//! `Static` is an in-memory source of canned rows, used by tests.
//!
//! The writable side — groundtruth's own `failing_state` bookkeeping — is a
//! separate concern handled by sqlx in [`crate::state`]; it never touches this
//! check-query path.

use anyhow::Result;
use hcl::Value;

/// The SQL dialect of a source. Introspection queries are standard
/// `information_schema` SQL across every supported engine, so the dialect only
/// tags which `NOT IN (...)` system-schema filter to apply during introspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
    BigQuery,
    Trino,
    Athena,
    /// In-memory test source; introspection is not meaningful.
    Static,
}

impl Dialect {
    /// Build a "sample up to `n` rows from `table`" query. `table` must already
    /// be validated as a safe identifier by the caller.
    pub fn sample_query(&self, table: &str, n: u32) -> String {
        format!("SELECT * FROM {table} LIMIT {n}")
    }
}

/// The data source for a check. `Clone` is cheap.
#[derive(Clone)]
pub enum Source {
    /// Broad multi-provider reads via connectorx.
    ConnectorX(ConnectorX),
    /// Amazon Athena via the AWS SDK (not a connectorx engine).
    Athena(crate::athena::Athena),
    /// In-memory canned rows — for tests (and any future static source). The
    /// SQL is ignored; the rows are returned as-is.
    Static(Vec<Value>),
}

impl Source {
    pub async fn query(&self, sql: &str) -> Result<Vec<Value>> {
        match self {
            Source::ConnectorX(cx) => cx.query(sql).await,
            Source::Athena(a) => a.query(sql).await,
            Source::Static(rows) => Ok(rows.clone()),
        }
    }

    /// The SQL dialect, used to build portable introspection queries.
    pub fn dialect(&self) -> Dialect {
        match self {
            Source::ConnectorX(cx) => cx.dialect(),
            Source::Athena(_) => Dialect::Athena,
            Source::Static(_) => Dialect::Static,
        }
    }
}

/// A connectorx-backed source. connectorx connects per query (no pool), so this
/// just holds the validated connection URL; a down/hung host becomes ERROR via
/// the runner's per-check timeout.
#[derive(Clone)]
pub struct ConnectorX {
    dsn: String,
}

impl ConnectorX {
    /// Validate the connection URL up front (parse only, no connection) so a
    /// malformed or unsupported `dsn` fails at startup instead of on first check.
    pub fn new(dsn: &str) -> Result<Self> {
        crate::cx::validate_connection_url(dsn)?;
        Ok(Self {
            dsn: dsn.to_string(),
        })
    }

    pub async fn query(&self, sql: &str) -> Result<Vec<Value>> {
        crate::cx::query(self.dsn.clone(), sql.to_string()).await
    }

    /// The SQL dialect this DSN routes to (by URL scheme).
    pub fn dialect(&self) -> Dialect {
        crate::cx::dialect_of(&self.dsn)
    }
}
