# Upstream behavior: Hermes to silver

## Method note

This document is a **behaviour extraction**, not a translation plan. It was produced by reading the
upstream Python source at `ref/hermes-agent-main` and asking, for each mechanism, *what observable
behaviour must silver preserve, and what is the smallest correct Rust implementation of it*.

Method:

1. Fix the MVP boundaries first: a single local user, one HTTP/SSE API, no plugin ABI.
2. Read each named upstream module and extract its public symbols, constants and state machines —
   never copy structure. Where a god-file was split into facade + `*_topic.py` siblings, the symbol
   cited is the sibling that owns the behaviour, not the facade.
3. For every loop/stall detector, record the **exact numeric constant and the exact firing rule** in
   the *Detections* section, because these are the parts that are cheap to port and expensive to
   rediscover.
4. Mark each row In/Out of MVP. "In (simplified)" means the behaviour is worth keeping
   but the Python mechanism is replaced.

All upstream paths below are relative to `ref/hermes-agent-main/`. The extraction reflects the
upstream tree as vendored in this repository; it is not a snapshot of any released version.

The *In/out of MVP* column and the constants are the plan as written before implementation.
Rows that changed since are marked, and [What silver left out](#what-silver-left-out-and-what-it-later-added)
lists the current status. For the tools see [tool-port-matrix.md](tool-port-matrix.md); for the
code as built see [architecture.md](architecture.md).

---

## Behaviour extraction table

| Area | Hermes source (path + symbol) | Behaviour to preserve | Proposed simplification | In/out of MVP scope |
| --- | --- | --- | --- | --- |
| Agent loop | `agent/conversation_loop.py::_run_conversation_turn` (loop head at line 1528), `run_conversation`; `agent/turn_facade.py::build_turn_context` | One turn = loop of *build request → call model → either return final text or execute tools and iterate*. Cancel is cooperative and checked between iterations. One authoritative persister per step. | A single concrete async function in the run worker: `loop { assemble; stream; if tool_calls { run tools; persist; continue } else { persist; break } }`. No plug-in phase indirection. | In |
| Turn phases | `agent/turn_preflight*.py`, `turn_iteration_prep.py`, `turn_request_assembly.py::assemble_api_request`, `turn_api_call.py::perform_api_call`, `turn_api_error.py::handle_api_error`, `turn_response_intake.py::normalize_model_response`, `turn_response_check.py::check_api_response`, `turn_tool_round.py::run_tool_round`, `turn_overflow.py`, `turn_truncation.py`, `turn_context_compaction.py`, `turn_recovery.py`, `turn_final_response.py::finish_text_response`, `turn_finalizer.py::finalize_turn` | Ordered separation of concerns: preflight/compaction gate → request assembly → provider call → error handling → response normalisation → tool round *or* final response → finalizer. Persist-before-execute is a durability invariant. | Collapse the ~30 phase modules into a handful of private functions in one `agent` module, each returning an enum verdict (`Continue` / `Break` / `Return`). Keep the *order*, drop the dynamic-dispatch `_run_phase` machinery. | In (simplified) |
| Message representation | `agent/message_metadata.py::append_message`; `agent/message_sanitization.py`; `agent/turn_response_intake.py::normalize_model_response`; `agent/turn_request_assembly.py::assemble_api_request` | Messages are plain OpenAI-format maps `{"role": system\|user\|assistant\|tool, ...}`; assistant reasoning rides `assistant_msg["reasoning"]`; tool rows carry `name` + `tool_call_id` + `content`. Persisted form and wire form are separate copies. | Native Rust enums `Role`/`Message` with serde, plus a distinct `wire_message` projection built per request. Do not replicate the Python per-provider dict surgery. | In |
| Tool-call round-trip | `agent/turn_tool_round.py::run_tool_round` + `stage_tool_call_message`; `agent/turn_tool_validation.py::validate_tool_calls`; `agent/message_sanitization.py::coalesce_tool_call_id`, `uniquify_tool_call_ids`, `close_interrupted_tool_sequence`, `_repair_tool_call_arguments`, `normalize_finish_reason`; `hermes_state_messages.py::append_message` | Every emitted `tool_call` must receive exactly one `role=tool` result with the matching id (mismatched/duplicate ids are repaired or uniquified); the assistant tool-call row is persisted **before** any side effect; unknown tool names get error results and a 3-strike exit; malformed JSON args are retried then answered with error results to preserve role alternation. | One `ToolCall` id type enforced end-to-end; repair functions as total functions; unknown tools answered with a typed error result. Keep persist-before-execute and role alternation. | In |
| Empty-response guard | `agent/empty_response_guard.py::record_empty_attempt`, `deterministic_empty`, `empty_retry_budget`, `resolve_guard_settings`, `EmptyAttempt`; `agent/turn_empty_response.py::recover_empty_response`, `_retry_empty`, `_terminal_empty` | Distinguish an unsignaled empty completion from a real one; retry a bounded number of times with backoff; stop retrying once two consecutive empties share the same `(model, provider, finish_reason)` signature and usage proves zero output; fall back to a different model/provider rather than looping; terminal `"(empty)"` sentinel is persisted once. | Keep deterministic-empty detection and a bounded retry count. Drop the USD cost estimator; use a fixed retry budget. (silver later gained a pricing module, but the guard is still passed no cost, so the budget stays 3.) Reasoning-only prefill retry is optional and can be omitted. | In (reduced: deterministic-empty in, cost-aware budget out) |
| Iteration budget | `agent/iteration_budget.py::IterationBudget` (`consume`, `refund`, `used`, `remaining`); loop head `agent/conversation_loop.py::_run_conversation_turn` line 1528; refund at `agent/turn_tool_round.py` line 185 | Hard cap on model calls per run; `remaining == 0` ends the loop; a one-turn `_budget_grace_call` allows the wrap-up turn; programmatic-only tool rounds are refunded. | A per-run `u32` counter guarded by the run's own lock (silver runs one task per session, so no shared atomic is required). Configurable `max_iterations`; default 500. No subagent budget. | In |
| Repetition guard | `agent/repetition_guard.py::is_repetition_dominated`, `is_runaway_repetition`, `REPETITION_LOOP_INTERRUPTED`; used at `agent/conversation_loop.py` line 292 | Before continuing a `finish_reason=length` fragment, detect a fragment *dominated* by a repeated 60+ char window and abort/scrub it instead of stitching more copies into the reply. | Pure function over `&str` returning bool; same constants (below). No checkpoint/restart machinery. | In |
| Tool-call stall guardrails | `agent/tool_guardrails.py::ToolCallGuardrailController` (`before_call`, `after_call`, `observe_call`, `_detect_identical_cycle`, `_check_loop_cap`), `ToolCallGuardrailConfig`, `LoopCapConfig`, `ToolCallSignature`, `classify_tool_failure`, `append_toolguard_guidance`, `toolguard_synthetic_result` | Per-turn controller tracking exact-args failures, same-tool failures, idempotent no-progress repeats, consecutive identical calls (tool+args+result), and repeating multi-call cycles. Warnings are guidance appended to the tool result; blocks/halts become synthetic results that stop the turn. Counters reset per turn. | One `ToolGuard` struct per turn with three `HashMap` counters + a bounded `VecDeque` history. Keep warn/block decisions; make hard-stop opt-in via config. Drop result-stub dedup in the first slice. | In (warn + optional halt) |
| Per-turn loop caps | `agent/tool_guardrails.py::LoopCapConfig`, `_LOOP_CAPS`, `_subagent_spawn_count` | Hard per-turn ceilings on runaway-prone tools, checked before the call so the (cap+1)-th is refused, independent of `hard_stop_enabled`; `0` disables. | Generic map `tool_name -> (counter, cap)` in the same guard struct. Web-search/subagent caps only become live when those tools are added. | Mechanism In; both caps are live now that web_search and delegate_task are registered |
| Turn liveness watchdog | `agent/turn_liveness.py::TurnLivenessWatchdog` (`_tick`, `_sample`, `schedule`), `resolve_turn_liveness_settings`, `ActivitySnapshot`; activity stamp via `AIAgent._touch_activity` | Sample an activity clock every poll; if idle exceeds the timeout, force-abort the turn and stop lease renewal so the session can be reclaimed. The abort must revalidate `(generation, timestamp)` under the same lock as activity stamps, so a resumed turn is never cancelled. | Tokio interval task per run holding `last_activity: Instant` + an `AtomicU64` generation; abort only if generation is unchanged; disable with `timeout_s <= 0`. | In |
| Memory MEMORY.md / USER.md | `tools/memory_tool.py::memory_tool`, `MEMORY_SCHEMA`, `get_memory_dir`; `tools/memory_tool_store.py::MemoryStore` (`load_from_disk`, `_render_block`, `add`/`replace`/`remove`/`apply_batch`, `_file_lock`, `_system_prompt_snapshot`), `ENTRY_DELIMITER`, `MEMORY_BLOCK_HEADERS`; `agent/system_prompt.py::_memory_parts` | Two files per scope (`MEMORY.md` agent notes, `USER.md` user profile); entries joined by a separator; char budgets; a **frozen snapshot** captured at run start enters the prompt and mid-run writes never mutate the prompt; writes are serialized and atomic; the model gets no public `read` op. | One Rust `MemoryStore` per scope, `PathBuf` + `tokio::sync::Mutex` per scope, temp-file + rename atomic write; snapshot cloned at run start. Keep the char limits (2200 / 1375) and separator. | In |
| Memory tool | `tools/memory_tool.py::memory_tool`, `_validate_single_op`, `_apply_write_gate`, `apply_memory_pending`, `check_memory_requirements`; `tools/memory_tool_store.py::MemoryStore._find_unique_match` | `add` / `replace` / `remove` with unique-match requirement for replace/remove; errors are explicit and include current entries; either a single op or an atomic batch; never receives `workspace_id` (scope comes from `RunContext`). | Concrete enum `MemoryOp` parsed from tool args; `replace`/`remove` use substring-unique match, returning the entry inventory on ambiguity. Drop the approval-staging path (approval lives at the tool-dispatch layer). | In |
| Memory external providers | `agent/memory_manager.py::MemoryManager`, `build_memory_context_block`, `inject_memory_provider_tools`, `sanitize_context`; `agent/memory_provider.py` (ABC) | Optional additive external memory context fenced as untrusted data, and provider-supplied tools. | Out of MVP; the built-in files own memory. Keep only the *concept* that external context, if ever added, is fenced and never authoritative over policy. | Out |
| Sessions | `hermes_state.py` (SessionDB facade) + `hermes_state_schema.py`, `hermes_state_sessions.py::SessionSessionsMixin`, `hermes_state_messages.py::SessionMessagesMixin`, `hermes_state_ids.py`, `hermes_state_wal.py` | A session is a durable, ordered transcript owned by exactly one scope (workspace or global); messages carry role/content/tool-call metadata; sessions are created implicitly by a run or explicitly by API; WAL + foreign keys + busy timeout. | silver's own schema with `sessions`, `messages`, `runs` tables; no compression-lineage / branch / profile columns. One module `sessions.rs` with typed SQL, not a mixin facade. | In |
| Session search | `tools/session_search_tool.py::session_search`, `_dispatch`, `_discover`, `_read_scoped`, `_scroll`, `_list_recent_sessions`, `SESSION_SEARCH_SCHEMA`; `hermes_state_search.py::SessionSearchMixin`, `hermes_state_fts.py` | `session_search(query)` searches only the run's scope; the model can never supply or override the scope filter; discovery returns links/snippets, or reads a session / scrolls around a message. | First slice: scoped `LIKE` with small limits; add SQLite FTS5 only when needed. Scope is injected from `RunContext`, never from tool args. | In |
| Run lifecycle | `gateway/run.py` (GatewayRunner facade); `gateway/run_turn.py::GatewayTurnMixin._handle_message_with_agent`, `_hmwa_acquire_turn_lease`, `_hmwa_persist_turn_transcript`, `_hmwa_deliver_turn_response`, `_hmwa_close_failed_turn`; `gateway/run_turn_runner.py::TurnRunner` | A run moves through queued → running → (waiting_approval) → completed/failed/cancelled; one active turn per session via a turn lease; transcript is persisted before delivery; terminal state is persisted and observable. | silver `RunManager` owns this; a `RunId` keyed state machine; SQLite row + in-memory handle; lease is the per-session mutex returning `409 session_busy`. | In |
| Streaming | `gateway/stream_consumer.py::GatewayStreamConsumer`; `gateway/run_turn_runner.py::_setup_stream_consumer`, `progress_callback`, `send_progress_messages`, `_stream_consumer`; `gateway/platforms/base.py::BasePlatformAdapter.send_draft` | Incremental text deltas plus tool-lifecycle progress events reach the client; the final response is declared once (no duplicate final after interim commentary); slow consumers cannot block the loop. | silver emits `text.delta` / `tool.*` SSE events from the run manager; a bounded per-subscriber channel drops a slow subscriber rather than blocking. No platform-specific stream adapters in core. | In |
| Approval | `gateway/run_turn_runner.py::_approval_notify_sync`, `_renders_exec_approval_buttons`, `_ExecApprovalDeclined`; `gateway/run.py::_format_exec_approval_fallback`, `_approval_send_outcome`, `_redact_approval_command`, `_APPROVAL_TIMEOUT_SECONDS` (300); `gateway/run_turn_runner_approval_settle.py::register_timeout_notice`; `gateway/platforms/base_exec_approval.py::approval_timeout_seconds` | An approval is bound to a specific tool call (args hash + pending state); the run enters `waiting_approval` and emits `approval.required`; approve executes exactly that call, deny returns a denied result to the loop; timeout resolves per policy; every decision is persisted. Approvals must bypass the busy-session queue. | silver stores an `approvals` row keyed by `approval_id` + `run_id` + args hash; the waiting task selects on the decision channel with a timeout. Single approval channel; no button rendering in core. | In |
| Steer | `agent/interrupt_control.py::InterruptControlMixin.steer`, `_drain_pending_steer`, `clear_interrupt`; `gateway/run_busy.py::_steer_running_agent`, `_steer_active_subagents`, `_busy_steer_command`, `_resolve_busy_steer_or_redirect` | Steer queues user text for injection at a safe point in the loop (after a tool result), without interrupting a mutating tool and without starting a new turn; multiple steers concatenate. | A `pending_steer: Option<String>` on the run, drained at the top of each iteration after tool results; role-safe as a standalone user row. | Shipped after the MVP: `POST /v1/runs/{id}/steer`, drained at the top of each iteration, reported as `steer.delivered` |
| Redirect | `agent/interrupt_control.py::InterruptControlMixin.redirect`, `_drain_pending_redirect`, `_has_pending_redirect`; `gateway/run_busy.py::_resolve_busy_steer_or_redirect` | Redirect cancels only the in-flight model request, keeps completed messages, appends the correction as a real user message, and retries the same logical iteration; during tool execution it degrades to steer. | Same pending-slot pattern as steer, but sets the cancel token for the provider call. | Out of MVP |
| Stop / interrupt | `agent/interrupt_control.py::InterruptControlMixin.interrupt`, `hard_interrupt`, `clear_interrupt`; `gateway/run.py::_INTERRUPT_REASON_STOP`/`_RESET`/`_TIMEOUT`; `gateway/run_busy.py::_busy_stop_command`, `_interrupt_running_agent_for_busy_event` | `stop` is a hard, idempotent cancellation: abort the in-flight provider request and tool process group, release the session, and produce a terminal `run.cancelled`. Soft interrupt (new message) vs hard stop are distinct and attributed. | silver `CancellationToken` per run; provider call and tool child process select on it; `POST /stop` sets it and returns known state. Attribution stored on the terminal event. | In |
| Path confinement | `tools/file_tools_paths.py::_resolve_base_dir`, `_authoritative_workspace_root`, `_registered_task_cwd_override`, `_sentinel_free_abs_cwd`, `_resolve_path_for_task`, `_path_resolution_warning` | Relative paths anchor to the run's authoritative workspace root (never the daemon process cwd); absolute paths are normalized; a relative path resolving outside the workspace root warns. | silver resolves every tool path against the run's registered canonical root and rejects paths that escape it (INV-6/INV-7 in architecture.md). No per-task cwd registry in the MVP. | In |
| File safety (credential denylist) | `agent/file_safety.py::build_write_denied_paths`, `build_write_denied_prefixes`, `get_safe_write_roots`, `build_write_approval_paths`, `_classify_write_denial`, `is_write_denied`, `get_write_denied_error`, `_HERMES_PROTECTED_SUBPATHS`, `_BLOCKED_PROJECT_ENV_BASENAMES`, `_CREDENTIAL_FILE_NAMES`, `_READ_DENIED_DIRS`, `get_read_block_error` | Writes to credential/system files and directories are denied or approval-gated; reads of daemon-owned secret stores are blocked; project-local `.env*` files are blocked anywhere. | A static deny-list of path prefixes checked by the file tools, plus a write-approval predicate. Deny messages are stable error codes. | In (simplified) |
| Gateway / adapters | `gateway/platform_registry.py::PlatformRegistry`, `PlatformEntry`, `register_deferred`; `gateway/platforms/base.py::BasePlatformAdapter`, `SendResult`, `ExecApprovalPrompt`; `gateway/platforms/_shared.py` (`get_scoped_secret`); `gateway/run_adapters.py`; adapters `signal.py`, `webhook.py`, `whatsapp_cloud.py`, `yuanbao.py`, `qqbot/adapter.py`, `weixin.py`, `bluebubbles.py`, `msgraph_webhook.py`, `api_server.py`; guide `gateway/platforms/ADDING_A_PLATFORM.md` | Adapters self-register (no hardcoded name ladder); one adapter per platform maps inbound messages to a session and outbound events to transport; adapters are dumb clients of the core. | silver keeps adapters as **separate processes** that speak the HTTP/SSE API; no in-process adapter ABC in the daemon for the MVP. Telegram was the planned first adapter; none is implemented. Plugin/WASM registration is out. | In (design); no adapter implemented; plugin platform registry out |

---

## Detections

These are the exact algorithms and constants to port. Numeric values are authoritative from the
upstream source; do not "round" them.

### 1. Repetition-dominated fragment detection

Source: `agent/repetition_guard.py`.

```python
MIN_FRAGMENT_LENGTH = 400
_REPEAT_WINDOW = 60
_MIN_REPEAT_COUNT = 5
_DOMINANCE_RATIO = 0.5
_RUNAWAY_DISTINCT_LINE_RATIO = 0.5
REPETITION_LOOP_INTERRUPTED = "[the reply degenerated into a repetition loop and was interrupted]"
```

- `is_repetition_dominated(text)` returns `False` for non-strings or `len < 400`.
  - Fast path `_line_repetition_dominated`: split on lines, strip, drop empties; if any single
    normalized line has count `>= 5` **and** `count * len(line) >= len(text) * 0.5`, it is dominated.
  - General path: a 60-char window slid one char at a time; a window must appear
    `needed = max(5, ceil(n * 0.5 / 60))` times to trip. First window reaching `needed` returns True.
- `is_runaway_repetition(text)` is stricter: requires `is_repetition_dominated` **and** either
  fewer than 5 non-empty lines (single-line loop) or
  `distinct_non_empty_lines <= non_empty_lines * 0.5`. This is the shape used to *drop* a partial
  on interrupt (`agent/conversation_loop.py::_apply_active_turn_redirect`, line 292); a merely
  repetitive but correct batch reply must not qualify.

### 2. Deterministic-empty + cost-aware retry budget

Source: `agent/empty_response_guard.py`; consumed by `agent/turn_empty_response.py::_retry_empty`.

```python
DEFAULT_EMPTY_RETRY_BUDGET = 3
REDUCED_EMPTY_RETRY_BUDGET = 1
DEFAULT_COST_THRESHOLD_USD = Decimal("0.25")
DEFAULT_GUARD_ENABLED = True
```

- `_zero_output(agent, response)`: normalise usage. Usage absent, or `output_tokens is None`, or
  `prompt_tokens <= 0` → `(usage_present=False, zero_output=False)` (fail open). Otherwise
  `zero_output = (output_tokens + reasoning_tokens) == 0` — reasoning tokens count as real generation.
- `deterministic_empty(agent)`: needs `len(attempts) >= 2`; all attempts share the same
  `(model, provider, finish_reason)` signature; and either every attempt has
  `usage_present and zero_output`, or every attempt has `not usage_present and not observed_generation`.
  Mixed evidence fails open.
- `empty_retry_budget(agent, response)`: if the estimated input cost of one attempt is
  `>= 0.25 USD`, the budget is 1; otherwise 3. Unknown pricing leaves it at 3.
- Retry rule in `_retry_empty`: retry only while
  `empty_candidate and _empty_content_retries < budget and not deterministic_empty`; backoff is
  `jittered_backoff(n, base_delay=5.0, max_delay=60.0)`. On exhaustion try the fallback chain, then
  persist one terminal `"(empty)"` sentinel row.
- Related bounded retries in the same phase: thinking-only prefill continuation at most **2** times
  (`agent/turn_empty_response.py`, line 226); incomplete `<REASONING_SCRATCHPAD>` retried at most **2**
  times then saved as a partial (`agent/turn_response_intake.py::normalize_model_response`, lines 155–169).

### 3. Identical-call streak and multi-call cycle detection

Source: `agent/tool_guardrails.py` (`ToolCallGuardrailController.observe_call`,
`_detect_identical_cycle`).

```python
STALL_GUARD_IDENTICAL_CALL_THRESHOLD = 3
_STALL_GUARD_MAX_CYCLE_PERIOD = 4
_STALL_GUARD_CYCLE_HISTORY = 64
IDENTICAL_RESULT_STUB_MIN_CHARS = 512
_RESULT_STUB_ARGS_PREVIEW_CHARS = 120
STALL_GUARD_REPEATABLE_TOOLS = frozenset({"process_manage"})
_STALL_GUARD_REPEATABLE_SUFFIXES = ("_get_result", "_poll")
```

- Signature identity: `ToolCallSignature.from_call(tool_name, args)` = `(tool_name, sha256(canonical
  sorted-key compact JSON of args))`. `_sha256` encodes with `surrogatepass`. Result hash
  `_result_hash(result)` = sha256 of canonical JSON when the result parses, else sha256 of the raw
  string.
- **Consecutive identical streak**: `(signature, result_hash)` equal to the previous observation
  increments the streak; anything else resets it to 1 (0 for non-string/multimodal results, which also
  clear the cycle history). At `count >= 3` a notice is appended unless the tool is repeatable
  (`process_manage`, or name ending `_get_result`/`_poll`). With `hard_stop_enabled`, at
  `count >= no_progress_block_after` (default 5) the controller records an
  `identical_call_streak_halt`.
- **Result dedup stub**: from the 2nd byte-identical repeat, when the call did not fail and
  `len(result) >= 512`, the current result is replaced by a reference stub (tool name, first
  `tool_call_id`, args preview capped at 120 chars, plus the persisted-output path if known).
- **Multi-call cycle**: history is a `deque(maxlen=64)` of `(signature, result_hash, repeatable)`.
  For each period `2..4`, require `len(history) >= period * 3`; count trailing laps whose each
  position matches (signature **and** result hash) the final lap; `laps >= 3` fires. A cycle made
  **only** of repeatable tools is exempt. Returns `(period, laps)`; emits an
  `identical_cycle_notice`; with `hard_stop_enabled` and `laps >= no_progress_block_after`
  records an `identical_cycle_halt`.
- Guardrail verdict text is injected as an appended note on warn/halt
  (`append_toolguard_guidance`) or as a synthetic `role=tool` JSON error
  (`toolguard_synthetic_result`, with `guardrail` metadata).

### 4. Exact-failure / same-tool-failure / no-progress counters

Source: `agent/tool_guardrails.py::ToolCallGuardrailConfig`. The values below are upstream's;
silver uses the same thresholds but defaults `hard_stop_enabled` to true (`agent.tool_call_hard_stop`).

```python
warnings_enabled = True
hard_stop_enabled = False
non_interactive_hard_stop_enabled = True
exact_failure_warn_after = 2
exact_failure_block_after = 5
same_tool_failure_warn_after = 3
same_tool_failure_halt_after = 8
no_progress_warn_after = 2
no_progress_block_after = 5
```

- `hard_stop_enabled` is forced to True on non-interactive platforms:
  `_is_non_interactive_platform(platform)` is true unless the platform is in
  `_ATTENDED_PLATFORMS = {cli, tui, desktop, acp, subagent, api_server}`.
- **Exact-failure** (`signature` key): increments on a failed call. A successful call to any
  `PROGRESS_RESET_TOOL_NAMES` tool, or any landed file mutation
  (`file_mutation_result_landed`), marks every currently-counted signature as "progress since
  failure"; the next failure of that signature resets its exact count. Block at
  `exact_failure_block_after = 5` (before call), warn at `>= 2`.
- **Same-tool-failure** (tool-name key, different args): increments on any failure of that tool.
  Warn at `>= 3`; halt at `>= 8` only when `hard_stop_enabled` and the tool is **not** in
  `FAILURE_TOLERANT_TOOL_NAMES = {terminal, execute_code, process_manage, process, browser_navigate,
  web_extract}`.
- **No-progress** (signature key, idempotent tools only): `_is_idempotent` = tool is in
  `IDEMPOTENT_TOOL_NAMES` and not in `MUTATING_TOOL_NAMES`. On a successful idempotent call, if the
  result hash equals the previous stored hash, increment `repeat_count`, else set it to 1. Warn at
  `>= 2`; block at `>= 5` (before call).
- **Failure classification** `classify_tool_failure`: a landed file mutation or a guardrail refusal is
  *not* a failure; `terminal` fails only on non-zero `exit_code`; `memory` fails on
  `success is False and "exceed the limit" in error`; otherwise the first 500 chars lowercased are
  checked for `'"error"'` or `'"failed"'`, or the result starts with `"Error"`.
- Threshold parsing `_int_at_least(value, default, minimum=1)` falls back to the default for junk or
  below-minimum values. `0` only has meaning for caps (minimum 0).

### 5. Per-turn loop caps

Source: `agent/tool_guardrails.py::LoopCapConfig`, `_LOOP_CAPS`, `_check_loop_cap`,
`_subagent_spawn_count`.

```python
_DEFAULT_MAX_WEB_SEARCHES_PER_TURN = 50
_DEFAULT_MAX_SUBAGENTS_PER_TURN = 50
```

- Caps are checked **before** the call, so the (cap+1)-th call is refused; they apply regardless of
  `hard_stop_enabled`; `0` disables.
- `web_search` increments by 1; `delegate_task` increments by `_subagent_spawn_count(args)`
  (`0` for control actions `list`/`steer`/`stop`, else `len(tasks)` for a non-empty list, else `1`).
- Refusals use codes `loop_web_search_cap` / `loop_subagent_cap` and become synthetic error results.
- Other per-turn caps worth keeping conceptually: `_MAX_OUTER_LOOP_ERRORS = 8` in
  `agent/conversation_loop.py` (max exceptions escaping the inner retry machinery per user turn), and
  `MemoryStore._MAX_CONSOLIDATION_FAILURES_PER_TURN = 3` in `tools/memory_tool_store.py`.

### 6. Sampled-idle turn liveness

Source: `agent/turn_liveness.py`.

```python
DEFAULT_TURN_LIVENESS_TIMEOUT_S = 600.0
DEFAULT_TURN_LIVENESS_POLL_S = 15.0
MIN_TURN_LIVENESS_POLL_S = 0.01
_CONFIG_TIMEOUT_KEY = "agent.turn_liveness.timeout_s"
_CONFIG_POLL_KEY = "agent.turn_liveness.poll_s"
```

- `resolve_turn_liveness_settings` returns `(timeout_s, poll_s)`; non-finite/malformed values
  (NaN, Inf, typo) warn and fall back to the defaults; `poll_s <= 0` falls back to 15.0;
  `timeout_s <= 0` opts out (`None`).
- `TurnLivenessWatchdog.schedule` polls every `poll_s` via the shared periodic scheduler.
- `_sample` takes the **same lock** as the activity stamp and reads
  `_turn_liveness_activity_generation` and `_last_activity_ts`; `idle_seconds = max(0, now -
  activity_ts)` (0.0 when no stamp). A missing turn-active flag ends the watchdog.
- Fire when `idle_seconds >= timeout_s`: surface an observational stall warning (rate-limited per
  activity generation), then call `commit_abort(snapshot, message)`. The commit revalidates the
  `(generation, timestamp)` under the activity lock and can veto (returns False) if the turn resumed.
  On a committed abort, deactivate the turn so lease renewal stops, then publish the definitive
  aborted settlement. Default abort message: `"Turn made no progress for {int(idle)}s; aborting to
  release the session."`
- Activity is stamped between iterations (`agent._touch_activity(...)`), notably by
  `agent/turn_tool_round.py` after tool results, so the watchdog measures real progress rather than
  one slow step.

---

## What silver left out, and what it later added

The list below began as the MVP's exclusions. Entries marked **shipped** were added later;
everything else is still out, and nothing in it should be ported or designed around without a
concrete need.

- **Context compression / summarisation** — **shipped**, simplified: deterministic compaction in
  `crates/silver-core/src/agent/compact.rs` plus an optional auxiliary-model summary, sized per
  run from the model's context window. Not ported: the 85% / 50% threshold layers, native
  Responses/Codex compaction and failure cooldowns (architecture.md 14.2).
- **Mixture-of-Agents (MoA)** — **shipped** as `[moa]` reference advisors briefing the acting
  model (`apps/silver/src/moa.rs`); not the upstream `moa_loop` machinery.
- **Provider credential pools / failover plumbing** — **shipped**, simplified: a key pool and a
  fallback chain with a 60 s to 4 h cooldown ladder (`credential_pool.rs`, `fallback.rs`), and
  native transports for Anthropic, Codex, Vertex, Bedrock, Copilot, ACP and OpenCode
  (provider-setup.md). Not ported: `credential_persistence.py`; the pool and cooldowns are in
  memory only. The last-seen rate-limit headers are exposed at `GET /v1/provider/status`.
- **Plugins / extension ABI** — `plugins/`, `hermes_cli/plugins*.py`, plugin hooks,
  `PluginContext`, WASM/Extism.
- **Browser automation** — `tools/browser_*.py`, `browser_provider.py`, `browser_registry.py`,
  `browser_control_*.py`.
- **TTS / STT / voice** — `agent/tts_*.py`, `agent/transcription_*.py`, `tools/tts_*.py`,
  `tools/transcription_*.py`, `tools/voice_*.py`, `tools/wake_word*.py`.
- **Cron / scheduling** — `cron/`, `agent/periodic_scheduler.py`, `tools/cronjob_tools.py`.
  (The liveness watchdog's poll loop is a per-run task, not a cron service.)
- **Kanban** — `tools/kanban_tools*.py`, `agent/kanban_stop.py`, `gateway/kanban_watchers*.py`,
  `plugins/kanban/`.
- **Multi-profile / multiplex** — partly: `SILVER_PROFILE` selects one profile directory per
  process. Not ported: `agent/secret_scope.py`, `gateway/run_profile_reconcile.py`, the
  cross-profile warning machinery
  (`agent/file_safety.py::classify_cross_profile_target`), one-process-many-profiles routing.
- **External memory providers** — `agent/memory_manager.py::MemoryManager`,
  `agent/memory_provider.py` providers, secret/vault backends
  (`agent/vault_backends/`, `agent/secret_sources/`).
- **Session factories/branches/compression lineage** — `hermes_state` branch/reset lineage,
  `hermes_state_portability.py`, `hermes_state_compression.py`. `hermes_state_rewind.py` has a
  small counterpart: `POST /v1/sessions/{id}/rewind` drops the newest user turn.
- **Subagents / delegation, partly** — `tools/delegate_task.py`'s batch and runtime are ported
  (docs/architecture.md 12.6). `async_delegation.py`, spawn depth, the background task queue and
  the steer/stop action are not: a delegation runs in the foreground, to a report, and a
  subagent cannot spawn another. A subagent holds its own iteration budget
  (`[delegation] max_iterations`) rather than a shared pool, so the parent and each subagent can
  together exceed `agent.max_iterations`.
- **Skills marketplace / hub / curator** — `tools/skills_hub*.py`, `tools/skill_*_manager*.py`,
  `agent/curator*.py`, `agent/learning_graph*.py`.
- **MCP client** and **web search/extract** — **shipped**: `[[mcp.server]]` (stdio and http; no
  OAuth, sampling or server mode) and `web_search`/`web_extract` over Brave or Tavily and plain
  HTTP (`apps/silver/src/mcp/`, `web.rs`).
- **Connectors, image/video generation, computer use, terminal backends beyond local** —
  `tools/connectors/`, `tools/image_generation*.py`, `tools/video_generation_tool.py`,
  `tools/computer_use/`, `tools/environments/*` (docker/ssh/modal/daytona/singularity).
- **OpenAI-compatible API, dashboard, desktop, ACP server** — `gateway/platforms/api_server*.py`,
  `hermes_cli/web_*`, `apps/desktop/`, `acp_adapter/`, `tui_gateway/`. silver's client is its own
  web UI over its own HTTP/SSE API. silver is an ACP *client* only, through the `copilot-acp`
  provider.

Also excluded, and not to be introduced by this port:

- plugin ABI, dynamic libraries or WASM plugins;
- generic workflow / multi-agent orchestration;
- full OpenAI API compatibility;
- distributed execution;
- Postgres, Redis, external queues or event bus;
- Kubernetes, service mesh or microservices;
- multi-tenant authentication;
- memory synchronisation across machines;
- vector databases or embeddings for memory;
- memory inheritance between global and workspace scopes;
- a skills marketplace;
- arbitrary remote command execution;
- internal process daemonisation;
- generic repository/service/provider abstractions without a concrete need.

Steer shipped after the MVP (see the Steer row above). **Redirect** (cancel only the in-flight
model request and retry with a correction) is still not implemented.

## Known deviations from Hermes

Behaviour silver deliberately does differently from the upstream module it ports:

- **Cooldowns**: credential-pool and fallback cooldowns use one 60 s to 4 h ladder, not Hermes'
  per-status TTLs, and live in memory.
- **MCP**: tools are named `<server>__<tool>` (Hermes uses `mcp__<server>__<tool>`); MCP OAuth,
  sampling, elicitation and server mode are not ported.
- **OAuth**: sign-in covers Nous, OpenRouter and ChatGPT/Codex, plus a generic device flow. The
  xAI, MiniMax, Qwen and Anthropic subscription flows are not implemented (the key-based presets
  cover those providers).
- **Cost**: `[cost]` is a spend cap that refuses a run; Hermes' `model_cost_guard.py` only warns
  about an expensive model.
- **Profiles**: `SILVER_PROFILE` picks one profile directory per process; there is no multiplexed
  gateway, sticky default or clone/export.
- **LSP** covers the common requests (diagnostics, hover, definition, references, symbols,
  rename), and worktree cleanup omits local-branch GC.
- **`patch`** advertises only the replace fields; the unified-diff mode is still accepted.
