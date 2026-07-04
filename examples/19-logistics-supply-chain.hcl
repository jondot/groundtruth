# Last-mile logistics. Expensive failures are silent: a carrier feed stops,
# shipments age past their promise, scans stop flowing from a region.
# pager for SLA/feed stalls, slack for the rest. hub DB = packages, ops DB = drivers/routes.

connection "postgres" "hub" {
  dsn = env("DATABASE_URL")
}

connection "postgres" "ops" {
  dsn = env("REPLICA_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.hub
  every   = "3m"
  on_fail = notify.webhook.ops
}

notify "webhook" "pager" {
  url = env("PAGER_WEBHOOK")
}

notify "webhook" "ops" {
  url = env("ALERT_WEBHOOK")
}

# shipment flow
check "shipments_being_created" {
  query = "select count(*) as n from shipments where created_at > now() - interval '15 minutes'"
  fail {
    when      = row.n == 0
    sustained = "30m"
  }
}

check "shipment_status_valid" {
  query = <<-SQL
    select id, status from shipments
    where status not in ('created','picked','in_transit','out_for_delivery','delivered','returned','lost','cancelled')
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# a scan stall blinds the control tower
check "scan_events_flowing" {
  query = "select extract(epoch from now() - max(scanned_at))::float8 as age from scan_events"
  warn  = row.age > duration("10m")
  fail  = row.age > duration("30m")
}

# a region going quiet usually means a depot's scanners are offline
check "no_silent_regions" {
  query = <<-SQL
    select region, max(scanned_at) as last_scan
    from scan_events
    group by region
    having max(scanned_at) < now() - interval '45 minutes'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# sla
# past promised_by and not delivered = contractual breach; >100 pages
check "sla_breaches" {
  query = <<-SQL
    select id, tracking_number, promised_by
    from shipments
    where status not in ('delivered','returned','cancelled')
      and promised_by < now()
  SQL
  warn {
    when   = rows.count > 0
    sample = 20
  }
  fail {
    when   = rows.count > 100
    sample = 20
  }
  on_fail = notify.webhook.pager
}

# in_transit but no scan in 24h = sitting in a hub
check "stuck_in_hub" {
  query = <<-SQL
    select count(*) as n from shipments
    where status = 'in_transit'
      and last_scan_at < now() - interval '24 hours'
  SQL
  warn = row.n > 50
  fail = row.n > 500
}

# out for delivery 12h+ and never marked delivered
check "stale_out_for_delivery" {
  query = <<-SQL
    select count(*) as n from shipments
    where status = 'out_for_delivery'
      and updated_at < now() - interval '12 hours'
  SQL
  warn = row.n > 0
}

# carrier feeds
# stale feed = flying blind on that carrier's packages
check "carrier_feeds_fresh" {
  for_each = ["ups", "fedex", "usps", "dhl"]
  query    = "select extract(epoch from now() - max(received_at))::float8 as age from carrier_events where carrier = '${each.value}'"
  warn     = row.age > duration("20m")
  fail     = row.age > duration("1h")
}

# events for a shipment we don't have = mapping/ID mismatch
check "no_orphaned_carrier_events" {
  query = <<-SQL
    select ce.id, ce.tracking_number from carrier_events ce
    left join shipments s on s.tracking_number = ce.tracking_number
    where s.id is null and ce.received_at > now() - interval '1 hour'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# routes & drivers (ops DB)
check "routes_have_drivers" {
  on    = connection.postgres.ops
  query = <<-SQL
    select id, route_date from routes
    where route_date = current_date and driver_id is null and status = 'planned'
  SQL
  warn {
    when   = rows.count > 0
    sample = 20
  }
}

check "no_orphaned_route_stops" {
  on    = connection.postgres.ops
  query = <<-SQL
    select rs.id, rs.route_id from route_stops rs
    left join routes r on r.id = rs.route_id where r.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "drivers_available_for_today" {
  on    = connection.postgres.ops
  query = <<-SQL
    select count(*) as n from drivers
    where status = 'active' and available_date = current_date
  SQL
  warn = row.n < 10
  fail = row.n == 0
}

check "driver_locations_fresh" {
  on    = connection.postgres.ops
  query = <<-SQL
    select count(*) as n from drivers
    where status = 'on_route'
      and (last_ping_at is null or last_ping_at < now() - interval '15 minutes')
  SQL
  warn = row.n > 5
}

check "delivery_addresses_valid" {
  query = <<-SQL
    select postal_code, latitude, longitude, country
    from shipments
    where created_at > now() - interval '1 hour'
  SQL
  validate {
    column "postal_code" {
      not_null = true
    }
    column "latitude" {
      type  = "float"
      range = { min = -90, max = 90 }
    }
    column "longitude" {
      type  = "float"
      range = { min = -180, max = 180 }
    }
    column "country" {
      allowed = ["US", "CA", "MX", "GB", "DE", "FR"]
    }
  }
}

# returns & exceptions
check "lost_package_rate" {
  query = <<-SQL
    select count(*) filter (where status = 'lost')::float8 / nullif(count(*), 0) as rate
    from shipments where created_at > now() - interval '7 days'
  SQL
  warn = row.rate > 0.005
  fail = row.rate > 0.02
}

check "returns_being_processed" {
  query = <<-SQL
    select count(*) as n from shipments
    where status = 'returned' and return_processed_at is null
      and updated_at < now() - interval '3 days'
  SQL
  warn = row.n > 0
}

check "exceptions_acknowledged" {
  query = <<-SQL
    select id, tracking_number, exception_type from delivery_exceptions
    where acknowledged = false and created_at < now() - interval '2 hours'
  SQL
  warn {
    when   = rows.count > 0
    sample = 15
  }
}

# capacity
check "warehouse_capacity" {
  query = <<-SQL
    select warehouse_id, current_units, capacity_units from warehouses
    where current_units::float8 / nullif(capacity_units, 0) > 0.95
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# delivered status but no delivered_at timestamp = inconsistent state machine
check "inbound_matches_outbound" {
  query = <<-SQL
    select count(*) as n from shipments
    where status = 'delivered' and delivered_at is null
  SQL
  fail = row.n > 0
}

check "hub_reachable" {
  query = "select 1 as ok"
  fail  = row.ok != 1
}

check "ops_reachable" {
  on    = connection.postgres.ops
  query = "select 1 as ok"
  fail  = row.ok != 1
}
