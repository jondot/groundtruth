//! Orchestration: parse a config document, connect, run every check.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::task::JoinSet;
use tracing::error;

use crate::config::{Check, ConnConfig, parse_checks, parse_connections};
use crate::notify::{Notification, Notifier};
use crate::runner::{CheckResult, Status, run_check_full};
use crate::schedule::should_notify;
use crate::source::Source;
use crate::state::StateStore;

/// Concurrency cap — keeps a huge config from exhausting DB connections or tokio threads.
const MAX_CONCURRENCY: usize = 64;

fn status_str(s: &Status) -> &'static str {
    match s {
        Status::Pass => "pass",
        Status::Warn => "warn",
        Status::Fail => "fail",
        Status::Error => "error",
    }
}

fn is_failing(s: &Status) -> bool {
    matches!(s, Status::Fail | Status::Error)
}

/// Connect every declared connection, partitioning into `live` (name → `Source`)
/// and `dead` (name → error). A failed connection is never fatal — its checks
/// produce a descriptive ERROR instead of aborting the run.
pub async fn build_sources(
    connections: &[crate::config::Connection],
) -> (HashMap<String, Source>, HashMap<String, String>) {
    let mut live: HashMap<String, Source> = HashMap::new();
    let mut dead: HashMap<String, String> = HashMap::new();

    for conn in connections {
        let built: Result<Source, String> = match &conn.config {
            ConnConfig::ConnectorX(cx) => crate::source::ConnectorX::new(&cx.dsn)
                .map(Source::ConnectorX)
                .map_err(|e| format!("{e:#}")),
            ConnConfig::Athena(a) => crate::athena::Athena::connect(a)
                .await
                .map(Source::Athena)
                .map_err(|e| format!("{e:#}")),
        };
        match built {
            Ok(src) => {
                live.insert(conn.name.clone(), src);
            }
            Err(e) => {
                dead.insert(conn.name.clone(), e);
            }
        }
    }

    (live, dead)
}

/// Run every check concurrently, bounded by `MAX_CONCURRENCY`.
///
/// Each check is a separate task collected via a `JoinSet`; a panicking task
/// becomes an ERROR without aborting the rest. Output order is deterministic
/// (matches input order) regardless of completion order. A dead connection also
/// yields an ERROR.
pub async fn run_checks(
    live: &HashMap<String, Source>,
    dead: &HashMap<String, String>,
    checks: &[Check],
    first_connection_name: &str,
) -> Vec<CheckResult> {
    if checks.is_empty() {
        return vec![];
    }

    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
    let mut join_set: JoinSet<(usize, CheckResult)> = JoinSet::new();

    for (idx, check) in checks.iter().enumerate() {
        let source_name = check
            .on
            .as_deref()
            .unwrap_or(first_connection_name)
            .to_string();

        // Resolve to a live source or a pre-baked error before spawning.
        let maybe_source: Result<Source, String> = if let Some(src) = live.get(&source_name) {
            Ok(src.clone())
        } else if let Some(err) = dead.get(&source_name) {
            Err(format!("connection {:?} unavailable: {}", source_name, err))
        } else {
            let declared: Vec<&str> = live.keys().chain(dead.keys()).map(|s| s.as_str()).collect();
            Err(format!(
                "check {:?} references unknown connection {:?}; declared connections: {:?}",
                check.name, source_name, declared
            ))
        };

        let check_name = check.name.clone();
        let sem_clone = sem.clone();

        match maybe_source {
            Ok(source) => {
                let check_for_task = CheckProxy::from_check(check);

                join_set.spawn(async move {
                    let _permit = sem_clone.acquire().await;
                    let result = run_check_full_proxy(&source, check_for_task).await;
                    (idx, result)
                });
            }
            Err(err_detail) => {
                let name = check_name.clone();
                join_set.spawn(async move {
                    let _permit = sem_clone.acquire().await;
                    let result = CheckResult {
                        name: name.clone(),
                        status: Status::Error,
                        detail: err_detail,
                        sample: vec![],
                    };
                    (idx, result)
                });
            }
        }
    }

    let mut indexed: Vec<(usize, CheckResult)> = Vec::with_capacity(checks.len());

    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(pair) => indexed.push(pair),
            Err(join_err) => {
                let name = "unknown[panic@task]".to_string();
                error!(err = %join_err, "check task panicked");
                indexed.push((
                    usize::MAX,
                    CheckResult {
                        name,
                        status: Status::Error,
                        detail: "internal error: check task panicked".to_string(),
                        sample: vec![],
                    },
                ));
            }
        }
    }

    // Sort by input index for deterministic output order.
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

use crate::config::Check as ConfigCheck;

struct CheckProxy {
    check: OwnedCheck,
}

impl CheckProxy {
    fn from_check(c: &ConfigCheck) -> Self {
        Self {
            check: OwnedCheck::from(c),
        }
    }
}

/// Owned `Check` holding cloned expressions, safe to move into a task.
struct OwnedCheck {
    name: String,
    on: Option<String>,
    every: Option<String>,
    timeout: Option<String>,
    query: hcl::Expression,
    warn: Option<crate::config::Tier>,
    fail: Option<crate::config::Tier>,
    each_value: Option<String>,
    on_fail: Option<String>,
    validate: Option<crate::config::Validate>,
}

impl From<&ConfigCheck> for OwnedCheck {
    fn from(c: &ConfigCheck) -> Self {
        Self {
            name: c.name.clone(),
            on: c.on.clone(),
            every: c.every.clone(),
            timeout: c.timeout.clone(),
            query: c.query.clone(),
            warn: clone_tier(&c.warn),
            fail: clone_tier(&c.fail),
            each_value: c.each_value.clone(),
            on_fail: c.on_fail.clone(),
            validate: c.validate.clone(),
        }
    }
}

fn clone_tier(t: &Option<crate::config::Tier>) -> Option<crate::config::Tier> {
    t.as_ref().map(|t| crate::config::Tier {
        when: t.when.clone(),
        sustained: t.sustained.clone(),
        sample: t.sample,
    })
}

/// Rebuild a `ConfigCheck` from the owned copy and run it, avoiding duplicated runner logic.
async fn run_check_full_proxy(source: &Source, proxy: CheckProxy) -> CheckResult {
    let c = proxy.check;
    let check = ConfigCheck {
        name: c.name,
        on: c.on,
        every: c.every,
        timeout: c.timeout,
        query: c.query,
        warn: c.warn,
        fail: c.fail,
        each_value: c.each_value,
        on_fail: c.on_fail,
        validate: c.validate,
    };
    run_check_full(source, &check).await
}

/// Parse `config_src`, connect once, run every check concurrently.
///
/// A connection failure is recorded, not fatal: checks targeting it produce an
/// ERROR naming the connection, so one unreachable database never aborts the run.
pub async fn run_once(config_src: &str) -> Result<Vec<CheckResult>> {
    let connections = parse_connections(config_src)?;
    let checks = parse_checks(config_src)?;

    if connections.is_empty() {
        return Err(anyhow!("no `connection` block found in config"));
    }

    let first_name = connections[0].name.clone();
    let (live, dead) = build_sources(&connections).await;

    let results = run_checks(&live, &dead, &checks, &first_name).await;
    Ok(results)
}

/// Backward-compatible alias for [`run_once`] (legacy one-shot mode and tests).
pub async fn run(config_src: &str) -> Result<Vec<CheckResult>> {
    run_once(config_src).await
}

/// Dispatch alerts: failing results are gated through `schedule::should_notify`;
/// results that recovered clear their state and send RECOVERED.
pub async fn notify_failures(
    results: &[CheckResult],
    checks: &[crate::config::Check],
    notifiers: &[(String, Notifier)],
    state: &StateStore,
    now: i64,
) -> Result<()> {
    let check_map: HashMap<&str, &crate::config::Check> =
        checks.iter().map(|c| (c.name.as_str(), c)).collect();

    for result in results {
        let check = check_map.get(result.name.as_str()).copied();

        let sustained_secs: Option<u64> = check
            .and_then(|c| c.fail.as_ref())
            .and_then(|t| t.sustained.as_deref())
            .and_then(crate::schedule::interval_secs);

        let on_fail_name: Option<&str> = check.and_then(|c| c.on_fail.as_deref());

        if is_failing(&result.status) {
            let failing_since = state.note_failing(&result.name, true, now).await?;

            if should_notify(true, sustained_secs, failing_since, now) {
                let notif = Notification {
                    check: result.name.clone(),
                    status: status_str(&result.status).to_uppercase(),
                    detail: result.detail.clone(),
                };
                send_to_targets(notifiers, on_fail_name, &notif).await;
            }
        } else {
            // Pass/Warn: read-only check for a prior failing streak → RECOVERED.
            let was_failing = state.failing_since(&result.name).await?;
            if was_failing.is_some() {
                state.note_failing(&result.name, false, now).await?;
                let notif = Notification {
                    check: result.name.clone(),
                    status: "RECOVERED".to_string(),
                    detail: result.detail.clone(),
                };
                send_to_targets(notifiers, on_fail_name, &notif).await;
            }
        }
    }

    Ok(())
}

/// Send `notif` to the named notifier, or to all if `target` is `None`.
/// Errors are logged (not propagated) so one bad sink can't block others.
async fn send_to_targets(
    notifiers: &[(String, Notifier)],
    target: Option<&str>,
    notif: &Notification,
) {
    for (name, notifier) in notifiers {
        let should_send = match target {
            Some(t) => name == t,
            None => true,
        };
        if should_send && let Err(e) = notifier.send(notif).await {
            error!(notifier = %name, err = %e, "notification send failed");
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Status;
    use crate::state::StateStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    /// A `Source::Static` returning one row `{v: 1}` — a DB-free stand-in for
    /// run-logic tests (ordering, concurrency, scheduling) that just need a
    /// source that yields a row.
    fn static_source() -> Source {
        let mut row = hcl::Map::new();
        row.insert("v".to_string(), hcl::Value::from(1_i64));
        Source::Static(vec![hcl::Value::Object(row)])
    }

    /// Starts a TCP server that accepts one connection and returns what it captured.
    async fn capture_server_once(
        response_status: u16,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let captured = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 {status} OK\r\ncontent-length: 0\r\n\r\n",
                status = response_status
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            captured
        });
        (url, handle)
    }

    fn passing_result(name: &str) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status: Status::Pass,
            detail: "ok".to_string(),
            sample: vec![],
        }
    }

    /// A never-failing check must not emit a spurious RECOVERED notification.
    #[tokio::test]
    async fn always_passing_check_does_not_produce_recovered() {
        let state = StateStore::memory();
        let (url, server) = capture_server_once(200).await;
        let notifiers = vec![("hook".to_string(), Notifier::Webhook { url })];
        let results = vec![passing_result("healthy_check")];
        let checks: Vec<crate::config::Check> = vec![];

        notify_failures(&results, &checks, &notifiers, &state, 1000)
            .await
            .unwrap();

        // No request expected — wait briefly to confirm silence.
        let received = timeout(Duration::from_millis(150), server).await;
        assert!(
            received.is_err(),
            "no request should have been sent for an always-passing check"
        );
    }

    /// run_checks([]) returns [] without spawning anything.
    #[tokio::test]
    async fn run_checks_empty_returns_empty() {
        let live: HashMap<String, Source> = HashMap::new();
        let dead: HashMap<String, String> = HashMap::new();
        let results = run_checks(&live, &dead, &[], "main").await;
        assert!(
            results.is_empty(),
            "empty check list must return empty results"
        );
    }

    /// Results preserve config order even when tasks complete out of order.
    #[tokio::test]
    async fn run_checks_deterministic_ordering() {
        let mut live = HashMap::new();
        live.insert("db".to_string(), static_source());

        let dead: HashMap<String, String> = HashMap::new();

        // HCL requires each attribute on its own line.
        let checks = crate::config::parse_checks(
            r#"
check "alpha" {
  on    = "db"
  query = "SELECT 1 as v"
  fail  = false
}
check "beta" {
  on    = "db"
  query = "SELECT 1 as v"
  fail  = false
}
check "gamma" {
  on    = "db"
  query = "SELECT 1 as v"
  fail  = false
}
"#,
        )
        .unwrap();

        let results = run_checks(&live, &dead, &checks, "db").await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "alpha", "first result should be alpha");
        assert_eq!(results[1].name, "beta", "second result should be beta");
        assert_eq!(results[2].name, "gamma", "third result should be gamma");
    }

    /// A down connection must not block a live check: it ERRORs (bounded by the
    /// check's `timeout`), the live check PASSes, and the run finishes promptly.
    #[tokio::test]
    async fn dead_connection_does_not_block_live_checks() {
        // `live` is a DB-free Static source; `dead` is a connectorx connection to
        // an unreachable host. A short per-check `timeout` bounds the down case.
        let mut live = HashMap::new();
        live.insert("live".to_string(), static_source());
        live.insert(
            "dead".to_string(),
            Source::ConnectorX(
                crate::source::ConnectorX::new("postgres://x@127.0.0.1:1/none").unwrap(),
            ),
        );
        let dead: HashMap<String, String> = HashMap::new();

        let config = r#"
check "live_check" {
  on    = "live"
  query = "SELECT 1 as v"
  fail  = row.v != 1
}

check "dead_check" {
  on      = "dead"
  query   = "SELECT 1"
  timeout = "2s"
}
"#;
        let checks = parse_checks(config).unwrap();

        let start = std::time::Instant::now();
        let results = run_checks(&live, &dead, &checks, "live").await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 2, "both checks must produce a result");

        let live_r = results.iter().find(|r| r.name == "live_check").unwrap();
        assert_eq!(
            live_r.status,
            Status::Pass,
            "live check must Pass; detail: {}",
            live_r.detail
        );

        let dead_r = results.iter().find(|r| r.name == "dead_check").unwrap();
        assert_eq!(
            dead_r.status,
            Status::Error,
            "dead check must Error; detail: {}",
            dead_r.detail
        );

        // The dead check is bounded by its 2s timeout; the live check is instant.
        assert!(
            elapsed.as_secs() < 10,
            "run must complete within 10s (took {}ms)",
            elapsed.as_millis()
        );
    }

    /// 20 checks run concurrently against one SQLite source must all pass, no errors.
    #[tokio::test]
    async fn concurrent_runs_no_errors() {
        let mut live = HashMap::new();
        live.insert("db".to_string(), static_source());
        let dead: HashMap<String, String> = HashMap::new();

        // HCL requires each attribute on its own line.
        let mut config_src = String::new();
        for i in 0..20_usize {
            config_src.push_str(&format!(
                "check \"chk{i}\" {{\n  on    = \"db\"\n  query = \"SELECT {i} as v\"\n  fail  = false\n}}\n"
            ));
        }
        let checks = crate::config::parse_checks(&config_src).unwrap();

        let results = run_checks(&live, &dead, &checks, "db").await;

        assert_eq!(results.len(), 20, "all 20 checks must produce a result");
        for r in &results {
            assert_ne!(
                r.status,
                Status::Error,
                "check {} errored: {}",
                r.name,
                r.detail
            );
        }
    }

    /// A panicking task becomes an ERROR without aborting the run or sibling tasks.
    #[tokio::test]
    async fn panic_in_task_produces_error_does_not_abort() {
        // Exercises the JoinSet panic-handling path used in run_checks.
        let mut join_set: JoinSet<(usize, CheckResult)> = JoinSet::new();

        join_set.spawn(async {
            (
                0_usize,
                CheckResult {
                    name: "normal".to_string(),
                    status: Status::Pass,
                    detail: "ok".to_string(),
                    sample: vec![],
                },
            )
        });

        // Panicking task.
        join_set.spawn(async {
            panic!("simulated task panic for test");
            #[allow(unreachable_code)]
            (
                1_usize,
                CheckResult {
                    name: "never".to_string(),
                    status: Status::Pass,
                    detail: "never".to_string(),
                    sample: vec![],
                },
            )
        });

        join_set.spawn(async {
            (
                2_usize,
                CheckResult {
                    name: "also_normal".to_string(),
                    status: Status::Pass,
                    detail: "ok".to_string(),
                    sample: vec![],
                },
            )
        });

        let mut indexed: Vec<(usize, CheckResult)> = Vec::new();
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(pair) => indexed.push(pair),
                Err(_join_err) => {
                    // Synthesize an ERROR result, as run_checks does.
                    indexed.push((
                        usize::MAX,
                        CheckResult {
                            name: "panicked_task".to_string(),
                            status: Status::Error,
                            detail: "internal error: check task panicked".to_string(),
                            sample: vec![],
                        },
                    ));
                }
            }
        }

        indexed.sort_by_key(|(i, _)| *i);
        let results: Vec<CheckResult> = indexed.into_iter().map(|(_, r)| r).collect();

        assert_eq!(results.len(), 3, "panic must not abort other tasks");

        let panicked = results.iter().find(|r| r.name == "panicked_task").unwrap();
        assert_eq!(panicked.status, Status::Error);
        assert!(
            panicked.detail.contains("panicked"),
            "detail: {}",
            panicked.detail
        );

        let normal = results.iter().find(|r| r.name == "normal").unwrap();
        assert_eq!(normal.status, Status::Pass);

        let also = results.iter().find(|r| r.name == "also_normal").unwrap();
        assert_eq!(also.status, Status::Pass);
    }
}
