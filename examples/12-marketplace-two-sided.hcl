# two-sided marketplace: both sides liquid, payouts on time, trust layer intact
# payout SLA breaches page, rest warns

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.main
  every   = "5m"
  on_fail = notify.webhook.ops
}

notify "webhook" "ops" {
  url = env("ALERT_WEBHOOK")
}

# supply side; collapse means demand has nothing to buy
check "active_listings_present" {
  query = "select count(*) as n from listings where status = 'active'"
  warn  = row.n < 100
  fail  = row.n == 0
}

# sustained zero during peak = broken checkout, not a quiet market
check "bookings_are_flowing" {
  query = "select count(*) as n from bookings where created_at > now() - interval '15 minutes'"
  fail {
    when      = row.n == 0
    sustained = "20m"
  }
}

# lopsided listings-vs-bookings ratio = liquidity problem worth a look
check "supply_demand_ratio" {
  query = <<-SQL
    with s as (select count(*) as c from listings where created_at > now() - interval '1 day'),
         d as (select count(*) as c from bookings where created_at > now() - interval '1 day')
    select s.c::float8 / nullif(d.c, 0) as ratio from s, d
  SQL
  warn = row.ratio > 50 || row.ratio < 0.2
}

# missed payout SLA erodes seller trust fast
check "payouts_within_sla" {
  query = <<-SQL
    select id, seller_id, amount_cents, due_at
    from payouts
    where status = 'pending'
      and due_at < now() - interval '2 hours'
  SQL
  fail {
    when   = rows.count > 0
    sample = 15
  }
}

# stuck in 'processing' = wedged transfer at the PSP
check "no_stuck_payouts" {
  query = <<-SQL
    select id, seller_id, processing_started_at
    from payouts
    where status = 'processing'
      and processing_started_at < now() - interval '1 hour'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_orphaned_bookings" {
  query = <<-SQL
    select b.id, b.listing_id
    from bookings b
    left join listings l on l.id = b.listing_id
    where l.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# review must tie to a real completed booking (anti-fake-review)
check "reviews_tied_to_completed_bookings" {
  query = <<-SQL
    select r.id, r.booking_id
    from reviews r
    left join bookings b on b.id = r.booking_id and b.status = 'completed'
    where b.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "review_ratings_in_range" {
  query = "select id, rating from reviews where rating < 1 or rating > 5"
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# aging open disputes = T&S backlog, not an outage
check "dispute_backlog" {
  query = <<-SQL
    select count(*) as n
    from disputes
    where status = 'open'
      and created_at < now() - interval '3 days'
  SQL
  warn = row.n > 20
}

# stale index sync = buyers see ghost listings
check "search_index_fresh" {
  query = <<-SQL
    select count(*) as n
    from listings
    where status = 'active'
      and (indexed_at is null or indexed_at < updated_at)
  SQL
  warn = row.n > 100
}
