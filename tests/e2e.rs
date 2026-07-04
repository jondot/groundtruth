//! End-to-end HCL run against live Postgres; self-seeds an `e2e` schema.
//! DSN from `GROUNDTRUTH_TEST_DSN`, defaulting to a local `groundtruth_test` DB.

use groundtruth::app::run;
use groundtruth::runner::Status;
use sqlx::PgPool;

fn dsn() -> String {
    std::env::var("GROUNDTRUTH_TEST_DSN")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/groundtruth_test".into())
}

/// Drop-and-recreate `e2e` with a seeded `orders` table for clean reruns.
async fn seed() {
    let pool = PgPool::connect(&dsn()).await.expect("connect for e2e seed");
    sqlx::query("DROP SCHEMA IF EXISTS e2e CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA e2e")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE e2e.orders (id serial primary key, created_at timestamptz not null default now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Five orders, all past-dated.
    sqlx::query("INSERT INTO e2e.orders (created_at) SELECT now() - interval '1 hour' FROM generate_series(1, 5)")
        .execute(&pool)
        .await
        .unwrap();
}

fn config() -> String {
    let dsn = dsn();
    format!(
        r#"
        connection "postgres" "main" {{
          dsn = "{dsn}"
        }}

        check "orders_present" {{
          query = "select count(*) as recent from e2e.orders"
          fail  = row.recent == 0
        }}

        check "no_orders_in_future" {{
          query = "select count(*) as bad from e2e.orders where created_at > now() + interval '1 day'"
          fail  = row.bad > 0
        }}
        "#
    )
}

#[tokio::test]
async fn runs_checks_end_to_end() {
    if std::env::var("GROUNDTRUTH_TEST_DSN").is_err() {
        eprintln!("GROUNDTRUTH_TEST_DSN unset — skipping runs_checks_end_to_end");
        return;
    }
    seed().await;

    let results = run(&config()).await.expect("run");
    assert_eq!(results.len(), 2);

    let orders = results.iter().find(|r| r.name == "orders_present").unwrap();
    assert_eq!(orders.status, Status::Pass, "detail: {}", orders.detail);

    let future = results
        .iter()
        .find(|r| r.name == "no_orders_in_future")
        .unwrap();
    assert_eq!(future.status, Status::Pass, "detail: {}", future.detail);
}
