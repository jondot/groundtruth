---
title: CLI reference
description: Every gt subcommand, its arguments, flags, exit codes, and output.
---

The `gt` binary has four subcommands. Each takes a config file path as a positional argument. There are no global flags.

Logs go to stderr, so stdout stays clean for `--json` output and the AI-agent server. Set the log level with `RUST_LOG` (default `info`).

```sh
gt run config.hcl
```

## Commands at a glance

| Command | What it does | Connects to databases? |
|---|---|---|
| `gt check <config>` | Parse and validate the config. | No |
| `gt run <config> [--json]` | Run every check once, print a report, exit. | Yes |
| `gt watch <config> [--addr <ip:port>]` | Run checks on schedule and serve HTTP. | Yes |
| `gt mcp <config>` | Serve checks to an AI client over stdio. | Yes (on demand) |

## `gt check`

Parse the config and report what it contains. This makes no database connection and runs no queries — it only validates the file.

```sh
gt check config.hcl
```

| Argument | Required | Meaning |
|---|---|---|
| `<config>` | yes | Path to the config file. |

On success it prints:

```text
OK: 4 check(s), 2 connection(s), 1 notifier(s)
```

**Exit code:** `0` on success, non-zero on a parse error. Unknown attributes or blocks, an unsupported connection scheme, or malformed values all fail here.

:::tip[Run this in CI]
`gt check` is a fast, offline gate. Wire it into a pre-merge or pre-deploy step to catch config mistakes before they reach a running daemon.
:::

## `gt run`

Connect to your databases, run every check once, print a report, and exit. Use this for one-off runs, CI gates, and ad-hoc inspection.

```sh
gt run config.hcl
gt run config.hcl --json
```

| Argument / flag | Required | Default | Meaning |
|---|---|---|---|
| `<config>` | yes | — | Path to the config file. |
| `--json` | no | off | Print a JSON array instead of the terminal report. |

Without `--json`, `gt run` prints one line per check:

```text
[PASS] orders_present 128 row(s)
[FAIL] no_orphans 3 row(s)
      order_id=91 customer_id=404
```

With `--json`, it prints an array of objects, one per check. See the [HTTP API & metrics reference](/docs/http-api) for the exact JSON shape (it is identical to `/checks?format=json`).

**Exit code:** non-zero if any check result is FAIL or ERROR; `0` when every check is PASS or WARN.

:::note
`gt run` uses in-memory state for the duration of the process. It does not read persisted state, apply `sustained` gating, or send notifications — those apply only in `gt watch`.
:::

## `gt watch`

Run as a daemon: evaluate checks on their schedules and serve results over HTTP. This is the mode you deploy.

```sh
gt watch config.hcl
gt watch config.hcl --addr 0.0.0.0:9090
```

| Argument / flag | Required | Default | Meaning |
|---|---|---|---|
| `<config>` | yes | — | Path to the config file. |
| `--addr <ip:port>` | no | `127.0.0.1:9090` | Address the HTTP server binds to. |

The config is re-read on each tick, so edits take effect without a restart. Connections, notifiers, and the state store are built once at startup — changing those requires a restart. The daemon shuts down gracefully on `SIGINT` and `SIGTERM`.

For endpoints, formats, status codes, authentication, and metrics, see the [HTTP API & metrics reference](/docs/http-api).

## `gt mcp`

Serve your checks to an AI client over stdio. Point any compatible client at the command `gt mcp config.hcl`.

```sh
gt mcp config.hcl
```

| Argument | Required | Meaning |
|---|---|---|
| `<config>` | yes | Path to the config file. |

The server communicates over stdio, so it runs as a subprocess of the client rather than binding a port. See [Using with AI agents](/docs/mcp) for the available tools and client setup.

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `RUST_LOG` | `info` | Log level filter. Logs are written to stderr. |
| `GROUNDTRUTH_TOKEN` | unset | Bearer token for the `gt watch` HTTP endpoints. When unset, the endpoints are open. |

Inside the config, `env("NAME")` reads any environment variable — commonly `DATABASE_URL` for a connection string. See the [Configuration reference](/docs/configuration) for details.
