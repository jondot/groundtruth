//! TFDV-style declarative `validate` rules over fresh per-test Postgres tables
//! (checks run through connectorx). Same harness pattern as tests/matrix_postgres.rs.
//!
//! Every DB-backed test is gated on `GROUNDTRUTH_TEST_DSN`
//! (e.g. `postgres://postgres:postgres@localhost:5432/groundtruth_test`) and skips cleanly when
//! unset, so the suite stays green without a DB. Each test owns a uniquely-named
//! table (nanosecond stamp) it creates and drops, to avoid collisions on the
//! shared test database. Parse-error tests need no DB and run unconditionally.

use groundtruth::app::run;
use groundtruth::config::parse_checks;
use groundtruth::runner::Status;
use sqlx::Executor;
use sqlx::postgres::PgPool;

/// A uniquely-named test table, namespaced per `tag` plus a nanosecond stamp.
fn unique_table(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("gt_validate_{tag}_{nanos}")
}

/// HCL connection preamble pointing connectorx at the Postgres test DSN.
fn conn(dsn: &str) -> String {
    format!(
        r#"connection "postgres" "main" {{
  dsn = "{dsn}"
}}
"#
    )
}

/// Connect a sqlx pool for fixture setup/teardown.
async fn pg_pool(dsn: &str) -> PgPool {
    PgPool::connect(dsn)
        .await
        .expect("connect to Postgres for fixture setup")
}

/// Drop a test table; best-effort teardown.
async fn drop_table(dsn: &str, table: &str) {
    if let Ok(pool) = PgPool::connect(dsn).await {
        let _ = pool
            .execute(format!("DROP TABLE IF EXISTS {table}").as_str())
            .await;
        pool.close().await;
    }
}

/// Run a single-check config and return its one result.
async fn run_validate(dsn: &str, check_hcl: &str) -> groundtruth::runner::CheckResult {
    let config = format!("{}\n{}", conn(dsn), check_hcl);
    let mut results = run(&config).await.expect("run");
    assert_eq!(results.len(), 1, "expected exactly one result");
    results.remove(0)
}

/// Standard env-gate; returns the DSN or skips the test.
macro_rules! dsn_or_skip {
    ($name:literal) => {{
        match std::env::var("GROUNDTRUTH_TEST_DSN") {
            Ok(d) => d,
            Err(_) => {
                eprintln!(concat!("GROUNDTRUTH_TEST_DSN unset — skipping ", $name));
                return;
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// 1. type — pass: all integers; fail: non-integer text values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn type_int_pass() {
    let dsn = dsn_or_skip!("type_int_pass");
    let table = unique_table("type_int_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    // An integer column: every value is a genuine int.
    pool.execute(format!("CREATE TABLE {table} (age integer)").as_str())
        .await
        .unwrap();
    for v in &[25i64, 30, 45] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "type_int" {{
  query = "select age from {table}"
  validate {{
    column "age" {{ type = "int" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "all ints should pass type=int; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn type_int_fail() {
    let dsn = dsn_or_skip!("type_int_fail");
    let table = unique_table("type_int_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    // A text column holding non-integer strings, so type=int sees bad values.
    pool.execute(format!("CREATE TABLE {table} (age text)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('not_a_number')").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('1.5')").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "type_int" {{
  query = "select age from {table}"
  validate {{
    column "age" {{ type = "int" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "non-integer text in int column should fail; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 2. not_null — pass: no nulls; fail: has null
// ---------------------------------------------------------------------------

#[tokio::test]
async fn not_null_pass() {
    let dsn = dsn_or_skip!("not_null_pass");
    let table = unique_table("not_null_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    for e in &["a@b.com", "c@d.com"] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(e))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "not_null" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ not_null = true }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "no nulls should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn not_null_fail() {
    let dsn = dsn_or_skip!("not_null_fail");
    let table = unique_table("not_null_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('a@b.com')").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES (NULL)").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "not_null" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ not_null = true }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "null present should fail; detail: {}",
        r.detail
    );
    assert!(!r.detail.is_empty());
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 3. null_rate — pass: under threshold; fail: over threshold
// ---------------------------------------------------------------------------

#[tokio::test]
async fn null_rate_pass() {
    let dsn = dsn_or_skip!("null_rate_pass");
    let table = unique_table("null_rate_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (v text)").as_str())
        .await
        .unwrap();
    // 0% nulls, under the 10% threshold.
    for i in 0..10i32 {
        pool.execute(
            sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(format!("v{i}")),
        )
        .await
        .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "null_rate" {{
  query = "select v from {table}"
  validate {{
    column "v" {{ null_rate = 0.1 }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "0% nulls under 10% threshold should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn null_rate_fail() {
    let dsn = dsn_or_skip!("null_rate_fail");
    let table = unique_table("null_rate_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (v text)").as_str())
        .await
        .unwrap();
    // 50% nulls, over the 10% threshold.
    for _ in 0..5i32 {
        pool.execute(format!("INSERT INTO {table} VALUES (NULL)").as_str())
            .await
            .unwrap();
    }
    for i in 0..5i32 {
        pool.execute(
            sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(format!("v{i}")),
        )
        .await
        .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "null_rate" {{
  query = "select v from {table}"
  validate {{
    column "v" {{ null_rate = 0.1 }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "50% nulls over 10% threshold should fail; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 4. allowed — pass: all values in set; fail: value outside set
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allowed_pass() {
    let dsn = dsn_or_skip!("allowed_pass");
    let table = unique_table("allowed_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (country text)").as_str())
        .await
        .unwrap();
    for c in &["US", "CA", "GB"] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(c))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "allowed" {{
  query = "select country from {table}"
  validate {{
    column "country" {{ allowed = ["US", "CA", "GB"] }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "all allowed values should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn allowed_fail() {
    let dsn = dsn_or_skip!("allowed_fail");
    let table = unique_table("allowed_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (country text)").as_str())
        .await
        .unwrap();
    // ZZ is outside the allowed set.
    for c in &["US", "CA", "ZZ"] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(c))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "allowed" {{
  query = "select country from {table}"
  validate {{
    column "country" {{ allowed = ["US", "CA", "GB"] }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "ZZ not in set should fail; detail: {}",
        r.detail
    );
    assert!(!r.sample.is_empty(), "sample should include offending row");
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 5. matches — pass: all match regex; fail: some don't match
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matches_pass() {
    let dsn = dsn_or_skip!("matches_pass");
    let table = unique_table("matches_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    for e in &["a@b.com", "x@y.org", "foo@bar.net"] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(e))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "matches" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ matches = "^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "all valid emails should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn matches_fail() {
    let dsn = dsn_or_skip!("matches_fail");
    let table = unique_table("matches_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('valid@example.com')").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('not-an-email')").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "matches" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ matches = "^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "invalid email should fail matches; detail: {}",
        r.detail
    );
    assert!(!r.sample.is_empty(), "sample should show bad values");
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 6. range — pass: all in [min, max]; fail: value out of range
// ---------------------------------------------------------------------------

#[tokio::test]
async fn range_pass() {
    let dsn = dsn_or_skip!("range_pass");
    let table = unique_table("range_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (age integer)").as_str())
        .await
        .unwrap();
    for v in &[0i64, 25, 120] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "range" {{
  query = "select age from {table}"
  validate {{
    column "age" {{ range = {{ min = 0, max = 120 }} }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "ages 0-120 should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn range_fail() {
    let dsn = dsn_or_skip!("range_fail");
    let table = unique_table("range_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (age integer)").as_str())
        .await
        .unwrap();
    // 200 is outside [0, 120].
    for v in &[25i64, 200] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "range" {{
  query = "select age from {table}"
  validate {{
    column "age" {{ range = {{ min = 0, max = 120 }} }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "age 200 outside [0,120] should fail; detail: {}",
        r.detail
    );
    assert!(
        !r.sample.is_empty(),
        "sample should include out-of-range value"
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 7. unique — pass: all unique; fail: duplicates present
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unique_pass() {
    let dsn = dsn_or_skip!("unique_pass");
    let table = unique_table("unique_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    for e in &["a@b.com", "c@d.com", "e@f.com"] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(e))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "unique" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ unique = true }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "all unique should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn unique_fail() {
    let dsn = dsn_or_skip!("unique_fail");
    let table = unique_table("unique_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('a@b.com')").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('a@b.com')").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "unique" {{
  query = "select email from {table}"
  validate {{
    column "email" {{ unique = true }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "duplicate should fail; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 8. outliers (iqr) — pass: no outliers; fail: clear outlier present
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outliers_iqr_pass() {
    let dsn = dsn_or_skip!("outliers_iqr_pass");
    let table = unique_table("iqr_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (score integer)").as_str())
        .await
        .unwrap();
    // Tight cluster, no outliers.
    for v in &[10i64, 11, 12, 11, 10, 13, 12, 11] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "outliers_iqr" {{
  query = "select score from {table}"
  validate {{
    column "score" {{ outliers = "iqr" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "no IQR outliers should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn outliers_iqr_fail() {
    let dsn = dsn_or_skip!("outliers_iqr_fail");
    let table = unique_table("iqr_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (score integer)").as_str())
        .await
        .unwrap();
    // Tight cluster plus one extreme outlier (1000).
    for v in &[10i64, 11, 12, 11, 10, 13, 12, 11, 1000] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "outliers_iqr" {{
  query = "select score from {table}"
  validate {{
    column "score" {{ outliers = "iqr" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "1000 is a clear IQR outlier; detail: {}",
        r.detail
    );
    assert!(!r.sample.is_empty(), "sample should include outlier values");
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 9. distribution=normal — pass: ~normal data; fail: clearly non-normal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distribution_normal_pass() {
    let dsn = dsn_or_skip!("distribution_normal_pass");
    let table = unique_table("normal_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (v double precision)").as_str())
        .await
        .unwrap();
    // Roughly normal, centered near 50.
    let values: &[f64] = &[
        48.0, 50.0, 52.0, 49.0, 51.0, 50.0, 48.5, 51.5, 49.5, 50.5, 47.0, 53.0, 50.0, 49.0, 51.0,
        50.0, 48.0, 52.0, 50.0, 49.5, 51.5, 50.0, 48.5, 51.5, 49.0, 51.0, 50.0, 50.0, 49.5, 50.5,
        48.0, 52.0, 50.0, 50.0, 49.0, 51.0, 50.5, 49.5, 51.0, 49.0, 50.0, 50.0, 51.0, 49.0, 50.0,
        50.5, 49.5, 50.0, 51.0, 49.0,
    ];
    for v in values {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "normal_dist" {{
  query = "select v from {table}"
  validate {{
    column "v" {{ distribution = "normal" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "normal data should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

#[tokio::test]
async fn distribution_normal_fail() {
    let dsn = dsn_or_skip!("distribution_normal_fail");
    let table = unique_table("normal_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (v double precision)").as_str())
        .await
        .unwrap();
    // Heavily right-skewed: many small values, a few huge ones.
    let values: &[f64] = &[
        1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0,
        2.0, 2.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 3.0, 100.0, 200.0, 300.0, 400.0,
        500.0, 600.0, 700.0, 800.0, 900.0, 1000.0, 2000.0, 3000.0, 5000.0, 8000.0, 10000.0,
        15000.0, 20000.0, 50000.0, 100000.0, 200000.0,
    ];
    for v in values {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "normal_dist" {{
  query = "select v from {table}"
  validate {{
    column "v" {{ distribution = "normal" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "skewed data should fail normality test; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 10. Error: column named in rule absent from query result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_column_absent_from_query() {
    let dsn = dsn_or_skip!("error_column_absent_from_query");
    let table = unique_table("absent_col");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (x integer)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES (1)").as_str())
        .await
        .unwrap();
    pool.close().await;

    // Query selects 'x' but the rule references 'email'.
    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "absent_col" {{
  query = "select x from {table}"
  validate {{
    column "email" {{ not_null = true }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Error,
        "absent column should be Error; detail: {}",
        r.detail
    );
    assert!(
        r.detail.contains("email"),
        "detail should name the missing column: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 11. Error: normality test with < 8 values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_normality_too_few_values() {
    let dsn = dsn_or_skip!("error_normality_too_few_values");
    let table = unique_table("normal_few");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (v double precision)").as_str())
        .await
        .unwrap();
    // 5 values, below the normality test's minimum of 8.
    for v in &[1.0f64, 2.0, 3.0, 4.0, 5.0] {
        pool.execute(sqlx::query(&format!("INSERT INTO {table} VALUES ($1)")).bind(v))
            .await
            .unwrap();
    }
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "normal_few" {{
  query = "select v from {table}"
  validate {{
    column "v" {{ distribution = "normal" }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Error,
        "fewer than 8 values for normality should be Error; detail: {}",
        r.detail
    );
    assert!(
        r.detail.contains("8") || r.detail.contains("normality") || r.detail.contains("few"),
        "detail should explain the issue: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 12. Parse error: validate + fail together → hard error (no DB needed)
// ---------------------------------------------------------------------------

#[test]
fn parse_error_validate_and_fail_together() {
    let src = r#"
check "bad" {
  query = "select 1 as x"
  fail = row.x == 0
  validate {
    column "x" { not_null = true }
  }
}
"#;
    let err = parse_checks(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("validate") || msg.contains("fail") || msg.contains("mutually exclusive"),
        "error should mention validate/fail conflict: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 13. Parse error: unknown column attribute → hard error (no DB needed)
// ---------------------------------------------------------------------------

#[test]
fn parse_error_unknown_column_attr() {
    let src = r#"
check "bad" {
  query = "select 1 as x"
  validate {
    column "x" { bogus_attr = true }
  }
}
"#;
    let err = parse_checks(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("bogus_attr"),
        "error must name the unknown attr: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 14. Parse error: bad regex in matches → hard error (no DB needed)
// ---------------------------------------------------------------------------

#[test]
fn parse_error_bad_regex() {
    let src = r#"
check "bad" {
  query = "select 1 as x"
  validate {
    column "x" { matches = "[invalid regex" }
  }
}
"#;
    let err = parse_checks(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("regex") || msg.contains("invalid") || msg.contains("matches"),
        "error should mention regex problem: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 15. Multiple columns — all pass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_column_all_pass() {
    let dsn = dsn_or_skip!("multi_column_all_pass");
    let table = unique_table("multi_pass");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text, age integer, country text)").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('a@b.com', 25, 'US')").as_str())
        .await
        .unwrap();
    pool.execute(format!("INSERT INTO {table} VALUES ('c@d.com', 30, 'CA')").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "multi" {{
  query = "select email, age, country from {table}"
  validate {{
    column "email" {{
      not_null = true
      matches = "^[^@\\s]+@[^@\\s]+\\.[^@\\s]+$"
    }}
    column "age" {{
      range = {{ min = 0, max = 120 }}
    }}
    column "country" {{
      allowed = ["US", "CA", "GB"]
    }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Pass,
        "all columns valid should pass; detail: {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 16. Multiple columns — one fails, detail mentions the failing column
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_column_one_fails() {
    let dsn = dsn_or_skip!("multi_column_one_fails");
    let table = unique_table("multi_fail");
    let pool = pg_pool(&dsn).await;
    pool.execute(format!("DROP TABLE IF EXISTS {table}").as_str())
        .await
        .unwrap();
    pool.execute(format!("CREATE TABLE {table} (email text, age integer, country text)").as_str())
        .await
        .unwrap();
    // age 200 is out of range.
    pool.execute(format!("INSERT INTO {table} VALUES ('a@b.com', 200, 'US')").as_str())
        .await
        .unwrap();
    pool.close().await;

    let r = run_validate(
        &dsn,
        &format!(
            r#"
check "multi" {{
  query = "select email, age, country from {table}"
  validate {{
    column "email"   {{ not_null = true }}
    column "age"     {{ range = {{ min = 0, max = 120 }} }}
    column "country" {{ allowed = ["US", "CA", "GB"] }}
  }}
}}
"#
        ),
    )
    .await;
    assert_eq!(
        r.status,
        Status::Fail,
        "age 200 out of range should fail; detail: {}",
        r.detail
    );
    assert!(
        r.detail.contains("age"),
        "detail must mention failing column 'age': {}",
        r.detail
    );
    drop_table(&dsn, &table).await;
}

// ---------------------------------------------------------------------------
// 17. validate + warn combination → parse error (no DB needed)
// ---------------------------------------------------------------------------

#[test]
fn parse_error_validate_and_warn_together() {
    let src = r#"
check "bad" {
  query = "select 1 as x"
  warn = row.x == 0
  validate {
    column "x" { not_null = true }
  }
}
"#;
    let err = parse_checks(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("validate") || msg.contains("warn") || msg.contains("mutually exclusive"),
        "error should mention validate/warn conflict: {msg}"
    );
}
