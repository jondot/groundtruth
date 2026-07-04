# groundtruth HCL reference

The complete, authoritative grammar for groundtruth (`gt`) config files. Every
attribute below is enforced by the parser — an unknown key or block is a hard
error, not a silent ignore. If it is not listed here, it does not exist.

## Top-level blocks

A config file may contain these blocks, in any order:

| Block | Repeats? | Purpose |
|---|---|---|
| `connection "<provider>" "<name>"` | yes | A database to run checks against (read-only). |
| `defaults` | once | Fallback `on`/`every`/`timeout`/`on_fail` for checks that omit them. |
| `notify "webhook" "<name>"` | yes | A webhook to POST to when a check fails. |
| `state` | once | groundtruth's own bookkeeping database (for sustained-failure tracking). |
| `check "<name>"` | yes | One monitored query plus its pass/fail conditions. |

## `connection`

Two shapes. For every engine except Athena, the only attribute is `dsn`, and the
**URL scheme selects the engine** — the `"<provider>"` label is cosmetic.

```hcl
connection "postgres" "main" {
  dsn = env("DATABASE_URL")          # env() is read when the config is parsed
}
```

Supported `dsn` schemes: `postgres://`, `postgresql://`, `mysql://`,
`bigquery://`, `trino+http://`. Redshift uses the `postgres://` scheme. Any other
scheme is rejected at `gt check`.

Athena is the one exception — it takes AWS coordinates, not a `dsn`:

```hcl
connection "athena" "lake" {
  region          = "us-east-1"
  database        = "default"
  output_location = "s3://bucket/results/"   # optional if the workgroup sets one
  workgroup       = "primary"                # optional
}
```

## `defaults`

Applied to any check that omits the same attribute. All four are optional.

```hcl
defaults {
  on      = connection.postgres.main   # or a bare string: "main"
  every   = "1m"
  timeout = "10s"
  on_fail = notify.webhook.alerts      # or a bare string: "alerts"
}
```

## `notify`

Only `webhook` exists. Only `url` is allowed. There is no Slack, email, or
PagerDuty notifier — point the webhook at whatever receives it.

```hcl
notify "webhook" "alerts" {
  url = env("ALERT_WEBHOOK")
}
```

## `state`

Optional. The **only** database groundtruth writes to. Its single attribute is
`dsn` (a `postgres://…` or `sqlite:…` URL). Absent → failure streaks are tracked
in memory only. There is **no `on` attribute** on `state`.

```hcl
state {
  dsn = env("DATABASE_URL")
}
```

## `check`

The core block. Attributes:

| Attribute | Type | Notes |
|---|---|---|
| `query` | string | **Required.** A read-only `SELECT`. May contain `${each.value}` when `for_each` is set. |
| `on` | connection ref | Which connection to query. Omit → `defaults.on` → first connection. |
| `every` | duration string | Schedule, e.g. `"30s"`, `"5m"`, `"1h"`, `"1d"`. Omit → `defaults.every` → 60s. |
| `timeout` | duration string | Per-query timeout. Omit → `defaults.timeout` → 30s. |
| `on_fail` | notifier ref | Which notifier to alert. Omit → `defaults.on_fail`. |
| `for_each` | list of strings | Expands this check once per item (see below). |

Sub-blocks: `warn`, `fail`, `validate`.

### `warn` / `fail` tiers

`warn` and `fail` each accept **two equivalent forms**. Use the bare form for a
simple condition; use the block form when you need `sustained` or `sample`.

Bare form — the value is a boolean expression; **true means the tier fires**:

```hcl
check "orders_fresh" {
  query = "select extract(epoch from now() - max(created_at)) as age from orders"
  warn  = row.age > duration("10m")
  fail  = row.age > duration("30m")
}
```

Block form:

```hcl
fail {
  when      = rows.count > 0    # required: the boolean condition
  sustained = "15m"             # optional: must hold this long before firing
  sample    = 5                 # optional: capture up to N failing rows in the alert
}
```

You may set `warn`, `fail`, or both. A tier cannot be both a bare attribute and a
block at the same time.

### `validate`

A declarative alternative to `warn`/`fail` for column-level data quality.
**Mutually exclusive with `warn`/`fail`** — a check has one or the other, never
both. Contains one `column "<name>"` block per column in the query result.

```hcl
check "orders_quality" {
  query = "select status, amount_cents from orders limit 1000"
  validate {
    column "status" {
      not_null = true
      allowed  = ["pending", "paid", "shipped", "cancelled"]
    }
    column "amount_cents" {
      type     = "int"
      range    = { min = 0, max = 100000000 }
      outliers = "iqr"
    }
  }
}
```

Column rule attributes (all optional):

| Attribute | Type | Meaning |
|---|---|---|
| `type` | string | One of `int`, `float`, `string`, `bool`, `timestamp`. |
| `not_null` | bool | Every value must be non-null. |
| `null_rate` | number | `null_count / total` must be ≤ this (0.0–1.0). |
| `allowed` | list of strings | Every non-null value must be in this set. |
| `matches` | string | Regex every non-null string value must match. |
| `range` | object | `{ min = …, max = … }`; numeric values must be within (inclusive). |
| `unique` | bool | All non-null values must be distinct. |
| `outliers` | string | `iqr` or `zscore` — flag statistical outliers. |
| `distribution` | string | `normal` — Jarque-Bera normality test. |

### `for_each`

Expands one check definition into several, one per list item. The current item is
available as `${each.value}` **inside `query` only** (not in `warn`/`fail`). The
expanded checks are named `<name>[<item>]`.

```hcl
check "rows_present" {
  for_each = ["orders", "payments", "customers"]
  query    = "select count(*) as n from ${each.value}"
  fail     = row.n == 0
}
```

## Expression vocabulary (`warn` / `fail` / `when`)

Conditions are HCL expressions evaluated against the query result. Available
names:

| Name | What it is |
|---|---|
| `row` | The **first** result row; access columns as `row.<column_alias>`. |
| `rows.count` | Number of rows returned (integer). |
| `rows.sample` | Array of up to the first 10 rows. |

Alias every scalar you want to test (`select count(*) as n …` → `row.n`). A typo'd
column or a non-boolean condition is reported as **ERROR**, never a silent pass.

Functions available in any expression (including `query`, `dsn`, `url`, `every`):

| Function | Returns |
|---|---|
| `duration("30m")` | Seconds as a number. Units: `s`, `m`, `h`, `d`. |
| `age(ts)` | Seconds between now and `ts` (an RFC-3339 string or a unix-epoch number). |
| `env("NAME")` | The environment variable's value; **errors if unset**, evaluated at parse time. |

## Supported databases

**Checks run against:** PostgreSQL, MySQL, BigQuery, Trino,
Redshift (via the Postgres scheme), and Amazon Athena.

**Not supported as check targets:** SQLite, Oracle, DuckDB, MongoDB, or anything
whose scheme isn't listed above. SQLite is usable *only* as the `state` store.

## Two common values that are NOT groundtruth grammar

- There is no `sql`, `assert`, `threshold`, `severity`, `alert`, `schedule`, or
  `pool_size` attribute. Use `query`, `warn`/`fail`, `every`, `on_fail`.
- `state` has no `on`. Connections use `dsn` and nothing else.
