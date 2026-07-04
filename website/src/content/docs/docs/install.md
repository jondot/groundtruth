---
title: Install
description: Install the gt binary with the install script, Cargo, a release download, or Docker.
---

groundtruth ships as a single binary, `gt`. Pick whichever install method fits your
setup, then verify it. When you're done, head to the [Quick Start](/docs/quickstart).

## Install script

The fastest path. This downloads the right binary for your platform:

```sh
curl -fsSL https://raw.githubusercontent.com/jondot/groundtruth/main/install.sh | sh
```

## From source (Cargo)

If you have a Rust toolchain:

```sh
cargo install --git https://github.com/jondot/groundtruth
```

This installs `gt` to `~/.cargo/bin/gt`. Make sure that directory is on your `PATH`.

## Release binaries

Prebuilt binaries are published on the
[GitHub releases page](https://github.com/jondot/groundtruth/releases), named by
target triple:

| Target triple | Platform |
|---|---|
| `x86_64-unknown-linux-gnu` | Linux, x86-64 |
| `aarch64-unknown-linux-gnu` | Linux, ARM64 |
| `x86_64-apple-darwin` | macOS, Intel |
| `aarch64-apple-darwin` | macOS, Apple Silicon |

Download the `.tar.gz` for your target, extract it, and put `gt` on your `PATH`.

Each archive ships a cosign `.sig` and `.crt` alongside it for keyless verification,
so you can confirm the download is the one that was published before you run it.

## Docker

A prebuilt image is published to GitHub Container Registry:

```sh
docker pull ghcr.io/jondot/groundtruth:latest
```

Mount your config into the container and pass it to `gt` like any other invocation.

## Verify the install

Confirm the binary runs:

```sh
gt --version
```

Then check that it can parse a config without touching any database. `gt check`
parses the file only — it makes no connection and runs no queries:

```sh
gt check config.hcl
```

On success it prints a summary such as `OK: 3 check(s), 1 connection(s), 0 notifier(s)`
and exits `0`. A parse error exits non-zero.

:::tip[Next]
Ready to write your first check? Follow the [Quick Start](/docs/quickstart). For the
full list of subcommands and flags, see the [CLI reference](/docs/cli).
:::
