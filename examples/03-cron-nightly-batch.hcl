# Nightly sales ETL (~02:00): catch a silent failure before analysts hit dashboards.
# Cron'd at 08:00 — only needs to be true once each morning, not every 5m.

connection "postgres" "warehouse" {
  dsn = env("WAREHOUSE_URL")
}

defaults {
  on    = connection.postgres.warehouse
  every = "0 8 * * *" # 08:00 daily, after the 02:00 load
}

# no success row since midnight = last night's job didn't complete
check "nightly_load_succeeded" {
  query = <<-SQL
    select count(*) as n
    from batch_runs
    where job = 'daily_sales'
      and status = 'success'
      and finished_at >= current_date
  SQL
  fail = row.n == 0
}

# a "success" that loaded almost nothing usually means an empty upstream export
check "nightly_load_volume_sane" {
  query = <<-SQL
    select count(*) as n
    from daily_sales
    where sale_date = current_date - 1
  SQL
  warn = row.n < 1000
  fail = row.n == 0
}
