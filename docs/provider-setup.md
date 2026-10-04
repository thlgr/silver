# Providers and models

How silver reaches a model: the preset catalog, the transports behind it, signing in, and the
settings that keep a run going when a provider misbehaves. File locations, precedence and the rest
of `config.toml` are in [configuration.md](configuration.md).

## Choosing a provider

There are two ways, and the second needs no restart:

- **Web UI**: Settings → Providers (or `/login`). Pick a preset, enter a key or complete its OAuth
  flow, and it takes effect on the next turn.
- **Files**: set `[model]` in `config.toml` and put the key in `secrets.env`:

      [model]
      provider = "anthropic"
      name = "claude-sonnet-4-5"
      api_key_env = "ANTHROPIC_API_KEY"

      # secrets.env
      ANTHROPIC_API_KEY=sk-ant-...

With nothing configured silver starts on `openai` and logs that the key is not set, so you can
sign in from the web UI, add it to `secrets.env` or export the variable.

## Preset catalog

The catalog is `crates/silver-protocol/src/providers.rs`; that file is authoritative for base
URLs, default models and key variables. A preset whose default model is empty leaves the choice
to sign-in time: silver asks the endpoint's own `/models` listing, and `/model` can name one
explicitly.

| Group | Preset ids |
| --- | --- |
| Frontier APIs | `openai`, `anthropic`, `google`, `xai`, `mistral`, `deepseek`, `moonshot`, `moonshot-cn`, `zai`, `minimax`, `minimax-cn`, `alibaba`, `alibaba-coding-plan`, `meta-ai`, `xiaomi`, `stepfun`, `upstage`, `arcee` |
| Routers and hosts | `openrouter`, `groq`, `together`, `cerebras`, `fireworks`, `deepinfra`, `nvidia`, `novita`, `nebius`, `huggingface`, `ai-gateway`, `gmi`, `kilocode`, `ollama-cloud`, `tencent-tokenhub`, `tencent-tokenplan`, `router`, `commandcode`, `actual`, `nous`, `qwen-portal` |
| Subscriptions and cloud SDKs | `copilot`, `openai-codex`, `opencode-zen`, `opencode-go`, `bedrock`, `vertex`, `azure-foundry`, `copilot-acp` |
| External agent modes (ACP) | `claude`, `cursor`, `pi`, `opencode`, `grok`, `gemini`, `qwen`, `goose`, `kimi`, `droid`, `amp`, `kilo`, `cline`, `auggie`, `vibe`, `kiro`, `devin`, `qoder`, `codebuddy`, `minimax-code`, `junie`, `antigravity`, `cortex`, `poolside` |
| Local | `ollama` (`:11434`), `lmstudio` (`:1234`), `llamacpp` (`:8080`), `vllm` (`:8000`) |
| Anything else | `custom`: you supply the base URL and model; the key variable is `SILVER_API_KEY` |

Key variables follow `<NAME>_API_KEY` (`OPENAI_API_KEY`, `GROQ_API_KEY`, …) with a few exceptions
(`GEMINI_API_KEY` for `google`, `HF_TOKEN`, `ARCEEAI_API_KEY`, `AWS_BEARER_TOKEN_BEDROCK`,
`COPILOT_GITHUB_TOKEN`). The catalog covers every provider upstream Hermes reaches with a key or
bearer token; where our id differs (`openai-api` → `openai`, `gemini` → `google`, `kimi-coding` →
`moonshot`, `nebius-token-factory` → `nebius`, `qwen-oauth` → `qwen-portal`) the mapping is
asserted in `providers.rs::UPSTREAM_PARITY`. `xai-oauth` and `minimax-oauth` are covered by the
key-based `xai` and `minimax`; their subscription OAuth flows are not implemented.

The daemon listens on `127.0.0.1:7777`, deliberately clear of llama-server's default `:8080`, so
the `llamacpp` preset works against a stock server.

## Transports

`model.kind` picks the wire protocol; a preset sets it for you.

| Kind | Used by | How it talks |
| --- | --- | --- |
| `openai_compatible` | most presets, `custom`, `llamacpp`, `vllm` | `POST {base_url}/chat/completions` streaming |
| `ollama` | `ollama`, `lmstudio` | the same transport with no required key |
| `anthropic` | `anthropic`, `minimax-cn`, `tencent-tokenplan` | `POST {base_url}/messages` with `x-api-key` and `anthropic-version: 2023-06-01`; `max_tokens` defaults to 4096 |
| `copilot` | `copilot` | A GitHub token (`gh auth token`, `GITHUB_TOKEN`, a PAT) is exchanged at `api.github.com/copilot_internal/v2/token` for a ~30-minute bearer, re-minted two minutes before expiry; Enterprise and proxied accounts route to the host the exchange names |
| `bedrock` | `bedrock` | SigV4-signed POST to `bedrock-runtime.<region>.amazonaws.com`, Anthropic Messages body, binary `vnd.amazon.eventstream` response. Credentials: `access-key:secret[:session-token]`, a Bedrock API key, or the `AWS_*` variables. Region from the base URL, then `AWS_REGION` |
| `vertex` | `vertex` | RS256-signed service-account JWT exchanged for an OAuth2 token (cached until expiry), or an access token, `GOOGLE_APPLICATION_CREDENTIALS`, or the metadata server. Claude models use `:streamRawPredict`; others the OpenAI-compatible `endpoints/openapi` surface. Project and region from the base URL, then `VERTEX_PROJECT`/`GOOGLE_CLOUD_PROJECT` and `VERTEX_REGION` |
| `codex` | `openai-codex` | ChatGPT sign-in (`/login openai-codex`): PKCE against `auth.openai.com` on `http://localhost:1455/auth/callback`, then the Responses API |
| `acp` | `copilot-acp`, the external agent modes | Spawns the program named by the base URL — a stored command, a mode's own CLI, or `copilot --acp --stdio` (overridden by `COPILOT_CLI_PATH`) — and speaks newline-delimited JSON-RPC on stdio |
| `opencode` | `opencode-zen`, `opencode-go` | One host, per-model API: Claude and Qwen on `/messages`, GPT, Grok and Muse on `/responses`, open models on `chat/completions`. Each request carries the conversation's cache key as `x-opencode-session` to keep the relay's prompt cache warm. Needs its own key (`OPENCODE_ZEN_API_KEY` / `OPENCODE_GO_API_KEY`); anonymous access ended |

**`copilot-acp` runs its own loop.** silver's tools are not offered to the external agent, its tool
calls never reach silver's approval gate, and only its prose comes back (its thinking and tool
activity show as reasoning). One ACP session is kept per silver session, so after the first turn
only the newest message is sent. A model written `provider/model` is selected through the agent's
own `model` option.

All transports share one TLS policy (`[security]`), redact the key from error bodies and truncate
them, and never echo a 401/403 body.

## External agent modes (ACP)

Besides `copilot-acp`, silver can drive any of the coding-agent CLIs in the catalog as an ACP
agent, spawning the CLI itself (local launch only — no npx/uvx registry downloads). The agent's
session starts in the run's workspace, so a [Messages](messages.md) bot works in its own folder. Pick one in
Settings → Providers like any preset; it needs no API key of silver's, the CLI holds its own
credentials. A mode is used when its preset is active and no base URL override is stored: silver
finds the CLI on your login shell's PATH (plus the usual install dirs, so a GUI-launched daemon
sees it too) and runs it in its ACP mode. A mode whose CLI, or ACP support, is missing fails its
first message with the install steps instead of a bare spawn error; `base_url` in `config.toml`
([example](#configuration)) overrides the resolution with an explicit command.

| Mode preset | Install | Sign in |
| --- | --- | --- |
| `claude` (Claude Code) | `curl -fsSL https://claude.ai/install.sh \| bash` | `claude auth login` |
| `cursor` (Cursor) | `curl https://cursor.com/install -fsS \| bash` | `cursor-agent login` |
| `pi` (Pi) | `npm install -g @earendil-works/pi-coding-agent` | run `pi`, type `/login` |
| `opencode` (OpenCode) | `curl -fsSL https://opencode.ai/install \| bash` | `opencode auth login` |
| `grok` (Grok Build) | `curl -fsSL https://x.ai/cli/install.sh \| bash` | `grok login --device-auth` |
| `gemini` (Gemini CLI) | `npm install -g @google/gemini-cli` | run `gemini` |
| `qwen` (Qwen Code) | `npm install -g @qwen-code/qwen-code` | run `qwen`, type `/auth` |
| `goose` | `curl -fsSL https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh \| bash` | `goose configure` |
| `kimi` (Kimi Code) | `curl -fsSL https://code.kimi.com/kimi-code/install.sh \| bash` | `kimi login` |
| `droid` (Factory Droid) | `npm install -g droid` | run `droid` |
| `amp` (Amp) | `curl -fsSL https://ampcode.com/install.sh \| bash` | `amp login` |
| `kilo` (Kilo) | `npm install -g @kilocode/cli` | `kilo auth login` |
| `cline` (Cline) | `npm install -g cline` | `cline auth` |
| `auggie` (Auggie) | `npm install -g @augmentcode/auggie` | `auggie login` |
| `vibe` (Mistral Vibe) | `curl -LsSf https://mistral.ai/vibe/install.sh \| bash` | `vibe --setup` |
| `kiro` (Kiro CLI) | `curl -fsSL https://cli.kiro.dev/install \| bash` | `kiro-cli login` |
| `devin` (Devin) | `curl -fsSL https://cli.devin.ai/install.sh \| bash` | `devin auth login` |
| `qoder` (Qoder CLI) | `npm install -g @qoder-ai/qodercli` | `qodercli login` |
| `codebuddy` (CodeBuddy Code) | `npm install -g @tencent-ai/codebuddy-code` | run `codebuddy` |
| `minimax-code` (MiniMax Code) | `npm install -g @minimax-ai/code` | `mcode login` |
| `junie` (Junie) | `curl -fsSL https://junie.jetbrains.com/install.sh \| bash` | run `junie` |
| `antigravity` (Google Antigravity) | `curl -fsSL https://antigravity.google/cli/install.sh \| bash` | run `agy` |
| `cortex` (Cortex Code) | `curl -LsS https://ai.snowflake.com/static/cc-scripts/install.sh \| sh` | run `cortex` |
| `poolside` (Poolside) | `curl -fsSL https://downloads.poolside.ai/pool/install.sh \| sh` | `pool login` |

**Claude Code** has no ACP mode in its own CLI, so silver runs it through the official adapter,
`@agentclientprotocol/claude-agent-acp` (pinned to the version the
[ACP registry](https://agentclientprotocol.com/registry) lists), which `npx` fetches on first use.
It needs Node.js and the `claude` CLI signed in (`claude auth login`); the adapter uses that
sign-in, so there is no key to give silver. Claude Code asks before it edits files or runs
anything but read-only commands. Those requests reach you as approval cards in a
[Messages](messages.md#other-coding-agents) bot, and are refused anywhere else.

## Signing in

Settings → Providers stores credentials in `<data_dir>/auth.json` (written through a private temp
file and atomic rename, mode `0600`):

    {"active": "<provider>", "providers": {"<id>": {"api_key": …, "base_url": …, "model": …}}}

- **The menu** lists every preset with a search box. Signing in stores a key or starts the OAuth
  flow; a signed-in provider can be made active; signing out (`/logout <provider>`) forgets it.
- **Local and custom endpoints** show an editable URL (`localhost:8080/v1` is read as
  `http://localhost:8080/v1`) and an optional key for a keyless server. **Save and use** activates
  the provider and picks a model from its `/models`, or warns when the endpoint lists none.
  `custom` is refused until it has an endpoint.
- **Routing**: with a provider active, every request resolves its endpoint and credential from the
  store and the default model becomes that provider's. Without one, runs use the endpoint built
  from `config.toml` at startup.
- **Picks stick**: a model chosen in the picker or with `/model` is stored as its provider's model
  and survives restarts. The picker filters long catalogs and accepts any typed model id.
- **Sessions keep their route**: a session remembers the model and provider its last run used, and
  resuming it routes there even after another provider became active.
- **Credential precedence** for a provider: the stored key, then an OAuth grant (refreshed when
  stale), then the preset's key variable. Local endpoints need none.
- **Keys never leave the store**: the key is sent once to `POST /v1/auth/{provider}` and no
  response echoes it.

### OAuth

The daemon implements `openrouter` (PKCE; the browser returns to a loopback port and the result is
a plain API key), `nous` (RFC 8628 device code) and `openai-codex` (above). The web UI polls while
the browser round trip happens. A `generic` device flow has no built-in endpoints and is configured
with environment variables, where `<P>` is the provider id upper-cased with every non-alphanumeric
byte replaced by `_` (`openai-codex` → `OPENAI_CODEX`):

| Variable | Meaning |
| --- | --- |
| `SILVER_OAUTH_<P>_CLIENT_ID`, `_CLIENT_SECRET` | OAuth client; the secret goes in HTTP Basic on the token request |
| `SILVER_OAUTH_<P>_SCOPE` | Space-delimited scopes |
| `SILVER_OAUTH_<P>_DEVICE_URL`, `_TOKEN_URL`, `_AUTHORIZE_URL` | Endpoints |
| `SILVER_OAUTH_<P>_REDIRECT_URI` | Static redirect URI |
| `SILVER_OAUTH_<P>_KEY_LABEL` | OpenRouter key label (default `silver`) |
| `SILVER_OAUTH_DEVICE_POLL_CAP_SECONDS` | Device poll interval cap (default 1) |
| `SILVER_OAUTH_PKCE_TIMEOUT_SECONDS` | Loopback callback window (default 120) |

Tokens live in `<data_dir>/oauth.json` (`0600`, atomic) and are redacted from logs.

## Reliability

- **Retries**: a transient failure (408, 409, 425, 429, 5xx, a transport error) is retried up to
  4 times with jittered exponential backoff, honouring a numeric `Retry-After`, and only while the
  stream has produced nothing. Each retry emits `run.waiting`. 401/403 and other 4xx are terminal.
- **Fallback chain**: `[[model.fallback]]` lists backup routes (`model` is required; `provider`,
  `base_url` and `api_key_env` inherit from the preset, then the primary). A recoverable error
  cools the failing route for 60 s, doubling to 4 h, and the next route is tried; with every
  route cooling the primary is tried anyway. A mid-stream error is never replayed on another
  provider, because that would duplicate output.
- **Credential pool**: several `[[model.credentials]]` entries (`api_key_env`, `weight`, `label`)
  for the primary route are tried in order. A rate-limit, billing or auth failure cools a key with
  the same 60 s → 4 h ladder. In memory only; no token refresh.
- **Rate-limit headers** from the last response are exposed at `GET /v1/provider/status`.
- **Cost cap**: `[cost]` refuses a run before it starts when a daily or per-run cap would be
  exceeded ([configuration.md](configuration.md#other-sections)).

### Mixture of Agents

Reference models brief the acting model before every turn. Each advisor answers the same
conversation with no tools and a "you are not the acting agent" framing, and the answers are
appended to the acting request as one trailing user message. Only the acting model's stream reaches
the agent loop, so tool calls, streaming and cancellation are unchanged.

    [moa]
    enabled = true
    reference_timeout_seconds = 60   # 0 waits forever

    [[moa.references]]
    provider = "groq"                # a preset id; empty means the daemon's own provider
    model = "llama-3.3-70b-versatile"

    [[moa.references]]
    provider = "deepseek"            # empty model uses the preset's default

Each advisor is one extra call per turn, billed to its provider, on the critical path up to
`reference_timeout_seconds`. An advisor that fails, times out or answers nothing is skipped; if
all are, the turn runs unbriefed. References are credentialed like the main route; one with no
credential is dropped at startup with a warning.

## Context window

The compaction budget is sized **per run** for the model that run uses, so a session override or
provider switch never keeps compacting at the startup model's window. The window comes from the
first of these that answers; each hit is remembered in `<data_dir>/models_cache.json`:

1. `model.context_length`, for the configured model only
2. a local server's native API, asked live every run because it reports the window the model was
   **loaded** with: LM Studio `GET /api/v0/models`, Ollama `GET /api/ps` and `POST /api/show`,
   llama.cpp `GET /props`
3. the models cache
4. the provider's own `/models` catalog, when it carries a context field
5. the [models.dev](https://models.dev) registry, fetched at most once a day into
   `<data_dir>/models_dev.json`, matched by provider and then by model id
6. the static family table in `silver-core`

LM Studio answers an unlisted id, such as its `local-model` placeholder, with the model it has
loaded; a run names that loaded model so its window and the model picker show what answers.

**Budget and compaction.** The budget is `window × 2.25` bytes (three bytes per token, a quarter
of the window reserved for the reply). A model on a local server works within 64k tokens however
large a window it was loaded with, because local inference slows as context grows and a tighter
32k made a 4B model invent details it had lost to compaction. When a request reaches the budget,
silver clears older tool results, then drops the oldest turns only if that is not enough, and
leaves a `[CONTEXT SUMMARY]` note (with the session's open todos). It frees a quarter of the
budget rather than just enough, so a local server keeps its prompt cache until the next compaction.
Tool results the model has not read yet are truncated rather than cleared. With an `[auxiliary]`
model (or `agent.summarize_with_main_model`) the dropped span is summarised once per run; any
failure falls back to the deterministic note. A provider context-length rejection triggers up to
two harder compactions before the run fails. Each run replays the newest 8 MiB of the session
transcript; compaction, not a message count, decides what the model stops seeing.

`context.updated` reports how full the budget is before every request and again with the
provider's own `prompt_tokens` afterwards. The web UI's `/context` and status line show that count;
`GET /v1/sessions/{id}/usage` returns the latest as `context`.

## Request shaping

- **Prompt caching**: Anthropic `cache_control` on the system block and last tool with the
  `prompt-caching-2024-07-31` header; a `prompt_cache_key` derived from the session id on the
  OpenAI-compatible and Responses surfaces. The system prompt keeps a stable prefix so caches
  survive ([system-prompt.md](system-prompt.md)).
- **Reasoning**: `model.reasoning_effort` maps to OpenAI's `reasoning_effort` or an Anthropic
  `thinking` budget (o-series and GPT-5 use `max_completion_tokens`). `reasoning_budget` is
  enforced by silver: once a call has streamed that many reasoning tokens without starting an
  answer or tool call, the stream is dropped, which stops a local server generating, and the model
  gets the tail of its thinking and is told to act. Unset it is 4096 for a local endpoint and off
  for a hosted one; after two cuts in a row the next call may think freely.
- **Usage and cost**: `cached_tokens` and `reasoning_tokens` are parsed and priced cache-aware
  from the models.dev `cost` block; `run.completed` carries `cost_usd` when the model is priced.
- **Pictures** ride the surface a provider accepts them on; see
  [tools.md](tools.md#images-and-documents).
