//! app::run_once and app::notify_failures over Postgres (via connectorx), with a
//! local TcpListener capture server asserting webhook delivery.
//!
//! Every test that touches a real database is gated on `GROUNDTRUTH_TEST_DSN`
//! (e.g. `postgres://postgres:postgres@localhost:5432/groundtruth_test`) and skips cleanly when
//! unset, so the suite stays green without a DB. Each DB-backed test owns a
//! uniquely-named table it creates and drops, to avoid collisions on the shared
//! test database.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use groundtruth::app::{notify_failures, run_checks, run_once};
use groundtruth::runner::Status;
use groundtruth::state::StateStore;

/// A uniquely-named test table, namespaced per `tag` plus a nanosecond stamp.
fn unique_table(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("gt_app_test_{tag}_{nanos}")
}

/// HCL connection preamble pointing connectorx at the Postgres test DSN.
fn conn_block(name: &str, dsn: &str) -> String {
    format!(
        r#"connection "postgres" "{name}" {{
  dsn = "{dsn}"
}}
"#
    )
}

/// Fresh Postgres table `orders (id serial primary key, amount double precision)`.
async fn setup_orders_table(dsn: &str, table: &str) {
    use sqlx::Executor;
    let pool = sqlx::postgres::PgPool::connect(dsn).await.unwrap();
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(
        format!("CREATE TABLE {table} (id serial primary key, amount double precision)").as_str(),
    )
    .await
    .unwrap();
    pool.close().await;
}

/// Same, seeded with `n` rows.
async fn setup_orders_table_n(dsn: &str, table: &str, n: usize) {
    use sqlx::Executor;
    setup_orders_table(dsn, table).await;
    let pool = sqlx::postgres::PgPool::connect(dsn).await.unwrap();
    for i in 0..n {
        pool.execute(
            sqlx::query(&format!("INSERT INTO {table} (amount) VALUES ($1)"))
                .bind((i as f64) + 1.0),
        )
        .await
        .unwrap();
    }
    pool.close().await;
}

/// Drop a test table; best-effort teardown.
async fn drop_table(dsn: &str, table: &str) {
    use sqlx::Executor;
    if let Ok(pool) = sqlx::postgres::PgPool::connect(dsn).await {
        let _ = pool
            .execute(format!("DROP TABLE IF EXISTS {table}").as_str())
            .await;
        pool.close().await;
    }
}

/// Accepts `count` connections, returns each raw request after replying 200 OK.
async fn capture_server(count: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let handle = tokio::spawn(async move {
        let mut captured = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
            stream.write_all(resp.as_bytes()).await.unwrap();
            captured.push(req);
        }
        captured
    });

    (url, handle)
}

// Regression: one down connection must not abort the whole run.
#[tokio::test]
async fn down_connection_does_not_abort_run() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping down_connection_does_not_abort_run");
        return;
    };

    // The "ok" connection is the real test DB; "bad" points connectorx at an
    // unreachable host. connectorx detects the down host at the check timeout
    // (not instantly), so the bad check gets a short `timeout`.
    let config = format!(
        r#"
{ok}connection "postgres" "bad" {{
  dsn = "postgres://x@127.0.0.1:1/none"
}}

check "good_check" {{
  on    = "ok"
  query = "SELECT 1 as n"
  fail  = row.n != 1
}}

check "bad_check" {{
  on      = "bad"
  query   = "SELECT 1 as n"
  timeout = "2s"
}}
"#,
        ok = conn_block("ok", &dsn),
    );

    // A dead DB is not a fatal error.
    let results = run_once(&config)
        .await
        .expect("run_once must succeed even when one connection is down");

    assert_eq!(results.len(), 2, "both checks must produce a result");

    let good = results.iter().find(|r| r.name == "good_check").unwrap();
    assert_eq!(
        good.status,
        Status::Pass,
        "working connection check must Pass; detail: {}",
        good.detail
    );

    let bad = results.iter().find(|r| r.name == "bad_check").unwrap();
    assert_eq!(
        bad.status,
        Status::Error,
        "dead connection check must be Error; detail: {}",
        bad.detail
    );
}

#[tokio::test]
async fn run_once_pass_and_fail() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping run_once_pass_and_fail");
        return;
    };
    let table = unique_table("run_once");
    setup_orders_table_n(&dsn, &table, 2).await;

    let config = format!(
        r#"
        {conn}
        check "orders_present" {{
          query = "select count(*) as cnt from {table}"
          fail  = row.cnt == 0
        }}

        check "orders_none_expected" {{
          query = "select count(*) as cnt from {table}"
          fail  = row.cnt > 0
        }}
        "#,
        conn = conn_block("main", &dsn),
    );

    let results = run_once(&config).await.unwrap();

    assert_eq!(results.len(), 2);
    let present = results.iter().find(|r| r.name == "orders_present").unwrap();
    assert_eq!(present.status, Status::Pass, "detail: {}", present.detail);

    let none = results
        .iter()
        .find(|r| r.name == "orders_none_expected")
        .unwrap();
    assert_eq!(none.status, Status::Fail, "detail: {}", none.detail);

    drop_table(&dsn, &table).await;
}

// No sustained window → notify fires on the first failure.
#[tokio::test]
async fn notify_fires_immediately_when_no_sustained() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!(
            "GROUNDTRUTH_TEST_DSN unset — skipping notify_fires_immediately_when_no_sustained"
        );
        return;
    };
    let table = unique_table("no_sustained");
    setup_orders_table_n(&dsn, &table, 1).await;

    let config = format!(
        r#"
        {conn}
        check "always_fail" {{
          query   = "select count(*) as cnt from {table}"
          fail    = row.cnt > 0
          on_fail = "hook"
        }}
        "#,
        conn = conn_block("main", &dsn),
    );

    let (url, server) = capture_server(1).await;

    let notifiers = vec![(
        "hook".to_string(),
        groundtruth::notify::Notifier::Webhook { url },
    )];

    let state = StateStore::memory();
    let now = 1_000_000_i64;

    let results = run_once(&config).await.unwrap();
    let checks = groundtruth::config::parse_checks(&config).unwrap();

    notify_failures(&results, &checks, &notifiers, &state, now)
        .await
        .unwrap();

    let captured = server.await.unwrap();
    assert_eq!(captured.len(), 1, "expected exactly 1 webhook request");
    assert!(
        captured[0].contains("always_fail"),
        "check name missing from payload: {}",
        captured[0]
    );
    assert!(
        captured[0].contains("FAIL"),
        "status missing from payload: {}",
        captured[0]
    );

    drop_table(&dsn, &table).await;
}

// Sustained window gates notification until it elapses.
#[tokio::test]
async fn notify_sustained_gate_blocks_then_fires() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping notify_sustained_gate_blocks_then_fires");
        return;
    };
    let table = unique_table("sustained");
    setup_orders_table_n(&dsn, &table, 1).await;

    let config = format!(
        r#"
        {conn}
        check "gated_fail" {{
          query   = "select count(*) as cnt from {table}"
          fail {{
            when      = row.cnt > 0
            sustained = "300s"
          }}
          on_fail = "hook"
        }}
        "#,
        conn = conn_block("main", &dsn),
    );

    let state = StateStore::memory();
    let checks = groundtruth::config::parse_checks(&config).unwrap();

    // Onset: within window, no notify.
    let t0 = 1_000_000_i64;

    let results = run_once(&config).await.unwrap();

    let r = results.iter().find(|r| r.name == "gated_fail").unwrap();
    assert_eq!(r.status, Status::Fail, "should fail");

    // No notifiers wired — proves the gate, not delivery.
    let no_notifiers: Vec<(String, groundtruth::notify::Notifier)> = Vec::new();
    notify_failures(&results, &checks, &no_notifiers, &state, t0)
        .await
        .unwrap();

    // Still inside the 300s window.
    let t1 = t0 + 100;
    let results2 = run_once(&config).await.unwrap();
    notify_failures(&results2, &checks, &no_notifiers, &state, t1)
        .await
        .unwrap();

    // Past the 300s window → fires.
    let t2 = t0 + 301;
    let (url, server) = capture_server(1).await;
    let notifiers = vec![(
        "hook".to_string(),
        groundtruth::notify::Notifier::Webhook { url },
    )];

    let results3 = run_once(&config).await.unwrap();
    notify_failures(&results3, &checks, &notifiers, &state, t2)
        .await
        .unwrap();

    let captured = server.await.unwrap();
    assert_eq!(
        captured.len(),
        1,
        "expected exactly 1 webhook after sustained window"
    );
    assert!(captured[0].contains("FAIL"), "status missing");
    assert!(captured[0].contains("gated_fail"), "check name missing");

    drop_table(&dsn, &table).await;
}

// Recovery after a fired failure sends a RECOVERED notification.
#[tokio::test]
async fn notify_recovery_sends_recovered() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping notify_recovery_sends_recovered");
        return;
    };
    let table = unique_table("recovery");
    {
        use sqlx::Executor;
        let pool = sqlx::postgres::PgPool::connect(&dsn).await.unwrap();
        pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
            .await
            .unwrap();
        pool.execute(format!("CREATE TABLE {table} (v int)").as_str())
            .await
            .unwrap();
        pool.execute(format!("INSERT INTO {table} VALUES (1)").as_str())
            .await
            .unwrap();
        pool.close().await;
    }

    let config = format!(
        r#"
        {conn}
        check "recoverable" {{
          query   = "select sum(v) as s from {table}"
          fail    = row.s > 0
          on_fail = "hook"
        }}
        "#,
        conn = conn_block("main", &dsn),
    );

    let state = StateStore::memory();
    let checks = groundtruth::config::parse_checks(&config).unwrap();

    // Fail: v=1 → sum=1 > 0.
    let t0 = 2_000_000_i64;
    let results1 = run_once(&config).await.unwrap();
    let r1 = results1.iter().find(|r| r.name == "recoverable").unwrap();
    assert_eq!(r1.status, Status::Fail);

    let (url1, server1) = capture_server(1).await;
    let notifiers = Arc::new(Mutex::new(vec![(
        "hook".to_string(),
        groundtruth::notify::Notifier::Webhook { url: url1 },
    )]));
    {
        let n = notifiers.lock().await;
        notify_failures(&results1, &checks, &n, &state, t0)
            .await
            .unwrap();
    }
    let c1 = server1.await.unwrap();
    assert_eq!(c1.len(), 1);
    assert!(c1[0].contains("FAIL"));

    // Heal: v=0 → sum=0 → pass.
    {
        use sqlx::Executor;
        let pool = sqlx::postgres::PgPool::connect(&dsn).await.unwrap();
        pool.execute(format!("UPDATE {table} SET v = 0").as_str())
            .await
            .unwrap();
        pool.close().await;
    }

    // Now passing → expect RECOVERED.
    let t1 = t0 + 60;
    let results2 = run_once(&config).await.unwrap();
    let r2 = results2.iter().find(|r| r.name == "recoverable").unwrap();
    assert_eq!(r2.status, Status::Pass, "after heal should pass");

    let (url2, server2) = capture_server(1).await;
    let notifiers2 = vec![(
        "hook".to_string(),
        groundtruth::notify::Notifier::Webhook { url: url2 },
    )];
    notify_failures(&results2, &checks, &notifiers2, &state, t1)
        .await
        .unwrap();

    let c2 = server2.await.unwrap();
    assert_eq!(c2.len(), 1, "expected 1 RECOVERED notification");
    assert!(
        c2[0].contains("RECOVERED"),
        "expected RECOVERED in payload: {}",
        c2[0]
    );

    drop_table(&dsn, &table).await;
}

// Sustained gating specifically over the Memory StateStore backend.
#[tokio::test]
async fn sustained_gates_notification_with_memory_backend() {
    let Ok(dsn) = std::env::var("GROUNDTRUTH_TEST_DSN") else {
        eprintln!(
            "GROUNDTRUTH_TEST_DSN unset — skipping sustained_gates_notification_with_memory_backend"
        );
        return;
    };
    let table = unique_table("memory_backend_sustained");
    setup_orders_table_n(&dsn, &table, 1).await;

    let config = format!(
        r#"
        {conn}
        check "sustained_check" {{
          query   = "select count(*) as cnt from {table}"
          fail {{
            when      = row.cnt > 0
            sustained = "600s"
          }}
          on_fail = "hook"
        }}
        "#,
        conn = conn_block("main", &dsn),
    );

    let state = StateStore::memory();
    let checks = groundtruth::config::parse_checks(&config).unwrap();
    let no_notifiers: Vec<(String, groundtruth::notify::Notifier)> = Vec::new();

    let t0 = 3_000_000_i64;

    // Onset: within the 600s window.
    let results = run_once(&config).await.unwrap();
    let r = results
        .iter()
        .find(|r| r.name == "sustained_check")
        .unwrap();
    assert_eq!(r.status, Status::Fail);

    notify_failures(&results, &checks, &no_notifiers, &state, t0)
        .await
        .unwrap();

    // Still inside the window.
    let t1 = t0 + 300;
    let results2 = run_once(&config).await.unwrap();
    notify_failures(&results2, &checks, &no_notifiers, &state, t1)
        .await
        .unwrap();

    // Past the window → fires.
    let t2 = t0 + 601;
    let (url, server) = capture_server(1).await;
    let notifiers = vec![(
        "hook".to_string(),
        groundtruth::notify::Notifier::Webhook { url },
    )];

    let results3 = run_once(&config).await.unwrap();
    notify_failures(&results3, &checks, &notifiers, &state, t2)
        .await
        .unwrap();

    let captured = server.await.unwrap();
    assert_eq!(
        captured.len(),
        1,
        "Memory backend should fire exactly 1 notification after sustained window"
    );
    assert!(captured[0].contains("FAIL"));
    assert!(captured[0].contains("sustained_check"));

    drop_table(&dsn, &table).await;
}

// 20 checks run concurrently: all Pass, wall-clock proves real concurrency.
// DB-free: uses a `Static` source via `run_checks` directly, so no DSN gate.
#[tokio::test]
async fn concurrency_proof_20_checks_no_errors() {
    // A DB-free `Static` source: this test proves run_checks overlaps work, so it
    // only needs a source that returns promptly. The checks use `fail = false`,
    // so they PASS regardless of rows.
    let mut live: std::collections::HashMap<String, groundtruth::source::Source> =
        std::collections::HashMap::new();
    live.insert(
        "db".to_string(),
        groundtruth::source::Source::Static(vec![]),
    );
    let dead: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let first = "db".to_string();

    let mut config = String::new();
    for i in 0..20_usize {
        config.push_str(&format!(
            "check \"proof{i}\" {{\n  on    = \"db\"\n  query = \"SELECT {i} as v\"\n  fail  = false\n}}\n"
        ));
    }

    let checks = groundtruth::config::parse_checks(&config).unwrap();
    assert_eq!(checks.len(), 20, "must have 20 checks");

    let start = std::time::Instant::now();
    let results = run_checks(&live, &dead, &checks, &first).await;
    let wall_ms = start.elapsed().as_millis();

    assert_eq!(results.len(), 20, "all 20 checks must produce a result");
    for r in &results {
        assert_eq!(
            r.status,
            Status::Pass,
            "check {} must Pass, got: {}",
            r.name,
            r.detail
        );
    }

    // Serial execution would be far slower; well under 5s proves overlap.
    println!("concurrency_proof: 20 checks completed in {wall_ms}ms");
    assert!(
        wall_ms < 5000,
        "20 concurrent checks must finish in <5s, took {wall_ms}ms"
    );
}
