---
title: How checks work
description: The check lifecycle, the four outcomes, the fail-loud principle, and why groundtruth is pull-first.
---

A check is a SQL query plus a decision. This page explains how groundtruth turns a
query result into one of four outcomes, why it refuses to fake a PASS, and how it
stays up when a database goes down. For the exact attributes and rules, see the
[Configuration reference](/docs/configuration).

## The check lifecycle

Every run of a check follows the same path:

1. **The query runs** against the check's connection, bounded by its timeout.
2. **The result becomes an expression context.** The rows are exposed as `row` (the
   first row, columns as `row.<col>`) and `rows` (`rows.count` and `rows.sample`, the
   first few rows). Along with the rows you get helper functions.
3. **The assertion evaluates.** Either the `warn` / `fail` expressions run against
   that context, or — if the check uses a `validate` block instead — the column rules
   run.
4. **The outcome is recorded** as PASS, WARN, FAIL, or ERROR, along with a detail
   string and any captured sample rows.

A check uses expressions or a `validate` block, never both — they are mutually
exclusive on the same check.

### The expression context

Inside a `warn` or `fail` expression you have:

- `row` — the first result row. Read a column as `row.n`, `row.status`, and so on.
- `rows` — `rows.count` is the number of rows returned; `rows.sample` is the first
  handful of rows.
- `duration("30m")` — converts a duration string to seconds.
- `age(ts)` — seconds since `ts`, where `ts` is an RFC3339 string or an epoch number.
  Use it for freshness checks against a `max(updated_at)` column.
- `env("NAME")` — reads an environment variable as a string; errors if it is unset.

So a freshness check reads naturally:

```hcl
check "orders_fresh" {
  query = "select max(created_at) as last from orders"
  fail  = age(row.last) > duration("15m")
}
```

## The two tiers and precedence

A check can define two tiers of assertion, `warn` and `fail`. Both are booleans over
the same context. When a check runs, groundtruth evaluates them in a fixed order:

- **`fail` is evaluated first and wins.** If it's true, the outcome is FAIL.
- **Otherwise `warn`.** If it's true, the outcome is WARN.
- **Otherwise PASS.**

So `fail` always takes precedence over `warn`. A check with only a `fail` tier is the
common case; add `warn` when you want an early, non-blocking signal before the
condition becomes serious.

## The four outcomes

Every check run ends in exactly one of these:

| Outcome | What it means | Counts as |
|---|---|---|
| **PASS** | No tier fired, or all validators passed. | up (healthy) |
| **WARN** | The `warn` tier fired and `fail` did not. | up (healthy) |
| **FAIL** | The `fail` tier fired, or a validator was violated. | down |
| **ERROR** | Something prevented an honest answer. | down |

WARN counts as healthy: it is a heads-up, not an outage. FAIL and ERROR both count as
down.

## Fail-loud: why ERROR exists

groundtruth never reports a check as PASS when it could not honestly evaluate it. When
it can't, the outcome is ERROR — a loud, distinct state — not a silent pass. A check
becomes ERROR when:

- the `when` expression isn't a boolean, references a missing column or typo, panics,
  or divides by zero;
- the query isn't a string;
- the database or query returns an error;
- the query times out;
- the connection is unknown or dead;
- an Athena query is reported as FAILED or CANCELLED.

The reason this matters: a monitor that silently passes on error is worse than no
monitor. ERROR tells you the check couldn't answer, which is itself something to fix.

## Resilience

Checks are designed to keep running even when things go wrong around them:

- **A down database is reported, not a crash.** Its checks become ERROR and the
  daemon stays up. This happens after the check timeout elapses (default 30s), not
  instantly.
- **A hung query is bounded by its timeout.** No single query can hang a check
  forever.
- **One broken check can't take down the others.** Checks run concurrently and in
  isolation; a failing or panicking check affects only its own result.
- **A failed connection is never fatal.** The checks on that connection become ERROR
  while every other check keeps running normally.

## Pull-first

groundtruth is pull-first. In watch mode the daemon runs checks on their schedule and
holds the latest outcome for each one. Your tooling reads those results when it wants
them — over the HTTP endpoints and the Prometheus metrics. You are never required to
stand up a receiver or ingest a stream; the daemon simply keeps the current answer
ready to be pulled.

Push is available when you want it: wire a check to a webhook and groundtruth will
notify you when it fails. But the default posture is that the source of truth lives in
the daemon and you come and get it.

## Next

- [Writing checks](/docs/writing-checks) — put tiers, schedules, and expressions to
  work.
- [Validating column data](/docs/data-validation) — the `validate` block and its
  rules.
- [Configuration reference](/docs/configuration) — every attribute and default.
