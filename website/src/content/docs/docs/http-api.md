---
title: HTTP API & metrics reference
description: The watch-mode endpoints, response shapes, authentication, and Prometheus metrics.
---

In watch mode, groundtruth serves your check results over HTTP. By default it binds `127.0.0.1:9090`; change it with `--addr` (see the [CLI reference](/docs/cli)).

```sh
gt watch config.hcl --addr 0.0.0.0:9090
```

## Endpoints

| Path | Format(s) | Status codes | Auth |
|---|---|---|---|
| `/healthz` | text (`ok`) | 200 | Always open |
| `/metrics` | Prometheus text | 200 | Protected |
| `/checks` | json (default), yaml, text | 200 if all returned checks PASS/WARN; 503 if any FAIL/ERROR | Protected |
| `/checks/{name}` | json, yaml, text | 200 / 503 / 404 | Protected |

`/checks/{name}` returns the named check, or 404 if the name is unknown.

### `/checks` query parameters

| Parameter | Values | Default | Meaning |
|---|---|---|---|
| `format` | `json` \| `yaml` \| `text` | `json` | Response format. A bad value returns 400. |
| `status` | `pass` \| `warn` \| `fail` \| `error` | — | Filter to results with this status. |
| `limit` | integer | — | Cap the number of results returned. |

The status code reflects the **filtered** set. WARN counts as healthy, so a set of PASS and WARN results returns 200; any FAIL or ERROR in the set returns 503.

```sh
curl "http://127.0.0.1:9090/checks?status=fail&format=json"
```

## Authentication

Set the `GROUNDTRUTH_TOKEN` environment variable to require a bearer token.

When set, every endpoint except `/healthz` requires an `Authorization: Bearer <token>` header. A missing or wrong token returns 401 with `WWW-Authenticate: Bearer`.

```sh
curl -H "Authorization: Bearer $GROUNDTRUTH_TOKEN" http://127.0.0.1:9090/checks
```

When `GROUNDTRUTH_TOKEN` is unset, the endpoints are open and groundtruth logs a warning at startup.

:::caution
`/healthz` is always open, with or without a token, so liveness probes never need credentials. Protect the other endpoints by setting `GROUNDTRUTH_TOKEN`.
:::

## JSON response shape

`/checks?format=json` (and `gt run --json`) returns an array of objects:

```json
[
  {
    "name": "orders_present",
    "status": "pass",
    "detail": "128 row(s)",
    "sample": []
  },
  {
    "name": "no_orphans",
    "status": "fail",
    "detail": "3 row(s)",
    "sample": [
      { "order_id": 91, "customer_id": 404 }
    ]
  }
]
```

| Field | Meaning |
|---|---|
| `name` | Check name. |
| `status` | Lowercase: `pass`, `warn`, `fail`, or `error`. |
| `detail` | Human-readable summary. |
| `sample` | Offending rows captured by a tier or validator, or `[]`. |

`/checks/{name}` returns a single object with the same shape.

## Prometheus metrics

`/metrics` exposes exactly two gauges, with one series per check.

```text
# HELP groundtruth_check_status Check status: 0=pass 1=warn 2=fail 3=error
# TYPE groundtruth_check_status gauge
groundtruth_check_status{check="orders_present"} 0

# HELP groundtruth_check_up 1 if check is passing or warning, 0 otherwise
# TYPE groundtruth_check_up gauge
groundtruth_check_up{check="orders_present"} 1
```

| Metric | Values |
|---|---|
| `groundtruth_check_status{check="..."}` | 0 = pass, 1 = warn, 2 = fail, 3 = error |
| `groundtruth_check_up{check="..."}` | 1 for pass/warn, 0 for fail/error |

There are no latency, duration, or timestamp metrics. See [Integrating](/docs/integrating) for scrape and alerting setup.

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `GROUNDTRUTH_TOKEN` | unset | Bearer token for the HTTP endpoints. Unset means open. |
| `RUST_LOG` | `info` | Log level filter. Logs go to stderr. |

Inside the config, `env("NAME")` can read any environment variable — commonly `DATABASE_URL` for a connection string. There is no special `GROUNDTRUTH_STATE_DSN` variable: if a config reads `env("GROUNDTRUTH_STATE_DSN")`, that works only because you named and set that variable yourself. See [Securing & persisting](/docs/securing-and-persisting) for state and credential handling.
