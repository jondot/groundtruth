# Demo config — run with:  DATABASE_URL=postgres://user@localhost:5432/mydb gt run groundtruth.hcl

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

# Optional: persist sustained-failure state across restarts
state {
  dsn = env("DATABASE_URL")
}

# PASS: orders exist
check "orders_present" {
  query = "select count(*) as recent from orders"
  fail  = row.recent == 0
}

# WARN: low volume (5 rows) but not zero
check "order_volume_healthy" {
  query = "select count(*) as recent from orders"
  warn  = row.recent < 10
  fail  = row.recent == 0
}

# FAIL: 3 orphaned line items — with the offending rows attached to the report
check "no_orphaned_line_items" {
  query = <<-SQL
    select li.id, li.order_id from line_items li
    left join orders o on o.id = li.order_id
    where o.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# FAIL: events table is stale (last update 41 min ago)
check "events_are_fresh" {
  query = "select extract(epoch from now() - max(updated_at))::float8 as age from events"
  fail  = row.age > duration("30m")
}

# ERROR: typo'd column — must surface as broken, never silently pass
check "deliberately_broken" {
  query = "select count(*) as recent from orders"
  fail  = row.recnt == 0
}

# Declarative data validation (validate is mutually exclusive with warn/fail)
check "users_data_quality" {
  query = "select email, age, status from users"
  validate {
    column "email" {
      not_null = true
      matches  = "^[^@]+@[^@]+\\.[a-zA-Z]{2,}$"
    }
    column "age" {
      type  = "int"
      range = { min = 0, max = 130 }
    }
    column "status" {
      allowed = ["active", "inactive", "pending"]
    }
  }
}
