# Example configs

Realistic `groundtruth` configs, each modeling a different kind of system — from a
SaaS smoke test to a fintech ledger. Read them for patterns, or run one against your
own database. Start with [`00-demo.hcl`](00-demo.hcl), a small tour that shows a
pass, a warn, a fail with sample rows, an error, and a `validate` block in one file.

## Running an example

Every example reads its connection string from an environment variable, so nothing is
hardcoded. Set the variables the example uses, then run it:

```sh
export DATABASE_URL="postgres://user:pass@localhost:5432/mydb"
gt check examples/01-saas-startup-smoke.hcl   # validate the config
gt run   examples/01-saas-startup-smoke.hcl   # run the checks once
```

`gt check` validates syntax without touching a database. It reads `env(...)` values at
parse time, so the variables an example references must be set even for `check` — an
unset variable is reported as an error rather than guessed.

## Environment variables used

Most examples need only `DATABASE_URL`. A few model multiple databases or alerting and
use additional variables:

| Variable | Used for |
|---|---|
| `DATABASE_URL` | The primary database (every example). |
| `REPLICA_URL` | A read replica, where the example separates reads from writes. |
| `WAREHOUSE_URL` | An analytics warehouse. |
| `ANALYTICS_URL` | A separate analytics/reporting database. |
| `ALERT_WEBHOOK` | Webhook URL for general alerts (e.g. a Slack incoming webhook). |
| `PAGER_WEBHOOK` | Webhook URL for high-severity paging. |

Check the top of each file to see which it uses. All connection strings use the
`postgres://` scheme; point them at any supported database by changing the scheme (see
the documentation for supported engines).
