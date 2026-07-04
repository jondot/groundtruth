---
title: Alerting
description: Schedule checks, gate notifications with sustained, and wire failing checks to webhooks in gt watch.
---

Alerting has three parts: how often a check runs, when a failure is worth notifying about, and where the notification goes. All of it happens in `gt watch`.

:::caution
`gt run` never notifies. It runs each check once with in-memory state and exits. Sustained gating, recovery, and webhook notifications only happen in the `gt watch` daemon. See the [CLI reference](/docs/cli) and [How checks work](/docs/concepts).
:::

## Schedule checks with `every`

Set `every` on a check (or in `defaults`) to control how often it runs.

```hcl
check "orders_present" {
  query = "select count(*) as n from orders"
  fail  = row.n == 0
  every = "5m"
}
```

`every` takes two forms:

- **Interval strings**: `Ns`, `Nm`, `Nh`, `Nd` (seconds, minutes, hours, days). No milliseconds, no fractions.
- **Cron**: a 5-field cron expression, evaluated in UTC.

```hcl
check "nightly_reconcile" {
  query = "select count(*) as n from unreconciled"
  fail  = row.n > 0
  every = "0 2 * * *"   # 02:00 UTC daily
}
```

If a check sets no `every`, it uses `defaults.every`, or 60s in watch mode. A check that has never run is due immediately at startup.

## Gate notifications with `sustained`

A single blip shouldn't page anyone. Set `sustained` on the `fail` tier to notify only after the check has been failing continuously for that duration.

```hcl
check "replication_lag" {
  query = "select lag_seconds as s from replication"
  fail {
    when      = row.s > 60
    sustained = "15m"
  }
}
```

The check still reports FAIL immediately in the report and metrics. The notification is held back until it's been failing without interruption for 15 minutes. If it recovers before then, no page is sent. This is what kills flapping pages — transient failures never reach your on-call.

`sustained` is read from the `fail` tier only. In a persisted state store, the failing-since timer survives daemon restarts; see [Securing & persisting](/docs/securing-and-persisting).

## Wire up a webhook

Declare a `notify "webhook"` block with a `url`, then connect a check to it with `on_fail`.

```hcl
notify "webhook" "slack" {
  url = env("SLACK_WEBHOOK_URL")
}

check "orders_present" {
  query   = "select count(*) as n from orders"
  fail    = row.n == 0
  on_fail = "slack"
}
```

`webhook` is the only notifier type, and `url` is its only attribute. To wire every check to the same notifier, set it once:

```hcl
defaults {
  on_fail = "slack"
}
```

### What gets sent

When a check fails past its `sustained` gate, groundtruth POSTs a JSON payload to the `url`:

```json
{
  "text": ":red_circle: orders_present FAIL: 0 row(s)",
  "check": "orders_present",
  "status": "FAIL",
  "detail": "0 row(s)"
}
```

When a previously-failing check passes again, groundtruth POSTs a recovery payload so you know the incident cleared:

```json
{
  "text": ":white_check_mark: orders_present RECOVERED: 5 row(s)",
  "check": "orders_present",
  "status": "RECOVERED",
  "detail": "5 row(s)"
}
```

Delivery is retried up to 3 times with short backoff. A failing check keeps notifying on each evaluation until it recovers — use `sustained` to suppress flaps, and let Alertmanager handle deduplication for production paging.

## Heartbeat monitoring for scheduled runs

`notify` blocks live in the `gt watch` daemon. If you run `gt run` on a schedule instead — from cron, a CI job, or [GitHub Actions](/docs/deploy) — a `heartbeat` block gives you alerting without a long-running server. After each run, `gt run` pings a cron-monitor (Better Stack, healthchecks.io, Cronitor, …). The monitor expects a ping on a schedule and pages you when one is missing — so a crashed job, a bad database URL, or a run that never started all page you, not just a failing check.

```hcl
heartbeat {
  url = env("HEARTBEAT_URL")
}
```

- **Green run** (all checks pass, or WARN-only) → POST to `url`.
- **Any FAIL or ERROR** → POST to the failure URL with a report body.

The failure URL defaults to `url` with `/fail` appended, which matches Better Stack and healthchecks.io out of the box. For monitors that signal failure a different way (Cronitor, Uptime Kuma), set it explicitly:

```hcl
heartbeat {
  url      = env("HEARTBEAT_URL")
  fail_url = env("HEARTBEAT_FAIL_URL")
}
```

The heartbeat is best-effort: a slow or unreachable monitor is logged and never changes the run's exit code — the monitor catches the resulting silence anyway.

### The failure report

By default the failure ping carries a plain-text summary, headline first, so it reads cleanly in any monitor's event log:

```
groundtruth: 2/14 checks FAILED on "orders.hcl"
FAIL  no_orphaned_line_items — 3 row(s)
  {"id":3,"order_id":999}
ERROR revenue_reconciliation — connection timeout
(+1 more check)
```

To send a monitor exactly the shape it wants — a Slack message, a PagerDuty event, a custom JSON webhook — set `fail_body` with `{{...}}` placeholders and a matching `content_type`:

```hcl
heartbeat {
  url          = env("HEARTBEAT_URL")
  content_type = "application/json"
  fail_body    = <<-EOT
    {"text": {{summary_json}}, "failed": {{failed}}, "checks": {{failures_json}}}
  EOT
}
```

Available placeholders:

| Placeholder | Value |
|---|---|
| `{{status}}` | `"fail"` or `"pass"` for the run |
| `{{total}}` | number of checks run |
| `{{failed}}` `{{passed}}` `{{warned}}` `{{errored}}` | counts by outcome (`failed` = FAIL + ERROR) |
| `{{config}}` | the config file name |
| `{{summary}}` | the default plain-text summary above |
| `{{failures}}` | the failing checks as a plain-text block |
| `{{json}}` | the whole run as one JSON object |
| `{{summary_json}}` `{{failures_json}}` | JSON-string versions of `summary` / `failures`, safe to drop inside a JSON template |

Values are escaped for the `content_type` you choose, so a quote or newline in a check name or row can never break your JSON. When you build a JSON body, reach for the `_json` placeholders. Unknown placeholders are rejected by `gt check`, so a typo fails before it ever runs.

## Production paging

The webhook is a convenience tier — good for a Slack ping or a lightweight hook. For production paging, let Prometheus and Alertmanager own the alert lifecycle (deduplication, grouping, silences, escalation) against groundtruth's `/metrics` gauges. See [Integrating](/docs/integrating).
