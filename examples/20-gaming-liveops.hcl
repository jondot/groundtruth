# Live multiplayer game with in-game economy and store. A duping exploit or a
# matchmaking stall is a revenue-and-reputation emergency.
# pager for economy/matchmaking, slack for the rest. store DB is separate.

connection "postgres" "game" {
  dsn = env("DATABASE_URL")
}

connection "postgres" "store" {
  dsn = env("REPLICA_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.game
  every   = "1m"
  on_fail = notify.webhook.liveops
}

notify "webhook" "pager" {
  url = env("PAGER_WEBHOOK")
}

notify "webhook" "liveops" {
  url = env("ALERT_WEBHOOK")
}

# player liveness
check "active_sessions_present" {
  query = "select count(*) as n from sessions where status = 'active' and last_heartbeat_at > now() - interval '2 minutes'"
  fail {
    when      = row.n == 0
    sustained = "5m"
  }
  on_fail = notify.webhook.pager
}

# sharp CCU drop mid-event = backend incident, not players leaving
check "concurrent_players_not_collapsed" {
  query = <<-SQL
    with now_ccu as (select count(*) c from sessions where status = 'active'),
         prior as (select player_count c from ccu_samples order by sampled_at desc limit 1 offset 5)
    select now_ccu.c::float8 / nullif(prior.c, 0) as ratio from now_ccu, prior
  SQL
  warn = row.ratio < 0.6
  fail = row.ratio < 0.3
}

# auth outage locks everyone out
check "logins_succeeding" {
  query = <<-SQL
    select count(*) filter (where success)::float8 / nullif(count(*), 0) as rate
    from login_events where created_at > now() - interval '5 minutes'
  SQL
  warn = row.rate < 0.9
  fail = row.rate < 0.6
}

# 'active' but no heartbeat for 10m = session GC leak
check "no_zombie_sessions" {
  query = <<-SQL
    select count(*) as n from sessions
    where status = 'active' and last_heartbeat_at < now() - interval '10 minutes'
  SQL
  warn = row.n > 100
  fail = row.n > 5000
}

# matchmaking
check "matchmaking_queue_depth" {
  query = "select count(*) as depth from matchmaking_queue where status = 'waiting'"
  warn  = row.depth > 5000
  fail {
    when      = row.depth > 20000
    sustained = "5m"
  }
}

check "matchmaking_wait_time" {
  query = <<-SQL
    select coalesce(extract(epoch from now() - min(queued_at)), 0)::float8 as oldest_wait
    from matchmaking_queue where status = 'waiting'
  SQL
  warn = row.oldest_wait > duration("3m")
  fail = row.oldest_wait > duration("10m")
}

check "matches_being_created" {
  query = "select count(*) as n from matches where created_at > now() - interval '5 minutes'"
  fail {
    when      = row.n == 0
    sustained = "10m"
  }
  on_fail = notify.webhook.pager
}

# formed but never started = orchestration failure
check "no_stuck_matches" {
  query = <<-SQL
    select id, created_at from matches
    where status = 'forming' and created_at < now() - interval '5 minutes'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# economy / anti-dupe -> pager
# minted must equal balances + sinks; a gap means someone's duping
check "currency_conservation" {
  query = <<-SQL
    select
      (select coalesce(sum(amount), 0) from currency_grants)
      - (select coalesce(sum(balance), 0) from wallets)
      - (select coalesce(sum(amount), 0) from currency_sinks) as discrepancy
  SQL
  fail    = row.discrepancy != 0
  on_fail = notify.webhook.pager
}

check "no_negative_wallets" {
  query = "select player_id, balance from wallets where balance < 0"
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.pager
}

# a unique instanced item owned by two players at once
check "no_duplicated_unique_items" {
  query = <<-SQL
    select item_instance_id, count(*) as owners from player_inventory
    where item_type = 'unique'
    group by item_instance_id having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.pager
}

# grants-to-sinks ratio spiking = inflation
check "economy_inflation" {
  query = <<-SQL
    with g as (select coalesce(sum(amount),0) v from currency_grants where created_at > now() - interval '1 hour'),
         s as (select coalesce(sum(amount),0) v from currency_sinks where created_at > now() - interval '1 hour')
    select g.v::float8 / nullif(s.v, 0) as ratio from g, s
  SQL
  warn = row.ratio > 3.0
  fail = row.ratio > 10.0
}

# store / purchases (store DB)
check "purchases_being_processed" {
  on    = connection.postgres.store
  query = "select count(*) as n from purchases where created_at > now() - interval '15 minutes'"
  warn  = row.n == 0
}

# completed purchase with no entitlement grant = player paid, got nothing
check "purchases_fulfilled" {
  on    = connection.postgres.store
  query = <<-SQL
    select p.id, p.player_id from purchases p
    left join entitlement_grants g on g.purchase_id = p.id
    where p.status = 'completed' and g.id is null
      and p.created_at > now() - interval '1 hour'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.pager
}

# dup receipt = double-grant from a replayed store callback
check "no_duplicate_receipts" {
  on    = connection.postgres.store
  query = <<-SQL
    select receipt_id, count(*) as n from purchases
    where receipt_id is not null group by receipt_id having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "purchase_amounts_valid" {
  on    = connection.postgres.store
  query = "select currency, amount_cents, product_id from purchases where created_at > now() - interval '1 day'"
  validate {
    column "currency" {
      not_null = true
      allowed  = ["USD", "EUR", "GBP", "JPY", "BRL", "KRW"]
    }
    column "amount_cents" {
      type  = "int"
      range = { min = 0, max = 50000000 }
    }
    column "product_id" {
      not_null = true
    }
  }
}

# refund spike = broken SKU or payment incident
check "refund_rate" {
  on    = connection.postgres.store
  query = <<-SQL
    select count(*) filter (where status = 'refunded')::float8 / nullif(count(*), 0) as rate
    from purchases where created_at > now() - interval '1 hour'
  SQL
  warn = row.rate > 0.05
  fail = row.rate > 0.2
}

# leaderboards & progression
check "no_duplicate_leaderboard_ranks" {
  query = <<-SQL
    select leaderboard_id, rank, count(*) as n from leaderboard_entries
    where season = 'current'
    group by leaderboard_id, rank having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# scores past theoretical max = cheating or overflow
check "leaderboard_scores_sane" {
  query = "select player_id, score from leaderboard_entries where season = 'current' and score > 1000000000"
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_orphaned_inventory" {
  query = <<-SQL
    select pi.id, pi.player_id from player_inventory pi
    left join players p on p.id = pi.player_id where p.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# anti-cheat
# high-confidence flags aging unreviewed
check "cheat_flags_triaged" {
  query = <<-SQL
    select count(*) as n from anti_cheat_flags
    where confidence = 'high' and reviewed = false
      and created_at < now() - interval '2 hours'
  SQL
  warn = row.n > 0
}

# impossible-movement signal from the telemetry pipeline
check "speedhack_signal" {
  query = <<-SQL
    select count(*) as n from movement_anomalies
    where severity = 'impossible' and created_at > now() - interval '10 minutes'
  SQL
  warn = row.n > 50
}

check "store_reachable" {
  on    = connection.postgres.store
  query = "select 1 as ok"
  fail  = row.ok != 1
}
