# Tiny SaaS smoke test: is the DB answering, and are we getting signups.
# Single Better Stack monitor on /checks; a 503 means look at the DB.

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

defaults {
  on    = connection.postgres.main
  every = "5m"
}

# empty users table = something wiped it; errors out if unreadable
check "users_table_healthy" {
  query = "select count(*) as n from users"
  fail  = row.n == 0
}

# zero signups in a day is a glance, not a page
check "signups_today" {
  query = "select count(*) as n from users where created_at >= current_date"
  warn  = row.n == 0
}
