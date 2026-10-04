# Configuration

silver needs no configuration to start: it binds `127.0.0.1:7777` and you pick a provider in the
web UI. Everything below is for changing a default. Provider choice, sign-in and model routing are
in [provider-setup.md](provider-setup.md).

## Running

    silver [--config <FILE>] [--bind <ADDRESS>] [--data-dir <DIRECTORY>] [--yolo]

- `--config` reads that TOML file instead of `<config dir>/config.toml`. A missing file means the
  built-in defaults.
- `--bind` overrides `server.bind`; `--data-dir` overrides `data.directory`.
- `--yolo` pins the [approval mode](tools.md#approvals) to `off` for the whole process.

silver runs in the foreground and logs with `tracing` (`RUST_LOG=info`; stderr plus size-rotated
`agent.log` and `errors.log` in `<data_dir>/logs`). Ctrl-C or SIGTERM shuts it down gracefully:
active runs are cancelled with origin `shutdown` and keep the text they had streamed. It does not
daemonise itself; supervise it with systemd, launchd or a container.

## Where files live

| Platform | Config directory (`config.toml`, `secrets.env`) |
| --- | --- |
| Linux | `~/.config/silver` |
| macOS | `~/Library/Application Support/dev.silver.silver` |
| Windows | `%APPDATA%\silver\silver\config` |

The data directory (`state.db`, memory, plans, checkpoints, `auth.json`, the models cache) is
separate: `data.directory`, `SILVER_DATA_DIR` or `--data-dir`, defaulting to the platform data
directory (Linux `~/.local/share/silver`). Both fall back to `./silver-config` and `./silver-data`.

- `SILVER_CONFIG_DIR` moves the config directory, and with it both files.
- `SILVER_CONFIG` points at `config.toml` directly; `secrets.env` still comes from the config
  directory. `--config` behaves the same way.
- `SILVER_PROFILE=<name>` selects `<config base>/profiles/<name>/` for config, data, secrets and
  logs. One process serves one profile.

## Precedence

Highest first:

1. Flags (`--config`, `--bind`, `--data-dir`, `--yolo`)
2. Process environment (`SILVER_*` and the provider key variable)
3. `secrets.env`, imported into the environment only for names not already set
4. `config.toml`
5. Built-in defaults

For the model, an empty `model.base_url`, `model.name` or `model.api_key_env` falls back to the
selected preset, and the transport kind comes from `model.kind` or the preset, so
`provider = "anthropic"` selects the Anthropic transport even when `kind` is omitted.

## Secrets

- `secrets.env` holds `NAME=value` lines (blank lines and `#` comments are skipped). Keep it mode
  `0600` in a `0700` directory; on Windows the per-user directory ACL is the protection.
- `config.toml` stores only the variable *name* (`model.api_key_env`), never a key.
- Because `secrets.env` only fills names that are not already set, an `OPENAI_API_KEY` exported by
  a supervisor always wins.
- Keys typed into the web UI go to `<data_dir>/auth.json` instead
  ([provider-setup.md](provider-setup.md#signing-in)).
- Prefer `SILVER_BEARER_TOKEN` in `secrets.env` over `server.bearer_token` in `config.toml`.

## Environment variables

| Variable | Effect |
| --- | --- |
| `SILVER_BIND` | `server.bind` |
| `SILVER_DATA_DIR` | `data.directory` |
| `SILVER_MODEL` | `model.name` |
| `SILVER_MODEL_PROVIDER` | `model.provider` (a preset id) |
| `SILVER_MODEL_BASE_URL` | `model.base_url` |
| `SILVER_BEARER_TOKEN` | `server.bearer_token`; every request must then send `Authorization: Bearer <token>` |
| `SILVER_MAX_CONCURRENT_RUNS` | `server.max_concurrent_runs` |
| `SILVER_CONFIG`, `SILVER_CONFIG_DIR`, `SILVER_PROFILE` | file locations, above |
| `SILVER_YOLO_MODE=1` | same as `--yolo` |
| `SILVER_CA_BUNDLE` | CA bundle for outbound TLS when `security.ca_bundle` is unset; `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `CURL_CA_BUNDLE` are tried after it |
| `SILVER_SKILLS_DIR`, `SILVER_AGENTS_DIR` | global skills and subagent directories ([tools.md](tools.md)) |
| `OPENAI_API_KEY` (or the preset's key variable) | the provider key named by `model.api_key_env` |
| `SILVER_API_KEY` | key for the `custom` preset |
| `BRAVE_API_KEY`, `TAVILY_API_KEY` | `web_search` backends |
| `OPENROUTER_API_KEY` | Jev hints, when no stored OpenRouter key exists |
| `SILVER_OAUTH_<P>_*` | the generic OAuth flow, see [provider-setup.md](provider-setup.md#oauth) |
| `RUST_LOG` | tracing filter |

## config.toml

Every key is optional and unknown keys are ignored. An invalid value stops startup with a message
naming the key: a non-loopback bind without a 16-character token, an `approval_timeout_seconds` of
0, an unknown `reasoning_effort`, a duplicate MCP server name.

```toml
[server]
bind = "127.0.0.1:7777"
max_concurrent_runs = 4
run_timeout_seconds = 1800      # hosted models only; a local run has no wall-clock limit
max_message_bytes = 1048576
request_body_limit_bytes = 2097152
# bearer_token = "..."          # prefer SILVER_BEARER_TOKEN in secrets.env
# cors_allowed_origins = []     # empty disables CORS
# rate_limit_per_minute = 0     # 0 disables the limiter

[data]
# directory = "/home/me/.local/share/silver"

[model]
provider = "openai"
name = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"  # the variable NAME, never the value
# kind, base_url, context_length, reasoning_effort, reasoning_budget: see below

[tools]
write_requires_approval = true
command_requires_approval = true
```

### `[server]`

| Key | Default | Notes |
| --- | --- | --- |
| `bind` | `127.0.0.1:7777` | A non-loopback bind requires `bearer_token` of at least 16 characters. |
| `max_concurrent_runs` | 4 | Bounds execution, not creation: a run over capacity still returns 202 and waits. |
| `run_timeout_seconds` | 1800 | Wall clock for a hosted-model run. |
| `max_message_bytes` | 1 MiB | Larger messages are refused with `context_too_large`. |
| `request_body_limit_bytes` | 2 MiB | Always enforced. |
| `bearer_token` | none | Compared in constant time. Without one every request is accepted. |
| `cors_allowed_origins` | `[]` | Bare `scheme://host[:port]` origins, or `"*"`. |
| `rate_limit_per_minute` | 0 | Per-client token bucket; 429 with `Retry-After`. Runs outside auth. |

### `[model]`

| Key | Default | Notes |
| --- | --- | --- |
| `provider` | `openai` | A preset id. |
| `kind` | from the preset | `openai_compatible`, `anthropic`, `ollama`, `copilot`, `bedrock`, `vertex`, `codex`, `acp`, `opencode`. |
| `name` | `gpt-4o-mini` | Model id; empty uses the preset's default. |
| `base_url` | from the preset | Any OpenAI-compatible or Anthropic endpoint. |
| `api_key_env` | `OPENAI_API_KEY` | Variable name. |
| `context_length` | detected | Overrides detection for this model ([how it is detected](provider-setup.md#context-window)). |
| `reasoning_effort` | unset | `none`, `minimal`, `low`, `medium` or `high`; also set per chat in the model picker. |
| `reasoning_budget` | 4096 local, off hosted | Reasoning tokens one call may spend before silver cuts it off and tells the model to act. `0` disables. |
| `fallback`, `credentials` | none | `[[model.fallback]]` and `[[model.credentials]]`, see [provider-setup.md](provider-setup.md#reliability). |

### External agent modes (ACP)

`kind = "acp"` spawns an external coding agent (Claude Code, OpenCode, Grok Build, …) over the
Agent Client Protocol instead of calling an API. The base URL is the command to spawn:
`base_url = "opencode acp"` runs that CLI on this machine. With no base URL, the model name picks
an agent mode from the catalog and silver finds that CLI itself on your login shell's PATH
(`claude`, `opencode`, `grok`, …); a mode whose CLI — or whose CLI's ACP support — is missing
fails its first message with the install steps. The catalog, install and sign-in commands for
every mode are in [provider-setup.md](provider-setup.md#external-agent-modes-acp). The spawned
agent keeps its own loop and tools; silver only relays its prose ([transports](provider-setup.md#transports)).

### `[tools]`

| Key | Default | Notes |
| --- | --- | --- |
| `write_requires_approval`, `command_requires_approval` | `true` | See [Approvals](tools.md#approvals). |
| `approval_mode` | `manual` | `manual`, `smart` or `off`. |
| `approval_timeout_seconds` | 300 | An unanswered approval counts as a denial. |
| `deny_commands` | `[]` | Glob patterns refused before the approval gate. |
| `tool_timeout_seconds` | 120 | Watchdog over every tool call; `bash` may ask for a longer `timeout`. |
| `enabled`, `disabled` | see below | Tool-name allow- and deny-list. |
| `default_toolsets`, `disabled_toolsets` | `[]`, `["web"]` | Toolset filters. |
| `env_passthrough` | `[]` | Extra variable names copied into commands the model runs. |
| `max_output_bytes` | 1 MiB | Caps only the workspace picture endpoint today; tool results are bounded by each tool. |

The default `disabled` is `run_command`, `execute_code`, `process_manage`, `skill_manage`,
`skills_list`. Clear it to expose every tool; see [Which tools a run
sees](tools.md#which-tools-a-run-sees).

### `[agent]`

| Key | Default | Notes |
| --- | --- | --- |
| `max_iterations` | 500 | Model calls per run. At the limit tools turn off and the model gets one call to answer. |
| `max_tool_calls_per_run` | 200 | Reaching it halts tools the same way. |
| `model_timeout_seconds` | 120 | Silent-stream budget for a hosted model. |
| `local_model_timeout_seconds` | 900 | The same for a local endpoint (see below). |
| `turn_liveness_timeout_seconds` | 600 | A turn with no activity this long is cancelled; `0` disables. |
| `turn_liveness_poll_seconds` | 15 | |
| `empty_guard_enabled` | `true` | Retries an empty completion up to 3 times, then fails the run. |
| `tool_call_hard_stop` | `true` | Stops a run that keeps failing one tool (8 failures) or repeats an identical failing call. `false` makes the guards warn only. |
| `summarize_with_main_model` | `false` | Summarise dropped context with the main model when there is no `[auxiliary]` route. |
| `jev_hints` | `false` | Opt-in hints from the Jev classifier on OpenRouter. Sends the task and excerpts of commands and output to OpenRouter ([how it works](architecture.md#1412-verify-on-stop-and-jev-hints)). |

`empty_cost_threshold_usd` is accepted but has no effect: the empty-response guard never receives
a cost estimate.

A **local endpoint** is a loopback, RFC 1918, link-local, Tailscale, `*.local`, unqualified or
container-internal host. LM Studio emits a tool call as one event only after generating all of it,
so a 4B model writing a 60 KB `write_file` is silent for minutes. A local stream is therefore
declared stalled after `local_model_timeout_seconds`, not `model_timeout_seconds`, the run has no
wall-clock limit, and the turn sends a `run.waiting` notice after 60 s.

### Other sections

| Section | Default | What it does |
| --- | --- | --- |
| `[auxiliary]` | off | A side model (`enabled`, `model`, `provider`, `base_url`, `api_key_env`) for summaries, session titles and `smart` approvals. |
| `[memory]` | 64 KiB | `max_prompt_bytes_per_file`: how much of `MEMORY.md` / `USER.md` enters the prompt. |
| `[delegation]` | on | Subagent limits, see [tools.md](tools.md#subagents). |
| `[moa]` | off | Mixture of Agents, see [provider-setup.md](provider-setup.md#mixture-of-agents). |
| `[checkpoints]` | on | `max_snapshots = 50`, `max_bytes = 64 MiB`. |
| `[lsp]` | on | `servers` (empty auto-detects), `timeout_seconds = 10`. |
| `[worktree]` | `root = ".worktrees"` | Where `/v1/worktrees` and `/worktree` create git worktrees, relative to the repository root. Its `enabled` key is not read. |
| `[[mcp.server]]` | none | `name`, `enabled`, `transport` (`stdio` or `http`), `command`/`args`/`env`, `url`/`headers`, `allowed_tools`, `timeout_seconds` (30). |
| `[cost]` | off | `max_usd_per_day`, `max_usd_per_run`, `warn_ratio = 0.8`. A run that would exceed a cap is refused before it starts. Prices come from models.dev; an unpriced model never counts. |
| `[oauth]` | on | `token_store` (default `<data_dir>/oauth.json`). |
| `[security]` | see below | `ssl_verify = true`, `ca_bundle`, `website_blocklist`, `block_sensitive_query_urls = false`. |
| `[monitoring]` | off | With `enabled` and an `endpoint`, run events are POSTed there as JSON through a bounded queue that drops when full. `redact = true`. |

`config_version` at the top of the file is managed by silver: a file from an older build is
migrated in memory on load, and a file from a newer build is never downgraded.
