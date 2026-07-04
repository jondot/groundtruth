---
name: authoring-groundtruth-checks
description: Use when writing or editing groundtruth (gt) HCL config files — defining database health or data-quality checks for a project, translating a schema into monitoring, or fixing a config gt check rejects.
---

# Authoring groundtruth checks

## Overview

groundtruth (`gt`) is a single-binary database health monitor configured in HCL.
A config declares `connection`s and `check`s; each check is a read-only `SELECT`
plus a `warn`/`fail` condition or a `validate` block. `gt` runs them on a
schedule and alerts through a webhook.

**Core principle: never hand back HCL you have not validated.** `gt check
<file>` parses and type-checks the whole config and names every error. The
difference between plausible HCL and correct HCL is one command — always run it.

## When to use

- Asked to add database monitoring, health checks, or data-quality checks with groundtruth / `gt`.
- Writing or editing a `.hcl` file that contains `check`, `connection`, or `defaults` blocks.
- A `gt check` run is failing and you need to fix the config.

## The authoring loop

1. **Find the connection.** The check connection is a database URL, almost always
   from an env var (`env("DATABASE_URL")`). Confirm the scheme is supported
   (Postgres, MySQL, BigQuery, Trino, Redshift, Athena — see
   `hcl-reference.md`). Write the `connection` block first.

2. **Introspect the real schema — do not guess column names.** Two ways:
   - If `gt` is registered as an MCP server, use its tools: `check_connection`
     (is the DB reachable?), `describe_schema` (tables → columns, types,
     nullability), `sample_table` (real rows).
   - Otherwise use the project's own database access (e.g. `psql \d`, a
     migrations directory, or ORM models).

3. **Look at real data before choosing thresholds.** Sample rows so freshness
   windows and numeric ranges reflect reality, not round guesses.

4. **Write checks by category.** Alias every scalar you test (`select count(*) as
   n …` → `row.n`):
   - *Freshness* — `select extract(epoch from now() - max(created_at)) as age …`, `fail = row.age > duration("30m")`.
   - *Row-count floors* — `select count(*) as n …`, `fail = row.n == 0`.
   - *Referential integrity* — anti-join for orphans, `fail { when = rows.count > 0, sample = 5 }`.
   - *Domain / enum validity* — a `validate` block with `allowed = [...]`, `not_null`, `range`, `unique`.

5. **Validate, then fix, then repeat.** Run it with the config's env vars set:
   ```sh
   DATABASE_URL=... gt check checks.hcl
   ```
   `env()` is evaluated at parse time, so the vars must be present. Read each
   error — they name the exact block and key — fix, and rerun until it prints
   `OK: N check(s), …`. Only then is the config done.

## Grammar

The full, authoritative grammar — every block, attribute, and allowed value — is
in **`hcl-reference.md`** next to this file. Read it before writing; the parser
rejects any key or block not listed there. Do not invent attributes from other
monitoring tools.

## Common mistakes

These are the exact constructs an agent invents when guessing the DSL from
memory. None of them exist — the parser rejects every one.

| Mistake | Reality |
|---|---|
| `source "postgres" "main" { ... }` | The block is `connection "postgres" "main"`. |
| `url = env(...)` in a connection | The attribute is `dsn`. |
| `source = source.postgres.main` on a check | The attribute is `on` (e.g. `on = connection.postgres.main`). |
| `assert { expr = "n == 0" }` | No `assert`/`expr`. Use `fail = row.n == 0` (bare) or a `fail { when = ... }` block. |
| `freshness { column, warn, fail }` block | No such block. Freshness is a plain query + condition: `query = "... extract(epoch from now() - max(created_at)) as age ..."`, `fail = row.age > duration("30m")`. |
| `sql = "..."` | The attribute is `query`. |
| `threshold`, `severity`, `alert`, `schedule`, `pool_size` | None exist. Use `query`, `warn`/`fail`, `every`, `on_fail`. |
| `notify "slack"` / `"email"` / `"pagerduty"` | Only `notify "webhook"` exists, with only `url`. Point the webhook wherever you need. |
| `state { on = ... }` | `state` has one attribute, `dsn`. It is groundtruth's own write DB, unrelated to check connections. |
| A check with both `validate` and `warn`/`fail` | Mutually exclusive — use one. |
| `connection "sqlite"` as a check target | SQLite is not a supported check target (only a `state` store). Same for Oracle, DuckDB, MongoDB. |
| Referencing `row.total` when the query says `select sum(x)` | Alias it: `select sum(x) as total`. An unaliased or typo'd column is an ERROR at runtime, not a pass. |
| Testing an empty result via `row.n` | `row` is the *first* row and is null when there are none — use `rows.count == 0` for "no rows". |

## Red flags — stop and run `gt check`

- About to report a `.hcl` file as done without running `gt check` on it.
- Reaching for an attribute because another monitoring tool has it.
- Guessing a column name instead of introspecting the schema.

All three mean: introspect or validate before continuing. A config that has not
passed `gt check` is not finished.
