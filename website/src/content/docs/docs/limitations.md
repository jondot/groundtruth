---
title: Limitations & scope
description: What groundtruth deliberately does not do, and the rough edges worth knowing before you rely on it.
---

groundtruth is intentionally small and single-node. This page lists what it
deliberately does not do, and the rough edges worth knowing before you rely on it.

## Deliberate scope

- **Databases it can monitor:** PostgreSQL (and Redshift over the Postgres wire),
  MySQL / MariaDB, BigQuery, Trino / Presto, and Amazon Athena.
  SQLite can be used as the state store, but not as a check target. SQL Server,
  Oracle, and DuckDB are not supported.
- **No cross-time trend detection.** The `validate` block checks a query result at a
  single point in time — column types, ranges, null rates, uniqueness, outliers, and
  distribution. It keeps no memory of previous runs, so it will not detect drift or
  trends over time. Write a SQL query that compares time windows if you need that.
- **One notifier type.** A single `webhook` notifier (works with Slack, Better Stack,
  or any generic endpoint), with sustained-failure gating, recovery notices, and
  retries. For a full alert lifecycle — deduplication, grouping, silences, escalation —
  scrape `/metrics` with Prometheus and let Alertmanager own paging.

## Known limitations

- **Numeric precision.** Numeric and decimal values are compared as 64-bit floats, so
  exact equality beyond ~15 significant digits is not reliable. This is fine for
  thresholds; for exact-cent comparisons, compare values as strings in SQL.
- **Connections load once at startup.** In `watch`, check definitions reload every
  tick, but `connection` blocks are read once — changing a connection requires a
  restart.
- **Normality test needs enough data.** `distribution = "normal"` needs at least 8
  non-null values (fewer is reported as ERROR) and is unreliable below roughly 30
  values.
- **Auth is a single static token.** `GROUNDTRUTH_TOKEN` is one bearer token read at
  startup — there is no mTLS, OAuth, or token rotation.
- **Single node.** State is kept in memory (lost on restart) or in one SQL database.
  There is no clustering, HA, or multi-node coordination.
- **Shutdown drops in-flight checks.** On Ctrl-C or SIGTERM, checks still running are
  cancelled rather than drained.
- **`sample = N` truncates.** Only the first N offending rows are captured into a
  result.
- **A down database is detected at the check timeout, not instantly.** An unreachable
  database surfaces as ERROR when the check `timeout` elapses (default 30s), not within
  a couple of seconds. Lower a check's `timeout` if you need faster detection — it
  never hangs the daemon.
