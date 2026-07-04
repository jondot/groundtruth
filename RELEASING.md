# Releasing groundtruth

groundtruth ships prebuilt binaries through four channels:

- GitHub Releases: a cosign-signed `tar.gz` per target (the curl installer pulls these).
- Docker: `ghcr.io/jondot/groundtruth` (`:latest` and `:vX.Y.Z`).
- npm: the `@jondot/groundtruth` package plus per-platform binary packages.
- crates.io: `cargo install groundtruth` (the binary is `gt`).

## Cutting a release

```sh
./scripts/release.sh 0.3.0
```

This requires a clean tree on `main`. It bumps `version` in `Cargo.toml`,
regenerates `Cargo.lock`, commits `release v0.3.0`, tags `v0.3.0`, and pushes.
Pushing the `v*` tag triggers `.github/workflows/release.yml`.

## What the Release workflow does

1. **build** (matrix, one runner per target):

   | Target | Runner |
   |--------|--------|
   | `x86_64-apple-darwin` | `macos-latest` |
   | `aarch64-apple-darwin` | `macos-latest` |
   | `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
   | `aarch64-unknown-linux-gnu` | `ubuntu-latest` (cross via `gcc-aarch64-linux-gnu`) |

   Each produces `gt-<target>.tar.gz`.

2. **release** — signs every `tar.gz` with cosign (keyless / sigstore, needs
   `id-token: write`) producing a `.sig` + `.crt`, then creates the GitHub Release
   with auto-generated notes and uploads the archives + signatures.

3. **docker** — builds and pushes `ghcr.io/jondot/groundtruth:<tag>` and `:latest`
   (linux/amd64) using the automatic `GITHUB_TOKEN`.

4. **publish-npm** — extracts each binary into its `npm/groundtruth-<platform>/`
   package, sets all versions to the tag, pins the main package's
   `optionalDependencies`, and publishes the platform packages then the main
   `@jondot/groundtruth` package.

5. **publish-crates** — runs `cargo publish` so `cargo install groundtruth` works.

## Required repository secrets & variables

Both publish jobs are **opt-in**, each gated by a repository variable so a release
can build, sign, and publish to GitHub Releases and Docker without any extra token:

| Channel | Secret | Enable variable |
|---------|--------|-----------------|
| npm | `NPM_TOKEN` (token with write access to the `@jondot` scope — publishes `@jondot/groundtruth` and the four `@jondot/groundtruth-<platform>` packages) | `PUBLISH_NPM=true` |
| crates.io | `CARGO_REGISTRY_TOKEN` | `PUBLISH_CRATES=true` |

`GITHUB_TOKEN` is provided automatically; the workflow requests `contents: write`,
`packages: write` (ghcr), and `id-token: write` (cosign keyless signing).

`cargo publish` and `npm publish` only succeed for a version not already on the
registry, so each publishes exactly once per new tag.

Set them once:

```sh
gh variable set PUBLISH_NPM --body true
gh variable set PUBLISH_CRATES --body true
gh secret set NPM_TOKEN            # paste an npm automation token
gh secret set CARGO_REGISTRY_TOKEN # paste a crates.io API token
```

## Not included (and why)

- **Windows.** No Windows artifact yet — it needs its own packaging (`.zip` +
  `gt.exe`, a `win32-x64` npm package, and a PowerShell installer). Add it
  deliberately later.
- **Apple notarization.** Artifacts are cosign-signed (generic), not Apple
  notarized; notarization needs developer-ID certificates and is out of scope.
