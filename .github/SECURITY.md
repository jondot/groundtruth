# Security

## Reporting a vulnerability

Please use GitHub's private
[security advisory](https://github.com/jondot/groundtruth/security/advisories/new)
flow. Do **not** open a public issue for security reports.

We aim to acknowledge within 72 hours and ship a fix or mitigation within
30 days of triage.

## Scope

groundtruth is a read-only database health monitor. It connects to your databases
with the credentials you supply, runs the SQL queries defined in your HCL
config, and reports results. It does not write to the monitored databases.

**In scope:**

- SQL injection or arbitrary query execution via HCL config parsing
- Credential leakage through logs, error messages, or the HTTP endpoints
- Webhook payload forgery or credential exposure in webhook delivery
- Path traversal or arbitrary file read through config or store paths
- Denial of service via a crafted HCL config or database response

**Out of scope:**

- Attacks that require the attacker to already have write access to the
  HCL config file (they can already inject arbitrary SQL at that point).
- Side-channel attacks on the monitored databases themselves.
- Issues in the monitored databases, not in groundtruth.

## Runtime exposure

- **HTTP endpoints** (`/metrics`, `/healthz`, `/checks`): these expose
  check results and status but not database credentials. Bind to
  `127.0.0.1` (the default) unless you control network access to the
  host.
- **`GROUNDTRUTH_TEST_DSN` / `DATABASE_URL`**: connection strings are read
  from environment variables at startup and never logged or included in
  check results or webhook payloads.
- **Webhook delivery**: payloads contain check name, status, and a
  sample of failing rows — whatever your SQL query returns. Do not write
  queries that select credential columns if you have webhooks configured.
- **MCP stdio server** (`gt mcp`): exposes `list_checks`,
  `run_check`, and `explain_failure` to the MCP client. The client
  receives query results and sample rows; treat it as you would any other
  read access to that data.

## Release artifact signing

Release binaries are signed with
[cosign](https://docs.sigstore.dev/cosign/overview) using keyless OIDC
signing (no static private key). Verify a download:

```sh
cosign verify-blob \
  --certificate-identity-regexp 'https://github.com/jondot/groundtruth/\.github/workflows/release\.yml' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate groundtruth-<target>.tar.gz.crt \
  --signature groundtruth-<target>.tar.gz.sig \
  groundtruth-<target>.tar.gz
```
