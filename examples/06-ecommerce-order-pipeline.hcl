# DTC store order -> payment -> fulfillment pipeline. Failure mode: checkout looks
# fine but orders stop being written after a deploy.
# Slack via webhook; sustained gating so a deploy blip doesn't page anyone.

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

# persist sustained-failure timers across daemon restarts
state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.main
  every   = "2m"
  timeout = "10s"
  on_fail = notify.webhook.oncall
}

notify "webhook" "oncall" {
  url = env("ALERT_WEBHOOK")
}

# zero orders for 15m straight = checkout is down, not a quiet spell
check "orders_are_flowing" {
  query = "select count(*) as n from orders where created_at > now() - interval '10 minutes'"
  fail {
    when      = row.n == 0
    sustained = "15m"
  }
}

# freshness backstop on the newest order
check "orders_fresh" {
  query = "select extract(epoch from now() - max(created_at))::float8 as age from orders"
  warn  = row.age > duration("30m")
  fail  = row.age > duration("2h")
}

# payments
# pending >1h = gateway webhook never landed; customer paid, order frozen
check "no_stuck_payments" {
  query = <<-SQL
    select id, order_id, amount_cents, created_at
    from payments
    where status = 'pending'
      and created_at < now() - interval '1 hour'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# failure spike points at a gateway incident or a pricing bug
check "payment_failure_rate" {
  query = <<-SQL
    select
      count(*) filter (where status = 'failed')::float8
      / nullif(count(*), 0) as fail_rate
    from payments
    where created_at > now() - interval '15 minutes'
  SQL
  warn = row.fail_rate > 0.10
  fail = row.fail_rate > 0.30
}

# same order paid twice = refunds + chargebacks incoming
check "no_duplicate_charges" {
  query = <<-SQL
    select order_id, count(*) as charges
    from payments
    where status = 'succeeded'
    group by order_id
    having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# negative stock = oversells
check "no_negative_inventory" {
  query = <<-SQL
    select sku, warehouse_id, on_hand
    from inventory
    where on_hand < 0
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# active SKUs running dry — heads-up for the buyer
check "low_stock_on_active_skus" {
  query = <<-SQL
    select count(*) as n
    from inventory i
    join products p on p.sku = i.sku
    where p.status = 'active' and i.on_hand < 5
  SQL
  warn = row.n > 0
}

check "no_orphaned_line_items" {
  query = <<-SQL
    select li.id, li.order_id
    from line_items li
    left join orders o on o.id = li.order_id
    where o.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# catches enum drift / bad writes
check "order_status_valid" {
  query = <<-SQL
    select id, status
    from orders
    where status not in ('cart','pending','paid','fulfilled','shipped','delivered','cancelled','refunded')
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# revenue signal, not an outage — warn only
check "abandoned_cart_backlog" {
  query = <<-SQL
    select count(*) as n
    from orders
    where status = 'cart'
      and updated_at < now() - interval '1 hour'
  SQL
  warn = row.n > 500
}
