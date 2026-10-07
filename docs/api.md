# HTTP API

The same port serves the API (`/health`, `/v1/*`) and the web UI (every other path). The UI is a
client like any other: anything it does, you can do with `curl`. Routes live in
`apps/silver/src/api/mod.rs`; wire types in `crates/silver-protocol`.

All routes are JSON except the SSE stream. Dates are RFC 3339 UTC and ids are opaque prefixed
strings. When `server.bearer_token` is set, every `/health` and `/v1/*` request needs
`Authorization: Bearer <token>`; UI paths do not.

## Starting a run

    curl -s localhost:7777/v1/runs -H 'content-type: application/json' -d '{
      "workspace_id": "ws_...",
      "message": { "content": [ { "type": "text", "text": "fix the parser bug" } ] }
    }'
    # 202 {"run_id": "...", "session_id": "...", "status": "queued", "events_url": "..."}
    curl -N localhost:7777/v1/runs/<run_id>/events

Body fields: `workspace_id` (omit or `null` for the global scope), `session_id` (omit to create
one; `source` + `external_key` reuse a session by identity), `message`, and optional `model`,
`preset`, `yolo`, `goal_budget` (makes the message the session's goal; `0` means the default 20)
and `plan_mode: true`. An `Idempotency-Key` header replays the existing run for the same session
and key. One run per session at a time: a second returns `session_busy`.

## Routes

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | `{status, database, active_runs, paused, uptime_seconds}`; `database` reflects a real query |
| GET | `/v1/capabilities` | Server and protocol version, configured provider and model, the tools a run would get (name, risk, `requires_workspace`), limits, features |
| GET | `/v1/tools` | Every registered tool with its toolset and whether the `[tools]` filters leave it `enabled` |
| GET | `/v1/skills` | Every listable skill: name, description, category |
| GET | `/v1/commands` | The slash-command catalog the web UI renders |
| POST | `/v1/workspaces` | Register `{name, path}` (`path` absolute or `~`-prefixed); 201; 400 for a relative or missing folder; 409 `conflict` on a duplicate name or root |
| GET | `/v1/workspaces` | List workspaces |
| GET | `/v1/workspaces/{id}` | One workspace |
| DELETE | `/v1/workspaces/{id}?force=bool` | Remove a registration (never the folder); refuses a workspace with sessions or runs unless `force`; 204 |
| POST | `/v1/workspaces/pick` | Open the desktop folder dialog on the machine running silver; `{path}`, or `{path: null}` if cancelled; 4xx if there is no dialog |
| POST | `/v1/workspaces/{id}/attachments` | Store `{name, data}` (base64) under `.silver/attachments/`; 201 `{path, name, bytes}` |
| GET | `/v1/workspaces/{id}/files?path=…` | One workspace picture (png, jpg, gif, webp) for the UI; confined to the root and capped by `tools.max_output_bytes` |
| GET | `/v1/workspaces/{id}/memory?q=` | The workspace's shared ai-memory pages (`{workspace, project, pages}`), recent first, or search hits when `q` is given; 503 when memory is off |
| GET | `/v1/workspaces/{id}/memory/page?path=…` | One memory page's Markdown body, read from the ai-memory server |
| POST | `/v1/sessions` | Create `{workspace_id?, source?, external_key?, title?}`; 201 |
| GET | `/v1/sessions?workspace_id=…` or `?scope=global` | Sessions in exactly one scope, newest first; `limit`, `cursor` (`<updated_at>\|<id>`), and `q=` to match titles and message text |
| GET | `/v1/sessions/{id}` | One session, with its `goal`, `active_run` and `plan_mode` |
| PATCH | `/v1/sessions/{id}` | Set `title`, `model`, `yolo_mode`, `preset` (blank clears), `plan_mode`, or `goal`: `"pause"`, `"resume"`, `"clear"` or `{"budget": N}` |
| DELETE | `/v1/sessions/{id}` | Delete; 204 |
| GET | `/v1/sessions/{id}/messages?limit=` | Messages oldest first (default 100, max 500) |
| GET | `/v1/sessions/{id}/usage` | Token totals, cost, a per-run breakdown and the latest `context` |
| GET | `/v1/sessions/{id}/plan` | The plan file: `{path, content?}` |
| GET | `/v1/sessions/{id}/injected` | Every `advisor.checked`, `context.injected` and `subagent.*` event, oldest first |
| GET | `/v1/sessions/{id}/trace?format=json\|jsonl` | The redacted transcript for export |
| POST | `/v1/sessions/{id}/rewind` | Remove the newest user turn and everything after it; returns the removed text. Blocked while a run is active |
| POST | `/v1/runs` | Create a run (above); 503 `daemon_paused` while the emergency stop is engaged |
| GET | `/v1/runs/{id}` | Run state |
| GET | `/v1/runs/{id}/events` | SSE stream; `Last-Event-ID` replays |
| POST | `/v1/runs/{id}/stop` | Idempotent cancellation; returns the known run |
| POST | `/v1/runs/{id}/steer` | Queue guidance for an active run; 202; `run_not_active` otherwise |
| POST | `/v1/runs/{id}/approval` | Decide `{approval_id, decision, answer?}`; `decision` is `approve`, `approve_session`, `approve_always` or `deny`; `answer` with `approve` replies to an `ask_user_question`; 204 |
| POST | `/agent` | The [AG-UI](agui.md) endpoint: POST a `RunAgentInput`, stream AG-UI events back over SSE |
| POST | `/v1/daemon/pause` | Emergency stop: refuse new runs, let running ones drain. Persists across restarts |
| POST | `/v1/daemon/resume` | Release it |
| GET | `/v1/daemon/status` | `{paused}` |
| GET | `/v1/approvals` | `{mode, frozen}`: the effective approval mode, and whether `--yolo` pinned it |
| POST | `/v1/approvals` | Persist a new global `{mode: "manual"\|"smart"\|"off"}`; refused while pinned |
| GET, POST | `/v1/server` | `{run_timeout_seconds}`: read or change the hosted-run wall-clock budget (Settings → General); persisted to `config.toml` |
| GET, POST | `/v1/advisor` | Jev hints: read `{enabled, has_key, questions}`; switch with `{enabled}` (saved as `agent.jev_hints`) |
| GET | `/v1/models?provider=` | The models the endpoint advertises (`{provider, default, authenticated, models}`); empty when it has no catalog. For an [agent mode](provider-setup.md#external-agent-modes-acp) they are the agent's own, read by starting it, and only when `provider` is named; `efforts` lists the levels its `effort` option offers |
| GET | `/v1/auth` | Every provider with its endpoint, model, whether a run could authenticate, where the credential comes from (`stored`, `oauth`, `env`, `none`), `configured`, and `installed` for an [agent mode](provider-setup.md#external-agent-modes-acp) (its CLI can be launched here; `null` for any other preset) |
| POST | `/v1/auth/{provider}` | Store `{api_key?, base_url?, model?, activate?}`; the key is never echoed |
| POST | `/v1/auth/{provider}/activate` | Route new runs through a signed-in provider; 400 if not signed in |
| DELETE | `/v1/auth/{provider}` | Forget a credential; 204 |
| POST | `/v1/oauth/{provider}/login` | Start a device or PKCE sign-in |
| GET | `/v1/oauth/{provider}/poll` | Advance an in-flight sign-in one step |
| GET | `/v1/oauth/status` | One entry per provider holding a grant |
| POST | `/v1/oauth/{provider}/logout` | Drop the grant and any in-flight login |
| GET | `/v1/provider/status` | The configured provider and its last-seen rate-limit headers |
| GET | `/v1/insights?days=N` | Runs aggregated per model and UTC day (default 30 days) |
| GET | `/v1/presets` | Minimal, Pi, then custom presets |
| POST | `/v1/presets` | Create `{name, tools, skills}`; 201 |
| PUT, DELETE | `/v1/presets/{id}` | Replace or delete a custom preset (built-ins and unknown ids are 400); sessions on a deleted preset move to Minimal |
| GET | `/v1/agents?workspace_id=…` | The subagent catalogue: built-ins first, then custom files |
| GET | `/v1/agents/{name}` | One agent plus the `markdown` to edit it with |
| POST | `/v1/agents?name=…&scope=global\|project` | Write a definition `{markdown}`; parsed first, so a bad one is a 400 and never reaches disk; 201 |
| PUT, DELETE | `/v1/agents/{name}?scope=…` | Replace or delete a custom definition; a built-in is refused |
| GET | `/v1/checkpoints?session_id=…&limit=…` | File snapshots taken before edits |
| POST | `/v1/checkpoints/{id}/restore` | Restore one into its workspace |
| GET | `/v1/diff?workspace_id=…&scope=working\|staged\|all\|session&stat=&path=` | The workspace's git diff |
| GET, POST | `/v1/chat/bots` | The [Messages](messages.md) roster `{bots}`, or create `{kind?, name, description?, instructions?, avatar_shape?, avatar_color?, provider?, model?, reasoning_effort?, workspace_id?, yolo?, members?}`; 201. `kind: "group"` needs `members`, and the same members return the group they already share |
| PATCH, DELETE | `/v1/chat/bots/{id}` | Change any of those fields and `pinned` (a blank `provider`, `model`, `workspace_id` or `reasoning_effort` clears it; a new folder starts a new session), or delete a bot with its chat and sessions; 204 |
| GET | `/v1/chat/bots/{id}/entries?thread=&before=&limit=` | The newest page of a chat, or of the thread on entry `thread`, oldest first; roots carry their thread `summary`. `before` is a `seq` |
| POST | `/v1/chat/bots/{id}/send` | `{text, thread_id?, nonce?}`: store the message and start whoever answers it; 201. A repeated `nonce` returns the first message |
| POST | `/v1/chat/bots/{id}/stop` | Stop a bot or group and drop what is queued; 204 |
| POST | `/v1/chat/bots/{id}/read` | `{thread_id?}`: mark the chat, or one thread, as read; 204 |
| POST | `/v1/chat/bots/{id}/new-session` | Start the bot over with a fresh context; 204 |
| POST | `/v1/chat/entries/{id}/react` | `{emoji}`: toggle the user's reaction |
| POST | `/v1/chat/entries/{id}/answer` | `{decision, answer?}`: decide an approval card, as `/v1/runs/{id}/approval` does; 204 |
| GET | `/v1/chat/events` | SSE: `bot`, `bot_removed`, `entry` and `resync` (see below) |
| GET, POST | `/v1/worktrees` | List git worktrees of a workspace, or create `{workspace_id, name?, sync?}` |
| DELETE | `/v1/worktrees/{name}?workspace_id=…&force=` | Remove one |

Anything else is the web UI: known files from the embedded bundle, a missing `/assets/*` file is a
404, and every other path returns `index.html`.

## Errors

One envelope. `details` is omitted when empty and `request_id` is never populated:

    { "error": { "code": "path_outside_workspace", "message": "..." } }

Codes: `workspace_not_found`, `workspace_unavailable`, `path_outside_workspace` (403),
`session_not_found`, `session_workspace_mismatch` (409), `session_busy`, `conflict` (409),
`run_not_found`, `run_not_active`, `approval_not_found`, `approval_stale`, `tool_not_allowed`,
`tool_timeout`, `provider_unavailable`, `provider_rate_limited`, `context_too_large`,
`daemon_restarted`, `invalid_request` and `internal`. Two more are plain strings outside that
enum: `daemon_paused` (503) and `rate_limited` (429, with `Retry-After`). `approval_stale` is
defined but nothing produces it. Internal causes are never sent to clients.

## Events

`GET /v1/runs/{id}/events` sends `id: <event_id>`, `event: <name>` and `data: <RunEvent JSON>`;
every payload carries `run_id`, `event_id`, `created_at` and a `type` equal to the event name.
Keep-alive is an SSE comment every 15 s.

| Event | Stored | Meaning |
| --- | --- | --- |
| `run.queued` | yes | Run accepted, with session and workspace |
| `run.started` | yes | Model and start time |
| `context.updated` | yes | How full the context budget is for the request about to go out; repeated with `prompt_tokens` once the provider has counted |
| `text.started` | yes | An agent began a new message; only its last message is the reply, so the text before it is dropped rather than streamed |
| `text.delta` | no | Incremental assistant text |
| `reasoning.delta` | no | Incremental model reasoning; clients show a thinking row and drop it when text or a tool call begins |
| `text.completed` | yes | Final assistant text |
| `tool.started` | yes | Call id, tool name, sanitised arguments (strings cut at 4,000 characters) |
| `tool.completed` | yes | Call id, status (`completed`, `failed`, `denied`, `blocked`) and the result, long strings cut in the middle at 4,000 characters |
| `approval.required` | yes | Approval id, call id, name, risk, description, argument preview |
| `approval.resolved` | yes | Approval id and the decision |
| `run.waiting` | yes | Why the turn is waiting: a provider retry, compaction, an iteration-budget notice, a dropped picture, or a model stream silent for 60 s |
| `steer.delivered` | yes | A queued steer message reached the model |
| `advisor.checked` | yes | A Jev check: `point` (`start`, `tools`, `answer`), the yes-probability per question, and the hints handed to the model |
| `context.injected` | yes | Text the model got that neither you nor a tool wrote, exactly as sent, with a `label`: the system prompt, each file loaded into it, every loop notice |
| `plan_mode.exited` | yes | You approved the plan |
| `subagent.started` / `.step` / `.completed` | yes | One task of a `delegate_task` batch: start (agent, model, worktree), each wrapped event of its own turn, and its outcome with the report the parent reads |
| `run.completed` | yes | Token usage, `cost_usd` when the model is priced, duration |
| `run.failed` | yes | Normalised error code and message |
| `run.cancelled` | yes | Origin: `client`, `liveness` or `shutdown` |
| `replay.gap` | no | The requested replay window could not be fully served |

`tool.output` and `heartbeat` are defined in the protocol but never sent.

Every stored event is persisted before it is published, so reconnecting with `Last-Event-ID`
replays exactly what you missed and then continues live; disconnecting never cancels the run.
Text and reasoning deltas are not stored, so a reconnect can see `replay.gap` over the sequence
numbers they used. It is a signal, not an error.

A run's status is `queued`, `running`, then `completed`, `failed` or `cancelled`.
`waiting_approval` exists in the protocol but is never stored: detect an approval from
`approval.required`, not from the run status. A run found `queued` or `running` after a restart
is marked `failed` with `daemon_restarted`; nothing resumes automatically.

## Bot chat

The [Messages](messages.md) mode is a client of `/v1/chat/*`. A **bot** has `kind` (`agent` or
`group`), the fields above, and what it is doing: `status` (`idle`, `working`, `needs_input` or
`error`), `activity`, the latest `thinking`, `working_chat` / `working_thread` (where the turn in
progress talks), `last_message`, `last_at` and `unread`. A group's status and activity are its busy
member's. An **entry** is one thing in a chat: `kind` `user`, `agent`, `notice` or `permission`,
with `seq` (an order that pages with `before`), `thread_id` (a reply's root), `author` (the bot),
`status` (`queued`, `failed` or `cancelled`
on a user message), `style` (`error`, `divider` or `request` on a notice), `reactions`, `thread`
(`{count, last_at, authors, unread}` on a root), `permission` (an approval card) and the `run_id` and
`session_id` that wrote it; `GET /v1/sessions/{session_id}/messages` has what that run did. A bot's
reply also has `limits`, when its provider has usage limits we can read: the windows as they were
when it was written, the session first, each `{name, percent, resets_at}` (`percent` is 0 to 100,
`resets_at` unix milliseconds). An agent's own row has `limits` too: the latest reading. See
[usage limits](messages.md#usage-limits).

`GET /v1/chat/events` carries `data: {"type": …}` frames named like the type: `bot` (a whole bot),
`bot_removed {id}`, `entry` (a whole entry) and `resync`, which says the stream fell behind. A client
connects first, then loads `/v1/chat/bots` and the entries it shows, and upserts by `id`; it reloads
on `resync` and whenever it reconnects. Nothing is replayed.
