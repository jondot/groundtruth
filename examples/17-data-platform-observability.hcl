# Central warehouse every team builds on. Five observability pillars as plain SQL,
# plus dbt run health and contract enforcement.
# gt run as a post-build CI gate AND gt watch for continuous freshness.

connection "postgres" "warehouse" {
  dsn = env("WAREHOUSE_URL")
}

connection "postgres" "raw" {
  dsn = env("DATABASE_URL")
}

defaults {
  on      = connection.postgres.warehouse
  every   = "15m"
  on_fail = notify.webhook.data
}

notify "webhook" "data" {
  url = env("ALERT_WEBHOOK")
}

# freshness
check "raw_sources_fresh" {
  on       = connection.postgres.raw
  for_each = ["raw_orders", "raw_clickstream", "raw_payments", "raw_crm_contacts"]
  query    = "select extract(epoch from now() - max(_ingested_at))::float8 as age from ${each.value}"
  warn     = row.age > duration("1h")
  fail     = row.age > duration("6h")
}

# what dashboards actually read
check "marts_fresh" {
  for_each = ["fct_orders", "fct_sessions", "fct_revenue", "dim_customers", "dim_products", "dim_dates"]
  query    = "select extract(epoch from now() - max(_loaded_at))::float8 as age from ${each.value}"
  warn     = row.age > duration("6h")
  fail     = row.age > duration("12h")
}

# volume
# today's load vs same weekday last week, to dodge weekly seasonality
check "fct_orders_volume_ratio" {
  query = <<-SQL
    with t as (select count(*) c from fct_orders where order_date = current_date - 1),
         p as (select count(*) c from fct_orders where order_date = current_date - 8)
    select t.c::float8 / nullif(p.c, 0) as ratio from t, p
  SQL
  warn = row.ratio < 0.5 || row.ratio > 2.0
  fail = row.ratio < 0.1 || row.ratio > 10.0
}

check "fct_sessions_not_empty" {
  query = "select count(*) as n from fct_sessions where session_date = current_date - 1"
  fail  = row.n == 0
}

# append-only fact below its contract floor = a rebuild dropped rows
check "fct_revenue_not_shrinking" {
  query = <<-SQL
    select count(*) as today, (select expected_min_rows from data_contracts where table_name = 'fct_revenue') as floor
    from fct_revenue
  SQL
  fail = row.today < row.floor
}

# distribution
check "dim_customers_distribution" {
  query = <<-SQL
    select lifetime_value, order_count, segment
    from dim_customers
    where _loaded_at > current_date - 1
  SQL
  validate {
    column "lifetime_value" {
      type      = "float"
      null_rate = 0.01
      range     = { min = 0, max = 10000000 }
      outliers  = "iqr"
    }
    column "order_count" {
      type  = "int"
      range = { min = 0, max = 100000 }
    }
    column "segment" {
      allowed = ["smb", "mid_market", "enterprise", "unknown"]
    }
  }
}

# normality test flags a shape shift in conversion rate (needs >= 8 days)
check "daily_conversion_distribution" {
  query = <<-SQL
    select conversion_rate
    from daily_metrics
    where metric_date > current_date - 30
  SQL
  validate {
    column "conversion_rate" {
      type         = "float"
      range        = { min = 0, max = 1 }
      distribution = "normal"
    }
  }
}

# negative or absurd amounts skew every downstream rollup
check "order_amounts_distribution" {
  query = <<-SQL
    select amount
    from fct_orders
    where order_date = current_date - 1
  SQL
  validate {
    column "amount" {
      type     = "float"
      not_null = true
      range    = { min = 0, max = 1000000 }
      outliers = "zscore"
    }
  }
}

# schema
# count drift = either a migration (bump the number) or an upstream break
check "fct_orders_column_count_stable" {
  query = <<-SQL
    select count(*) as n
    from information_schema.columns
    where table_name = 'fct_orders'
  SQL
  fail = row.n != 14
}

# a renamed/dropped column breaks downstream BI
check "fct_orders_required_columns_present" {
  query = <<-SQL
    select count(*) as present
    from information_schema.columns
    where table_name = 'fct_orders'
      and column_name in ('order_sk','customer_sk','order_date','amount','status')
  SQL
  fail = row.present != 5
}

# catch a silent numeric -> text type change on amount
check "amount_column_is_numeric" {
  query = <<-SQL
    select count(*) as n
    from information_schema.columns
    where table_name = 'fct_orders' and column_name = 'amount'
      and data_type in ('numeric','double precision','real')
  SQL
  fail = row.n != 1
}

# lineage / integrity
check "fct_orders_surrogate_key_unique" {
  query = <<-SQL
    select order_sk, count(*) n from fct_orders
    group by order_sk having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "fct_orders_customer_fk" {
  query = <<-SQL
    select f.order_sk from fct_orders f
    left join dim_customers d on d.customer_sk = f.customer_sk
    where d.customer_sk is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "fct_orders_date_fk" {
  query = <<-SQL
    select f.order_sk from fct_orders f
    left join dim_dates d on d.date_key = f.order_date
    where d.date_key is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "no_future_dated_facts" {
  query = "select count(*) as n from fct_orders where order_date > current_date"
  fail  = row.n > 0
}

# loaded today but dated >3 days back = backfill or stuck upstream partition
check "late_arriving_facts" {
  query = <<-SQL
    select count(*) as n from fct_orders
    where _loaded_at > current_date and order_date < current_date - 3
  SQL
  warn = row.n > 1000
}

# dbt run health
check "latest_dbt_run_passed" {
  query = "select status from dbt_run_results order by run_started_at desc limit 1"
  fail  = row.status != "success"
}

check "dbt_run_fresh" {
  query = "select extract(epoch from now() - max(run_started_at))::float8 as age from dbt_run_results"
  fail  = row.age > duration("6h")
}

check "no_dbt_test_failures" {
  query = <<-SQL
    select count(*) as n from dbt_test_results
    where run_id = (select run_id from dbt_run_results order by run_started_at desc limit 1)
      and status = 'fail'
  SQL
  fail = row.n > 0
}

# reverse-etl / downstream contracts
# stale CRM sync means sales sees old data
check "reverse_etl_sync_fresh" {
  query = "select extract(epoch from now() - max(synced_at))::float8 as age from sync_state where destination = 'salesforce'"
  warn  = row.age > duration("2h")
  fail  = row.age > duration("12h")
}

# SCD2 dims must have exactly one current row per key
check "scd2_single_current_row" {
  query = <<-SQL
    select customer_id, count(*) as n from dim_customers_history
    where is_current = true
    group by customer_id having count(*) > 1
  SQL
  fail {
    when   = rows.count > 0
    sample = 10
  }
}

check "warehouse_reachable" {
  query = "select 1 as ok"
  fail  = row.ok != 1
}
