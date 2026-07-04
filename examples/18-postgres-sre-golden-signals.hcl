# Postgres infra health via pg_stat_* catalogs: saturation, lag, bloat, wraparound, locks.
# Calibrate thresholds to YOUR baseline before paging; start with reachability + saturation.

connection "postgres" "primary" {
  dsn = env("DATABASE_URL")
}

connection "postgres" "replica" {
  dsn = env("REPLICA_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.primary
  every   = "1m"
  timeout = "10s"
  on_fail = notify.webhook.pager
}

notify "webhook" "pager" {
  url = env("PAGER_WEBHOOK")
}

# reachability
check "primary_reachable" {
  query = "select 1 as ok"
  fail  = row.ok != 1
}

check "replica_reachable" {
  on    = connection.postgres.replica
  query = "select 1 as ok"
  fail  = row.ok != 1
}

# saturation
# fraction of max_connections; past ~0.9 you hit "too many clients"
check "connection_saturation" {
  query = <<-SQL
    select count(*)::float8 / (select setting::float8 from pg_settings where name = 'max_connections') as ratio
    from pg_stat_activity
  SQL
  warn = row.ratio > 0.70
  fail = row.ratio > 0.90
}

# idle-in-transaction holds locks and pins xmin, blocking vacuum
check "idle_in_transaction" {
  query = <<-SQL
    select count(*) as n from pg_stat_activity
    where state = 'idle in transaction'
      and now() - state_change > interval '5 minutes'
  SQL
  warn = row.n > 5
  fail = row.n > 20
}

# one very old open txn blocks vacuum across the whole DB
check "oldest_open_transaction" {
  query = <<-SQL
    select coalesce(max(extract(epoch from now() - xact_start)), 0)::float8 as age
    from pg_stat_activity
    where state <> 'idle' and xact_start is not null
  SQL
  warn = row.age > duration("10m")
  fail = row.age > duration("1h")
}

# query health
# excludes this monitor's own query
check "long_running_queries" {
  query = <<-SQL
    select count(*) as n from pg_stat_activity
    where state = 'active'
      and now() - query_start > interval '5 minutes'
      and query not ilike '%pg_stat_activity%'
  SQL
  warn = row.n > 0
  fail = row.n > 5
}

check "blocked_queries" {
  query = "select count(*) as n from pg_stat_activity where wait_event_type = 'Lock'"
  warn  = row.n > 5
  fail  = row.n > 20
}

# cumulative since stats reset, so warn = investigate, not page
check "deadlocks_seen" {
  query = "select coalesce(sum(deadlocks), 0) as n from pg_stat_database"
  warn  = row.n > 0
}

# cache / io
# sustained drop = working set outgrew shared_buffers/OS cache, latency cliff ahead
check "cache_hit_ratio" {
  query = <<-SQL
    select sum(blks_hit)::float8 / nullif(sum(blks_hit) + sum(blks_read), 0) as ratio
    from pg_stat_database
  SQL
  warn = row.ratio < 0.95
  fail = row.ratio < 0.90
}

# rollback spike signals app-level errors
check "rollback_ratio" {
  query = <<-SQL
    select sum(xact_rollback)::float8 / nullif(sum(xact_commit) + sum(xact_rollback), 0) as ratio
    from pg_stat_database
  SQL
  warn = row.ratio > 0.05
}

# queries spilling to disk = work_mem too small
check "temp_files_spilling" {
  query = "select coalesce(sum(temp_files), 0) as n from pg_stat_database"
  warn  = row.n > 100000
}

# bloat / vacuum
# dead-tuple ratio as bloat proxy; high on a hot table = autovacuum can't keep up
check "table_bloat_ratio" {
  query = <<-SQL
    select coalesce(max(n_dead_tup::float8 / nullif(n_live_tup, 0)), 0) as ratio
    from pg_stat_user_tables
    where n_live_tup > 10000
  SQL
  warn = row.ratio > 0.20
  fail = row.ratio > 0.50
}

check "tables_overdue_for_vacuum" {
  query = <<-SQL
    select relname, n_dead_tup, last_autovacuum
    from pg_stat_user_tables
    where n_dead_tup > 50000
      and (last_autovacuum is null or last_autovacuum < now() - interval '1 day')
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# xid wraparound
# oldest unfrozen xid; ~2.1B forces a shutdown, act well before that
check "transaction_wraparound_headroom" {
  query = "select max(age(datfrozenxid)) as xid_age from pg_database"
  warn  = row.xid_age > 1000000000
  fail  = row.xid_age > 1800000000
}

# replication
check "replica_replay_lag" {
  on    = connection.postgres.replica
  query = "select coalesce(extract(epoch from now() - pg_last_xact_replay_timestamp()), 0)::float8 as lag"
  warn  = row.lag > duration("30s")
  fail  = row.lag > duration("5m")
}

# byte lag of standbys, measured on the primary
check "standby_byte_lag" {
  query = <<-SQL
    select coalesce(max(pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn)), 0)::float8 as bytes
    from pg_stat_replication
  SQL
  warn = row.bytes > 104857600  # 100 MB
  fail = row.bytes > 1073741824 # 1 GB
}

check "standby_connected" {
  query = "select count(*) as n from pg_stat_replication where state = 'streaming'"
  fail {
    when      = row.n == 0
    sustained = "5m"
  }
}

# inactive slots retain WAL forever and fill the disk
check "no_inactive_replication_slots" {
  query = "select count(*) as n from pg_replication_slots where active = false"
  warn  = row.n > 0
}

# capacity
# placeholder ceilings; calibrate to your disk
check "table_size_ceiling" {
  for_each = ["events", "orders", "audit_log"]
  query    = "select pg_total_relation_size('${each.value}') as bytes"
  warn     = row.bytes > 53687091200  # 50 GB
  fail     = row.bytes > 214748364800 # 200 GB
}

# int4 PK creeping toward 2.1B; flag sequences past 80% of int4 max
check "sequence_exhaustion" {
  query = <<-SQL
    select count(*) as n
    from pg_sequences
    where last_value is not null
      and last_value > 1717986918  -- 80% of 2,147,483,647
      and max_value >= 9223372036854775807 -- declared bigint, but values imply int4 origin
  SQL
  warn = row.n > 0
}

check "database_size_ceiling" {
  query = "select pg_database_size(current_database()) as bytes"
  warn  = row.bytes > 536870912000  # 500 GB
}
