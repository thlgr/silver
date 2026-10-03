# syntax=docker/dockerfile:1

# ---- Web stage -------------------------------------------------------------
# The web UI is embedded into the binary at compile time (rust-embed over
# apps/web/dist), so it must be built before cargo runs.
FROM node:22-bookworm-slim AS web

WORKDIR /web
COPY apps/web/package.json apps/web/package-lock.json ./
RUN npm ci
COPY apps/web/ ./
RUN npm run build

# ---- Build stage -----------------------------------------------------------
# The locked dependencies require rustc 1.88 (the workspace's declared
# rust-version of 1.85 is below the highest dependency MSRV), so use 1.88.
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# 1. Copy only the manifests and the lockfile. This layer rebuilds only when a
#    Cargo.toml or Cargo.lock changes, so the dependency graph stays cached
#    while sources change.
COPY Cargo.toml Cargo.lock ./
COPY crates/silver-protocol/Cargo.toml crates/silver-protocol/Cargo.toml
COPY crates/silver-core/Cargo.toml crates/silver-core/Cargo.toml
COPY apps/silver/Cargo.toml apps/silver/build.rs apps/silver/

# 2. Stub every target and build the dependency graph into the cargo registry
#    and target caches. The stubs are removed so the real sources below are
#    recompiled while third-party crates are not.
RUN mkdir -p \
        crates/silver-protocol/src \
        crates/silver-core/src \
        apps/silver/src \
 && echo 'fn main() {}' > apps/silver/src/main.rs \
 && touch crates/silver-protocol/src/lib.rs \
            crates/silver-core/src/lib.rs \
            apps/silver/src/lib.rs \
 && cargo build --release --workspace --locked \
 && rm -rf crates/silver-protocol/src \
            crates/silver-core/src \
            apps/silver/src

# 3. Real sources and the built web UI; only the workspace crates are
#    recompiled. build.rs reruns when apps/web/dist changes.
COPY . .
COPY --from=web /web/dist apps/web/dist
RUN cargo build --release --workspace --locked

# ---- Runtime stage ---------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# rustls is used instead of OpenSSL, so only the CA bundle is needed.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Non-root user. The daemon writes state.db and managed memory under
# $HOME/.local/share/silver.
RUN groupadd --system --gid 10001 silver \
 && useradd --system --uid 10001 --gid 10001 --create-home --home-dir /home/silver silver

COPY --from=builder /build/target/release/silver /usr/local/bin/silver

# Keep sessions, runs and memory across container restarts.
VOLUME ["/home/silver/.local/share/silver"]

# Listen on all interfaces so the published port is reachable. The same port
# serves the HTTP API (/health, /v1/*) and the web UI (everything else). A
# non-loopback bind requires server.bearer_token in config.toml.
ENV RUST_LOG=info \
    SILVER_BIND=0.0.0.0:7777

EXPOSE 7777

USER silver
WORKDIR /home/silver

# silver runs in the foreground.
CMD ["silver"]
