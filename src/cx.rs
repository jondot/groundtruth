//! Broad multi-provider source via connectorx.
//!
//! connectorx runs a read-only query against any of its backends (MySQL,
//! BigQuery, Trino; Redshift via the Postgres wire) and
//! returns Apache Arrow. Because every groundtruth check is a read-only `SELECT`,
//! one Arrow → `hcl::Value` coercion (`arrow_cell_to_value`) covers *every*
//! provider — no per-engine coercion sprawl.
//!
//! connectorx's `get_arrow` is synchronous and CPU-bound (rayon internally), so
//! the async [`query`] runs it on `spawn_blocking`. The per-check timeout is
//! applied by the runner (`with_timeout` around `source.query()`), so a down or
//! hung provider surfaces as ERROR there rather than hanging the daemon — no
//! second timeout is needed here.

use anyhow::{Context, Result, bail};
use arrow::array::{
    Array, BooleanArray, Date32Array, Date64Array, Decimal128Array, Decimal256Array, Float32Array,
    Float64Array, Int8Array, Int16Array, Int32Array, Int64Array, LargeStringArray, StringArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use connectorx::prelude::{CXQuery, SourceConn, SourceType, get_arrow};
use hcl::{Map, Value};

use crate::source::Dialect;

/// Validate a check connection URL by asking connectorx to route it — no
/// hardcoded provider list. The dsn scheme is authoritative: `postgres://`,
/// `mysql://`, `bigquery://`, `trino+http://`, ... An unknown scheme, or one
/// that needs a backend this binary doesn't ship, fails loud here (so
/// `gt check` catches it) rather than at first query.
pub fn validate_connection_url(dsn: &str) -> Result<()> {
    let sc = SourceConn::try_from(dsn).context("invalid connection URL")?;
    match sc.ty {
        SourceType::Postgres | SourceType::MySQL | SourceType::BigQuery | SourceType::Trino => {
            Ok(())
        }
        SourceType::Unknown => bail!(
            "unrecognized connection URL scheme in {dsn:?}; connectorx routes by scheme \
             (postgres://, mysql://, bigquery://, trino+http://, ...)"
        ),
        // Routable by connectorx but not compiled into this build (native client
        // libs / embedded / Kerberos C deps).
        other => bail!(
            "{other:?} connections are not supported in this build \
             (SQLite is a state-store-only backend; SQL Server, Oracle, and DuckDB \
             are not compiled in)"
        ),
    }
}

/// Map a connection URL to its [`Dialect`] by scheme. Falls back to `Postgres`
/// for an unrecognized scheme — introspection is only ever called on a source
/// [`validate_connection_url`] already accepted, so the fallback is unreachable
/// in practice and just keeps this total.
pub fn dialect_of(dsn: &str) -> Dialect {
    match SourceConn::try_from(dsn).map(|sc| sc.ty) {
        Ok(SourceType::Postgres) => Dialect::Postgres,
        Ok(SourceType::MySQL) => Dialect::MySql,
        Ok(SourceType::BigQuery) => Dialect::BigQuery,
        Ok(SourceType::Trino) => Dialect::Trino,
        _ => Dialect::Postgres,
    }
}

/// Run `sql` against `dsn` via connectorx and return each row as an
/// `hcl::Value::Object`. Blocking — call through [`query`] from async code.
pub fn query_blocking(dsn: &str, sql: &str) -> Result<Vec<Value>> {
    let source_conn = SourceConn::try_from(dsn).context("parsing connectorx connection URL")?;
    let queries = [CXQuery::naked(sql)];
    let destination = get_arrow(&source_conn, None, &queries, None)
        .map_err(|e| anyhow::anyhow!("connectorx query failed: {e}"))?;
    let batches = destination
        .arrow()
        .map_err(|e| anyhow::anyhow!("collecting Arrow result: {e}"))?;
    batches_to_values(&batches)
}

/// Async wrapper: runs the blocking connectorx query on a blocking thread. The
/// runner wraps this in the per-check timeout, so we don't double up here.
pub async fn query(dsn: String, sql: String) -> Result<Vec<Value>> {
    match tokio::task::spawn_blocking(move || query_blocking(&dsn, &sql)).await {
        Ok(result) => result,
        Err(join_err) => bail!("connectorx worker thread failed: {join_err}"),
    }
}

/// Flatten Arrow record batches into one `hcl::Value::Object` per row.
fn batches_to_values(batches: &[RecordBatch]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            let mut obj = Map::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let array = batch.column(col_idx);
                let value = arrow_cell_to_value(array, row, field.name())?;
                obj.insert(field.name().clone(), value);
            }
            out.push(Value::Object(obj));
        }
    }
    Ok(out)
}

/// Coerce one Arrow cell to `hcl::Value`. Numbers → Number, text/temporal →
/// String (RFC-3339-ish), bool → Bool, NULL → Null. Unhandled Arrow types fail
/// loudly naming the column rather than silently becoming Null.
fn arrow_cell_to_value(array: &dyn Array, row: usize, col: &str) -> Result<Value> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }

    macro_rules! prim {
        ($ty:ty, $as:expr) => {{
            let a = array
                .as_any()
                .downcast_ref::<$ty>()
                .with_context(|| format!("column {col:?}: Arrow downcast failed"))?;
            #[allow(clippy::redundant_closure_call)]
            Ok($as(a.value(row)))
        }};
    }

    match array.data_type() {
        DataType::Boolean => prim!(BooleanArray, Value::from),
        DataType::Int8 => prim!(Int8Array, |v| Value::from(v as i64)),
        DataType::Int16 => prim!(Int16Array, |v| Value::from(v as i64)),
        DataType::Int32 => prim!(Int32Array, |v| Value::from(v as i64)),
        DataType::Int64 => prim!(Int64Array, Value::from),
        DataType::UInt8 => prim!(UInt8Array, |v| Value::from(v as i64)),
        DataType::UInt16 => prim!(UInt16Array, |v| Value::from(v as i64)),
        DataType::UInt32 => prim!(UInt32Array, |v| Value::from(v as i64)),
        DataType::UInt64 => prim!(UInt64Array, |v| Value::from(v as i64)),
        DataType::Float32 => prim!(Float32Array, |v| Value::from(v as f64)),
        DataType::Float64 => prim!(Float64Array, Value::from),
        DataType::Utf8 => prim!(StringArray, |v: &str| Value::from(v.to_string())),
        DataType::LargeUtf8 => prim!(LargeStringArray, |v: &str| Value::from(v.to_string())),
        // Postgres NUMERIC and SUM/AVG come back as Arrow decimals. Coerce to
        // f64 via the column's scale (fine for thresholds; same lossy contract
        // the native NUMERIC path had).
        DataType::Decimal128(_precision, scale) => {
            let a = array
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .with_context(|| format!("column {col:?}: Arrow downcast failed"))?;
            let v = a.value(row) as f64 / 10f64.powi(*scale as i32);
            Ok(Value::from(v))
        }
        DataType::Decimal256(_precision, scale) => {
            let a = array
                .as_any()
                .downcast_ref::<Decimal256Array>()
                .with_context(|| format!("column {col:?}: Arrow downcast failed"))?;
            // i256 → f64 via its decimal string (no direct to_f64).
            let raw: f64 = a
                .value(row)
                .to_string()
                .parse()
                .with_context(|| format!("column {col:?}: Decimal256 out of f64 range"))?;
            Ok(Value::from(raw / 10f64.powi(*scale as i32)))
        }
        DataType::Date32 => {
            let a = array
                .as_any()
                .downcast_ref::<Date32Array>()
                .with_context(|| format!("column {col:?}: Arrow downcast failed"))?;
            match a.value_as_date(row) {
                Some(d) => Ok(Value::from(d.to_string())),
                None => Ok(Value::Null),
            }
        }
        DataType::Date64 => {
            let a = array
                .as_any()
                .downcast_ref::<Date64Array>()
                .with_context(|| format!("column {col:?}: Arrow downcast failed"))?;
            match a.value_as_datetime(row) {
                Some(dt) => Ok(Value::from(dt.to_string())),
                None => Ok(Value::Null),
            }
        }
        DataType::Timestamp(unit, _tz) => {
            let dt = match unit {
                TimeUnit::Second => array
                    .as_any()
                    .downcast_ref::<TimestampSecondArray>()
                    .and_then(|a| a.value_as_datetime(row)),
                TimeUnit::Millisecond => array
                    .as_any()
                    .downcast_ref::<TimestampMillisecondArray>()
                    .and_then(|a| a.value_as_datetime(row)),
                TimeUnit::Microsecond => array
                    .as_any()
                    .downcast_ref::<TimestampMicrosecondArray>()
                    .and_then(|a| a.value_as_datetime(row)),
                TimeUnit::Nanosecond => array
                    .as_any()
                    .downcast_ref::<TimestampNanosecondArray>()
                    .and_then(|a| a.value_as_datetime(row)),
            };
            match dt {
                Some(dt) => Ok(Value::from(dt.and_utc().to_rfc3339())),
                None => Ok(Value::Null),
            }
        }
        other => bail!(
            "column {col:?} has unhandled Arrow type {other:?}; \
             add explicit handling in arrow_cell_to_value"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proves connectorx connects, returns Arrow, and the coercion is correct.
    // Honors GROUNDTRUTH_TEST_DSN (Postgres); skips when unset so CI without a DB stays green.
    #[test]
    fn connectorx_postgres_roundtrip() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping connectorx roundtrip");
            return;
        };
        let rows = query_blocking(
            &dsn,
            "select 1::int4 as n, 'hello'::text as s, 3.5::float8 as f, true as b, null::int4 as z",
        )
        .expect("connectorx query");
        assert_eq!(rows.len(), 1);
        let Value::Object(o) = &rows[0] else {
            panic!("expected object row")
        };
        assert_eq!(o.get("n"), Some(&Value::from(1i64)));
        assert_eq!(o.get("s"), Some(&Value::from("hello".to_string())));
        assert_eq!(o.get("f"), Some(&Value::from(3.5f64)));
        assert_eq!(o.get("b"), Some(&Value::from(true)));
        assert_eq!(o.get("z"), Some(&Value::Null));
    }
}
