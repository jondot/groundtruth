# Double-entry payments ledger: books must balance, nothing stuck, no key settled twice.
# PagerDuty webhook; these are correctness invariants so most fail immediately.

connection "postgres" "core" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.core
  every   = "1m"
  timeout = "15s"
  on_fail = notify.webhook.pager
}

notify "webhook" "pager" {
  url = env("PAGER_WEBHOOK")
}

# ledger invariants
# across the whole ledger, debits must equal credits to the cent
check "ledger_is_balanced" {
  query = <<-SQL
    select coalesce(sum(amount_cents), 0) as net
    from ledger_entries
  SQL
  fail = row.net != 0
}

# exactly two legs per transaction: one debit, one credit
check "transactions_have_two_legs" {
  query = <<-SQL
    select transaction_id, count(*) as legs
    from ledger_entries
    group by transaction_id
    having count(*) <> 2
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# negative balance only allowed on overdraft-enabled accounts
check "no_unexpected_negative_balances" {
  query = <<-SQL
    select a.id, a.balance_cents
    from accounts a
    where a.balance_cents < 0
      and a.overdraft_enabled = false
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# two settled transactions sharing an idempotency key = we double-charged someone
check "no_duplicate_idempotency_keys" {
  query = <<-SQL
    select idempotency_key, count(*) as n
    from transactions
    where status = 'settled'
    group by idempotency_key
    having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# processing >30m = wedged in a provider call
check "no_stuck_transactions" {
  query = <<-SQL
    select id, amount_cents, created_at
    from transactions
    where status = 'processing'
      and created_at < now() - interval '30 minutes'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# sustained 20m gap = provider integration down
check "settlements_flowing" {
  query = "select count(*) as n from transactions where status = 'settled' and settled_at > now() - interval '15 minutes'"
  fail {
    when      = row.n == 0
    sustained = "20m"
  }
}

# nonzero gap between our ledger and the bank statement needs a human
check "bank_reconciliation_matches" {
  query = <<-SQL
    select coalesce(gap_cents, 0) as gap
    from reconciliation_runs
    order by run_at desc
    limit 1
  SQL
  warn = row.gap != 0
  fail = row.gap > 10000 || row.gap < -10000
}

# a missing nightly run hides drift
check "reconciliation_is_fresh" {
  query = "select extract(epoch from now() - max(run_at))::float8 as age from reconciliation_runs"
  fail  = row.age > duration("28h")
}

# known ISO currency + sane amount on recent rows
check "transaction_fields_valid" {
  query = "select currency, amount_cents from transactions where created_at > now() - interval '1 day'"
  validate {
    column "currency" {
      not_null = true
      allowed  = ["USD", "EUR", "GBP", "CAD", "AUD"]
    }
    column "amount_cents" {
      type     = "int"
      not_null = true
      range    = { min = 1, max = 100000000 } # $0.01 .. $1,000,000
    }
  }
}

check "no_orphaned_ledger_entries" {
  query = <<-SQL
    select le.id, le.transaction_id
    from ledger_entries le
    left join transactions t on t.id = le.transaction_id
    where t.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# not necessarily fraud, but finance wants eyes on big transfers
check "large_transfer_review" {
  query = "select count(*) as n from transactions where amount_cents > 5000000 and created_at > now() - interval '1 hour'"
  warn  = row.n > 0
}
