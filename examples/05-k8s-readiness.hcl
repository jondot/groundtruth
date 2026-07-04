# K8s readiness gate: hold a pod out of the Service until the DB is reachable and
# the expected migration is applied, else new code hits missing columns and crashes.
# readinessProbe -> /checks/schema_migration_applied:9090; 503 holds traffic off.

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

defaults {
  on    = connection.postgres.main
  every = "15s" # react fast during a deploy
}

# gate on this release's version being present in the migration tool's table
check "schema_migration_applied" {
  query = <<-SQL
    select count(*) as n
    from schema_migrations
    where version = '20260615120000'
  SQL
  fail = row.n == 0
}

# proves the pool can issue a query, not just open a socket
check "core_table_reachable" {
  query = "select count(*) as n from accounts"
  fail  = row.n < 0
}
