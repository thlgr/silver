# 0005. One binary serves the API and the embedded web UI; the CLI/TUI is retired

Status: accepted. Supersedes the binary names in 0002.

## Context

The workspace shipped two binaries: the daemon `silverd` and the CLI/TUI client `silver`, which
reached the daemon through `silver-client`. The web UI (`apps/web`) had become the primary
client, but it ran as a separate Vite server that proxied `/v1` to the daemon. Two processes and
two ports were needed to use the product, and the terminal client duplicated every feature the web
UI gained.

## Decision

- The daemon becomes `apps/silver`: package, library and binary are all named `silver`.
- The web UI is built by Vite into `apps/web/dist` and compiled into the binary with `rust-embed`
  (`apps/silver/src/api/ui.rs`). The Axum router serves it as the fallback for every path
  outside `/health` and `/v1/*`, on the same port as the API, with its own same-origin CSP. UI
  paths are not behind the bearer-token middleware.
- The CLI/TUI, `silver-client` and the TUI e2e script are removed from the tree.

## Consequences

- Building a release binary needs Node.js: `apps/web` must be built before `cargo build`. The
  Dockerfile, Nix derivation and `scripts/install.sh` do this. Without `dist` the crate still
  compiles (`allow_missing`) and `/` returns a hint instead of the UI.
- INV-1 and INV-2 hold: the web bundle is static and reaches the agent only through HTTP/SSE.
- The embedded UI does not yet authenticate against a server with `server.bearer_token` set, so
  a non-loopback deployment exposes the API but not a usable UI until a browser auth scheme exists.
- `silver setup`, `silver run`/`chat` and the terminal UI are gone from the supported surface;
  provider sign-in, approvals and session controls live in the web UI and the HTTP API.
