---
title: Introduction
description: A single-binary database health-check monitor that runs SQL assertions on a schedule and serves the results over HTTP.
---

groundtruth answers one question on a schedule: is your data actually correct and
fresh? You write checks as SQL plus a small assertion, and groundtruth runs them,
tracks the outcome of each one, and serves the latest results over HTTP. It is a
single binary called `gt`. There is nothing else to install and no agent to run
alongside your databases.

## The problem it removes

You already know how to ask "are orders arriving?" or "is this table stale?" — it's
a SQL query. What's missing is something to run that query on a schedule, decide
pass or fail, remember the result, and let your tooling read it. groundtruth is that
piece. You point it at a database with a connection string, describe the checks in
one config file, and run it. No monitoring stack to stand up, no per-database agent
to deploy.

## The mental model

The whole system fits on one screen:

- **Connections** — where to run: a database plus a connection string.
- **Checks** — a SQL `query` and an assertion. The assertion is a `warn` or `fail`
  expression over the result, or a `validate` block that checks columns.
- **Outcomes** — every check run ends in one of four states: **PASS**, **WARN**,
  **FAIL**, or **ERROR**.
- **Results** — groundtruth holds the latest outcome for each check. You pull the
  results over HTTP, or wire a check to a webhook so groundtruth pushes when it fails.

That's the entire loop: connect, run the query, evaluate the assertion, record the
outcome, expose it.

Here is a check in full:

```hcl
connection "postgres" "main" {
  dsn = env("DATABASE_URL")
}

check "orders_present" {
  on    = connection.postgres.main
  query = "select count(*) as n from orders"
  fail  = row.n == 0
}
```

If the `orders` table is empty, this check is FAIL. If the query itself errors — bad
column, dead connection, timeout — the check is ERROR, never a fake PASS.

## What connects to what

You connect groundtruth to the databases you want to watch. Supported check targets:

- **PostgreSQL** (and Redshift over the Postgres wire)
- **MySQL / MariaDB**
- **BigQuery**
- **Trino / Presto**
- **Amazon Athena**

groundtruth only ever reads from these databases. SQLite, Oracle, and DuckDB are not
supported as check targets.

## What makes it distinct

- **One binary, many engines.** The same `gt` and the same check syntax work across
  every supported database.
- **Fail-loud, never fake-PASS.** A broken query, a bad expression, or a down
  database becomes ERROR — it is never quietly reported as passing.
- **Config as HCL.** Checks live in a readable config file you keep in version
  control.
- **Pull-first.** The daemon holds the latest results and your tooling reads them
  over HTTP. Webhooks are available when you want a push.
- **Built for AI agents.** groundtruth speaks MCP, so an agent can list checks, run
  one, and ask why it failed.

## Next steps

- [Install](/docs/install) — get the `gt` binary.
- [Quick Start](/docs/quickstart) — from nothing to a passing check in about five
  minutes.
- [How checks work](/docs/concepts) — the check lifecycle and the four outcomes.
