# shared-schema multi-tenancy (tenant_id on every table): leaks, quota bypass, billing drift
# isolation breaches page; quota/billing warn

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.main
  every   = "5m"
  on_fail = notify.webhook.platform
}

notify "webhook" "platform" {
  url = env("ALERT_WEBHOOK")
}

# null tenant_id = row visible to every tenant
check "no_null_tenant_id_projects" {
  query = "select count(*) as n from projects where tenant_id is null"
  fail  = row.n > 0
}

# orphaned rows can resurface under the wrong account after id reuse
check "no_orphaned_tenant_rows" {
  query = <<-SQL
    select p.id, p.tenant_id
    from projects p
    left join tenants t on t.id = p.tenant_id
    where t.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "core_tables_not_empty" {
  for_each = ["tenants", "users", "projects", "subscriptions"]
  query    = "select count(*) as n from ${each.value}"
  fail     = row.n == 0
}

# seats over plan limit = revenue leakage
check "no_seat_overage" {
  query = <<-SQL
    select t.id, t.name, count(u.id) as seats_used, t.seat_limit
    from tenants t
    join users u on u.tenant_id = t.id and u.status = 'active'
    group by t.id, t.name, t.seat_limit
    having count(u.id) > t.seat_limit
  SQL
  warn {
    when   = rows.count > 0
    sample = 20
  }
}

# active tenant, no subscription row = using it for free
check "active_tenants_have_subscription" {
  query = <<-SQL
    select t.id, t.name
    from tenants t
    left join subscriptions s on s.tenant_id = t.id
    where t.status = 'active' and s.id is null
  SQL
  warn {
    when   = rows.count > 0
    sample = 20
  }
}

# suspended tenant still authenticating = billing-bypass bug
check "suspended_tenants_are_locked" {
  query = <<-SQL
    select distinct t.id
    from tenants t
    join users u on u.tenant_id = t.id
    where t.status = 'suspended'
      and u.last_login_at > now() - interval '1 hour'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# usage events feed metering; stall = invoices undercount
check "usage_events_fresh" {
  query = "select extract(epoch from now() - max(created_at))::float8 as age from usage_events"
  warn  = row.age > duration("30m")
  fail  = row.age > duration("2h")
}

# Stripe sync stamps each tenant on success
check "billing_sync_fresh" {
  query = <<-SQL
    select count(*) as n
    from tenants
    where status = 'active'
      and (billing_synced_at is null or billing_synced_at < now() - interval '24 hours')
  SQL
  warn = row.n > 0
}

# expired trials never converted or downgraded clutter the funnel
check "expired_trials_backlog" {
  query = <<-SQL
    select count(*) as n
    from subscriptions
    where plan = 'trial'
      and trial_ends_at < now()
      and status = 'active'
  SQL
  warn = row.n > 25
}
