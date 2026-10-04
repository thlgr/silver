# silver architecture

This document describes the architecture that is actually implemented in this workspace. Where the
code differs from the original design, the code is the source of truth and the difference is
called out in [Known gaps](#13-known-gaps-between-code-and-spec). User-facing behaviour is in
[tools.md](tools.md), [configuration.md](configuration.md), [provider-setup.md](provider-setup.md),
[api.md](api.md) and [web-ui.md](web-ui.md).

## 1. The daemon-only executor invariant (INV-1)

Only the silver server executes the agent loop, tools, memory and persistence. This is enforced
by the dependency graph, not by convention:

- silver-core contains the agent loop, tools, memory and guards. Its Cargo.toml depends on
  silver-protocol, serde, tokio, tokio-util, futures, sha2, hex, tracing, async-trait, regex and
  pdf-extract (plus uuid, chrono and thiserror). It does not depend on Axum, tokio-rusqlite,
  reqwest or any binary crate.
- silver (`apps/silver`) depends on silver-core and silver-protocol and owns the API, SQLite
  persistence, the model providers, the run manager and the embedded web UI.
- The web UI (`apps/web`, Svelte + Vite) is a static bundle. `rust-embed` compiles
  `apps/web/dist` into the binary (`apps/silver/src/api/ui.rs`), and the router serves it as the
  fallback for every path outside `/health` and `/v1/*`, on the same port as the API. It holds
  presentation state only and reaches the agent through `/v1` fetches and the SSE stream.

A client therefore cannot execute a tool or touch memory except through HTTP/SSE (INV-2).

The earlier CLI/TUI client (`silver` CLI, `silver-client`) was retired; the web UI is the only
client.

## 2. Crate graph

    silver -> silver-core -> silver-protocol
    silver -> silver-protocol
    silver -> apps/web/dist (embedded at compile time)

silver-protocol is the shared contract: identifiers, Scope, RunStatus, MessageRole, ContentPart,
RiskLevel, ErrorCode, RunEvent/EventPayload, the provider preset catalog, the slash-command catalog
and all request/response DTOs. It depends only on serde, serde_json, uuid, chrono, base64 and
thiserror (see ADR 0001).

## 3. Scope isolation (INV-1..INV-10)

A run belongs to exactly one Scope:

    enum Scope { Global, Workspace(WorkspaceId) }

Scope::Global has workspace_id = None. None never means "all workspaces"; every query treats it as
the single global scope.

| Invariant | Enforcement in code |
| --- | --- |
| INV-1 daemon is the only executor | Crate dependency graph (section 1). |
| INV-2 everything is a client | The web UI is a static bundle that speaks HTTP/SSE; it links no server code. |
| INV-3 scope is never ambiguous | CreateRunRequest carries Option<WorkspaceId>; Session carries Option<WorkspaceId>. Db::list_sessions and Db::search_messages return an empty result when neither global nor a workspace is given, and use "workspace_id IS ?1" in SQL so NULL matches only NULL. |
| INV-4 no implicit context crossing | The memory scope and session-search scope come from RunContext; the session_search tool passes ctx.run.scope straight through and ignores any scope argument. find_session_by_external_key matches within one scope only. There is no global-to-workspace memory inheritance. |
| INV-5 a session never changes workspace | RunManager::resolve_session returns CoreError::SessionWorkspaceMismatch when session.workspace_id != request.workspace_id. There is no update path for a session's workspace. |
| INV-6 the client never chooses paths in a run | CreateRunRequest has workspace_id but no cwd/path field. Paths enter only inside tool arguments and are resolved by the daemon. |
| INV-7 file access is confined to the registered root | Workspace::resolve_path / confine_path (section 6). Every workspace tool calls RunContext::require_workspace first. The one path outside the root is the session's plan file, chosen by the daemon (section 12.5). |
| INV-8 memory is explicit and auditable | Memory changes only through the memory tool's add/replace/remove; a run snapshot is frozen at start; MemoryStore writes temp-file + rename with a per-scope lock. |
| INV-9 approvals are decided by the daemon | ApprovalPolicy is daemon-owned; clients only POST a decision for an existing approval_id. The registry rejects an approval that is not pending for that run. |
| INV-10 the protocol is interface-independent | EventPayload is domain facts (text.delta, tool.started, approval.required, ...); the web UI renders them. |

## 4. Run lifecycle

### 4.1 Declared state machine

silver-protocol declares RunStatus { Queued, Running, WaitingApproval, Completed, Failed,
Cancelled } and RunStatus::can_transition_to:

    Queued          -> Running | Cancelled
    Running         -> WaitingApproval | Completed | Failed | Cancelled
    WaitingApproval -> Running | Cancelled
    any             -> itself
    everything else -> false

Completed, Failed and Cancelled are terminal (is_terminal).

### 4.2 What the daemon actually does

The daemon does not call can_transition_to and does not drive WaitingApproval. It writes status
directly through SQL:

1. RunManager::create_run inserts a run with status queued, then returns 202 with the run id.
2. The spawned worker acquires a semaphore permit and calls Db::start_run, setting status = running
   and started_at.
3. The worker runs the agent turn. If a tool needs approval, the agent blocks inside the turn while
   the DB row stays "running". No update_run_status call exists in production, so WaitingApproval
   is never persisted. Clients detect approval from the approval.required SSE event, not from run
   status.
4. When the turn returns, Db::finish_run writes completed, failed or cancelled plus finished_at and
   the optional error code/message.

So the effective lifecycle is queued -> running -> {completed | failed | cancelled}. WaitingApproval
and can_transition_to are never used by the daemon. This is recorded as a gap in section 13.

### 4.3 Run creation and concurrency

RunManager::create_run performs, in order:

1. Reject a message whose serialised size exceeds server.max_message_bytes with ContextTooLarge.
2. resolve_workspace: load the workspace or return WorkspaceNotFound; re-canonicalise the
   canonical root and return WorkspaceUnavailable if it moved since registration.
3. resolve_session:
   - with a session_id: load it or SessionNotFound; reject a workspace mismatch;
   - without one but with external_key: reuse an existing session for (source, external_key, scope)
     if present;
   - otherwise create a session bound to the requested scope.
4. Take the in-memory per-session lock (HashSet<SessionId>). A second concurrent create returns
   SessionBusy.
5. Re-check Db::has_active_run (queued, running or waiting_approval). If active, release the lock and
   return SessionBusy.
6. Insert the run and insert an ActiveRun { broadcast::Sender, RunControl } into the registry.

One active run per session is the MVP rule. Runs in different sessions may run in parallel, bounded
by a global tokio Semaphore of server.max_concurrent_runs. The permit is acquired inside the worker,
so a run over capacity still returns 202 immediately and waits; max_concurrent_runs bounds
execution, not creation. On completion the worker removes the run from the registry and releases the
session lock.

Goals: a session's goal is JSON in sessions.goal ({objective, status, used, max}), set by a
create request with goal_budget. When the worker finishes, a Completed outcome calls
RunManager::continue_goal, which (unless the emergency stop is engaged) either marks an active
goal exhausted at its budget or bumps `used` and calls create_run with "[GOAL CONTINUATION] keep
working toward: <objective>". A busy session simply skips it: the run holding the session
continues the goal when it completes. A Cancelled outcome pauses the goal; Failed leaves it
active and idle. continue_goal returns a boxed future because it runs inside the task create_run
spawns; the worker reaches the manager through a Weak self-reference.

Stop is idempotent: POST /v1/runs/{id}/stop looks the run up (404 if absent), cancels the active
RunControl with origin "client" if the run is active, and returns the current RunView. Steering is
accepted only for an active run (RunNotActive otherwise) and queues a string drained by the loop.

## 5. The agent turn loop

silver-core::agent::Agent::run_turn is the whole loop. The provider is transport only.

1. Emit run.started.
2. Build the system prompt (base prompt + tool/security policy + workspace instructions + rendered
   memory) and assemble messages: system, then the newest 8 MiB of persisted history (oldest
   first), then the new user message. Compaction decides what of it reaches the model.
3. Persist the user message before any model call (persist-before-execute). A persistence failure
   ends the run as failed.
4. Start the liveness watcher if turn_liveness_timeout_s is Some (section 7.5).
5. Loop:
   - if cancelled, emit run.cancelled with the recorded origin;
   - on a hosted model, if the run deadline (`server.run_timeout_seconds`) passed, fail with
     ProviderUnavailable "run timeout: …"; a local model has no wall-clock limit;
   - drain queued steering into standalone user rows at the top of the iteration (never mid-tool);
   - consume one iteration from the budget; the first exhaustion turns tools off and grants one
     grace model call to answer, the second fails with "iteration budget exhausted";
   - once the run's working messages reach the budget, compact them in place; build the request
     with the tool specs visible for the scope (workspace tools are filtered out when there is no
     workspace) and emit context.updated with its size, and again with the provider's
     `prompt_tokens` once the response reports usage;
   - stream the response: text deltas become text.delta events and are appended; reasoning deltas
     become reasoning.delta events (live only, never stored); tool-call deltas are accumulated by
     index; usage is merged;
   - a finish_reason=length response whose text is repetition-dominated aborts the run;
   - an empty response feeds the empty-response guard; Retry continues, SkipToFallback fails with
     ProviderUnavailable "model returned an empty completion";
   - with no tool calls, persist the assistant text (if any), emit text.completed and run.completed,
     and return Completed;
   - with tool calls, persist the assistant tool-call message first, then process each call in
     order.
6. Per tool call:
   - run the guardrail before_call decision; a refusal emits tool.completed with status blocked and
     a synthetic tool result; a halt closes the rest of the batch, turns tools off for the rest of
     the turn and asks the model to answer with what it has (a tool call after that ends the run
     with a stop message naming the first cause). Passing max_tool_calls_per_run halts the same
     way, so no limit ends a run without a reply. File lookups (read_file, list_files, search_files) and shells are
     failure-tolerant: distinct failing calls never halt them;
    - a name outside the run's tool set, unknown or left out by the preset, gets an
      `unknown tool` result that lists the callable tools;
   - emit tool.started with a sanitised argument preview;
   - if ApprovalPolicy requires approval for the risk, emit approval.required, await the gate, emit
     approval.resolved, and on deny/timeout return a denied result to the loop and turn tools off
     for the rest of the turn (the model answers, and says what the user can do next); on cancel
     finish cancelled;
   - otherwise execute inside a timeout wrapper that a stop also interrupts, and record the
     outcome with after_call and observe_call, appending any warning guidance. The file tools
     run on the blocking pool, so a long walk cannot hold off the stop or the timeout, and the
     walk itself ends once its result is no longer awaited;
   - emit tool.completed and push a tool result message bound to the call id.

Tool results are pushed to the in-memory message list and persisted in the correct order. An
assistant message carrying tool calls is persisted before execution so a crash cannot leave an
unanswered tool call in the transcript.

An optional `Advisor` (silver-core::advisor; the daemon's is Jev, behind `agent.jev_hints`) is
asked at three points: after the user message is persisted, after each round of tool calls, and
when a reply without tool calls would end the turn. It gets the task text, every executed call as
a `Step` (command or arguments, exit status, head and tail of the output) and the pending answer,
and returns its answers (yes-probability per question) and hints, or None when it is switched off
or failed. Every check emits `advisor.checked` with the answers and the hints it newly handed the
model, so a client can show what the advisor saw. Each distinct hint reaches the model once per
run, only in the working copy, like the other loop notices: appended to the user message, to the
newest tool result, or, at the end, as a user nudge that sends the model back to work (sharing
the verify-on-stop path, which takes precedence; unlike a verify nudge, the answer it sends back
is left out of the final text). The advisor is not asked once tools are off. The daemon's switch
is live (`/v1/advisor`) and saved to config.toml.

Everything else the loop writes for the model, rather than the user or a tool, is emitted as
`context.injected` with a label and the exact text: the system prompt and each file it loaded
(project instructions, MEMORY.md, USER.md) when the run starts, then each loop notice where it is
added (tool-guard and subdirectory AGENTS.md text on a tool result; the iteration budget and limit,
tools turned off, verify-on-stop, truncation, reasoning cut and compaction summary). A client
shows them in place of guessing, and `/v1/sessions/{id}/injected` returns them, with the advisor
checks, for a reopened session.

## 6. Path confinement

Workspace::resolve_path delegates to confine_path(canonical_root, requested):

1. Reject an empty path.
2. Reject a leading '~', '~/' or '~\\'. A tilde is never implicit authorisation for the home
   directory.
3. Absolute paths are used as-is; relative paths are joined to the canonical root. A relative path
   that spells out the root without its leading '/' (a small model copying the root from the
   prompt) is read as that absolute path, unless it names a real path under the root.
4. lexical_normalize removes '.' and pops '..' lexically before any filesystem access.
5. The longest existing prefix is canonicalised (resolving symlinks), the remaining components are
   appended, and the result must satisfy Path::starts_with(canonical_root). A symlink in the
   existing prefix that points outside the root is therefore rejected.
6. Any failure returns CoreError::PathOutsideWorkspace ("path_outside_workspace", HTTP 403).

Path::starts_with compares whole components, so /srv/root2 is not considered inside /srv/root.
See docs/security.md for the limits of this algorithm.

## 7. Detection guards and their exact constants

All guards live in silver-core::guard and are driven from the turn loop.

### 7.1 Iteration budget

guard::iteration_budget::IterationBudget is an AtomicU32 consume/refund counter.

- default max_iterations = 500.
- One grace model call when the budget first reports exhausted; a second exhaustion fails the run.
- max_tool_calls_per_run default = 200; exceeding it fails the run.

### 7.2 Repetition guard

guard::repetition (ported from agent/repetition_guard.py):

- MIN_FRAGMENT_LENGTH = 400
- REPEAT_WINDOW = 60
- MIN_REPEAT_COUNT = 5
- DOMINANCE_RATIO = 0.5
- RUNAWAY_DISTINCT_LINE_RATIO = 0.5
- REPETITION_LOOP_INTERRUPTED = "[the reply degenerated into a repetition loop and was interrupted]"

The turn loop calls only is_repetition_dominated, and only when finish_reason is Length. On a match
the run fails with Internal and that message. is_runaway_repetition is implemented but is
not called from the turn loop.

### 7.3 Empty-response guard

guard::empty_response (ported from agent/empty_response_guard.py):

- DEFAULT_EMPTY_RETRY_BUDGET = 3
- REDUCED_EMPTY_RETRY_BUDGET = 1
- DEFAULT_COST_THRESHOLD_USD = 0.25
- DEFAULT_GUARD_ENABLED = true

An empty completion is recorded as an EmptyAttempt. The guard retries while attempts <= retry_budget,
classifies deterministic-empty after two or more consecutive attempts with the same
(model, provider, finish_reason) signature and unanimous evidence, and returns SkipToFallback on
exhaustion or determinism. The turn loop then resets and fails the run with ProviderUnavailable and
"model returned an empty completion".

The turn loop always passes estimated_cost_usd = None (the pricing module exists and
`run.completed` carries a cost estimate, but the guard does not consult it), so the cost-aware
reduction to a budget of 1 never triggers in production and `agent.empty_cost_threshold_usd` has no
effect; the effective budget is 3.

### 7.4 Tool-call stall guardrails

guard::tool_guardrails (ported from agent/tool_guardrails.py). The daemon builds
ToolCallGuardrailConfig from Default with hard_stop_enabled = config.agent.tool_call_hard_stop; it
never calls from_mapping:

- warnings_enabled = true
- hard_stop_enabled = config.agent.tool_call_hard_stop (default true)
- non_interactive_hard_stop_enabled = true
- exact_failure_warn_after = 2
- exact_failure_block_after = 5
- same_tool_failure_warn_after = 3
- same_tool_failure_halt_after = 8
- no_progress_warn_after = 2
- no_progress_block_after = 5
- loop_caps: max_web_searches = 50, max_subagents = 50

Identical-call detection constants:

- STALL_GUARD_IDENTICAL_CALL_THRESHOLD = 3
- STALL_GUARD_MAX_CYCLE_PERIOD = 4
- STALL_GUARD_CYCLE_HISTORY = 64
- IDENTICAL_RESULT_STUB_MIN_CHARS = 512
- RESULT_STUB_ARGS_PREVIEW_CHARS = 120
- repeatable tools: process_manage, and names ending _get_result or _poll

Decision codes: repeated_exact_failure_warning, repeated_exact_failure_block, same_tool_failure_warning,
same_tool_failure_halt, idempotent_no_progress_warning, idempotent_no_progress_block,
identical_call_streak_halt, identical_cycle_halt, loop_web_search_cap, loop_subagent_cap.

Behaviour: before_call blocks at the configured block thresholds when hard_stop_enabled is true, and
applies the web_search/delegate_task loop caps regardless of hard_stop_enabled. after_call counts
exact-args failures, same-tool failures and idempotent no-progress repeats; observe_call tracks the
consecutive identical (signature, result hash) streak and multi-call cycles and can return a result
stub. A Warn appends guidance to the tool result; a Block or Halt becomes a synthetic tool result.

The tool-name sets are aligned with the registry: IDEMPOTENT_TOOL_NAMES includes read_file,
list_files, search_files, session_search, web_search, web_extract, skill_view and skills_list;
MUTATING_TOOL_NAMES includes write_file, patch, run_command, bash, execute_code, memory,
todo_list, skill_manage and process_manage; FAILURE_TOLERANT_TOOL_NAMES includes run_command,
bash, execute_code, process_manage and web_extract; PROGRESS_RESET_TOOL_NAMES includes
write_file, patch, run_command, bash, execute_code, memory, todo_list and skill_manage.

With agent.tool_call_hard_stop = false, before_call always allows and only warning guidance is
produced. With the default (true), before_call blocks at the
configured block thresholds, so ToolStatus::Blocked and the hard-stop branch in the turn loop are
reachable. The loop caps are per-turn ceilings: max_web_searches corresponds to the registered web_search
tool and max_subagents to the registered delegate_task tool, so both fire regardless of
hard_stop_enabled. A batch counts by its task count, so 50 subagents in one turn is the refusal.

### 7.5 Turn liveness watchdog

guard::liveness (ported from agent/turn_liveness.py):

- DEFAULT_TURN_LIVENESS_TIMEOUT_S = 600.0
- DEFAULT_TURN_LIVENESS_POLL_S = 15.0
- MIN_TURN_LIVENESS_POLL_S = 0.01
- turn_liveness_timeout_s <= 0 opts out (the daemon maps a config value of 0 to None).

ActivityClock::touch stamps an Instant and bumps a generation under a mutex; the loop touches it on
model requests, text/reasoning deltas and after tools. Every poll_s the watcher samples
idle_seconds and, at timeout_s, calls commit_abort which revalidates turn_active and the generation
under the same lock. If committed, it calls the callback (the turn loop cancels the run with origin
"liveness") and deactivates the turn; a declined commit keeps polling.

## 8. Events, persistence and replay

### 8.1 Event production

Each run has one EventEmitter holding an AtomicU64 and an unbounded mpsc sender. emit assigns the
next sequence number (starting at 1), timestamps the event and sends it to the channel. The
sequence number is the SSE id and the run_events primary-key component, so the core needs no shared
coordinator to keep persistence and streaming in the same order.

### 8.2 The event lock and the drain task

The worker spawns exactly one drain task per run. For every event the drain task:

1. acquires the RunManager-wide event_lock (a tokio::sync::Mutex);
2. if payload.is_replayable(), appends the event to run_events;
3. sends the event to the run's broadcast channel;
4. releases the lock.

Persist-then-publish happens inside one critical section. is_replayable excludes TextDelta,
ReasoningDelta, Heartbeat and ToolOutput; those are broadcast live but never stored. Every other
event type is stored.

### 8.3 Why replay is race-free

subscribe(run_id, last) acquires the same event_lock, then inside the lock:

1. looks up the run's active broadcast sender and calls subscribe() to get a live receiver;
2. reads run_events for sequence > last;
3. releases the lock.

Because the drain task cannot persist or broadcast while subscribe holds the lock, and subscribe
subscribes and reads the database while holding it, there is no interleaving in which an event is
broadcast but not yet persisted, or persisted but not yet broadcast to a receiver acquired in
between:

- any event persisted before the lock is acquired is returned by the step-2 DB read;
- any event emitted after the lock is released is delivered to the step-1 live receiver;
- the event that crosses the boundary cannot be delivered twice because EventSubscription::next
  skips any live event whose event_id <= last_seq, where last_seq starts at the highest replay id.

Without the lock, a client could subscribe after an event was persisted but before it was
broadcast, then miss it forever because the DB snapshot predates the write and the live receiver
postdates the broadcast. The lock removes that window. The cost is that persistence and publication
are serialised across all runs, not just within one.

### 8.4 Subscription and replay

An EventSubscription first drains buffered replay events, then live events. Replay events carry a
gap marker when the daemon detects one: subscribe queues a replay.gap (event_id = last) when the
first stored event after last is greater than last + 1. Because TextDelta and ReasoningDelta
consume sequence numbers without being stored, an ordinary reconnect with Last-Event-ID can see
replay.gap for sequence holes created by coalesced deltas; the event is a signal, not a fatal error.
A slow subscriber whose broadcast buffer overflows gets a synthetic replay.gap from the Lagged
branch and the stream continues; the subscriber is not disconnected.

### 8.5 SSE framing

GET /v1/runs/{id}/events builds an Axum SSE stream. Each RunEvent is serialised to JSON and sent as
a single SSE event with id = event_id, event = payload name, data = the JSON. KeepAlive is 15
seconds with the comment text "keep-alive" (an SSE comment, not a heartbeat event). Disconnecting
does not cancel the run; a client can reconnect with Last-Event-ID and replay.

## 9. Persistence and recovery

One SQLite database at <data_dir>/state.db is driven by a single tokio-rusqlite connection.
Db::initialize sets PRAGMA foreign_keys = ON, busy_timeout = 5000 ms and journal_mode = WAL.

The schema is a ladder of embedded migrations: `migrations/0001_initial.sql` and
`0002_chat.sql` (`bots`, `chat_entries`, `chat_reads`, see [messages.md](messages.md)), each applied
once and stamped into `PRAGMA user_version`. Tables: workspaces, sessions, runs, messages, tool_calls,
approvals, run_events, memory_changes, checkpoints, spend_events, presets, documents and
document_chunks, plus the FTS5 indexes over messages and document chunks and the triggers that keep
them in sync.

- workspace ids, session ids and run ids are stored in their prefixed text form; timestamps are
  RFC3339.
- sessions has a partial unique index on (source, external_key, COALESCE(workspace_id, '__global__')).
- run_events is keyed by (run_id, sequence).
- approvals has run_id REFERENCES runs(id) but tool_call_id has no foreign key (ADR 0003).
- tool_calls and memory_changes are present but have no production writer in the MVP; tool
  lifecycle and memory audit are carried by run_events instead.
- `sessions.preset` holds the session's preset id (NULL is Minimal).
- `documents` and `document_chunks` have two external-content FTS5 tables over
  the chunk text (`document_fts` unicode61, `document_fts_trigram` trigram), the same pair the
  message index uses. `documents` is unique on (workspace_id, path) and its size and mtime decide
  whether a re-index is skipped, so an edited document replaces its chunks instead of adding to
  them. Documents are workspace-scoped (INV-3): `search_documents` passes the run's own
  `ctx.run.scope` and a global run reaches no workspace's documents. Ingestion is explicit, so the
  doubled write cost is paid only for a document the model asked about.
- A SQLite constraint violation (DbError::ConstraintViolation) becomes DbError::Conflict; the API
  turns it into CoreError::Conflict (code `conflict`, HTTP 409). This is how a duplicate workspace
  name or canonical root is reported.

Startup: main calls Db::migrate, which applies each migration the database has not seen in one
transaction together with its `PRAGMA user_version` stamp, and nothing to a current one. A database
made before the schema was squashed to one file has the same schema only if it reached the old
version 15; delete an older `state.db`.
Then Db::recover_interrupted_runs runs, which sets status = failed, error_code = 'daemon_restarted' and
finished_at for every queued, running or waiting_approval run. There is no automatic resume.

## 10. Provider configuration and resolution

User-facing settings are in [configuration.md](configuration.md) and
[provider-setup.md](provider-setup.md); this section is how they become a model.

### 10.1 Loading configuration

`main::load_config` runs, in order:

1. `load_secrets_env()` reads `<config dir>/secrets.env` (`KEY=VALUE` lines, blank lines and `#`
   comments skipped, matching quotes stripped) and calls `std::env::set_var` only when the name is
   not already set, so the process environment always wins.
2. `Config::load` parses `--config <FILE>` or `config_file_path()` (a missing file means the
   built-in defaults), overlays the `SILVER_*` variables (`apply_env`) and migrates the file to the
   current `CONFIG_VERSION` in memory. The ladder is ordered and idempotent, and a file written by
   a newer build is never downgraded.
3. `--bind` and `--data-dir` override the result.
4. `Config::validate` fails closed (a non-loopback bind without a bearer token of 16 or more
   characters, a zero approval timeout, a duplicate MCP server name, a bad fallback route, ...).

Paths are owned by `apps/silver/src/config.rs`: `config_dir()` is `SILVER_CONFIG_DIR`, else
`ProjectDirs::from("dev", "silver", "silver").config_dir()`, else `./silver-config`;
`config_file_path()` is `SILVER_CONFIG`, else `<config dir>/config.toml`; `data_dir()` is
`data.directory`, else the platform data directory, else `./silver-data`. The effective precedence
is flags > process environment > secrets.env > config.toml > defaults.

### 10.2 Resolving the model

`Config` resolves the model from the file plus the shared preset catalog:

- `kind()`: a parsed `model.kind` other than `openai_compatible` wins; otherwise the
  `model.provider` preset decides, and an unknown provider falls back to `openai_compatible`.
- `resolved_base_url()`, `resolved_model()` and `resolved_api_key_env()`: the non-empty file value,
  else the preset's, and for the base URL last `https://api.openai.com/v1`.
- When no key is present for a preset that requires one, `main` logs a warning naming the
  variable and the ways to supply it.

`main` then stacks the layers, innermost first:

1. the **primary**: one transport, or a `CredentialPoolModel` when `[[model.credentials]]` has
   several keys (`credential_pool.rs`);
2. the **fallback chain** (`FallbackModel`, `fallback.rs`) with the primary at index 0, when
   `[[model.fallback]]` is configured. A route whose key variable is unset is skipped;
3. `RoutedModel` (`routed.rs`), which consults the credential store (`auth.json`, then OAuth
   grants, then the key variable) on every request, so `/login` takes effect on the next turn and
   a session can be pinned to its own provider (`with_session_provider`);
4. the **acting** model, wrapped in `MoaModel` (`moa.rs`) when `[moa]` is enabled.

The auxiliary model (`[auxiliary]`) is a separate transport on the same HTTP client; it summarises
dropped context, titles sessions and reviews `smart` approvals.

`routed::build_transport` selects the transport by `ProviderKind`: `OpenAiCompatible` and `Ollama`
use `provider.rs`, `Anthropic` uses `anthropic.rs`, and `Copilot`, `Bedrock`, `Vertex`, `Codex`,
`Acp` and `OpenCode` have their own modules (`copilot.rs`, `bedrock/`, `vertex.rs`, `codex.rs`,
`acp.rs`, `opencode.rs`). All of them share one `reqwest::Client` built by
`config::http_client_builder`, the single TLS policy (`[security]`).

### 10.3 Transports

The preset catalog lives in `crates/silver-protocol/src/providers.rs`. A transport contains no
agent logic, persistence or tool policy.

- `provider.rs` implements the OpenAI-compatible `/chat/completions` streaming transport.
- `anthropic.rs` implements the native Anthropic Messages API: `POST {base_url}/messages` with
  `x-api-key` and `anthropic-version: 2023-06-01`, `stream: true`, and `max_tokens` 4096 unless the
  request sets one. System messages are concatenated into `system`, assistant tool calls become
  `tool_use` blocks, tool results become `tool_result` blocks in user messages (merging
  consecutive results and inserting a leading user turn when needed), and tool specs use
  `input_schema`. SSE frames (`message_start`, `content_block_start`, `content_block_delta`,
  `content_block_stop`, `message_delta`, `message_stop`) decode into `ModelStreamEvent`s and stop
  reasons map to `FinishReason`. A 401 or 403 never echoes the body; other error bodies are
  redacted against the key and truncated.

## 11. Web UI

`apps/web` (Svelte 5 + Vite) is presentation-only; it holds no agent logic, SQLite or filesystem
access.

- Serving: `apps/silver/src/api/ui.rs` embeds `apps/web/dist` with `rust-embed` (debug builds
  read the directory from disk, release builds carry the bytes). Known files are served as is;
  fingerprinted `assets/*` get a one-year immutable cache and a miss returns 404; any other path
  gets `index.html` so client-side routes load.
- Headers: UI responses carry `default-src 'self'; style-src 'self' 'unsafe-inline'; img-src
  'self' data:; frame-ancestors 'none'`; API responses keep `default-src 'none'`.
- Auth: the bearer middleware skips UI paths; only `/health` and `/v1/*` are API routes. The
  built UI shows a token prompt when a call answers 401, keeps the token in `localStorage` and
  sends it as `Authorization: Bearer` on every call, `<img>` files included; Settings has Sign out.
- Connection: an unreachable server shows a "Connection lost" pill and a "Can't reach silver"
  error. A `GET /v1/daemon/status` every 15 s (3 s while away) notices the change and reloads
  the lists once it is back.
- Workspaces and search: "Add workspace" opens the desktop's own folder dialog through
  `POST /v1/workspaces/pick` and registers the returned path. Cancelling closes it; the typing
  form appears only when that machine has no dialog, which the server reports as a 4xx.
  The session search box is debounced and calls `GET /v1/sessions?q=` once per scope, so it
  matches message content as well as titles. The model picker lists only providers the user has
  configured (`configured` on `GET /v1/auth`) or that are active.
- Development: `npm run dev` in `apps/web` serves on :5173 and proxies `/v1` to `SILVER_URL`
  (default `http://127.0.0.1:7777`), adding `SILVER_BEARER_TOKEN` server-side.
- State: `src/lib/state.svelte.js` holds all state and every API call; `src/lib/commands.js` maps
  the `GET /v1/commands` catalog to handlers. Run events arrive through a `fetch` stream
  (`EventSource` cannot send the bearer token) that reconnects with `Last-Event-ID`. One reducer serves the run and its subagents: a
  `subagent.*` event is filed under the `delegate_task` call it belongs to and the payload it
  carries is applied to that task's own step list, so a delegated turn renders like a turn. A
  delegation is a card of its own in the transcript, never folded into a tool group, and a row
  opens that task in the Agents tab (`app.agent` holds only the selection; the view is derived
  from the same steps).
  Pending approvals are a queue rather than one card, because parallel subagents can ask at once;
  each decision names its own tool call. The Agents tab of the workspace panel lists the
  subagent catalogue and edits the custom definitions as Markdown, the file the daemon reads. /goal lives in the daemon (section 4.3); the UI
  follows a run the daemon started in the open session through SessionView.active_run, and
  re-lists sessions every 2 s while any is working or has an active goal. /loop and /heartbeat
  are still timers in the page.
- Messages mode ([messages.md](messages.md)): the server half is `apps/silver/src/chat/` (`ChatHub`
  and its per-bot job queue in `turn.rs`, room turns in `group.rs`, bot-to-bot requests in
  `team.rs`, SQL in `store.rs`) behind `/v1/chat/*`; each bot turn is a normal run through
  `RunManager::create_run` in a session with source `chat`, so INV-1 and INV-9 hold unchanged. The
  client half is `src/lib/chat.svelte.js` and `src/components/messages/`, which only renders what
  `GET /v1/chat/events` says. The team tools (`list_bots`, `ask_bot`) are registered like
  `delegate_task` but hidden from every run whose session is not a bot's (`Agent::without_tools`).

## 12. Tool registry and backends

### 12.1 The registry

`ToolRegistry` (`crates/silver-core/src/tool.rs`) is a `Vec<Arc<dyn Tool>>`.
`silver_core::tools::register_default_tools` (`crates/silver-core/src/tools/mod.rs`)
registers the 20 concrete tools in module order: `fs` (`read_file`, `list_files`,
`search_files`), `vision` (`view_image`), `write` (`write_file`, `patch`), `command`
(`run_command`), `execute_code`, `bash`, `process` (`process_manage`), `memory`,
`session_search`, `documents` (`search_documents`), `todo`, `skills` (`skills_list`,
`skill_view`, `skill_manage`), `lsp` and `web` (`web_search`, `web_extract`). The 21st,
`delegate_task`, needs a subagent runner, so the daemon registers it itself
(`tools::delegate::register`) when `[delegation] enabled` is true, and the chat's two team tools
(`tools::team::register`: `list_bots`, `ask_bot`, backed by the `Team` service).
The `Tool` trait supplies the name, description, JSON Schema, a `risk(&args)` function,
`requires_workspace()` and a `timeout_hint()`; `specs(has_workspace)` and
`names(has_workspace)` filter workspace-gated tools out of a global run's schema. The
`/v1/capabilities` handler (`apps/silver/src/api/health.rs`) walks `registry.all()` and
reports each tool's name, risk and workspace flag.

### 12.2 ToolServices and injection

Optional, transport-free backends live in `crates/silver-core/src/services.rs` so the core never
depends on reqwest, SQLite or a live filesystem beyond a `Workspace`:

- `ToolServices.terminal: Option<Arc<dyn TerminalBackend>>` — `run`, `processes` and
  `process_action`, plus the `resolve_cwd` helper that confines a working directory to the run's
  workspace.
- `ToolServices.todos: Option<Arc<dyn TodoStore>>` — per-session todo-list read/write.
- `ToolServices.skills: Option<Arc<dyn SkillsBackend>>` — list, view and manage skill documents.
- `ToolServices.web: Option<Arc<dyn WebBackend>>` — `search` and `extract`.
- `ToolServices.subagents: Option<Arc<dyn Subagents>>` — the subagent catalogue and the runner
  that starts one (section 12.6).

`silver` constructs the concrete implementations once in `main`
(`apps/silver/src/main.rs`): `TerminalManager`, `TodoStore`, `SkillsStore` rooted at the
data directory, and `HttpWebBackend`. `RunManager::new` stores a `ToolServices` clone, and each
run task clones it again into the `Arc<RunContext>` it builds (the `services` field,
`crates/silver-core/src/context.rs`). `ToolContext` exposes that context to every tool, so a
tool reads `ctx.run.services.<backend>`. A missing backend is a normal state: the tool returns a
clear "unavailable" outcome instead of panicking.

### 12.2a Images and documents

`view_image` (`tools/vision.rs`) resolves its `path` through `RunContext::resolve_path` and
`SafetyRoots::read_denied_reason`, exactly as `read_file` does, then refuses anything that is not
a png, jpeg, gif or webp by extension or is over 5 MB. It returns a `ToolOutcome` whose `content`
is a one-line receipt (`loaded shot.png (image/png, 84 KB) — question: ...`) and whose new
`image` field is a `ContentPart::Image`. The turn loop pushes that part onto the same `tool`
message the result is on, so the next request carries the picture; the transcript row keeps the
receipt alone, so a run's messages never grow by megabytes of base64. Each transport renders the
part in its own vocabulary and puts it on a turn that accepts an image — `image_url` on a `user`
message (OpenAI, and therefore Bedrock through `build_anthropic_body` for Anthropic), an `image`
block in the `user` message the tool result already becomes (Anthropic), and `input_image` on the
message that follows a `function_call_output` (Responses). `ContentPart::as_text` ignores images,
so the FTS projection, session titles and compaction never see the pixels; `estimate_bytes` and
`estimate_messages_tokens_rough` charge a flat `IMAGE_TOKENS` instead, because base64 is pixels,
not text.

A picture changes the surface it rides: `surface_for_request` in `opencode.rs` moves a picture on
a DeepSeek model off the relay's chat/completions shim — which rejects an inline image with a bare
400 (opencode#40811) — onto the Anthropic Messages surface, the one the relay already serves for
its Claude, Qwen and MiniMax models and the one DeepSeek's own Anthropic-format endpoint takes
image blocks on. A turn without a picture keeps the surface its model is documented on.

If the surface a picture rides refuses it anyway, the turn loop recovers: a terminal refusal on a
request carrying pictures drops them (`ModelRequest::drop_pictures`), names why in the receipt
the picture rode, emits a `run.waiting` note for the user, and sends the request once more — so a
surface that rejects images degrades to the receipt instead of failing the run.

The web UI's composer stores what the user attaches at `<workspace>/.silver/attachments/` through
`POST /v1/workspaces/{id}/attachments` (the same `.silver` convention as project agents) and
puts `[attached: <name> → <path>]` in the prompt, so the same two tools read an attachment as
read any other workspace file. Pictures and PDFs are accepted on their bytes; every other file
must be valid UTF-8 text. The first attach writes `.silver/.gitignore` (`*`) so the folder never
appears in the user's `git status` or the Changes panel. The bytes never reach `MessageInput`, the
transcript or the 1 MiB message limit; only the path does, and the model spends one tool call to
look at it.

`read_file` extracts a PDF's text with `pdf-extract` when the file's own bytes are not UTF-8, so
the offset/limit paging, the redaction pass and the "past the end" guard are unchanged.
`search_documents` (`tools/documents.rs`) takes a `query` and an optional `path`: with a path it
reads and extracts that one workspace file and hands the text to the `DocumentIndex` service,
then searches what the workspace has indexed. The service is the daemon's
`DbDocumentIndex` (`apps/silver/src/document_index.rs`) over the tables in section 9; the core
owns the extraction and the daemon owns the storage. The tool never takes a `workspace_id`: the
scope is `ctx.run.scope`.

### 12.3 Risk and approval model

Every tool returns a `RiskLevel` from `risk(&args)`: `read`, `memory`, `write`,
`process` or `destructive`. Only `write_file` and `patch` return `destructive`, for a path that
`safety::write_approval_required` flags (`~/.ssh/config`, which can execute code through
`ProxyCommand`). `ApprovalPolicy` is daemon-owned and built from configuration in `main`:

    ApprovalPolicy { write_requires_approval, command_requires_approval, deny_commands }

`requires_approval` maps `read` and `memory` to never-gated, `write` to
`write_requires_approval`, `process` to `command_requires_approval`, and `destructive` to
always. When a call is gated, the turn loop emits `approval.required` and blocks on the gate; the
client decides through `POST /v1/runs/{id}/approval` and the daemon writes an `approvals` row
bound to the run, the tool call and an argument hash. Approval gates execution only: it never
widens path confinement, so an approved `write_file` targeting a path outside the workspace still
fails with `path_outside_workspace`.

### 12.4 Presets

A preset is a named tool and skill selection stored in the `presets` table;
`sessions.preset` holds the session's preset id (NULL is the built-in Minimal). Deleting a
preset moves its sessions to Minimal in the same transaction, and a stale id resolves to
Minimal.

`RunManager::run_preset` resolves the stored id against `presets()` (Minimal's tools are the
agent's config-filtered roster, Pi is `["bash"]`, then custom by name). A preset run clones
the shared agent with `Agent::with_only_tools` (the toolset filter is lifted and the deny-list
cleared, so the list is the whole selection) and wraps `services.skills` in `PresetSkills`,
which filters `list`/`list_for_platform` and returns `None` from `view` for a rejected name. A
Minimal run keeps the shared agent and store.

The system prompt follows the tool set: help, enforcement, execution and steering blocks need at
least one tool; the coding brief swaps file-tool lines for shell lines (or drops them) and yields
to a no-file-tools note when no files/shell tool is present; MEMORY and USER PROFILE snapshots
need the `memory` tool. The memory guidance is a short rule to save before replying: a 4B model read
the longer Hermes memory-vs-skills text as "don't save", and said it would remember without calling
the tool. The `memory` schema advertises only `target`, `action`, `content` and `old_text`; the
executor still accepts the Hermes `operations` batch and `new_text` alias. With `bash` and a non-root daemon, a sudo tip tells the model not to
install anything and to hand the user install commands for the OS named from `/etc/os-release`.
A 4B model forgets that mid-task, so `bash` repeats the tip as a `note` on the result of a
sudo or package-manager command, or of output such as `must be root`.

Security: a preset can turn on any registered tool, while approvals, `deny_commands`, the
hardline floor and path confinement still hold for every tool.

### 12.5 Plan mode

`silver-core::plan` holds plan mode. `sessions.plan_mode` is NULL when off,
`entered`, `active` or `exited`. `/plan` (`plan_mode` on PATCH or on a create-run request) sets
`entered`, or `exited` when leaving; `Db::start_plan_run` hands each admitted run its
`PlanMode` and moves the row on (`entered` -> `active`, `exited` -> NULL), so the re-entry and
exit notices go out once. The run gets a `Plan { mode, file }` in `RunContext`, where `file` is
`<data_dir>/plans/<session_id>.md`; `RunContext::resolve_path` lets the file tools reach that
one path outside the workspace.

The turn loop adds the notice for the mode to the user message (working copy only, emitted as
`context.injected`). A plan-mode run also gets two tools the registry does not hold, so no
preset, filter or catalog sees them: `ask_user_question` and `exit_plan_mode`. A write of the
plan file counts as `read` risk, so it needs no approval, whichever tool the model reached for:
`Plan::is_written_by` takes a file tool naming that path, or a shell command whose every `>`
target, every file a `tee` is given and every file `sed -i`/`perl -i` rewrites is that path, at
least one is, and no other program in the line changes (a `mkdir -p` of the plan's folder,
which the daemon already made, excepted). The targets come from the parser the danger checks
already use (`tool::shell_programs`), which reads a heredoc body as data unless a shell on its
line reads it (an unquoted one still runs its `$(…)` and backticks), so the plan's prose never
reads as a command, while a relative guess, a second file or a `rm` is a miss and the refusal
below applies; a file written other than the plan (a mistyped session id, a `$VAR` the parser
cannot expand) is named in it. That classification cannot see an interpreter, so
`core::sandbox` is the real boundary: in plan mode `bash` and `run_command` are spawned inside
bubblewrap with `--ro-bind / /` and `--bind <plans_dir>`, the plan *directory* because `sed -i`
writes a temporary file beside its target, and the plan-mode notice tells the model the rest is
read-only. `sandbox::available` probes bwrap once with a real run (a version check passes even
where user namespaces are disabled) and without it the classification is the whole of plan
mode's protection, which is what a plan-mode `python3 -c "open(...)"` walks through. While plan
mode holds any call above `read`/`memory` risk is refused like a
`deny_commands` match, except a shell command that changes nothing: `plan::shell_change` runs
each program of the command line through that parser and a blacklist of programs, flags and
git/package-manager subcommands. Both plan tools go to the user through `ApprovalGate::ask_user`, which the daemon
routes to the manual gate whatever the approval mode or YOLO flag, and never through a
remembered decision. The question is the approval's description and its reply comes back as
`ApprovalOutcome::Answered` (the `answer` field of the decision); `exit_plan_mode` is asked
only once the plan file has text, with the plan as the
description. Approved, it clears the run's plan flag and emits `plan_mode.exited`, on which the
drain task sets the session to `exited`, and the loop ends the run (closing any later calls of
the batch) instead of asking the model again. The web UI answers `plan_mode.exited` by sending
"Implement the approved plan." once the run is over, so the work is its own turn: /retry or
/undo of it never reaches the plan. A run keeps its tool set to the end, so the cached prompt
prefix holds.

### 12.6 Subagents

`delegate_task` takes a `tasks` list and runs each entry as its own `Agent::run_turn` with a
child `RunContext`. A subagent is not a process, a session or a run row: it is another loop
inside the run that called it, and it keeps the parent's `run_id`, session, scope, workspace,
services and approval gate, so an approval it asks for resolves against the run the user is
watching, a write it makes is checkpointed and undoable, and a stop reaches it
(`RunControl::child_of`). Its own transcript is discarded (`DiscardTranscript`): what the parent
keeps is the report the tool returns, and what a client shows of the work is the `subagent.*`
events in the run's stream, which are persisted like any other and replayed to a reopened
session.

- **Definitions** live in `crates/silver-core/src/subagent.rs`: the built-in catalogue
  (`general-purpose` only), the frontmatter parser and the tool
  resolution. `resolve_tools` intersects a definition's allow-list with the tools the parent run
  may actually call, subtracts its deny-list, and always drops `delegate_task`, so delegation
  cannot nest. A definition whose prompt matches the injection scanner is replaced by the visible
  blocked marker, like a project instruction that does.
- **The store** (`apps/silver/src/subagents.rs`) reads `~/.silver/agents/*.md` overlaid by
  `<workspace>/.silver/agents/*.md`, the project definition winning, and is the backend behind
  `/v1/agents`. A file the parser rejects is logged and skipped; one that is not a definition at
  all is ignored. Writes go through a temp file plus rename and are refused when the name or the
  document would not parse. Built-ins are read-only: the first definition of a name wins, so a
  project file cannot shadow one.
- **The prompt** carries the catalogue as a `## Subagents` block, gated on `delegate_task` being
  among the run's tools, and the daemon filters it to the definitions that run can use
  (`Subagents::prompt_index`). It is in the prompt rather than the tool description for the same
  reason the skills index is: the text stays identical between requests, so a provider's cached
  prefix survives a new definition.
- **The runner** clones the shared `Agent` per task, replaces its config's iteration budget and
  its tool list, and gives the child its own `EventEmitter`, drained by a task that republishes
  each event as `subagent.step` on the parent's emitter. Tasks of one call run concurrently
  (the call's width is the concurrency window, and a wider call is refused with the count); each
  turn is bounded by `[delegation] timeout_seconds`, and a definition may lower
  `max_iterations` but never raise it. `isolation: worktree` gives a task its own
  `git worktree add`, which is removed again if it left the tree clean and reported in the
  result when it did not.
- **Plan mode** travels down as restrictions, not as tools: the child gets `Plan::read_only`, so
  a subagent of a planning run cannot write and can neither put a plan to the user nor leave the
  parent in plan mode. The advisor is switched off for the same reason a subagent's steps are
  already visible.

## 13. Known gaps between code and spec

These are documented deliberately so the docs describe the code, not the plan. Each was checked
against the code when this list was last revised.

1. WaitingApproval is never persisted: no `update_run_status` call exists in production, so a run
   blocked on an approval still reads `running` (section 4.2). Clients detect approval from
   `approval.required`.
2. Status transitions are not validated at runtime. `RunStatus::can_transition_to` is never
   called; SQL writes status directly.
3. Configuration that does less than its name suggests: `tools.max_output_bytes` is read only by
   the workspace picture endpoint and truncates no tool result; `agent.empty_cost_threshold_usd`
   has no effect (section 7.3); `worktree.enabled` is never read; `agent.verify_on_stop` and
   `verify_max_nudges` are `AgentConfig` defaults (true, 3) with no `config.toml` key.
4. Defined but unused protocol surface: the `tool_calls` and `memory_changes` tables have no
   production writer (tool lifecycle and memory audit ride on `run_events`);
   `EventPayload::ToolOutput` and `Heartbeat` are never emitted (keep-alive is an SSE comment);
   `ApiError.request_id` is never populated; `ErrorCode::ApprovalStale` is never produced.
5. A subagent's own transcript is not persisted, so its intermediate turns are visible only as the
   `subagent.*` events of the run that started it, and its tokens are not added to the parent
   run's `run.completed` usage or cost. See section 12.6.
6. `session_search` stems by cutting each query word to five characters, a crude cross-language
   stand-in for a real stemmer: "conditions" finds "condition", but short words over-match.
7. `is_runaway_repetition` is implemented but the turn loop calls only
   `is_repetition_dominated` (section 7.2).
8. A `bash(background = true)` process is meant to outlive its run; it stops through
   `process_manage` or when the daemon exits. A child that starts its own session (`setsid`)
   escapes the process-group kill that ends a foreground call.
9. Plan mode's write boundary is the kernel only where `bwrap` is installed; elsewhere it is a
   command classification that cannot see an interpreter (section 12.5).
10. The web UI does not refetch run state or messages on a `replay.gap`, and `/loop` and
    `/heartbeat` are timers in the page, not in the daemon.

Not implemented by design: adapters (Telegram, Discord, WhatsApp; the intended design is separate
processes speaking this HTTP/SSE API with `source` + `external_key` session identity), a plugin
ABI or WASM host, process sandboxing, semantic or vector memory, global-to-workspace memory
inheritance, multi-tenant authentication, and an MCP server mode.

## 14. Behaviour added after the MVP

Each item traces to the Hermes module it ports unless noted; see
[upstream-behavior.md](upstream-behavior.md).

### 14.1 Provider reliability
- `CoreError::ProviderTransient { message, retry_after_ms, rate_limited }` plus `CoreError::retry_class()`
  classify 408/409/425/429/5xx and transport failures as transient; 401/403 and other 4xx are terminal.
  `CoreError::is_context_overflow()` recognises context-length bodies regardless of variant.
- `OpenAiCompatibleProvider` and `AnthropicProvider` map non-success HTTP responses to transient errors,
  read a numeric `Retry-After`, and turn in-stream `{"error": ...}` frames into typed failures instead of
  silently ignoring them (`apps/silver/src/provider.rs`, `anthropic.rs`).
- The turn loop retries a transient failure up to `AgentConfig.retry_max_attempts` (default 4) with
  jittered exponential backoff in `[capped/2, capped]` bounded by `retry_max_delay`, but only when no
  content or usage has been observed (a dirty stream is terminal). Every retry emits `run.waiting`.
- Fallback routes and credential pools cool a failing route or key for 60 s doubling to 4 h
  ([provider-setup.md](provider-setup.md#reliability)).

### 14.2 Context compaction, overflow and truncation
- `crates/silver-core/src/agent/compact.rs` is the deterministic half of the Hermes compressor: it
  clears old tool results (keeping the tool-call pairing), then drops whole older turns while preserving
  the system message and recent turns, and records the removal in a `[CONTEXT SUMMARY]` note. The
  note also restates the session's pending and in_progress todo items, read at each compaction,
  since the todo_list results that carried them may be gone. It never mutates the persisted transcript.
  With an auxiliary model the dropped span is summarised once per run and reused; any failure falls
  back to the deterministic note.
- The loop compacts the run's working messages **in place** when their estimated size reaches the
  budget (`max_context_bytes`, default 350 KB, or the resolved window, capped at 64k tokens for a
  model on a local server), and every other request only appends to them. A compaction frees a quarter of the budget rather than just enough to fit, and
  the note names no counts and replaces the previous one, so the prompt a local server has cached
  stays valid until the next compaction. Recomputing the compaction for every request instead
  rewrote the prompt near the top on each call once the budget was reached, and a 4B model
  re-read the whole conversation at every step.
- `context_keep_recent_tool_results` (6) and `context_keep_recent_turns` (8) bound what stays
  verbatim. When those alone overflow, fewer are kept: tool results first, down to the ones the
  model has not read yet (those after its last message), which are truncated rather than
  cleared, since a placeholder only makes the model call the tool again; then, only if the
  request still exceeds the budget, turns down to the newest user turn. Results go first because
  a tool can be run again, while a dropped turn can be the task itself (a verify-on-stop nudge is
  a second user turn within one run).
- A provider context-length rejection triggers up to two harder compactions (at most `threshold / 2`,
  lower when the error names a smaller window) before the run fails with a message saying how many
  tokens the system prompt and tools need.
- A `finish_reason=length` reply is continued up to `max_truncation_continuations` (default 3) times;
  partials are persisted, the continuation is requested with a user-role prompt, and the final
  `text.completed` carries the full assembled text without duplicating persisted segments.
- Tool-call repair: missing or duplicate ids are fixed, malformed JSON arguments become an error
  result, and an interrupted tool sequence is closed so the transcript never ends with an
  unanswered `assistant(tool_calls)`. Thinking tags (`<think>`, `<thinking>`, `<reasoning>`) are
  scrubbed from content across deltas.

### 14.3 File safety
- `crates/silver-core/src/safety.rs` ports `agent/file_safety.py`: write denial for credential and system
  paths, read denial for credential files/directories and any project-local `.env*` basename,
  approval-gated `~/.ssh/config`, and an NT namespace guard. Denials surface as
  `{"code":"read_denied"|"write_denied"}` tool results.

### 14.4 Network security, redaction and logging
- `apps/silver/src/url_safety.rs` rejects non-http(s) schemes and loopback/private/link-local/CGNAT/ULA/
  metadata addresses, checking them pre-flight and on every manually followed redirect hop, and rejects
  the configured website blocklist.
- `config.security` adds `ssl_verify`, `ca_bundle`, `website_blocklist` and `block_sensitive_query_urls`;
  `config::http_client_builder` is the single TLS policy shared by the web backend and the model providers.
- `redact::redact` is the one redaction engine (about 60 token families, auth headers, PEM, database
  URLs, JWTs). It scrubs provider error bodies, shell and file output, API errors and every log
  line; source reads use a source-safe pass. `logging::init` keeps stderr and adds size-rotated
  `agent.log` and `errors.log` under `<data_dir>/logs`.

### 14.5 Context, memory and skills
- Project context discovery loads `SOUL.md`, `.hermes.md`, `AGENTS.override.md`/`AGENTS.md`,
  `CLAUDE.md`, `.cursorrules` and `.cursor/rules/*.mdc` along the git-root to workspace chain,
  strips YAML frontmatter, applies a dynamic head/tail byte cap and scans for prompt injection,
  fencing blocked content. A workspace inside a folder its enclosing repository ignores (a
  reference copy under `ref/`, say) is its own project: that repository's `AGENTS.md` chain and git
  facts stay out of its prompt. An `AGENTS.md` found in a subdirectory is appended to a tool result
  once per directory. The workspace snapshot carries the git branch, upstream, ahead/behind,
  status counts and recent commits.
- `memory.rs` enforces the Hermes 2200/1375-char final-state budgets, skips duplicate entries and applies
  a batch through one atomic `SetContent` write. Entries are line-section-line delimited whole
  blocks; `replace` and `remove` act on a whole entry; writes and loads are scanned; an unreadable
  file refuses the write, and external drift is snapshotted to `.bak` under a cross-process lock.
- Skills frontmatter reads `platforms`, `env` and `disabled`; listing gates on the run platform and
  environment, and a sidecar `usage.json` records per-skill view/patch counters.

### 14.6 Session database
- `messages.text` is a plain-text projection, with per-run token columns on `runs`. An
  external-content FTS5 index (`message_fts`) over it is kept in sync by triggers, and a trigram
  index (`message_fts_trigram`) over the same projection serves substring and CJK recall.
- `Db::search_messages` ranks by `bm25` with a syntax-safe `MATCH`; a wildcard-escaped `LIKE` fallback
  runs only when FTS itself errors.
- Untitled sessions are titled deterministically from the first user message (`derive_title`); a user-set
  title is never overwritten.
- Rewind (`POST /v1/sessions/{id}/rewind`) removes the newest user turn and what followed it;
  retention prune, VACUUM and a startup sweep run off the startup path under a time budget, and
  database files are private.
- Every 256 successful writes the daemon checkpoints the WAL and runs `PRAGMA optimize` off the hot path.

### 14.7 Approval hardline floor
- `ApprovalPolicy::hardline_denial` refuses destructive shell commands (`rm -rf /`, `mkfs*`, raw device
  writes, fork bombs, `curl | sh`, `sudo -S`, host power control) before the approval gate, with a
  quote- and position-aware matcher, so an always-approve decision cannot reach them. `execute_code`
  is not matched because it is not a shell.

### 14.8 Versioned configuration
- `Config` carries a top-level `config_version` and the shared `silver_protocol::CONFIG_VERSION`.
  `Config::migrate` applies an ordered ladder, is idempotent, reports gaps, and never downgrades a file
  written by a newer build. Both `Config::load` and `Config::load_default` migrate in memory and log
  each applied step. `Config::render_toml`/`write_to` write atomically (temp + rename); the daemon
  uses them to persist a runtime approval-mode or Jev change.

### 14.9 Session search
- `session_search` is built for a 4B model's queries: any query word may match (as a five-character
  prefix), hits are ranked by BM25 and shown one per session with the matching message id, and only
  user and assistant messages are searched or read. Tool results are left out: they are mostly file
  contents still in the workspace, and they buried the conversation. A read shows only messages with
  visible text, and arguments a shape does not use are ignored rather than rejected.

### 14.10 Usage cost
- `crates/silver-core/src/pricing.rs` turns a `ModelPrice` and token usage into a USD estimate;
  it holds no prices. The daemon's `ModelContextResolver::price` reads them from the models.dev
  registry (`cost` block), and an unpriced model yields `None`. `run.completed` carries an optional
  `cost_usd`, the daemon persists it on the run (`runs.cost_usd`) and `SessionUsage`/`RunUsage`
  aggregate it. `[cost]` caps use the same prices.

### 14.11 Daemon control and HTTP hardening
- Approval waits time out after `tools.approval_timeout_seconds` (default 300, deny-on-silence).
  `POST /v1/daemon/pause|resume` and `GET /v1/daemon/status` implement the emergency stop: while
  paused, `POST /v1/runs` returns 503 `daemon_paused` and no run row is created; in-flight runs
  drain. The stop is a sentinel file in the data directory, so it survives a restart.
- `POST /v1/runs` honours an `Idempotency-Key` header, unique per session (`runs.idempotency_key`),
  replaying the existing run and tolerating an insert race.
- `[server] cors_allowed_origins` adds a restricted CORS layer only when non-empty;
  `rate_limit_per_minute` adds a per-client token bucket (429 + `Retry-After`, code `rate_limited`)
  only when non-zero. The limiter runs outside auth so unauthenticated floods are rejected.
- The bearer token is compared in constant time, `Config::validate` refuses a non-loopback bind
  without a token of at least 16 characters, and every response carries security headers.

### 14.12 Verify-on-stop and Jev hints
- **Verify-on-stop** (`AgentConfig.verify_on_stop`, on, up to `verify_max_nudges` = 3): after a code
  edit (not `.md`/`.txt`/`.rst`) with no fresh passing test, lint or build command, nor a run of the
  edited file itself, a reply that would end the turn gets a bounded nudge to verify first. A nudge
  answered without a tool call ends them, and none is sent once tools are off (after a denial),
  since the model could not act on it.
- **Jev hints** (`agent.jev_hints`, off by default), for small local models: Jev, TypeSafe's
  classifier on OpenRouter, reads the task, each round of tool calls and the answer about to be
  given (section 5). The model gets a short hint when one applies, each at most once per run: read
  the docs before building an outside project, search before answering about current facts,
  change approach when attempts keep failing the same way, read the build instructions before
  installing packages or giving up, fix the environment instead of editing the project's own
  files, and answer once the build is done. **The task and excerpts of commands and output are
  sent to OpenRouter**, with the stored OpenRouter key (`/login`) or `OPENROUTER_API_KEY`. A check
  takes about half a second and costs a fraction of a cent; a failure just gives no hint.
