# db-level security detections off the auth tables (not logs): abuse, privilege hygiene, secret expiry
# complements the SIEM; the sharp ones page

connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

state {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.main
  every   = "2m"
  on_fail = notify.webhook.security
}

notify "webhook" "security" {
  url = env("ALERT_WEBHOOK")
}

# failed-login surge across accounts = credential stuffing
check "failed_login_burst" {
  query = <<-SQL
    select count(*) as attempts
    from login_attempts
    where success = false
      and created_at > now() - interval '5 minutes'
  SQL
  warn = row.attempts > 200
  fail = row.attempts > 1000
}

# many distinct accounts failing from one IP = targeted spray
check "distributed_login_spray" {
  query = <<-SQL
    select ip_address, count(distinct user_id) as accounts
    from login_attempts
    where success = false
      and created_at > now() - interval '10 minutes'
    group by ip_address
    having count(distinct user_id) > 25
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# one account hammering reset = takeover recon
check "password_reset_abuse" {
  query = <<-SQL
    select user_id, count(*) as resets
    from password_reset_requests
    where created_at > now() - interval '1 hour'
    group by user_id
    having count(*) > 10
  SQL
  warn {
    when   = rows.count > 0
    sample = 10
  }
}

# signup spike = bot registration
check "signup_velocity" {
  query = <<-SQL
    select count(*) as n
    from users
    where created_at > now() - interval '5 minutes'
  SQL
  warn = row.n > 100
}

# admins idle 90d+ should be reviewed/disabled
check "dormant_admin_accounts" {
  query = <<-SQL
    select id, email, last_login_at
    from users
    where role = 'admin'
      and status = 'active'
      and (last_login_at is null or last_login_at < now() - interval '90 days')
  SQL
  warn {
    when   = rows.count > 0
    sample = 20
  }
}

# privileged users without MFA = softest target
check "admins_without_mfa" {
  query = <<-SQL
    select id, email
    from users
    where role in ('admin', 'owner')
      and status = 'active'
      and mfa_enabled = false
  SQL
  fail {
    when   = rows.count > 0
    sample = 20
  }
}

# sudden jump in admin count = privilege escalation
check "admin_count_sane" {
  query = "select count(*) as n from users where role = 'admin' and status = 'active'"
  warn  = row.n > 15
}

# disabled account with a live session = revocation didn't propagate
check "disabled_accounts_have_no_sessions" {
  query = <<-SQL
    select s.id, s.user_id
    from sessions s
    join users u on u.id = s.user_id
    where u.status = 'disabled'
      and s.expires_at > now()
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# rotate before expiry or integrations break
check "api_tokens_expiring_soon" {
  query = <<-SQL
    select count(*) as n
    from api_tokens
    where revoked = false
      and expires_at between now() and now() + interval '7 days'
  SQL
  warn = row.n > 0
}

# piles of long-expired sessions = cleanup job is broken
check "expired_sessions_reaped" {
  query = "select count(*) as n from sessions where expires_at < now() - interval '7 days'"
  warn  = row.n > 10000
}
