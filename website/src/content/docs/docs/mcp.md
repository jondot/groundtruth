---
title: Using with AI agents
description: Serve groundtruth over MCP so an AI agent can list and run checks, get a diagnostic hint on failure, and inspect the database to help author new checks.
---

`gt mcp config.hcl` serves groundtruth's checks to an AI agent over MCP, transported over stdio. An agent can then enumerate your checks, run a specific one on demand, get a diagnostic hint when a check fails, and inspect the database — connectivity, schema, and sample rows — to help write new checks.

```sh
gt mcp config.hcl
```

The server name is `groundtruth`. It reads the config at startup and is stateless — it holds no state between calls. See the [CLI reference](/docs/cli) for the command and [Writing checks](/docs/writing-checks) for the config it reads.

## Tools

The server exposes six tools: three for working with your checks, and three for inspecting the database.

### Working with checks

#### `list_checks`

No arguments. Returns an array of the configured checks, each as `{ name, on, query, conditions }`, where `query` is the resolved SQL. This is config only — it does not touch the database.

#### `run_check`

Argument: `{ name }`. Runs that one check and returns `{ name, status, detail, sample }`. If the name is unknown, it errors and lists the available check names.

#### `explain_failure`

Argument: `{ name }`. Runs the check. If it passed, it says so. If it FAILed or ERRORed, it returns `{ name, status, detail, query, sample, hint }`, where `hint` is a diagnostic pointer at the likely cause.

### Inspecting the database

These let an agent see the real schema and data before writing new checks, so it works from actual column names and values rather than guesses. Each targets a configured connection by name, or the first connection when `connection` is omitted.

#### `check_connection`

Argument: `{ connection? }`. Confirms the database is reachable and returns `{ connection, ok, elapsed_ms }` (plus an `error` when it can't connect). Times out after 10 seconds.

#### `describe_schema`

Arguments: `{ connection?, schema? }`. Returns the tables and their columns as `{ connection, tables: [{ schema, table, columns: [{ name, type, nullable }] }] }`. Pass `schema` to restrict to one schema; omit it to list all user schemas.

#### `sample_table`

Arguments: `{ connection?, table, limit? }`. Returns up to `limit` real rows (default 10, maximum 100) as `{ table, row_count, rows }`. `table` may be schema-qualified, e.g. `public.orders`.

## Wiring it into a client

Point any MCP client — Cursor, Claude Desktop, or a custom agent — at the `gt mcp` command with your config path as an argument:

```json
{
  "mcpServers": {
    "groundtruth": {
      "command": "gt",
      "args": ["mcp", "/path/to/config.hcl"]
    }
  }
}
```

Once connected, the agent can call `list_checks` to see what's monitored, `run_check` to check something right now, `explain_failure` to get a starting point when a check is failing, and `describe_schema` / `sample_table` to explore the database when writing new checks.
