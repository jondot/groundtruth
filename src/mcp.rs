//! MCP stdio server (rmcp 1.7): list/run/explain checks for agents.
//! Stateless — config_src is passed in at startup.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{ConnConfig, parse_checks, parse_connections};
use crate::runner::{Status, build_query_context, run_check_full};
use crate::source::{Dialect, Source};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NameArg {
    /// The name of the check.
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConnArg {
    /// Connection name to target. Omit to use the first connection in the config.
    connection: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DescribeArg {
    /// Connection name to target. Omit to use the first connection in the config.
    connection: Option<String>,
    /// Restrict to one schema (e.g. `public`). Omit to list all user schemas.
    schema: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SampleArg {
    /// Connection name to target. Omit to use the first connection in the config.
    connection: Option<String>,
    /// Table to sample, optionally schema-qualified (e.g. `public.orders`).
    table: String,
    /// Max rows to return (default 10, capped at 100).
    limit: Option<u32>,
}

// Mirrors app::build_sources, but eager — any connection failure aborts.
async fn build_sources(config_src: &str) -> Result<(HashMap<String, Source>, String)> {
    let connections = parse_connections(config_src)?;
    if connections.is_empty() {
        anyhow::bail!("no `connection` block found in config");
    }
    let first_name = connections[0].name.clone();
    let mut sources: HashMap<String, Source> = HashMap::new();
    for conn in connections {
        let source = match &conn.config {
            ConnConfig::ConnectorX(cx) => {
                Source::ConnectorX(crate::source::ConnectorX::new(&cx.dsn)?)
            }
            ConnConfig::Athena(a) => Source::Athena(crate::athena::Athena::connect(a).await?),
        };
        sources.insert(conn.name.clone(), source);
    }
    Ok((sources, first_name))
}

fn tool_ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn tool_err(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

/// Accept only safe SQL identifiers (optionally schema-qualified) for
/// introspection args: each dot-separated part is non-empty ASCII
/// alphanumerics/underscore. This blocks quotes and semicolons, so the value is
/// safe to interpolate into an introspection query.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Error text listing the available connection names.
fn unknown_conn(name: &str, sources: &HashMap<String, Source>) -> String {
    let mut avail: Vec<&String> = sources.keys().collect();
    avail.sort();
    format!("unknown connection {name:?}; available: {avail:?}")
}

/// A string field from an `hcl::Value::Object` row, or "" if absent/non-string.
fn field_str(row: &hcl::Value, key: &str) -> String {
    if let hcl::Value::Object(o) = row
        && let Some(hcl::Value::String(s)) = o.get(key)
    {
        return s.clone();
    }
    String::new()
}

/// Build the `information_schema.columns` introspection query. When `schema` is
/// given it is filtered to exactly that schema (already identifier-validated by
/// the caller); otherwise system schemas are excluded per dialect.
fn describe_sql(dialect: Dialect, schema: Option<&str>) -> String {
    let base = "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                FROM information_schema.columns";
    let filter = match schema {
        Some(s) => format!(" WHERE table_schema = '{s}'"),
        None => match dialect {
            Dialect::Postgres => {
                " WHERE table_schema NOT IN ('pg_catalog', 'information_schema')".to_string()
            }
            Dialect::MySql => " WHERE table_schema NOT IN ('mysql', 'information_schema', \
                 'performance_schema', 'sys')"
                .to_string(),
            _ => String::new(),
        },
    };
    format!("{base}{filter} ORDER BY table_schema, table_name, ordinal_position")
}

#[derive(Clone)]
pub struct GroundtruthMcp {
    config_src: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl GroundtruthMcp {
    #[tool(
        description = "List every check defined in the config: name, SQL query, conditions (warn/fail), and connection name."
    )]
    async fn list_checks(&self) -> CallToolResult {
        let checks = match parse_checks(&self.config_src) {
            Ok(c) => c,
            Err(e) => return tool_err(format!("config parse error: {e:#}")),
        };

        let list: Vec<Value> = checks
            .iter()
            .map(|c| {
                let ctx = build_query_context(c);
                let query_str = match hcl::eval::Evaluate::evaluate(&c.query, &ctx) {
                    Ok(hcl::Value::String(s)) => s,
                    _ => format!("{:?}", c.query),
                };

                let conditions: Vec<&str> = [
                    c.warn.as_ref().map(|_| "warn"),
                    c.fail.as_ref().map(|_| "fail"),
                ]
                .into_iter()
                .flatten()
                .collect();

                json!({
                    "name": c.name,
                    "on": c.on.as_deref().unwrap_or("(default)"),
                    "query": query_str,
                    "conditions": conditions,
                })
            })
            .collect();

        tool_ok(
            serde_json::to_string_pretty(&list).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }

    #[tool(
        description = "Run a single named check against its configured connection and return the result: name, status, detail, sample rows."
    )]
    async fn run_check(&self, Parameters(NameArg { name }): Parameters<NameArg>) -> CallToolResult {
        let checks = match parse_checks(&self.config_src) {
            Ok(c) => c,
            Err(e) => return tool_err(format!("config parse error: {e:#}")),
        };

        let check = match checks.iter().find(|c| c.name == name) {
            Some(c) => c,
            None => {
                return tool_err(format!(
                    "check {:?} not found; available: {:?}",
                    name,
                    checks.iter().map(|c| &c.name).collect::<Vec<_>>()
                ));
            }
        };

        let (sources, first_name) = match build_sources(&self.config_src).await {
            Ok(s) => s,
            Err(e) => return tool_err(format!("connection error: {e:#}")),
        };

        let source_name = check.on.as_deref().unwrap_or(&first_name);
        let source = match sources.get(source_name) {
            Some(s) => s,
            None => return tool_err(format!("unknown connection {:?}", source_name)),
        };

        let result = run_check_full(source, check).await;

        let status_str = match result.status {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Error => "error",
        };

        let sample_json: Vec<Value> = result
            .sample
            .iter()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .collect();

        let out = json!({
            "name": result.name,
            "status": status_str,
            "detail": result.detail,
            "sample": sample_json,
        });

        tool_ok(
            serde_json::to_string_pretty(&out).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }

    #[tool(
        description = "Run a check and, if it FAILs or ERRORs, return a structured explanation with the query, failing sample rows, and a diagnostic hint. If it passes, says so."
    )]
    async fn explain_failure(
        &self,
        Parameters(NameArg { name }): Parameters<NameArg>,
    ) -> CallToolResult {
        let checks = match parse_checks(&self.config_src) {
            Ok(c) => c,
            Err(e) => return tool_err(format!("config parse error: {e:#}")),
        };

        let check = match checks.iter().find(|c| c.name == name) {
            Some(c) => c,
            None => {
                return tool_err(format!(
                    "check {:?} not found; available: {:?}",
                    name,
                    checks.iter().map(|c| &c.name).collect::<Vec<_>>()
                ));
            }
        };

        // Resolve the SQL for display.
        let ctx = build_query_context(check);
        let query_str = match hcl::eval::Evaluate::evaluate(&check.query, &ctx) {
            Ok(hcl::Value::String(s)) => s,
            _ => format!("{:?}", check.query),
        };

        let (sources, first_name) = match build_sources(&self.config_src).await {
            Ok(s) => s,
            Err(e) => return tool_err(format!("connection error: {e:#}")),
        };

        let source_name = check.on.as_deref().unwrap_or(&first_name);
        let source = match sources.get(source_name) {
            Some(s) => s,
            None => return tool_err(format!("unknown connection {:?}", source_name)),
        };

        let result = run_check_full(source, check).await;

        let status_str = match result.status {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Error => "error",
        };

        let is_bad = matches!(result.status, Status::Fail | Status::Error);

        let sample_json: Vec<Value> = result
            .sample
            .iter()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .collect();

        if !is_bad {
            let out = json!({
                "name": result.name,
                "status": status_str,
                "detail": result.detail,
                "message": "Check passed — no failure to explain."
            });
            return tool_ok(
                serde_json::to_string_pretty(&out)
                    .unwrap_or_else(|e| format!("serialize error: {e}")),
            );
        }

        let hint = if result.status == Status::Error {
            format!(
                "An ERROR occurred evaluating the check condition: {}",
                result.detail
            )
        } else {
            let tier_name = if check.fail.is_some() { "fail" } else { "warn" };
            format!(
                "The `{tier_name}` tier fired. {} Review the sample rows below and the SQL query.",
                result.detail
            )
        };

        let out = json!({
            "name": result.name,
            "status": status_str,
            "detail": result.detail,
            "query": query_str,
            "sample": sample_json,
            "hint": hint,
        });

        tool_ok(
            serde_json::to_string_pretty(&out).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }

    #[tool(
        description = "Check that a connection is reachable: runs `SELECT 1` against the named connection (or the first one if omitted) and reports ok/error plus round-trip time in milliseconds. Times out after 10s."
    )]
    async fn check_connection(
        &self,
        Parameters(ConnArg { connection }): Parameters<ConnArg>,
    ) -> CallToolResult {
        let (sources, first_name) = match build_sources(&self.config_src).await {
            Ok(s) => s,
            Err(e) => return tool_err(format!("connection error: {e:#}")),
        };
        let name = connection.unwrap_or(first_name);
        let source = match sources.get(&name) {
            Some(s) => s,
            None => return tool_err(unknown_conn(&name, &sources)),
        };

        let started = Instant::now();
        let probe =
            tokio::time::timeout(Duration::from_secs(10), source.query("SELECT 1 AS ok")).await;
        let elapsed_ms = started.elapsed().as_millis();

        let out = match probe {
            Ok(Ok(_)) => json!({ "connection": name, "ok": true, "elapsed_ms": elapsed_ms }),
            Ok(Err(e)) => json!({
                "connection": name, "ok": false, "elapsed_ms": elapsed_ms,
                "error": format!("{e:#}"),
            }),
            Err(_) => json!({
                "connection": name, "ok": false, "elapsed_ms": elapsed_ms,
                "error": "timed out after 10s",
            }),
        };
        tool_ok(
            serde_json::to_string_pretty(&out).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }

    #[tool(
        description = "List tables and their columns (name, type, nullable) from information_schema, grouped by table. Optionally restrict to one schema. Use this to author checks against real column names instead of guessing."
    )]
    async fn describe_schema(
        &self,
        Parameters(DescribeArg { connection, schema }): Parameters<DescribeArg>,
    ) -> CallToolResult {
        if let Some(s) = &schema
            && !is_safe_identifier(s)
        {
            return tool_err(format!(
                "invalid schema name {s:?}: expected letters, digits, or underscore"
            ));
        }

        let (sources, first_name) = match build_sources(&self.config_src).await {
            Ok(s) => s,
            Err(e) => return tool_err(format!("connection error: {e:#}")),
        };
        let name = connection.unwrap_or(first_name);
        let source = match sources.get(&name) {
            Some(s) => s,
            None => return tool_err(unknown_conn(&name, &sources)),
        };

        let sql = describe_sql(source.dialect(), schema.as_deref());
        let rows = match source.query(&sql).await {
            Ok(r) => r,
            Err(e) => return tool_err(format!("introspection query failed: {e:#}")),
        };

        // Rows arrive ordered by (schema, table, ordinal), so consecutive rows
        // sharing a (schema, table) form one table's column list.
        let mut tables: Vec<Value> = Vec::new();
        let mut cur_key: Option<(String, String)> = None;
        let mut cur_cols: Vec<Value> = Vec::new();
        for row in &rows {
            let key = (field_str(row, "table_schema"), field_str(row, "table_name"));
            if cur_key.as_ref() != Some(&key) {
                if let Some((s, t)) = cur_key.take() {
                    tables.push(json!({ "schema": s, "table": t, "columns": std::mem::take(&mut cur_cols) }));
                }
                cur_key = Some(key);
            }
            cur_cols.push(json!({
                "name": field_str(row, "column_name"),
                "type": field_str(row, "data_type"),
                "nullable": field_str(row, "is_nullable").eq_ignore_ascii_case("yes"),
            }));
        }
        if let Some((s, t)) = cur_key.take() {
            tables.push(json!({ "schema": s, "table": t, "columns": cur_cols }));
        }

        let out = json!({ "connection": name, "tables": tables });
        tool_ok(
            serde_json::to_string_pretty(&out).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }

    #[tool(
        description = "Return up to `limit` sample rows (default 10, max 100) from a table so you can see real data shape and values before writing a check. `table` may be schema-qualified (e.g. public.orders)."
    )]
    async fn sample_table(
        &self,
        Parameters(SampleArg {
            connection,
            table,
            limit,
        }): Parameters<SampleArg>,
    ) -> CallToolResult {
        if !is_safe_identifier(&table) {
            return tool_err(format!(
                "invalid table name {table:?}: expected an identifier like `orders` or `public.orders`"
            ));
        }
        let n = limit.unwrap_or(10).clamp(1, 100);

        let (sources, first_name) = match build_sources(&self.config_src).await {
            Ok(s) => s,
            Err(e) => return tool_err(format!("connection error: {e:#}")),
        };
        let name = connection.unwrap_or(first_name);
        let source = match sources.get(&name) {
            Some(s) => s,
            None => return tool_err(unknown_conn(&name, &sources)),
        };

        let sql = source.dialect().sample_query(&table, n);
        let rows = match source.query(&sql).await {
            Ok(r) => r,
            Err(e) => return tool_err(format!("sample query failed: {e:#}")),
        };

        let rows_json: Vec<Value> = rows
            .iter()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .collect();
        let out = json!({ "table": table, "row_count": rows_json.len(), "rows": rows_json });
        tool_ok(
            serde_json::to_string_pretty(&out).unwrap_or_else(|e| format!("serialize error: {e}")),
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GroundtruthMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("groundtruth", env!("CARGO_PKG_VERSION")),
        )
    }
}

impl GroundtruthMcp {
    pub fn new(config_src: String) -> Self {
        Self {
            config_src,
            tool_router: Self::tool_router(),
        }
    }
}

/// Start the rmcp MCP server over stdio and block until stdin closes.
pub async fn serve_stdio(config_src: String) -> anyhow::Result<()> {
    let server = GroundtruthMcp::new(config_src);
    let transport = rmcp::transport::io::stdio();
    let service = rmcp::service::serve_server(server, transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server init failed: {e}"))?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test unique table name to avoid collisions across concurrent tests
    /// and reruns against the shared Postgres test DB.
    fn unique_table(label: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("gt_mcp_test_{label}_{nanos}")
    }

    /// Build a config whose checks run over connectorx/Postgres. `bad_table` is
    /// the seeded table the `always_fail` check selects from.
    fn make_config(dsn: &str, bad_table: &str) -> String {
        format!(
            r#"
connection "postgres" "main" {{
  dsn = "{dsn}"
}}

check "always_pass" {{
  on    = "main"
  query = "SELECT 1 AS n"
  fail  = rows.count > 100
}}

check "always_fail" {{
  on    = "main"
  query = "SELECT name FROM {bad_table}"
  fail {{
    when   = rows.count > 0
    sample = 3
  }}
}}
"#
        )
    }

    /// Create and populate `table` in Postgres so `always_fail` returns a row.
    async fn seed_db(dsn: &str, table: &str) {
        let pool = sqlx::postgres::PgPool::connect(dsn).await.unwrap();
        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE TABLE {table} (name TEXT)"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("INSERT INTO {table} VALUES ('row1')"))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    /// Drop the seeded table at the end of a test.
    async fn drop_db(dsn: &str, table: &str) {
        if let Ok(pool) = sqlx::postgres::PgPool::connect(dsn).await {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
                .execute(&pool)
                .await;
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn test_list_checks_contains_check_names() {
        // list_checks only parses config — no DB connection happens — so this
        // test needs no real DB. A static (unconnected) postgres dsn suffices.
        let config = make_config("postgres://user@localhost:5432/db", "bad_rows");

        let server = GroundtruthMcp::new(config);
        let result = server.list_checks().await;

        assert!(
            result.is_error != Some(true),
            "list_checks should not be an error"
        );
        let text = match result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(ref t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("always_pass"),
            "should contain 'always_pass'; got: {text}"
        );
        assert!(
            text.contains("always_fail"),
            "should contain 'always_fail'; got: {text}"
        );
    }

    #[tokio::test]
    async fn test_run_check_passing() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping test_run_check_passing");
            return;
        };
        let table = unique_table("run_check_pass");
        seed_db(&dsn, &table).await;
        let config = make_config(&dsn, &table);

        let server = GroundtruthMcp::new(config);
        let result = server
            .run_check(Parameters(NameArg {
                name: "always_pass".into(),
            }))
            .await;

        assert!(
            result.is_error != Some(true),
            "run_check should not be an error"
        );
        let text = match result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(ref t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["status"], "pass");

        drop_db(&dsn, &table).await;
    }

    #[tokio::test]
    async fn test_run_check_not_found_returns_error() {
        // No DB needed: the check lookup fails before any connection is made.
        let config =
            r#"connection "postgres" "main" { dsn = "postgres://user@localhost:5432/db" }"#;
        let server = GroundtruthMcp::new(config.to_string());
        let result = server
            .run_check(Parameters(NameArg {
                name: "does_not_exist".into(),
            }))
            .await;

        assert_eq!(
            result.is_error,
            Some(true),
            "missing check should return is_error: true"
        );
    }

    #[tokio::test]
    async fn test_explain_failure_on_failing_check() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!(
                "GROUNDTRUTH_TEST_DSN unset — skipping test_explain_failure_on_failing_check"
            );
            return;
        };
        let table = unique_table("explain_failure");
        seed_db(&dsn, &table).await;
        let config = make_config(&dsn, &table);

        let server = GroundtruthMcp::new(config);
        let result = server
            .explain_failure(Parameters(NameArg {
                name: "always_fail".into(),
            }))
            .await;

        assert!(
            result.is_error != Some(true),
            "explain_failure should not itself be a protocol error"
        );
        let text = match result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(ref t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["status"], "fail");
        assert!(parsed.get("hint").is_some(), "must include hint");
        assert!(parsed.get("sample").is_some(), "must include sample");

        drop_db(&dsn, &table).await;
    }

    #[tokio::test]
    async fn test_server_info_name_is_groundtruth() {
        let server = GroundtruthMcp::new(String::new());
        let info = server.get_info();
        assert_eq!(info.server_info.name, "groundtruth");
        assert!(!info.server_info.version.is_empty());
    }

    /// Text payload of a successful tool result.
    fn ok_text(result: &CallToolResult) -> String {
        assert!(result.is_error != Some(true), "expected success, got error");
        match result.content.first().unwrap().raw {
            rmcp::model::RawContent::Text(ref t) => t.text.clone(),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn is_safe_identifier_accepts_and_rejects() {
        assert!(is_safe_identifier("orders"));
        assert!(is_safe_identifier("public.orders"));
        assert!(is_safe_identifier("s_1.t_2"));
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("orders; drop table x"));
        assert!(!is_safe_identifier("a'b"));
        assert!(!is_safe_identifier("public."));
        assert!(!is_safe_identifier(".orders"));
    }

    #[test]
    fn describe_sql_filters_by_schema_when_given() {
        let sql = describe_sql(Dialect::Postgres, Some("public"));
        assert!(sql.contains("WHERE table_schema = 'public'"), "got: {sql}");
        let sql = describe_sql(Dialect::Postgres, None);
        assert!(sql.contains("NOT IN ('pg_catalog'"), "got: {sql}");
    }

    #[test]
    fn sample_query_uses_limit() {
        assert_eq!(
            Dialect::MySql.sample_query("public.orders", 5),
            "SELECT * FROM public.orders LIMIT 5"
        );
        assert_eq!(
            Dialect::Postgres.sample_query("orders", 10),
            "SELECT * FROM orders LIMIT 10"
        );
    }

    #[tokio::test]
    async fn test_check_connection_ok() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping test_check_connection_ok");
            return;
        };
        let config = make_config(&dsn, "unused_table");
        let server = GroundtruthMcp::new(config);
        let result = server
            .check_connection(Parameters(ConnArg { connection: None }))
            .await;
        let parsed: Value = serde_json::from_str(&ok_text(&result)).unwrap();
        assert_eq!(parsed["ok"], true, "payload: {parsed}");
        assert_eq!(parsed["connection"], "main");
    }

    #[tokio::test]
    async fn test_check_connection_unknown_name_errors() {
        // Connection resolution fails before any DB call, so no live DB needed.
        let config = make_config("postgres://user@localhost:5432/db", "unused_table");
        let server = GroundtruthMcp::new(config);
        let result = server
            .check_connection(Parameters(ConnArg {
                connection: Some("nope".into()),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn test_describe_schema_lists_seeded_table() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!(
                "GROUNDTRUTH_TEST_DSN unset — skipping test_describe_schema_lists_seeded_table"
            );
            return;
        };
        let table = unique_table("describe");
        seed_db(&dsn, &table).await;
        let config = make_config(&dsn, &table);
        let server = GroundtruthMcp::new(config);

        let result = server
            .describe_schema(Parameters(DescribeArg {
                connection: None,
                schema: Some("public".into()),
            }))
            .await;
        let parsed: Value = serde_json::from_str(&ok_text(&result)).unwrap();
        let tables = parsed["tables"].as_array().unwrap();
        let seeded = tables
            .iter()
            .find(|t| t["table"] == table)
            .unwrap_or_else(|| panic!("seeded table {table} not in {parsed}"));
        let col = seeded["columns"].as_array().unwrap();
        assert!(
            col.iter().any(|c| c["name"] == "name"),
            "expected a `name` column: {seeded}"
        );

        drop_db(&dsn, &table).await;
    }

    #[tokio::test]
    async fn test_sample_table_returns_rows() {
        let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
            eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping test_sample_table_returns_rows");
            return;
        };
        let table = unique_table("sample");
        seed_db(&dsn, &table).await;
        let config = make_config(&dsn, &table);
        let server = GroundtruthMcp::new(config);

        let result = server
            .sample_table(Parameters(SampleArg {
                connection: None,
                table: table.clone(),
                limit: Some(5),
            }))
            .await;
        let parsed: Value = serde_json::from_str(&ok_text(&result)).unwrap();
        assert_eq!(parsed["row_count"], 1, "payload: {parsed}");
        assert_eq!(parsed["rows"][0]["name"], "row1");

        drop_db(&dsn, &table).await;
    }

    #[tokio::test]
    async fn test_sample_table_rejects_bad_identifier() {
        // Validation happens before any DB call.
        let config = make_config("postgres://user@localhost:5432/db", "unused_table");
        let server = GroundtruthMcp::new(config);
        let result = server
            .sample_table(Parameters(SampleArg {
                connection: None,
                table: "orders; drop table users".into(),
                limit: None,
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
    }
}
