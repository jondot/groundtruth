---
title: Deploying
description: Run the groundtruth daemon under systemd, Docker, or Kubernetes, and understand how it holds up when a database goes down.
---

groundtruth is a single binary (`gt`). There is no separate agent or runtime to install alongside it. You can run it two ways: as a scheduled one-shot (`gt run`) that reports to an external monitor, or as a long-running daemon (`gt watch`) that exposes HTTP endpoints for your own tooling (see [Integrating](/docs/integrating)).

## Deploy free with GitHub Actions

The lightest way to run groundtruth is on a schedule with no server at all. `gt init` scaffolds a starter config and a GitHub Actions workflow that runs your checks every 15 minutes:

```sh
gt init
```

This writes `groundtruth.hcl` and `.github/workflows/groundtruth.yml`. The config includes a [`heartbeat` block](/docs/alerting#heartbeat-monitoring-for-scheduled-runs); the workflow installs `gt` and runs `gt run groundtruth.hcl` on a cron schedule.

Two steps to go live:

1. Create a free check at a cron-monitor (Better Stack, healthchecks.io, …) and copy its ping URL.
2. In your repo, add two Actions secrets: `DATABASE_URL` (your database) and `HEARTBEAT_URL` (the ping URL). Push.

Every run pings the monitor: green when checks pass, a failure report when they don't. Because the monitor also expects the ping on schedule, a workflow that fails to start — a bad credential, a broken runner — pages you through the same channel. Scheduled Actions are free on public repositories and included in the free tier for private ones.

This lane has no always-on process, so it can't serve the `/metrics` and `/checks` endpoints. When you want those — for Prometheus scraping or a Kubernetes readiness probe — move to `gt watch` below.

## systemd

Run `gt watch` as a service:

```ini
[Unit]
Description=groundtruth
After=network-online.target

[Service]
ExecStart=/usr/local/bin/gt watch /etc/groundtruth/config.hcl --addr 0.0.0.0:9090
Environment=DATABASE_URL=postgres://user:pass@db:5432/app
Environment=GROUNDTRUTH_TOKEN=a-long-random-secret
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

groundtruth shuts down gracefully on `SIGINT`/`SIGTERM`, so `systemctl stop` and restarts are clean.

## Docker

Mount your config and pass credentials as environment variables:

```sh
docker run --rm \
  -p 9090:9090 \
  -e DATABASE_URL="postgres://user:pass@db:5432/app" \
  -e GROUNDTRUTH_TOKEN="a-long-random-secret" \
  -v "$PWD/config.hcl:/etc/groundtruth/config.hcl:ro" \
  ghcr.io/jondot/groundtruth:latest \
  watch /etc/groundtruth/config.hcl --addr 0.0.0.0:9090
```

## Kubernetes

A complete, ready-to-apply setup: config in a ConfigMap, credentials in a Secret, the daemon in a Deployment, and a Service in front.

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: groundtruth-config
data:
  config.hcl: |
    connection "postgres" "main" {
      dsn = env("DATABASE_URL")
    }

    check "orders_present" {
      on    = connection.postgres.main
      query = "select count(*) as n from orders"
      fail  = row.n == 0
    }
---
apiVersion: v1
kind: Secret
metadata:
  name: groundtruth-secrets
type: Opaque
stringData:
  DATABASE_URL: "postgres://user:pass@db:5432/app"
  GROUNDTRUTH_TOKEN: "a-long-random-secret"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: groundtruth
spec:
  replicas: 1
  selector:
    matchLabels:
      app: groundtruth
  template:
    metadata:
      labels:
        app: groundtruth
    spec:
      containers:
        - name: groundtruth
          image: ghcr.io/jondot/groundtruth:latest
          args:
            - watch
            - /etc/groundtruth/config.hcl
            - --addr
            - 0.0.0.0:9090
          ports:
            - containerPort: 9090
          envFrom:
            - secretRef:
                name: groundtruth-secrets
          livenessProbe:
            httpGet:
              path: /healthz
              port: 9090
          readinessProbe:
            httpGet:
              path: /checks
              port: 9090
          volumeMounts:
            - name: config
              mountPath: /etc/groundtruth
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: groundtruth-config
---
apiVersion: v1
kind: Service
metadata:
  name: groundtruth
spec:
  selector:
    app: groundtruth
  ports:
    - port: 9090
      targetPort: 9090
```

Liveness uses `/healthz` (always open, always `200` while the process lives). Readiness uses `/checks`, which returns `503` when any check is FAIL or ERROR, so the pod leaves the Service until data is healthy again.

## Resilience

groundtruth is built to keep running when the databases it watches misbehave:

- **A database goes down.** Its checks become ERROR and `/checks` returns `503`. The daemon does not crash — it keeps serving and keeps running the checks that target other connections.
- **A query hangs.** Each check is bounded by its `timeout` (default 30s). A hung query is reported as ERROR after the timeout, not held forever.
- **One check breaks.** Checks run concurrently and in isolation, so a single failing or panicking check can't crash the daemon or take down the others.

## Persisting state in production

By default groundtruth keeps its failing-check state in memory, which is lost on restart. For production, add a persisted state store so `sustained` timers and failing history survive restarts. See [Securing & persisting](/docs/securing-and-persisting).
