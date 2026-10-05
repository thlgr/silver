# Building, installing and operating

For day-to-day development commands and the checks to run before finishing a change, see
[AGENTS.md](../AGENTS.md). Runtime settings are in [configuration.md](configuration.md).

## Prerequisites

- Rust stable, 1.88 or newer (the locked dependencies need it; no nightly features).
- Node.js 22 and npm, to build the web UI the binary embeds.
- A working C compiler (MSVC on Windows): SQLite is bundled (`tokio-rusqlite`, feature `bundled`), so no system
  libsqlite3 is needed.
- A provider key for real model calls.

## Build

    cargo build --release -p silver

A release build builds the web UI first (`npm ci` when `node_modules` is missing, then
`npm run build` in `apps/web`) and embeds it, so `target/release/silver` runs without the web
sources. Without npm the build warns and embeds whatever `apps/web/dist` already holds. A debug
build skips the UI build and reads `apps/web/dist` from disk. With no `dist` at all the API still
works and `/` explains how to build the UI.

## Install

`scripts/install.sh` builds the UI and the release binary and installs `silver` into
`$SILVER_PREFIX`, else `$HOME/.local/bin`. It never uses `sudo`, reuses release binaries that
already exist, and prints the line to add to `PATH` if needed.

    ./scripts/install.sh
    SILVER_PREFIX="$HOME/bin" ./scripts/install.sh
    ./scripts/install.sh --build      # force a rebuild
    ./scripts/install.sh --no-build   # only copy

Then run `silver`, open <http://127.0.0.1:7777> and pick a provider
([provider-setup.md](provider-setup.md)).

## Prebuilt binaries

Every `v*` tag publishes a release with one archive per system, so nothing needs building:

| Archive | System |
| --- | --- |
| `silver-<version>-linux-x86_64.tar.gz`, `silver-<version>-linux-arm64.tar.gz` | Linux on x86_64 and ARM64 (static: any distro, Alpine included) |
| `silver-<version>-macos-arm64.tar.gz`, `silver-<version>-macos-x86_64.tar.gz` | macOS on Apple Silicon and Intel |
| `silver-<version>-windows-x86_64.zip` | Windows 10 and 11 (x86_64; ARM64 PCs run it under emulation) |

Each archive holds the `silver` binary, `README.md` and `LICENSE`; `SHA256SUMS` lists every
archive's checksum. The macOS binaries are not signed: if macOS blocks one downloaded in a browser,
run `xattr -d com.apple.quarantine silver`.

On Windows the `bash` tool needs [Git for Windows](https://git-scm.com/download/win); silver finds
its `bash.exe` under `%ProgramFiles%\Git` or `%LOCALAPPDATA%\Programs\Git`, else on `PATH`. The
`bash.exe` in `System32` is WSL's launcher and is only a last resort. When a command is stopped on
Windows only the shell itself is killed, not every process it started.

## Running as a service

silver runs in the foreground and does not daemonise. Supervise it with systemd, launchd or a
container; give it `SILVER_DATA_DIR` for a stable state location and put secrets in `secrets.env`
or the unit's environment. Shutdown on SIGTERM cancels active runs cleanly. Run `GET /health` for
liveness: it reports `status`, `database`, `active_runs`, `paused` and `uptime_seconds`.

## Container

The `Dockerfile` builds the UI in a Node stage, embeds it in a Rust stage and packs the binary into
a `debian:bookworm-slim` image that runs as a non-root user and exposes port 7777. The container
binds `0.0.0.0:7777` (`SILVER_BIND`), and silver refuses a non-loopback bind without a bearer
token of at least 16 characters, so pass one; the web UI asks for it on first load.

    docker build -t silver .
    docker run --rm -p 7777:7777 \
      -e SILVER_BEARER_TOKEN="$(openssl rand -hex 16)" \
      -e OPENAI_API_KEY="$OPENAI_API_KEY" \
      -v silver-data:/home/silver/.local/share/silver \
      silver

The volume holds `state.db` and the managed ai-memory data.
[`docker-compose.yml`](../docker-compose.yml) runs the same image with the `silver-data` volume, a
read-only bind mount of `./config.toml` (which must exist, and must carry `server.bearer_token`
or be supplemented with `SILVER_BEARER_TOKEN`), port 7777 and `restart: unless-stopped`:

    docker compose up --build

## Nix

[`nix/flake.nix`](../nix/flake.nix) exposes `packages.<system>.silver` (the default) and
`packages.<system>.web`, plus a dev shell with the Rust toolchain, Node.js, clippy and rustfmt.
`nix/silver.nix` builds the UI with `buildNpmPackage` (dependencies from `package-lock.json`) and
copies it into `apps/web/dist` before the `buildRustPackage` build. The flake lives under `nix/`:

    nix build ./nix
    nix develop ./nix

Inputs track `nixos-unstable`; commit the generated `flake.lock` to pin the revision.

## CI

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs on every push and pull request:

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace`;
- the web build (`npm ci && npm run build` in `apps/web`);
- the mock-provider tools script, and a `docker build`.

[`.github/workflows/e2e.yml`](../.github/workflows/e2e.yml) is the nightly and manual counterpart:
it re-runs the scripts and validates the compose file.

[`.github/workflows/release.yml`](../.github/workflows/release.yml) runs on a `v*` tag: one build job
per target in its matrix, then a publish job that writes `SHA256SUMS` and the release notes
(`scripts/release_notes.sh`: install steps, a downloads table and the Conventional Commit subjects
since the previous tag, grouped into breaking changes, features, fixes and performance) and creates
the prerelease. Add a system by adding a matrix row and a line to the table in that script. Run the workflow
by hand (Actions → Release → Run workflow) to check every build without publishing.

## Scripts

- `scripts/tools_e2e.sh` drives real tool calls (`bash`, `todo_list`, `write_file`) through
  `scripts/mock_openai_server.py`, an offline OpenAI-compatible server (`python3
  scripts/mock_openai_server.py <port>`).
- `scripts/ai_memory_e2e.py` drives a native run against a stub ai-memory server: the run's
  lifecycle reaches `/hook/batch` and the project handoff lands in the system prompt.
- `scripts/release_notes.sh TAG DIST_DIR` prints the release notes (needs `GITHUB_REPOSITORY`).
- `scripts/slop.py` flags new AI slop (clones, unwraps, `#[allow]`, filler comments); see AGENTS.md.
