---
title: Integrating
description: Wire groundtruth into Prometheus, Kubernetes probes, uptime monitors, and CI.
---

groundtruth is pull-first: it exposes check results over HTTP, and your existing tools scrape or probe them. Start the daemon, point your systems at it, and read the status codes.

## Start the daemon

```sh
gt watch config.hcl --addr 0.0.0.0:9090
```

`--addr` defaults to `127.0.0.1:9090`. Bind to `0.0.0.0` when other pods or hosts need to reach it. See the [HTTP API reference](/docs/http-api) for every endpoint and query parameter.

## Prometheus

Scrape `/metrics`:

```yaml
scrape_configs:
  - job_name: groundtruth
    metrics_path: /metrics
    static_configs:
      - targets: ["groundtruth:9090"]
```

groundtruth exposes two gauges, one series per check (exact metric names are in the [HTTP API reference](/docs/http-api)):

- A **status** gauge encoding the outcome as `0=pass`, `1=warn`, `2=fail`, `3=error`.
- An **up** gauge that is `1` for pass or warn and `0` for fail or error.

If `GROUNDTRUTH_TOKEN` is set, `/metrics` requires the bearer token — add an `authorization` credentials block to the scrape config.

:::tip
For production paging, let Prometheus + Alertmanager own the alert lifecycle (grouping, silences, escalation). Treat groundtruth's webhook notifier as a convenience tier, not your pager.
:::

## Kubernetes probes

Use the status codes directly: `/checks` returns `200` when every returned check is PASS or WARN, and `503` if any is FAIL or ERROR.

```yaml
livenessProbe:
  httpGet:
    path: /healthz
    port: 9090
readinessProbe:
  httpGet:
    path: /checks
    port: 9090
```

- **Liveness** → `/healthz`. Always open (no token) and always `200` while the process is alive, so it never depends on database health.
- **Readiness** → `/checks`. The pod leaves the Service when any check fails, so traffic that depends on healthy data is held back.

To gate on a single check, probe `/checks/{name}` instead:

```yaml
readinessProbe:
  httpGet:
    path: /checks/orders_present
    port: 9090
```

An unknown name returns `404`.

For a full Deployment, ConfigMap, Secret, and Service, see [Deploying](/docs/deploy).

## Uptime monitors

Point Better Stack, Pingdom, UptimeRobot, or any HTTP monitor at `/checks` (all checks) or `/checks/{name}` (one check). The monitor treats `503` as down and `200` as up, which matches groundtruth's health semantics. If a token is set, add the `Authorization: Bearer <token>` header in the monitor's request settings.

## Gating CI

Run every check once and fail the pipeline on any FAIL or ERROR:

```sh
gt run config.hcl
```

`gt run` exits non-zero if any check is FAIL or ERROR, which fails the CI step. For machine-readable output, use `--json` and inspect it:

```sh
gt run config.hcl --json | jq -e 'all(.[]; .status == "pass" or .status == "warn")'
```

The JSON is an array of `{ name, status, detail, sample }`, where `status` is lowercase `pass`, `warn`, `fail`, or `error`.
