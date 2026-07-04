# groundtruth

[![CI](https://github.com/jondot/groundtruth/actions/workflows/ci.yml/badge.svg)](https://github.com/jondot/groundtruth/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

A single-binary, run-and-forget **database monitor**. You write checks (SQL + an
assertion) in HCL; groundtruth runs them — once or on a schedule — and reports each as
**pass / warn / fail / error**, with the offending rows attached. Declarative data
validation, bearer-token auth, pluggable state store, Prometheus metrics, and an MCP
server for AI agents are built in. No Python, no agents to install, no warehouse.

```
  [PASS] orders_present                   1 row(s)
  [FAIL] no_orphaned_line_items           3 row(s)
      id=3 order_id=999
      id=4 order_id=998
      id=5 order_id=997
  [WARN] table_not_empty[orders]          1 row(s)
  [PASS] table_not_empty[line_items]      1 row(s)
```

## Install

**Shell script (macOS and Linux):**

```sh
curl -fsSL https://raw.githubusercontent.com/jondot/groundtruth/main/install.sh | sh
```

Or pin a specific version:

```sh
curl -fsSL https://raw.githubusercontent.com/jondot/groundtruth/main/install.sh | sh -s -- --version v0.2.0
```

**Docker:**

```sh
docker run ghcr.io/jondot/groundtruth:latest --help

# Run a one-shot check:
docker run --rm \
  -e DATABASE_URL=postgres://... \
  -v "$PWD/groundtruth.hcl:/etc/groundtruth/groundtruth.hcl:ro" \
  ghcr.io/jondot/groundtruth:latest run /etc/groundtruth/groundtruth.hcl
```

**npm** (installs the `gt` binary for your platform):

```sh
npm install -g @jondot/groundtruth
```

**crates.io:**

```sh
cargo install groundtruth
```

**From source:**

```sh
cargo install --git https://github.com/jondot/groundtruth
```

## Quick start

```sh
cargo build --release
./target/release/gt check  config.hcl     # validate config (loud on typos)
./target/release/gt run    config.hcl     # run once; exit non-zero on fail/error
./target/release/gt run    config.hcl --json
./target/release/gt watch  config.hcl --addr 127.0.0.1:9090   # daemon
./target/release/gt mcp    config.hcl     # MCP server over stdio (for AI agents)
```

### Pull endpoints (in `watch`)
groundtruth is **pull-first**: the daemon holds the latest results and exposes them; your
existing tooling pulls on its own schedule and owns delivery reliability.

| Endpoint | For |
|---|---|
| `GET /healthz` | liveness (always open, even when auth is enabled) |
| `GET /metrics` | Prometheus scrape (status/up gauges) |
| `GET /checks` | **200** if none failing, **503** if any fail/error — drop-in for k8s/LB probes & uptime monitors (Better Stack, Pingdom, UptimeRobot) |
| `GET /checks/{name}` | per-check health code (200/503/404) — one uptime monitor per critical check |

`/checks` supports `?format=json|yaml|text`, `?status=fail`, `?limit=N`. The status
code reflects the returned set, so `curl /checks/orders_present` *is* a health check.
Sustained-failure state uses an in-process Memory backend by default; configure a
`state { dsn = "postgres://…" }` block to persist it to groundtruth's own bookkeeping DB.

## The language (HCL)

```hcl
connection "postgres" "main" {
  dsn = env("DATABASE_URL")     # env() resolves at load; missing var = loud error
}
connection "trino" "lake" {
  dsn = "trino+http://user@trino:8080/hive"   # scheme selects the engine
}

defaults {
  on    = connection.postgres.main
  every = "5m"                  # interval ("30s","5m","2h","1d") OR cron ("0 9 * * *")
}

# Optional: persist sustained-failure state to groundtruth's own writable DB
# (Postgres or SQLite — separate from the read-only connections you monitor)
state {
  dsn = env("GROUNDTRUTH_STATE_DSN")
}

# One universal webhook — its payload carries `text` (renders in Slack) plus
# structured {check,status,detail} (maps in Better Stack & generic consumers).
notify "webhook" "oncall" {
  url = env("ALERT_WEBHOOK")
}

# Liveness — page only after the failure is SUSTAINED
check "orders_are_flowing" {
  query = "select count(*) as recent from orders where created_at > now() - interval '5 minutes'"
  fail {
    when      = row.recent == 0      # NOTE: block attributes are newline-separated (no commas)
    sustained = "15m"
  }
  on_fail = notify.webhook.oncall
}

# Data sanity — attach the offending rows to the report
check "no_orphaned_line_items" {
  query = <<-SQL
    select li.id, li.order_id from line_items li
    left join orders o on o.id = li.order_id
    where o.id is null
  SQL
  fail {
    when   = rows.count > 0
    sample = 5
  }
}

# Declarative data validation (TFDV-style) — mutually exclusive with warn/fail
check "users_data_quality" {
  query = "select email, age, status from users"
  validate {
    column "email" {
      not_null = true
      matches  = "^[^@]+@[^@]+$"
    }
    column "age" {
      type  = "int"
      range = { min = 0, max = 130 }
    }
    column "status" {
      allowed = ["active", "inactive", "pending"]
    }
  }
}

# Fan-out — one block, N checks
check "table_not_empty" {
  for_each = ["orders", "payments", "shipments"]
  query    = "select count(*) as n from ${each.value}"
  warn     = row.n < 1
}
```

**Eval context** inside `when`: `row` (first row's columns), `rows` (`.count`,
`.sample`), `each.value`.
**Functions**: `duration("30m")`, `age(ts)`, `env("VAR")`.

### Fail loud, never silently
A monitor that silently does nothing is the worst bug. So:
- A **typo'd attribute** (`fial = …`) or unknown block is a hard config error, not a silently-dropped check.
- A `when` that can't evaluate (typo'd column, type error, even division by zero) is **ERROR**, never a fake PASS — and a poison expression can't crash the daemon (panics are caught).
- An **unhandled SQL type** errors loudly naming the column, instead of silently becoming `null`.
- A `baseline {}` block is now a **hard config error** — the anomaly feature has been removed.

## Features

| Area | What's there |
|---|---|
| Engines | PostgreSQL (+Redshift), MySQL/MariaDB, BigQuery, Trino, and Amazon Athena. (SQLite is state-store-only; SQL Server/Oracle/DuckDB not supported.) |
| Checks | threshold (`warn`/`fail`), failing-row `sample`, `for_each`, `defaults`, `on` routing |
| Data validation | declarative `validate` block: `type`, `not_null`, `null_rate`, `allowed`, `matches` (regex), `range`, `unique`, `outliers` (iqr/zscore), `distribution` (normal/Jarque-Bera) |
| Scheduling | interval or cron, per check; `sustained` gating so flaps don't page |
| Pull | `/metrics`, `/checks` (json/yaml/text, health-code semantics, filters) — the recommended integration path |
| Security | bearer-token auth via `GROUNDTRUTH_TOKEN`; constant-time comparison; `/healthz` always open |
| State | pluggable: in memory (default) or a SQL database via `state { dsn = "postgres://…" or "sqlite:…" }` (its own separate DB) |
| Delivery | one universal `webhook` (fits Slack/Better Stack/generic) with sustained-gating, recovery, and retry |
| Output | terminal (with samples), `--json`, Prometheus `/metrics` |
| AI-first | HCL (models know it) + structured JSON + MCP server (`list_checks`, `run_check`, `explain_failure`) via official **rmcp** SDK |
| Footprint | one static binary, ~25 MB, no runtime deps |

## Deploy (run and forget)

**Free, scheduled, no server.** `gt init` scaffolds a config and a GitHub Actions
workflow that runs your checks every 15 minutes and pings a cron-monitor (Better Stack,
healthchecks.io, …) via a [`heartbeat`](https://getgt.vercel.app/docs/alerting) block:

```sh
gt init                     # writes groundtruth.hcl + .github/workflows/groundtruth.yml
# add repo secrets DATABASE_URL and HEARTBEAT_URL, then push — that's it.
```

Green on success, a failure report on FAIL/ERROR, and a page if a run never happens.
Free on public repos. See the [deploy guide](https://getgt.vercel.app/docs/deploy). For
`/metrics` and `/checks`, run the daemon instead:

One binary, no agent, no runtime. Resilient by design: a **down database** surfaces
its checks as ERROR (and `/checks` → 503) instead of crashing the monitor, and a
**hung query** is bounded by a timeout (default 30s, override per check with
`timeout = "5s"`).

```sh
docker compose up -d        # see docker-compose.yml — mount your groundtruth.hcl, set DATABASE_URL
# or directly:
docker build -t groundtruth .
docker run -d -p 9090:9090 \
  -e DATABASE_URL=postgres://... \
  -e GROUNDTRUTH_TOKEN=mysecrettoken \
  -v "$PWD/groundtruth.hcl:/etc/groundtruth/groundtruth.hcl:ro" \
  groundtruth watch /etc/groundtruth/groundtruth.hcl --addr 0.0.0.0:9090
```

Sustained-failure state defaults to in-process memory. To persist it across restarts,
add `state { dsn = env("GROUNDTRUTH_STATE_DSN") }` to your config. Point an uptime monitor /
k8s probe at `/checks` and a scraper at `/metrics`.

## Architecture
HCL is both the config language and the expression evaluator (`hcl-rs`). Check queries
run read-only through `connectorx` (Athena goes through the AWS SDK); groundtruth's own
state store is the only writable database, backed by `sqlx`. Query rows become HCL values
injected as `row` / `rows`, which `when` expressions evaluate against. Statistical
validation (outliers, normality) runs natively on the result set. Known limitations and
deliberate scope calls are in the [docs](https://getgt.vercel.app/docs/limitations).

## Security

The `watch` HTTP endpoints can be protected with a bearer token. Set the
`GROUNDTRUTH_TOKEN` environment variable before starting the daemon:

```sh
GROUNDTRUTH_TOKEN=mysecrettoken gt watch config.hcl --addr 0.0.0.0:9090
```

When the token is set:
- Every endpoint **except** `/healthz` requires `Authorization: Bearer <token>`.
- `/healthz` remains open so liveness probes never need a secret.
- Missing, wrong, or malformed tokens → **HTTP 401** with a `WWW-Authenticate: Bearer` header.
- The `Bearer` scheme keyword is matched case-insensitively (RFC 7235).
- Comparison is **constant-time** (`constant_time_eq`) to prevent timing attacks.

When `GROUNDTRUTH_TOKEN` is **not** set, all endpoints are open (backward-compatible)
and groundtruth prints a one-time warning to stderr at startup.

## Use from an AI agent (MCP)
```sh
gt mcp config.hcl
```
Speaks JSON-RPC 2.0 / MCP over stdio via the official **rmcp** SDK. Tools:
`list_checks`, `run_check {name}`, `explain_failure {name}` (returns name/status/detail/
query/sample/hint). Point an MCP-capable client at the command.
