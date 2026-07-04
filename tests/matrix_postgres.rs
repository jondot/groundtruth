//! Check matrix against the Postgres provider; each test owns a
//! `mtx_<intent>_<state>` schema it drops-and-recreates for clean reruns.
//! Every test is gated on `GROUNDTRUTH_TEST_DSN` and skips cleanly when unset,
//! so the suite stays green without a database.

use groundtruth::app::run;
use groundtruth::runner::Status;
use sqlx::PgPool;

/// Skip the calling test when no test database is configured.
macro_rules! require_db {
    () => {
        if std::env::var("GROUNDTRUTH_TEST_DSN").is_err() {
            eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping DB-backed test");
            return;
        }
    };
}

fn dsn() -> String {
    std::env::var("GROUNDTRUTH_TEST_DSN")
        .expect("GROUNDTRUTH_TEST_DSN must be set (guarded by require_db!)")
}

/// Direct sqlx pool for fixture setup/teardown.
async fn pg_pool() -> PgPool {
    PgPool::connect(&dsn())
        .await
        .expect("connect to Postgres for fixture setup")
}

/// Drop-then-create a schema so reruns start clean.
async fn reset_schema(pool: &PgPool, schema: &str) {
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(pool)
        .await
        .unwrap();
}

/// HCL connection preamble.
fn conn_block() -> String {
    let d = dsn();
    format!(
        r#"connection "postgres" "main" {{
  dsn = "{d}"
}}
"#
    )
}

// ---------------------------------------------------------------------------
// 1. table_not_empty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn table_not_empty_pass() {
    require_db!();
    let schema = "mtx_not_empty_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (1)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "not_empty" {{
  query = "select count(*) as n from {schema}.t"
  fail  = row.n == 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "not_empty").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "non-empty table should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn table_not_empty_fail() {
    require_db!();
    let schema = "mtx_not_empty_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    // No rows inserted.

    let config = format!(
        r#"{conn}
check "not_empty" {{
  query = "select count(*) as n from {schema}.t"
  fail  = row.n == 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "not_empty").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "empty table should fail; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 2. freshness / recency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn freshness_pass() {
    require_db!();
    let schema = "mtx_freshness_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.events (id serial, ts timestamptz)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.events (ts) VALUES (now() - interval '1 minute')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "freshness" {{
  query = "select count(*) as n from {schema}.events where ts > now() - interval '10 minutes'"
  fail  = row.n == 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "freshness").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "recent row should pass freshness; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn freshness_fail() {
    require_db!();
    let schema = "mtx_freshness_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.events (id serial, ts timestamptz)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    // Only stale rows (1 hour old).
    sqlx::query(&format!(
        "INSERT INTO {schema}.events (ts) VALUES (now() - interval '1 hour')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "freshness" {{
  query = "select count(*) as n from {schema}.events where ts > now() - interval '10 minutes'"
  fail  = row.n == 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "freshness").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "stale rows only should fail freshness; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 3. row_count_band
// ---------------------------------------------------------------------------

#[tokio::test]
async fn row_count_band_pass() {
    require_db!();
    let schema = "mtx_rowband_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    for i in 1..=5i32 {
        sqlx::query(&format!("INSERT INTO {schema}.t VALUES ({i})"))
            .execute(&pool)
            .await
            .unwrap();
    }

    let config = format!(
        r#"{conn}
check "row_band" {{
  query = "select count(*) as n from {schema}.t"
  fail  = row.n < 3 || row.n > 10
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "row_band").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "5 rows within [3,10] should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn row_count_band_fail() {
    require_db!();
    let schema = "mtx_rowband_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    // 1 row, below the lower bound of 3.
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (1)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "row_band" {{
  query = "select count(*) as n from {schema}.t"
  fail  = row.n < 3 || row.n > 10
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "row_band").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "1 row below lower bound should fail; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 4. null_rate / completeness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn null_rate_pass() {
    require_db!();
    let schema = "mtx_nullrate_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int, email text)"))
        .execute(&pool)
        .await
        .unwrap();
    for i in 1..=10i32 {
        sqlx::query(&format!(
            "INSERT INTO {schema}.t VALUES ({i}, 'user{i}@example.com')"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }

    let config = format!(
        r#"{conn}
check "null_rate" {{
  query = "select avg(case when email is null then 1.0 else 0 end) as null_rate from {schema}.t"
  fail  = row.null_rate > 0.05
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "null_rate").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "no nulls should pass null_rate check; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn null_rate_fail() {
    require_db!();
    let schema = "mtx_nullrate_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (id int, email text)"))
        .execute(&pool)
        .await
        .unwrap();
    // 50% nulls, well over the 5% threshold.
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (1, NULL)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES (2, 'ok@example.com')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "null_rate" {{
  query = "select avg(case when email is null then 1.0 else 0 end) as null_rate from {schema}.t"
  fail  = row.null_rate > 0.05
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "null_rate").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "50% nulls should fail null_rate check; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 5. uniqueness / no-duplicates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uniqueness_pass() {
    require_db!();
    let schema = "mtx_unique_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (k text)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('a'), ('b'), ('c')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "uniqueness" {{
  query = "select count(*) - count(distinct k) as dups from {schema}.t"
  fail  = row.dups > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "uniqueness").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "all unique values should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn uniqueness_fail() {
    require_db!();
    let schema = "mtx_unique_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (k text)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('a'), ('a'), ('b')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "uniqueness" {{
  query = "select count(*) - count(distinct k) as dups from {schema}.t"
  fail  = row.dups > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "uniqueness").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "duplicate 'a' should fail uniqueness; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 6. referential_integrity / orphans
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphans_pass() {
    require_db!();
    let schema = "mtx_orphans_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.parent (id int primary key)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.child (id int, parent_id int)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.parent VALUES (1), (2)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.child VALUES (10, 1), (11, 2)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "orphans" {{
  query = "select c.id from {schema}.child c left join {schema}.parent p on p.id = c.parent_id where p.id is null"
  fail {{
    when   = rows.count > 0
    sample = 5
  }}
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "orphans").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "no orphans should pass; detail: {}",
        r.detail
    );
    assert!(r.sample.is_empty(), "passing check must have empty sample");
}

#[tokio::test]
async fn orphans_fail_attaches_sample() {
    require_db!();
    let schema = "mtx_orphans_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.parent (id int primary key)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.child (id int, parent_id int)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.parent VALUES (1)"))
        .execute(&pool)
        .await
        .unwrap();
    // Two children reference missing parent 999.
    sqlx::query(&format!(
        "INSERT INTO {schema}.child VALUES (10, 999), (11, 999), (12, 1)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "orphans" {{
  query = "select c.id from {schema}.child c left join {schema}.parent p on p.id = c.parent_id where p.id is null"
  fail {{
    when   = rows.count > 0
    sample = 5
  }}
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "orphans").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "orphan rows should fail; detail: {}",
        r.detail
    );
    assert!(
        !r.sample.is_empty(),
        "failing orphan check must attach sample rows"
    );
    assert!(r.sample.len() <= 5, "sample must not exceed requested 5");
}

// ---------------------------------------------------------------------------
// 7. value_range / domain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn value_range_pass() {
    require_db!();
    let schema = "mtx_valrange_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (price numeric)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES (10.0), (25.5), (0.0)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "value_range" {{
  query = "select count(*) as bad from {schema}.t where price < 0"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "value_range").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "all non-negative prices should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn value_range_fail() {
    require_db!();
    let schema = "mtx_valrange_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (price numeric)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (10.0), (-5.0)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "value_range" {{
  query = "select count(*) as bad from {schema}.t where price < 0"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "value_range").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "negative price should fail value_range; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 8. accepted_values / categorical
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepted_values_pass() {
    require_db!();
    let schema = "mtx_categ_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (status text)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('active'), ('inactive'), ('pending')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "accepted_values" {{
  query = "select count(*) as bad from {schema}.t where status not in ('active', 'inactive', 'pending')"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "accepted_values")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "all accepted values should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn accepted_values_fail() {
    require_db!();
    let schema = "mtx_categ_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (status text)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('active'), ('unknown_status')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "accepted_values" {{
  query = "select count(*) as bad from {schema}.t where status not in ('active', 'inactive', 'pending')"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "accepted_values")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "unexpected status value should fail; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 9. aggregate_sanity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aggregate_sanity_pass() {
    require_db!();
    let schema = "mtx_aggr_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (amount numeric)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES (100.0), (50.0), (25.0)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "aggregate_sanity" {{
  query = "select coalesce(sum(amount), 0) as total from {schema}.t"
  fail  = row.total < 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "aggregate_sanity")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "positive sum should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn aggregate_sanity_fail() {
    require_db!();
    let schema = "mtx_aggr_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (amount numeric)"))
        .execute(&pool)
        .await
        .unwrap();
    // Negative sum stands in for refunds exceeding sales.
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (-100.0), (-50.0)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "aggregate_sanity" {{
  query = "select coalesce(sum(amount), 0) as total from {schema}.t"
  fail  = row.total < 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "aggregate_sanity")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "negative sum should fail aggregate_sanity; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 10. cardinality
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cardinality_pass() {
    require_db!();
    let schema = "mtx_card_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (category text)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('A'), ('B'), ('C'), ('D')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "cardinality" {{
  query = "select count(distinct category) as c from {schema}.t"
  fail  = row.c < 3
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "cardinality").unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "4 distinct categories >= 3 should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn cardinality_fail() {
    require_db!();
    let schema = "mtx_card_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (category text)"))
        .execute(&pool)
        .await
        .unwrap();
    // 2 distinct categories, below the minimum of 3.
    sqlx::query(&format!(
        "INSERT INTO {schema}.t VALUES ('A'), ('A'), ('B')"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "cardinality" {{
  query = "select count(distinct category) as c from {schema}.t"
  fail  = row.c < 3
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "cardinality").unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "only 2 categories should fail cardinality; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 11. cross_table_consistency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_table_consistency_pass() {
    require_db!();
    let schema = "mtx_cross_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.a (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE TABLE {schema}.b (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.a VALUES (1), (2), (3)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.b VALUES (10), (20), (30)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "cross_consistency" {{
  query = "select (select count(*) from {schema}.a) - (select count(*) from {schema}.b) as diff"
  fail  = row.diff != 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "cross_consistency")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "equal counts should pass cross_consistency; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn cross_table_consistency_fail() {
    require_db!();
    let schema = "mtx_cross_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.a (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE TABLE {schema}.b (id int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.a VALUES (1), (2), (3)"))
        .execute(&pool)
        .await
        .unwrap();
    // b has 2 rows vs a's 3.
    sqlx::query(&format!("INSERT INTO {schema}.b VALUES (10), (20)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "cross_consistency" {{
  query = "select (select count(*) from {schema}.a) - (select count(*) from {schema}.b) as diff"
  fail  = row.diff != 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "cross_consistency")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "unequal counts should fail cross_consistency; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 12. business_invariant: order total == sum of line items
// ---------------------------------------------------------------------------

#[tokio::test]
async fn business_invariant_pass() {
    require_db!();
    let schema = "mtx_bivar_pass";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.orders (id int primary key, total numeric)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.line_items (id int, order_id int, amount numeric)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.orders VALUES (1, 150.0), (2, 75.0)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.line_items VALUES (1, 1, 100.0), (2, 1, 50.0), (3, 2, 75.0)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "business_invariant" {{
  query = "select count(*) as bad from {schema}.orders o where o.total <> (select coalesce(sum(li.amount),0) from {schema}.line_items li where li.order_id = o.id)"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "business_invariant")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Pass,
        "totals match should pass; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn business_invariant_fail() {
    require_db!();
    let schema = "mtx_bivar_fail";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!(
        "CREATE TABLE {schema}.orders (id int primary key, total numeric)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.line_items (id int, order_id int, amount numeric)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    // Stated total 200 vs line items summing to 150.
    sqlx::query(&format!("INSERT INTO {schema}.orders VALUES (1, 200.0)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {schema}.line_items VALUES (1, 1, 100.0), (2, 1, 50.0)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "business_invariant" {{
  query = "select count(*) as bad from {schema}.orders o where o.total <> (select coalesce(sum(li.amount),0) from {schema}.line_items li where li.order_id = o.id)"
  fail  = row.bad > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results
        .iter()
        .find(|r| r.name == "business_invariant")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Fail,
        "mismatched total should fail business_invariant; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// 13. warn_vs_fail tiers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn warn_vs_fail_tiers_warn_only() {
    require_db!();
    let schema = "mtx_warnfail_pg";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (n int)"))
        .execute(&pool)
        .await
        .unwrap();
    // 15 rows: trips warn (>10) but not fail (>50).
    sqlx::query(&format!(
        "INSERT INTO {schema}.t SELECT generate_series(1, 15)"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let config = format!(
        r#"{conn}
check "warn_vs_fail" {{
  query = "select count(*) as n from {schema}.t"
  warn  = row.n > 10
  fail  = row.n > 50
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run");
    let r = results.iter().find(|r| r.name == "warn_vs_fail").unwrap();
    assert_eq!(
        r.status,
        Status::Warn,
        "15 rows trips warn but not fail; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// ERROR paths — E1, E2, E3
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_missing_table() {
    require_db!();
    // E1: query references a non-existent table.
    let config = format!(
        r#"{conn}
check "e1_missing_table" {{
  query = "select count(*) as n from mtx_nonexistent_9999x.t"
  fail  = row.n == 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run must succeed even on error");
    let r = results
        .iter()
        .find(|r| r.name == "e1_missing_table")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Error,
        "missing table must be Error; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn error_missing_column_in_assertion() {
    require_db!();
    // E2: assertion references a column absent from the result.
    let schema = "mtx_e2_pg";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (n int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (1)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "e2_missing_col" {{
  query = "select n from {schema}.t"
  fail  = row.nonexistent > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run must succeed even on error");
    let r = results.iter().find(|r| r.name == "e2_missing_col").unwrap();
    assert_eq!(
        r.status,
        Status::Error,
        "missing column in assertion must be Error; detail: {}",
        r.detail
    );
}

#[tokio::test]
async fn error_divide_by_zero_in_assertion() {
    require_db!();
    // E3: divide-by-zero in the assertion must be Error, not a crash.
    let schema = "mtx_e3_pg";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (n int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (0)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "e3_divide_by_zero" {{
  query = "select 0 as n from {schema}.t limit 1"
  fail  = (1 / row.n) > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config)
        .await
        .expect("run must not crash on divide-by-zero");
    let r = results
        .iter()
        .find(|r| r.name == "e3_divide_by_zero")
        .unwrap();
    assert_eq!(
        r.status,
        Status::Error,
        "divide-by-zero in assertion must be Error; detail: {}",
        r.detail
    );
}

// ---------------------------------------------------------------------------
// Prove E3 does not poison other checks in the same run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_does_not_poison_other_checks() {
    require_db!();
    // An erroring check must not affect a sibling in the same run.
    let schema = "mtx_poison_pg";
    let pool = pg_pool().await;
    reset_schema(&pool, schema).await;
    sqlx::query(&format!("CREATE TABLE {schema}.t (n int)"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("INSERT INTO {schema}.t VALUES (5)"))
        .execute(&pool)
        .await
        .unwrap();

    let config = format!(
        r#"{conn}
check "good_check" {{
  query = "select n from {schema}.t"
  fail  = row.n == 0
}}

check "bad_check" {{
  query = "select n from {schema}.t"
  fail  = row.nonexistent > 0
}}
"#,
        conn = conn_block(),
    );

    let results = run(&config).await.expect("run must succeed");
    let good = results.iter().find(|r| r.name == "good_check").unwrap();
    let bad = results.iter().find(|r| r.name == "bad_check").unwrap();

    assert_eq!(
        good.status,
        Status::Pass,
        "good check must Pass; detail: {}",
        good.detail
    );
    assert_eq!(
        bad.status,
        Status::Error,
        "bad check must be Error; detail: {}",
        bad.detail
    );
}
