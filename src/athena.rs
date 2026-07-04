//! Amazon Athena check source via the official AWS SDK.
//!
//! Athena isn't a wire-protocol database (connectorx can't reach it), so it gets
//! its own [`Source`](crate::source::Source) variant. A query is the standard
//! async dance: `StartQueryExecution` → poll `GetQueryExecution` until terminal
//! → page through `GetQueryResults`, coercing each cell to `hcl::Value` by the
//! column type Athena reports. Read-only, like every other check source.
//!
//! Credentials resolve through the standard AWS chain (env vars, shared profile,
//! SSO, or instance/container role) — nothing groundtruth-specific.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use aws_sdk_athena::Client;
use aws_sdk_athena::types::{QueryExecutionContext, QueryExecutionState, ResultConfiguration};
use hcl::{Map, Value};

use crate::config::AthenaConfig;

/// How often to poll query state. The runner's per-check `timeout` bounds the
/// total wait, so a slow Athena query becomes ERROR there rather than hanging.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// An Athena source. `Client` is cheap to clone (Arc-backed), so is this.
#[derive(Clone)]
pub struct Athena {
    client: Client,
    database: String,
    output_location: Option<String>,
    workgroup: Option<String>,
}

impl Athena {
    /// Build a client from the connection config. Loads AWS config (region +
    /// credential chain) but opens no network connection until a query runs.
    pub async fn connect(cfg: &AthenaConfig) -> Result<Self> {
        let region = aws_config::Region::new(cfg.region.clone());
        let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(region)
            .load()
            .await;
        Ok(Self {
            client: Client::new(&shared),
            database: cfg.database.clone(),
            output_location: cfg.output_location.clone(),
            workgroup: cfg.workgroup.clone(),
        })
    }

    /// Run `sql` and return each result row as an `hcl::Value::Object`.
    pub async fn query(&self, sql: &str) -> Result<Vec<Value>> {
        let query_id = self.start(sql).await?;
        self.wait_until_done(&query_id).await?;
        self.collect_results(&query_id).await
    }

    /// Submit the query; return its execution id.
    async fn start(&self, sql: &str) -> Result<String> {
        let ctx = QueryExecutionContext::builder()
            .database(&self.database)
            .build();

        let mut req = self
            .client
            .start_query_execution()
            .query_string(sql)
            .query_execution_context(ctx);

        if let Some(loc) = &self.output_location {
            req = req
                .result_configuration(ResultConfiguration::builder().output_location(loc).build());
        }
        if let Some(wg) = &self.workgroup {
            req = req.work_group(wg);
        }

        let out = req
            .send()
            .await
            .context("Athena StartQueryExecution failed")?;
        out.query_execution_id()
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Athena returned no query execution id"))
    }

    /// Poll until the query reaches a terminal state; error loudly on FAILED/CANCELLED.
    async fn wait_until_done(&self, query_id: &str) -> Result<()> {
        loop {
            let exec = self
                .client
                .get_query_execution()
                .query_execution_id(query_id)
                .send()
                .await
                .context("Athena GetQueryExecution failed")?;

            let status = exec.query_execution().and_then(|q| q.status());
            let state = status.and_then(|s| s.state());

            match state {
                Some(QueryExecutionState::Succeeded) => return Ok(()),
                Some(QueryExecutionState::Failed) | Some(QueryExecutionState::Cancelled) => {
                    let reason = status
                        .and_then(|s| s.state_change_reason())
                        .unwrap_or("no reason given");
                    bail!("Athena query {state:?}: {reason}");
                }
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    /// Page through results and coerce to `hcl::Value` objects.
    async fn collect_results(&self, query_id: &str) -> Result<Vec<Value>> {
        let mut rows_out: Vec<Value> = Vec::new();
        let mut next_token: Option<String> = None;
        let mut first_page = true;

        loop {
            let mut req = self.client.get_query_results().query_execution_id(query_id);
            if let Some(tok) = &next_token {
                req = req.next_token(tok);
            }
            let page = req.send().await.context("Athena GetQueryResults failed")?;

            let result_set = page.result_set();

            // Column names + types from metadata (authoritative for coercion).
            let columns: Vec<(String, String)> = result_set
                .and_then(|rs| rs.result_set_metadata())
                .map(|m| {
                    m.column_info()
                        .iter()
                        .map(|c| (c.name().to_string(), c.r#type().to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let rows = result_set.map(|rs| rs.rows()).unwrap_or_default();

            for (i, row) in rows.iter().enumerate() {
                // Athena repeats the column headers as the first row of page one.
                if first_page && i == 0 {
                    continue;
                }
                let data = row.data();
                let mut obj = Map::new();
                for (col_idx, (name, ty)) in columns.iter().enumerate() {
                    let raw = data.get(col_idx).and_then(|d| d.var_char_value());
                    obj.insert(name.clone(), coerce(ty, raw));
                }
                rows_out.push(Value::Object(obj));
            }

            first_page = false;
            match page.next_token() {
                Some(tok) => next_token = Some(tok.to_string()),
                None => break,
            }
        }

        Ok(rows_out)
    }
}

/// Coerce one Athena cell (always delivered as a string) by its column type.
/// Numbers → Number, boolean → Bool, NULL → Null, everything else → String.
/// An unparseable number falls back to the raw string rather than failing.
fn coerce(ty: &str, raw: Option<&str>) -> Value {
    let Some(s) = raw else { return Value::Null };
    match ty {
        "tinyint" | "smallint" | "integer" | "int" | "bigint" => s
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(s)),
        "double" | "float" | "real" | "decimal" => s
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(s)),
        "boolean" => match s {
            "true" => Value::from(true),
            "false" => Value::from(false),
            other => Value::from(other),
        },
        _ => Value::from(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_types() {
        assert_eq!(coerce("bigint", Some("42")), Value::from(42_i64));
        assert_eq!(coerce("double", Some("3.5")), Value::from(3.5_f64));
        assert_eq!(coerce("boolean", Some("true")), Value::from(true));
        assert_eq!(coerce("varchar", Some("hi")), Value::from("hi"));
        assert_eq!(coerce("integer", None), Value::Null);
        // Unparseable number degrades to string, never panics.
        assert_eq!(coerce("integer", Some("NaN-ish")), Value::from("NaN-ish"));
    }
}
