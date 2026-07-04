---
title: Writing checks
description: Task-oriented recipes for common groundtruth checks — thresholds, freshness, referential integrity, tiers, samples, fan-out, and defaults.
---

Each recipe below is a self-contained pattern you can copy into your config. For the full list of check attributes and their defaults, see the [Configuration reference](/docs/configuration). To assert on individual columns instead of a single expression, see [Validating column data](/docs/data-validation).

## Fail when a table is empty

Run a count and fail when it comes back zero.

```hcl
check "orders_present" {
  on    = connection.postgres.main
  query = "select count(*) as n from orders"
  fail  = row.n == 0
}
```

The query returns one row; `row.n` reads the `n` column. If it's `0`, the check is FAIL.

## Fail when a threshold is crossed

Any boolean expression works, so compare against a number directly.

```hcl
check "pending_backlog" {
  query = "select count(*) as n from jobs where status = 'pending'"
  fail  = row.n > 1000
}
```

FAIL once more than 1000 jobs are stuck pending.

## Fail on stale data (freshness)

Compute the age of your newest row and compare it to a duration.

```hcl
check "events_fresh" {
  query = "select max(updated_at) as latest from events"
  fail  = age(row.latest) > duration("30m")
}
```

`age(ts)` returns seconds since `ts` (an RFC3339 string or an epoch number); `duration("30m")` returns seconds. This fails when the most recent row is older than 30 minutes.

## Fail on orphaned rows (referential integrity)

Count the bad rows and require zero.

```hcl
check "no_orphaned_orders" {
  query = <<-SQL
    select count(*) as n
    from order_items oi
    left join orders o on o.id = oi.order_id
    where o.id is null
  SQL
  fail = row.n != 0
}
```

Any order item pointing at a missing order makes the count non-zero and the check FAIL.

## Warn and fail on one check

Use block form to set two tiers on the same query. `fail` is evaluated first and wins; otherwise `warn`; otherwise PASS.

```hcl
check "disk_headroom" {
  query = "select free_pct as pct from storage_stats"
  warn { when = row.pct < 20 }
  fail { when = row.pct < 5 }
}
```

Below 20% free is a WARN (still counted as healthy); below 5% is a FAIL.

## Capture the rows that failed

Add `sample = N` in block form to attach up to N offending rows to the result.

```hcl
check "invalid_emails" {
  query  = "select id, email from users where email not like '%@%'"
  fail   = { when = rows.count > 0  sample = 10 }
}
```

The sampled rows print indented under the check in terminal output (`key=val key=val`) and appear in the JSON `sample` array from `gt run --json` and `/checks?format=json`.

:::note
`rows.count` is the number of returned rows and `rows.sample` holds up to the first 10 of them. `row` is the first row only.
:::

## Run one check across many items

`for_each` expands a check into one instance per list item, named `name[item]`, with `${each.value}` available in the query.

```hcl
check "table_not_empty" {
  for_each = ["orders", "users", "payments"]
  query    = "select count(*) as n from ${each.value}"
  fail     = row.n == 0
}
```

This produces `table_not_empty[orders]`, `table_not_empty[users]`, and `table_not_empty[payments]`, each checking its own table.

## Point a check at a specific connection

Set `on` to choose which connection a check runs against. Reference it as `connection.<provider>.<name>` or a bare name string.

```hcl
connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

connection "mysql" "reporting" {
  dsn = env("REPORTING_URL")
}

check "reporting_rows" {
  on    = connection.mysql.reporting
  query = "select count(*) as n from daily_rollup"
  fail  = row.n == 0
}
```

Without `on`, a check uses `defaults.on`, or the first declared connection.

## Set defaults once

Put shared values in a `defaults` block and drop them from individual checks.

```hcl
defaults {
  on      = connection.postgres.main
  every   = "5m"
  timeout = "10s"
}

check "orders_present" {
  query = "select count(*) as n from orders"
  fail  = row.n == 0
}

check "users_present" {
  query = "select count(*) as n from users"
  fail  = row.n == 0
}
```

Both checks inherit the connection, schedule, and per-query timeout. Any check that sets its own `on`, `every`, or `timeout` overrides the default. `defaults` can also set `on_fail`; see [Alerting](/docs/alerting).

:::tip[More examples]
The [`examples/` directory](https://github.com/jondot/groundtruth/tree/main/examples) has 20 real-world configs covering SaaS smoke tests, order pipelines, payment ledgers, analytics warehouses, and more.
:::
