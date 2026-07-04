# db-backed job queue (Oban/pg-boss style): wedged workers and poison jobs back it up silently
# sustained gating gives the autoscaler a window to absorb bursts before paging

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.main
  every   = "1m"
  on_fail = notify.webhook.oncall
}

notify "webhook" "oncall" {
  url = env("ALERT_WEBHOOK")
}

# warn early; page only on a deep backlog that survives 10m (autoscaler's window)
check "queue_depth" {
  query = "select count(*) as depth from jobs where status = 'pending'"
  warn  = row.depth > 1000
  fail {
    when      = row.depth > 5000
    sustained = "10m"
  }
}

# oldest-pending age beats raw depth as a latency signal
check "oldest_pending_job_age" {
  query = <<-SQL
    select coalesce(extract(epoch from now() - min(enqueued_at)), 0)::float8 as age
    from jobs
    where status = 'pending'
  SQL
  warn = row.age > duration("5m")
  fail = row.age > duration("30m")
}

# running far longer than any job should = wedged worker
check "no_stuck_running_jobs" {
  query = <<-SQL
    select id, queue, worker_id, started_at
    from jobs
    where status = 'running'
      and started_at < now() - interval '15 minutes'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "dead_letter_growth" {
  query = "select count(*) as n from jobs where status = 'dead' and failed_at > now() - interval '15 minutes'"
  warn  = row.n > 10
  fail  = row.n > 100
}

# one job class looping on retry burns the whole pool
check "retry_storm" {
  query = <<-SQL
    select job_class, count(*) as retries
    from jobs
    where status = 'retrying'
      and updated_at > now() - interval '5 minutes'
    group by job_class
    having count(*) > 200
  SQL
  warn {
    when   = rows.count > 0
    sample = 5
  }
}

check "job_failure_rate" {
  query = <<-SQL
    select
      count(*) filter (where status = 'failed')::float8
      / nullif(count(*), 0) as fail_rate
    from jobs
    where finished_at > now() - interval '10 minutes'
  SQL
  warn = row.fail_rate > 0.05
  fail = row.fail_rate > 0.25
}

# no heartbeat = pool is down
check "workers_alive" {
  query = "select count(*) as n from worker_heartbeats where last_seen_at > now() - interval '90 seconds'"
  fail {
    when      = row.n == 0
    sustained = "3m"
  }
}

# overdue cron enqueues = scheduler stalled
check "no_overdue_scheduled_jobs" {
  query = <<-SQL
    select name, next_run_at
    from scheduled_jobs
    where enabled = true
      and next_run_at < now() - interval '5 minutes'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# completions should keep ticking up during the day
check "throughput_not_collapsed" {
  query = "select count(*) as n from jobs where status = 'completed' and finished_at > now() - interval '5 minutes'"
  warn  = row.n == 0
}

# dup enqueues of an idempotent job = missing unique constraint
check "no_duplicate_enqueues" {
  query = <<-SQL
    select idempotency_key, count(*) as n
    from jobs
    where idempotency_key is not null
      and enqueued_at > now() - interval '1 hour'
    group by idempotency_key
    having count(*) > 1
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}
