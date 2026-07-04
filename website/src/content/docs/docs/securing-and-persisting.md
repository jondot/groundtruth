---
title: Securing & persisting
description: Lock down the watch HTTP endpoints with a bearer token and keep failing-check state across daemon restarts.
---

Two things you set for production: an auth token on the HTTP endpoints, and a persisted state store so timers survive restarts.

## Securing the HTTP endpoints

In watch mode groundtruth serves HTTP (see the [HTTP API reference](/docs/http-api)). By default those endpoints are open. Set the `GROUNDTRUTH_TOKEN` environment variable to require authentication.

```sh
export GROUNDTRUTH_TOKEN="a-long-random-secret"
gt watch config.hcl --addr 0.0.0.0:9090
```

When `GROUNDTRUTH_TOKEN` is set, every endpoint except `/healthz` requires an `Authorization: Bearer <token>` header. Send it on each request:

```sh
curl -H "Authorization: Bearer a-long-random-secret" \
  http://localhost:9090/checks
```

A request without the header, or with the wrong token, gets `401` and a `WWW-Authenticate: Bearer` response header. The scheme name is case-insensitive and the token is compared in constant time.

`/healthz` stays open regardless, so a liveness probe never needs the token.

:::caution
When `GROUNDTRUTH_TOKEN` is unset, `/metrics`, `/checks`, and `/checks/{name}` are open to anyone who can reach the port, and groundtruth logs a warning at startup. Set the token, or keep the port private, before you expose the daemon.
:::

## Persisting state

groundtruth keeps a small amount of bookkeeping: for each failing check, how long it has been failing. This drives `sustained` notification gating (see [How checks work](/docs/concepts)).

By default this state is kept **in memory** and is lost when the daemon restarts. After a restart every check starts fresh, so a check that had been failing for an hour looks like it just started failing.

To keep state across restarts, add a `state` block with a connection string:

```hcl
state {
  dsn = "postgres://gt:secret@localhost:5432/groundtruth"
}
```

This is groundtruth's **own** bookkeeping database. It is separate from the databases you monitor — those are only ever read from. The state store is the only database groundtruth writes to. On startup groundtruth creates its table there:

```sql
failing_state("check" TEXT PRIMARY KEY, since INTEGER NOT NULL)
```

### Scheme routing

The `dsn` scheme selects where state is stored:

| `dsn` value | Backend |
|---|---|
| `postgres://...` or `postgresql://...` | PostgreSQL |
| `sqlite:...` prefix, or a bare file path | SQLite |

For SQLite, use `?mode=rwc` to create the file if it doesn't exist:

```hcl
state {
  dsn = "sqlite:groundtruth.db?mode=rwc"
}
```

### Notes

- `gt run` always uses in-memory state. It never notifies and never reads or writes a state store, so persisted state only matters for production `gt watch`.
- groundtruth is not clustered — one shared state database, no leader election or HA.
- Changing the `state` block takes effect only after a daemon restart. Connections, notifiers, and the state store are built once at startup.

:::tip
For production `gt watch`, use a persisted state store so `sustained` timers and failing history survive restarts and redeploys.
:::
