# silver

An experimental coding agent. One Rust binary owns the agent loop, tools, sessions, memory and SQLite
persistence, and serves an HTTP + SSE API and a web UI on the same port. It works with hosted
models and with small local ones (LM Studio, llama.cpp, Ollama). It started as a selective Rust port of
the behaviour of [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent); but now heavily
modified to make it more focused for software development and for small/dumb LLMs use.

> [!WARNING]
> Most of the source was generated with LLMs, with human guidance and review.

## Quick start

    cargo build --release -p silver   # also builds the web UI: needs Rust 1.88+, Node 22 and npm
    target/release/silver

Open <http://127.0.0.1:7777>, add a workspace, and pick a provider under Settings → Providers
(or export `OPENAI_API_KEY`). `./scripts/install.sh` installs the binary into `~/.local/bin`.
Prebuilt binaries for Linux (x86_64, ARM64), macOS (Apple Silicon, Intel) and Windows (x86_64) are
on the [releases page](https://github.com/thlgr/silver/releases).

A run works either inside one registered **workspace** (file and shell tools, its own sessions and
memory) or with **no workspace** (no file or shell tools, a separate global scope). Scopes never
mix.

## Documentation

| | |
| --- | --- |
| [docs/tools.md](docs/tools.md) | The tools, approvals, presets, plan mode, subagents, attachments |
| [docs/agui.md](docs/agui.md) | The AG-UI endpoint (`POST /agent`) for AG-UI-compatible fronts |
| [docs/web-ui.md](docs/web-ui.md) | What the browser UI does, slash commands, UI development |
| [docs/messages.md](docs/messages.md) | The Messages mode: bots, group chats, threads, bots asking each other |
| [docs/configuration.md](docs/configuration.md) | Flags, `config.toml`, `secrets.env`, environment variables |
| [docs/provider-setup.md](docs/provider-setup.md) | Providers, sign-in, fallbacks, context window |
| [docs/operations.md](docs/operations.md) | Install, container, Nix, CI |
| [docs/security.md](docs/security.md) | Threat model and its limits |
| [docs/api.md](docs/api.md) | HTTP routes, errors, SSE events |
| [docs/architecture.md](docs/architecture.md) | Invariants, run lifecycle, agent loop, known gaps |
| [docs/tool-port-matrix.md](docs/tool-port-matrix.md), [docs/upstream-behavior.md](docs/upstream-behavior.md), [docs/system-prompt.md](docs/system-prompt.md), [docs/adr/](docs/adr/) | Hermes port decisions and background |

Contributing: read [AGENTS.md](AGENTS.md). License: [Unlicense](LICENSE), public domain.
