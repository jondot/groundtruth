---
title: Configuration reference
description: Every config block, attribute, expression, and validator groundtruth accepts.
---

groundtruth reads a single config file. This page documents every block and attribute it accepts.

The config is fail-loud: any unknown attribute or block is a hard error. If groundtruth doesn't recognize something, it stops rather than silently ignoring it.

## A complete example

```hcl
# Where to run checks. The scheme in the dsn selects the engine.
connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

# Values inherited by any check that doesn't set its own.
defaults {
  on      = connection.postgres.main
  every   = "60s"
  timeout = "30s"
  on_fail = "ops"
}

# A check with a boolean tier.
check "orders_present" {
  query = "select count(*) as n from orders"
  fail  = row.n == 0
}

# A check with block-form tiers and per-item expansion.
check "recent_events" {
  for_each = ["us", "eu", "ap"]
  query    = "select max(created_at) as last from events where region = '${each.value}'"
  fail {
    when      = age(row.last) > duration("15m")
    sustained = "10m"
    sample    = 5
  }
}

# Column-level validation (mutually exclusive with warn/fail).
check "users_quality" {
  query = "select email, age from users"
  validate {
    column "email" { not_null = true }
    column "age"   { range = { min = 0, max = 130 } }
  }
}

# Where to send alerts.
notify "webhook" "ops" {
  url = env("SLACK_WEBHOOK_URL")
}

# Persist bookkeeping so timers survive a restart.
state {
  dsn = "postgres://gt:gt@localhost/groundtruth"
}
```

## `connection`

Declares a database to run checks against. Two labels: the provider and an instance name.

```hcl
connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}
```

For every engine except Athena, the only attribute is `dsn`. The provider label is cosmetic — the **scheme** in the dsn selects the engine.

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `dsn` | yes | string | — | Connection string. `env(...)` allowed. |

Amazon Athena uses a different shape (provider label `"athena"`, no `dsn`):

```hcl
connection "athena" "warehouse" {
  region          = "us-east-1"
  database        = "analytics"
  output_location = "s3://my-athena-results/"
  workgroup       = "primary"
}
```

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `region` | yes | string | — | AWS region. |
| `database` | yes | string | — | Athena database name. |
| `output_location` | no | string | — | S3 URI for query results. |
| `workgroup` | no | string | — | Athena workgroup. |

Athena resolves AWS credentials through the standard AWS chain — environment variables, shared profile, SSO, or an instance/container role. There is nothing extra to configure in groundtruth.

### Supported engines

| Engine | dsn scheme |
|---|---|
| PostgreSQL (also Redshift over the Postgres wire) | `postgres://` |
| MySQL / MariaDB | `mysql://` |
| BigQuery | `bigquery://` |
| Trino / Presto | `trino+http://` |
| Amazon Athena | (no dsn — see above) |

**Not supported as check targets:** SQLite (usable only as a state store), Oracle, and DuckDB. An unrecognized scheme fails at `gt check`.

## `defaults`

Optional. Sets values that any check inherits when it doesn't specify its own.

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `on` | no | connection | — | Default connection for checks. |
| `every` | no | string | — | Default schedule. |
| `timeout` | no | string | — | Default per-check query timeout. |
| `on_fail` | no | string | — | Default notifier name. |

## `check`

Defines one health check. One label: the check name.

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `query` | yes | string | — | SQL to run. Supports `${each.value}` in `for_each` checks. |
| `on` | no | connection | `defaults.on`, else the first declared connection | Which connection to run against. `connection.<provider>.<name>` or a bare name string. |
| `every` | no | string | `defaults.every`, else `60s` in watch mode | Schedule. Interval string or 5-field cron. |
| `timeout` | no | string | `defaults.timeout`, else `30s` | Per-query timeout, e.g. `"10s"`. |
| `for_each` | no | list of strings | — | Expands into one check per item. |
| `on_fail` | no | string | `defaults.on_fail` | Notifier to fire when the check fails. |
| `warn` | no | bool or block | — | Warn tier. |
| `fail` | no | bool or block | — | Fail tier. |

Allowed sub-blocks: `warn`, `fail`, and `validate`.

### Schedules (`every`)

`every` accepts either:

- An **interval string**: `Ns`, `Nm`, `Nh`, or `Nd` (seconds, minutes, hours, days). No milliseconds, no fractions.
- A **5-field cron expression**, interpreted in UTC.

A check that has never run is due immediately at startup.

### `for_each`

`for_each` takes a list of strings and expands into one check per item, named `name[item]`. Inside the query, `${each.value}` interpolates the current item.

```hcl
check "region_health" {
  for_each = ["us", "eu"]
  query    = "select count(*) as n from sessions where region = '${each.value}'"
  fail     = row.n == 0
}
```

### Tiers: `warn` and `fail`

Each tier has two forms.

**Bare boolean:**

```hcl
fail = row.n == 0
```

**Block form:**

```hcl
fail {
  when      = row.n == 0
  sustained = "15m"
  sample    = 10
}
```

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `when` | yes (in block form) | boolean expr | — | The condition that fires the tier. |
| `sustained` | no | string | — | Only notify after the check has been failing continuously for this long. Read from the `fail` tier. |
| `sample` | no | non-negative int | — | Capture up to N offending rows into the result. |

Defining the same tier as both an attribute and a block is an error. `fail` is evaluated first and wins; otherwise `warn`; otherwise the check passes.

### Expression context

Inside `when` (and bare boolean tiers) you have:

| Name | Meaning |
|---|---|
| `row` | The first result row. Access columns as `row.<col>`. |
| `rows.count` | Number of rows returned. |
| `rows.sample` | Up to the first 10 rows. |
| `duration("30m")` | Converts a duration string to seconds. |
| `age(ts)` | Seconds since `ts` (an RFC3339 string or an epoch number). |
| `env("NAME")` | The value of an environment variable. Errors if the variable is unset. |

### `validate`

`validate` declares per-column rules. It is **mutually exclusive** with `warn`/`fail` on the same check.

```hcl
validate {
  column "age" {
    type  = "int"
    range = { min = 0, max = 130 }
  }
}
```

| Rule | Value | Meaning |
|---|---|---|
| `type` | `int` \| `float` \| `string` \| `bool` \| `timestamp` | Non-null values must be this type. `timestamp` accepts RFC3339, `YYYY-MM-DDTHH:MM:SS`, or `YYYY-MM-DD HH:MM:SS`. |
| `not_null` | `true` | Fail if any NULL is present. |
| `null_rate` | number (0–1) | Fail if the fraction of NULLs exceeds this. |
| `allowed` | list of strings | Non-null values must be in this set. |
| `matches` | regex string | Non-null string values must match. |
| `range` | `{ min = .., max = .. }` | Non-null numeric values in `[min, max]` inclusive; non-numeric fails. `min > max` is a config error. |
| `unique` | `true` | Non-null values must be unique. |
| `outliers` | `"iqr"` \| `"zscore"` | Flag outliers. `iqr` needs at least 4 values; `zscore` flags \|z\| > 3 and needs at least 3 values. |
| `distribution` | `"normal"` | Normality test; needs at least 8 non-null values; a violation is reported when p < 0.05. |

Rules skip NULLs unless noted. Any violation makes the check FAIL. A column missing from the result, or a rule that can't evaluate, makes it ERROR. Up to 10 offending rows are attached. An empty result passes. See [Validating column data](/docs/data-validation) for prose and examples.

## `notify "webhook"`

Declares a webhook alert target. Two labels: the type and an instance name. The only supported type is `webhook`.

```hcl
notify "webhook" "ops" {
  url = env("SLACK_WEBHOOK_URL")
}
```

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `url` | yes | string | — | Endpoint to POST to. `env(...)` allowed. |

Any other type or attribute is an error. Wire a check to a notifier with `on_fail = "ops"`. See [Alerting](/docs/alerting) for payload and behavior.

## `state`

Optional. Persists groundtruth's own bookkeeping so failure timers survive a restart.

```hcl
state {
  dsn = "postgres://gt:gt@localhost/groundtruth"
}
```

| Attribute | Required | Type | Default | Meaning |
|---|---|---|---|---|
| `dsn` | yes | string | — | Connection string for the state store. `env(...)` allowed. |

`state` has no sub-blocks and no `on` attribute — only `dsn`. When absent, state is kept in memory and lost on restart. This is the only database groundtruth writes to; the databases you monitor are only read from. See [Securing & persisting](/docs/securing-and-persisting) for scheme routing and production guidance.

## Notes

:::caution[`baseline` is no longer supported]
The `baseline` block has been removed. A config that still uses it is a hard error.
:::

Reading `env()`: `env("NAME")` reads whatever variable you name. There is no special state variable — if a config uses `state { dsn = env("GROUNDTRUTH_STATE_DSN") }`, that works only because you chose that name and set it. See the [HTTP API & metrics reference](/docs/http-api) for the environment variables groundtruth reads directly.
