# fleet telemetry into TimescaleDB; the silent failure is lag creep or devices going dark while dashboards show stale values
# ingestion stall pages, sensor drift warns

connection "postgres" "tsdb" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.tsdb
  every   = "1m"
  on_fail = notify.webhook.ops
}

notify "webhook" "ops" {
  url = env("ALERT_WEBHOOK")
}

# newest reading should be seconds old; minutes = stalled ingestion
check "ingestion_fresh" {
  query = "select extract(epoch from now() - max(received_at))::float8 as age from readings"
  fail {
    when      = row.age > duration("5m")
    sustained = "5m"
  }
}

# device-clock vs receive-time; growing lag = falling behind even while rows land
check "ingestion_lag" {
  query = <<-SQL
    select coalesce(avg(extract(epoch from received_at - measured_at)), 0)::float8 as lag
    from readings
    where received_at > now() - interval '5 minutes'
  SQL
  warn = row.lag > duration("30s")
  fail = row.lag > duration("5m")
}

# busy fleet always produces readings; zero = total stall
check "reading_volume" {
  query = "select count(*) as n from readings where received_at > now() - interval '1 minute'"
  warn  = row.n < 100
  fail  = row.n == 0
}

# no heartbeat in 15m = offline
check "offline_devices" {
  query = <<-SQL
    select count(*) as n
    from devices
    where status = 'provisioned'
      and (last_seen_at is null or last_seen_at < now() - interval '15 minutes')
  SQL
  warn = row.n > 10
  fail = row.n > 100
}

# future timestamps = clock/timezone bug on the device
check "no_future_timestamps" {
  query = "select count(*) as n from readings where measured_at > now() + interval '2 minutes'"
  warn  = row.n > 0
}

# values outside physical bounds = faults, not measurements
check "sensor_values_sane" {
  query = <<-SQL
    select temperature_c, humidity_pct, battery_pct
    from readings
    where received_at > now() - interval '5 minutes'
  SQL
  validate {
    column "temperature_c" {
      type  = "float"
      range = { min = -50, max = 125 }
    }
    column "humidity_pct" {
      type  = "float"
      range = { min = 0, max = 100 }
    }
    column "battery_pct" {
      type  = "float"
      range = { min = 0, max = 100 }
    }
  }
}

# readings from an unregistered device_id = spoofed or stale firmware
check "no_readings_from_unknown_devices" {
  query = <<-SQL
    select r.id, r.device_id
    from readings r
    left join devices d on d.id = r.device_id
    where d.id is null
      and r.received_at > now() - interval '10 minutes'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# dup (device + measured_at) inflates aggregates
check "no_duplicate_readings" {
  query = <<-SQL
    select device_id, measured_at, count(*) as n
    from readings
    where received_at > now() - interval '5 minutes'
    group by device_id, measured_at
    having count(*) > 1
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# critically low battery needs a field visit before it drops off
check "low_battery_devices" {
  query = <<-SQL
    select count(*) as n
    from devices
    where status = 'provisioned'
      and battery_pct < 10
  SQL
  warn = row.n > 0
}

# too many firmware versions in the field complicates support
check "firmware_version_spread" {
  query = "select count(distinct firmware_version) as versions from devices where status = 'provisioned'"
  warn  = row.versions > 8
}
