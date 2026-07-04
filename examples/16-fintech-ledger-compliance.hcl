# Regulated fintech: core banking ledger + reporting store for filings.
# drift here becomes a regulator conversation.
# pager for money correctness, compliance webhook for regulatory/AML.

connection "postgres" "core" {
  dsn = env("DATABASE_URL")
}

connection "postgres" "reporting" {
  dsn = env("ANALYTICS_URL")
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

notify "webhook" "compliance" {
  url = env("ALERT_WEBHOOK")
}

# ledger invariants
# double-entry: every entry nets to zero across the book
check "ledger_balanced_globally" {
  query = "select coalesce(sum(amount_cents), 0) as net from ledger_entries"
  fail  = row.net != 0
}

check "transactions_have_two_legs" {
  query = <<-SQL
    select transaction_id, count(*) as legs from ledger_entries
    group by transaction_id having count(*) <> 2
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# cached balance must equal sum of its ledger entries
check "account_balance_matches_ledger" {
  query = <<-SQL
    select a.id, a.balance_cents, coalesce(sum(le.amount_cents), 0) as ledger_balance
    from accounts a
    left join ledger_entries le on le.account_id = a.id
    group by a.id, a.balance_cents
    having a.balance_cents <> coalesce(sum(le.amount_cents), 0)
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_unexpected_negative_balances" {
  query = <<-SQL
    select id, balance_cents from accounts
    where balance_cents < 0 and overdraft_enabled = false
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_orphaned_ledger_entries" {
  query = <<-SQL
    select le.id, le.transaction_id from ledger_entries le
    left join transactions t on t.id = le.transaction_id where t.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# idempotency
check "no_duplicate_settled_idempotency_keys" {
  query = <<-SQL
    select idempotency_key, count(*) as n from transactions
    where status = 'settled' group by idempotency_key having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_duplicate_external_refs" {
  query = <<-SQL
    select external_ref, count(*) as n from transactions
    where external_ref is not null group by external_ref having count(*) > 1
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# liveness / stuck money
check "settlements_flowing" {
  query = "select count(*) as n from transactions where status = 'settled' and settled_at > now() - interval '15 minutes'"
  fail {
    when      = row.n == 0
    sustained = "20m"
  }
}

check "no_stuck_transactions" {
  query = <<-SQL
    select id, amount_cents, created_at from transactions
    where status = 'processing' and created_at < now() - interval '30 minutes'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_stuck_disbursements" {
  query = <<-SQL
    select id, amount_cents, requested_at from disbursements
    where status = 'pending' and requested_at < now() - interval '2 hours'
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# reconciliation
# any gap warns; >$100 pages
check "bank_reconciliation_matches" {
  query = <<-SQL
    select coalesce(gap_cents, 0) as gap from reconciliation_runs
    order by run_at desc limit 1
  SQL
  warn = row.gap != 0
  fail = row.gap > 10000 || row.gap < -10000
}

check "reconciliation_is_fresh" {
  query = "select extract(epoch from now() - max(run_at))::float8 as age from reconciliation_runs"
  fail  = row.age > duration("28h")
}

check "processor_balance_reconciled" {
  query = <<-SQL
    select coalesce(max(abs(difference_cents)), 0) as max_diff
    from processor_reconciliation
    where as_of_date = current_date - 1
  SQL
  warn = row.max_diff > 0
  fail = row.max_diff > 50000
}

check "transaction_fields_valid" {
  query = "select currency, amount_cents, status from transactions where created_at > now() - interval '1 day'"
  validate {
    column "currency" {
      not_null = true
      allowed  = ["USD", "EUR", "GBP", "CAD", "AUD", "JPY"]
    }
    column "amount_cents" {
      type     = "int"
      not_null = true
      range    = { min = 1, max = 1000000000 }
    }
    column "status" {
      allowed = ["created", "processing", "settled", "failed", "reversed"]
    }
  }
}

# compliance / AML / KYC -> compliance channel
check "kyc_pending_backlog" {
  query = <<-SQL
    select count(*) as n from customers
    where kyc_status = 'pending' and created_at < now() - interval '48 hours'
  SQL
  warn    = row.n > 0
  on_fail = notify.webhook.compliance
}

check "active_accounts_are_kyc_verified" {
  query = <<-SQL
    select c.id from customers c
    where c.account_status = 'active' and c.kyc_status <> 'verified'
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.compliance
}

# OFAC hit not yet reviewed
check "sanctioned_party_screening_clear" {
  query = <<-SQL
    select customer_id from sanctions_screening
    where result = 'hit' and reviewed = false
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.compliance
}

# txns over $10k need a SAR review filed
check "large_transactions_have_sar_review" {
  query = <<-SQL
    select t.id, t.amount_cents from transactions t
    left join sar_reviews s on s.transaction_id = t.id
    where t.amount_cents > 1000000
      and t.created_at > now() - interval '7 days'
      and s.id is null
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.compliance
}

check "aml_velocity_flag" {
  query = <<-SQL
    select account_id, sum(amount_cents) as total from transactions
    where created_at > now() - interval '24 hours' and direction = 'outbound'
    group by account_id having sum(amount_cents) > 5000000
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
  on_fail = notify.webhook.compliance
}

# audit trail
check "audit_log_fresh" {
  query = "select extract(epoch from now() - max(occurred_at))::float8 as age from audit_log"
  fail  = row.age > duration("15m")
}

# every audit entry must name an actor; null = untraceable change
check "audit_entries_attributed" {
  query = <<-SQL
    select count(*) as n from audit_log
    where actor_id is null and occurred_at > now() - interval '1 hour'
  SQL
  fail = row.n > 0
}

# regulatory reporting (reporting store)
check "daily_regulatory_extract_present" {
  on    = connection.postgres.reporting
  query = <<-SQL
    select count(*) as n from regulatory_extracts
    where report_date = current_date - 1 and status = 'complete'
  SQL
  fail    = row.n == 0
  on_fail = notify.webhook.compliance
}

check "reporting_store_fresh" {
  on    = connection.postgres.reporting
  query = "select extract(epoch from now() - max(_loaded_at))::float8 as age from transactions_fact"
  warn  = row.age > duration("6h")
  fail  = row.age > duration("24h")
}

check "reporting_store_reachable" {
  on    = connection.postgres.reporting
  query = "select 1 as ok"
  fail  = row.ok != 1
}
