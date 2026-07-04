# Contributing to groundtruth

Thanks for considering a contribution. groundtruth is a single-crate Rust binary — a
database health monitor that runs SQL checks defined in HCL and reports
pass / warn / fail / error. This doc covers the usual paths.

## Before you start

- **Security bugs:** don't open a public issue. Use GitHub's private
  [security advisory](https://github.com/jondot/groundtruth/security/advisories/new)
  flow. See [`SECURITY.md`](SECURITY.md).
- **Big changes:** open a discussion or issue first. Align on approach
  before investing implementation time.
- **Small fixes:** typos, confusing error messages, missing test cases —
  just open a PR.

## Setup

Requires:

- Rust stable (`rustup default stable`). Edition 2024.
- A local Postgres instance for the integration tests. The easiest way:

  ```sh
  docker run -d --name pg17 \
    -e POSTGRES_USER=postgres \
    -e POSTGRES_PASSWORD=postgres \
    -e POSTGRES_DB=postgres \
    -p 5432:5432 \
    postgres:17
  ```

- Set `GROUNDTRUTH_TEST_DSN` before running tests:

  ```sh
  export GROUNDTRUTH_TEST_DSN=postgres://postgres:postgres@localhost:5432/postgres
  ```

Then:

```sh
git clone https://github.com/jondot/groundtruth
cd groundtruth
cargo build
cargo test
```

The integration tests are self-seeding — they create and tear down their
own schemas in the target database. You don't need to seed anything manually.

SQLite tests run without any extra setup (SQLite is embedded).

## Running the linters

CI enforces these; run them locally before pushing:

```sh
cargo fmt --all                              # format
cargo fmt --all -- --check                  # check only (what CI does)
cargo clippy --all-targets -- -D warnings   # lint
```

## The "fail loud" philosophy

groundtruth is built around the principle that a monitor that silently does
nothing is the worst possible outcome. When adding or changing code:

- A config error (typo'd attribute, unknown block) must be a **hard error at
  load time**, never a silent skip.
- A `when` expression that can't evaluate (bad column, type mismatch, division
  by zero) must surface as **ERROR status**, never a fake PASS.
- An unhandled SQL type must error loudly, naming the column — not silently
  become `null`.
- Panics inside expression evaluation are caught (via `catch_unwind`) so the
  daemon keeps running, but they are surfaced as ERROR on that check.

## Adding a new check type

Check evaluation lives in `src/eval.rs`. The eval context is an
`hcl::Value` map built from query rows — `row`, `rows`, `baseline`, and
`each.value` for `for_each` checks.

To add a new check attribute or evaluation mode:

1. Add the field to the `Check` struct in `src/config.rs` (strict — unknown
   fields error at parse time, which is intentional).
2. Build the eval context for it in `src/eval.rs`.
3. Add integration tests in `tests/` covering the happy path and at least one
   failure path.
4. Document the new attribute in `README.md` under "The language".

## Adding a new database engine

Sources are a single `enum Source` in `src/source.rs`. Adding a backend is
one new variant + a `connect` / `query` arm. The existing Postgres and SQLite
arms are the reference. Add tests in `tests/` (follow `matrix_postgres.rs` /
`matrix_sqlite.rs`).

## Code style

- `cargo fmt --all` must pass.
- `cargo clippy --all-targets -- -D warnings` must pass.
- Prefer narrow, focused commits. One logical change per PR.

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org/):

```
feat(eval): add mad baseline method
fix(config): reject unknown attributes in check block
docs: clarify sustained gating in README
test(postgres): cover null-column error path
chore: update sqlx to 0.9
```

Scopes: `eval`, `config`, `source`, `runner`, `store`, `schedule`,
`notify`, `metrics`, `mcp`, `report`, `ci`, `docs`.

## Opening a pull request

1. Fork, create a branch (`feat/my-thing`, `fix/my-thing`).
2. Keep the change focused. One logical change per PR.
3. Add or update tests. A PR that adds behavior without tests will be asked
   for them.
4. Update `README.md` if you change user-visible HCL syntax or CLI behavior.
5. Add a `CHANGELOG.md` entry under `## [Unreleased]` for user-visible changes.

## Licensing

groundtruth is MIT OR Apache-2.0 licensed (see `LICENSE-MIT` and `LICENSE-APACHE`).
By opening a PR you agree your contribution lands under the same dual license.
