# E-commerce at scale: primary owns truth, replica serves reads, warehouse powers merchandising.
# pager for revenue/correctness, slack for the rest.

connection "postgres" "primary" {
  dsn = env("DATABASE_URL")
}

connection "postgres" "replica" {
  dsn = env("REPLICA_URL")
}

connection "postgres" "analytics" {
  dsn = env("ANALYTICS_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.primary
  every   = "2m"
  timeout = "10s"
  on_fail = notify.webhook.slack
}

notify "webhook" "pager" {
  url = env("PAGER_WEBHOOK")
}

notify "webhook" "slack" {
  url = env("ALERT_WEBHOOK")
}

# orders
check "orders_are_flowing" {
  query = "select count(*) as n from orders where created_at > now() - interval '10 minutes'"
  fail {
    when      = row.n == 0
    sustained = "15m"
  }
  on_fail = notify.webhook.pager
}

check "orders_fresh" {
  query = "select extract(epoch from now() - max(created_at))::float8 as age from orders"
  warn  = row.age > duration("20m")
  fail  = row.age > duration("1h")
}

check "checkout_conversion_not_collapsed" {
  query = <<-SQL
    select
      count(*) filter (where status <> 'cart')::float8
      / nullif(count(*), 0) as conv
    from orders
    where created_at > now() - interval '15 minutes'
  SQL
  warn = row.conv < 0.4
  fail = row.conv < 0.1
}

check "order_status_valid" {
  query = <<-SQL
    select id, status from orders
    where status not in ('cart','pending','paid','fulfilled','shipped','delivered','cancelled','refunded')
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# payments
check "payment_failure_rate" {
  query = <<-SQL
    select count(*) filter (where status = 'failed')::float8 / nullif(count(*), 0) as fail_rate
    from payments where created_at > now() - interval '15 minutes'
  SQL
  warn = row.fail_rate > 0.10
  fail = row.fail_rate > 0.30
}

check "no_stuck_payments" {
  query = <<-SQL
    select id, order_id, created_at from payments
    where status = 'pending' and created_at < now() - interval '1 hour'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_duplicate_charges" {
  query = <<-SQL
    select order_id, count(*) as charges from payments
    where status = 'succeeded' group by order_id having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.pager
}

# paid-or-later orders must have a successful capture
check "captured_matches_orders" {
  query = <<-SQL
    select count(*) as n from orders o
    where o.status in ('paid','fulfilled','shipped','delivered')
      and not exists (
        select 1 from payments p where p.order_id = o.id and p.status = 'succeeded'
      )
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# inventory
check "no_negative_inventory" {
  query = "select sku, warehouse_id, on_hand from inventory where on_hand < 0"
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_oversold_skus" {
  query = <<-SQL
    select sku from inventory where reserved > on_hand
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "low_stock_active_skus" {
  query = <<-SQL
    select count(*) as n from inventory i
    join products p on p.sku = i.sku
    where p.status = 'active' and i.on_hand < 5
  SQL
  warn = row.n > 0
}

# fulfillment
check "fulfillment_not_backed_up" {
  query = <<-SQL
    select count(*) as n from orders
    where status = 'paid' and updated_at < now() - interval '2 hours'
  SQL
  warn = row.n > 50
  fail = row.n > 500
}

check "shipments_have_tracking" {
  query = <<-SQL
    select id, order_id from shipments
    where status = 'shipped' and (tracking_number is null or tracking_number = '')
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_orphaned_shipments" {
  query = <<-SQL
    select s.id, s.order_id from shipments s
    left join orders o on o.id = s.order_id where o.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# integrity
check "core_tables_not_empty" {
  for_each = ["orders", "products", "customers", "inventory", "payments"]
  query    = "select count(*) as n from ${each.value}"
  fail     = row.n == 0
}

check "no_orphaned_line_items" {
  query = <<-SQL
    select li.id, li.order_id from line_items li
    left join orders o on o.id = li.order_id where o.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

check "line_items_reference_real_products" {
  query = <<-SQL
    select li.id, li.sku from line_items li
    left join products p on p.sku = li.sku where p.sku is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

check "new_customer_quality" {
  query = <<-SQL
    select email, country, marketing_status
    from customers where created_at > now() - interval '1 day'
  SQL
  validate {
    column "email" {
      not_null = true
      matches  = "^[^@]+@[^@]+\\.[a-zA-Z]{2,}$"
    }
    column "country" {
      not_null = true
    }
    column "marketing_status" {
      allowed = ["subscribed", "unsubscribed", "pending"]
    }
  }
}

# fraud
# burst of failures = someone validating stolen cards
check "card_testing_signal" {
  query = <<-SQL
    select count(*) as n from payments
    where status = 'failed' and created_at > now() - interval '5 minutes'
  SQL
  warn = row.n > 100
  fail = row.n > 500
}

check "velocity_same_card" {
  query = <<-SQL
    select card_fingerprint, count(*) as attempts from payments
    where created_at > now() - interval '10 minutes'
    group by card_fingerprint having count(*) > 20
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "search_index_in_sync" {
  on    = connection.postgres.replica
  query = <<-SQL
    select count(*) as n from products
    where status = 'active' and (indexed_at is null or indexed_at < updated_at)
  SQL
  warn = row.n > 100
}

check "revenue_rollup_fresh" {
  on    = connection.postgres.analytics
  query = "select extract(epoch from now() - max(_loaded_at))::float8 as age from daily_revenue"
  warn  = row.age > duration("6h")
  fail  = row.age > duration("24h")
}

check "replica_not_lagging" {
  on      = connection.postgres.replica
  query   = "select extract(epoch from now() - pg_last_xact_replay_timestamp())::float8 as lag"
  warn    = row.lag > duration("30s")
  fail    = row.lag > duration("5m")
  on_fail = notify.webhook.pager
}

check "replica_reachable" {
  on    = connection.postgres.replica
  query = "select 1 as ok"
  fail  = row.ok != 1
}
