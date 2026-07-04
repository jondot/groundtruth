---
title: Quick Start
description: Go from nothing to a passing check and then a failing one in about five minutes using a throwaway Postgres.
---

This gets you a working check in about five minutes. You'll spin up a throwaway
Postgres, write one check, watch it pass, then make it fail on purpose. You need
`gt` installed ([Install](/docs/install)) and Docker.

## 1. Start a throwaway Postgres

```sh
docker run --rm -d --name gt-demo \
  -e POSTGRES_PASSWORD=secret \
  -p 5432:5432 postgres
```

This runs a disposable Postgres on `localhost:5432`. You'll delete it at the end.

## 2. Write a config

Create `config.hcl` with one connection and one check. The check runs `select 1`,
which always returns a single row with a column named `n`:

```hcl
connection "postgres" "main" {
  dsn = "postgres://postgres:secret@localhost:5432/postgres"
}

check "always_ok" {
  on    = connection.postgres.main
  query = "select 1 as n"
  fail  = row.n == 0
}
```

The check fails only if `row.n` is `0`. It never is, so this check passes.

## 3. Parse the config

`gt check` parses the file without connecting to anything. Use it to catch typos
before you run:

```sh
gt check config.hcl
```

You'll see something like `OK: 1 check(s), 1 connection(s), 0 notifier(s)`.

## 4. Run the check

Now actually connect and run every check once:

```sh
gt run config.hcl
```

You'll see a passing line:

```
[PASS] always_ok 1 row(s)
```

`gt run` exits `0` because everything passed.

## 5. Make it fail

Edit the check so the assertion is true. Change the `fail` line to compare against
`1`, which the query does return:

```hcl
check "always_ok" {
  on    = connection.postgres.main
  query = "select 1 as n"
  fail  = row.n == 1
}
```

Run it again:

```sh
gt run config.hcl
```

Now the check fails, and `gt run` exits non-zero:

```
[FAIL] always_ok 1 row(s)
```

That non-zero exit is what makes `gt run` useful in a script or CI step: a failing
data check fails the command.

## 6. Serve the results

Leave the failing check in place and start the daemon. `gt watch` runs checks on a
schedule and serves the results over HTTP, binding `127.0.0.1:9090` by default:

```sh
gt watch config.hcl
```

In another terminal, pull the current results:

```sh
curl http://127.0.0.1:9090/checks
```

You get back JSON with each check's status. Because the check is failing, `/checks`
responds `503` — a signal your tooling can act on. Stop the daemon with `Ctrl-C`.

## Clean up

```sh
docker rm -f gt-demo
```

## Where to go next

- [How checks work](/docs/concepts) — the lifecycle behind PASS, WARN, FAIL, and
  ERROR.
- [Writing checks](/docs/writing-checks) — expressions, tiers, schedules, and
  `for_each`.
- [Configuration reference](/docs/configuration) — every block and attribute.
