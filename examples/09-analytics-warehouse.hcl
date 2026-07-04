# dbt warehouse observability: freshness, volume, distribution, schema as SQL checks
# `gt run` gates each dbt build in CI; `gt watch` covers freshness between builds

connection "postgres" "warehouse" {
  dsn = env("WAREHOUSE_URL")
}

defaults {
  on    = connection.postgres.warehouse
  every = "15m"
}

# marts build every 4h, so 6h stale = a missed build
check "mart_freshness" {
  for_each = ["fct_orders", "fct_sessions", "dim_customers", "dim_products"]
  query    = "select extract(epoch from now() - max(_loaded_at))::float8 as age from ${each.value}"
  warn     = row.age > duration("6h")
  fail     = row.age > duration("12h")
}

# day-over-day swing; 10x/0.1x is almost always a broken join or dup source
check "fct_orders_volume_stable" {
  query = <<-SQL
    with today as (
      select count(*) as c from fct_orders where order_date = current_date - 1
    ),
    prior as (
      select count(*) as c from fct_orders where order_date = current_date - 8
    )
    select today.c::float8 / nullif(prior.c, 0) as ratio
    from today, prior
  SQL
  warn = row.ratio < 0.5 || row.ratio > 2.0
  fail = row.ratio < 0.1 || row.ratio > 10.0
}

# zero rows for a day = silent pipeline break
check "daily_facts_loaded" {
  query = "select count(*) as n from fct_orders where order_date = current_date - 1"
  fail  = row.n == 0
}

# null spikes, bad enums, malformed emails, revenue outliers in one pass
check "dim_customers_quality" {
  query = <<-SQL
    select email, lifetime_value, segment, country
    from dim_customers
    where _loaded_at > current_date - 1
  SQL
  validate {
    column "email" {
      matches   = "^[^@]+@[^@]+\\.[a-zA-Z]{2,}$"
      null_rate = 0.02 # tolerate up to 2% missing emails
    }
    column "lifetime_value" {
      type     = "float"
      range    = { min = 0, max = 1000000 }
      outliers = "zscore" # flag |z| > 3 spend values for review
    }
    column "segment" {
      allowed = ["smb", "mid_market", "enterprise", "unknown"]
    }
    column "country" {
      not_null = true
    }
  }
}

# dup surrogate key fans out every downstream join
check "fct_orders_surrogate_key_unique" {
  query = <<-SQL
    select order_sk, count(*) as n
    from fct_orders
    group by order_sk
    having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "fct_orders_customer_fk" {
  query = <<-SQL
    select f.order_sk, f.customer_sk
    from fct_orders f
    left join dim_customers d on d.customer_sk = f.customer_sk
    where d.customer_sk is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

# future-dated facts = timezone or parsing bug
check "no_future_dated_orders" {
  query = "select count(*) as n from fct_orders where order_date > current_date"
  fail  = row.n > 0
}

check "latest_dbt_run_passed" {
  query = <<-SQL
    select status
    from dbt_run_results
    order by run_started_at desc
    limit 1
  SQL
  fail = row.status != "success"
}

# stalled scheduler hides every other check
check "dbt_run_is_fresh" {
  query = "select extract(epoch from now() - max(run_started_at))::float8 as age from dbt_run_results"
  fail  = row.age > duration("6h")
}

check "raw_events_landing_fresh" {
  query = "select extract(epoch from now() - max(received_at))::float8 as age from raw_events"
  warn  = row.age > duration("30m")
  fail  = row.age > duration("3h")
}
