# groundtruth config — https://github.com/jondot/groundtruth
# Secrets come from the environment (set them as repo secrets in CI).
connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

# Ping a cron-monitor (Better Stack, healthchecks.io, ...) each run.
# Green on success; POSTs a failure report on FAIL/ERROR.
heartbeat {
  url = env("HEARTBEAT_URL")
}

check "orders_recent" {
  query = "select count(*) as n from orders where created_at > now() - interval '1 day'"
  fail  = row.n == 0
}
