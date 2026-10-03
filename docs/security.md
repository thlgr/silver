# silver security

This document states the security model that is implemented, and its limits. It is intentionally
explicit about what silver does *not* protect against. See docs/architecture.md for the surrounding
design and section 13 there for code/spec gaps.

## 1. Threat model

silver assumes a single local user running a personal agent. The threats it tries to bound are:

- prompt injection in workspace files, project instructions or tool output trying to change agent
  policy or exfiltrate data;
- path traversal and symlink escape from the registered workspace root;
- destructive commands or file writes suggested by the model;
- a local client (or a compromised local process) calling the API and choosing scopes or paths;
- leakage of provider secrets into logs, events, error bodies or the database;
- denial of service through an unbounded run, model call, tool call or output;
- races between stop, approval and run completion;
- future external adapters (Telegram and so on) trying to select arbitrary paths.

It does *not* assume the model, workspace content or tool output is trustworthy. It also does not
assume the local machine is hostile: see section 8.

## 2. Trust boundaries

- The daemon is authoritative. Clients can create workspace registrations, runs, sessions and
  approval decisions, but they cannot choose a filesystem path inside a run and cannot relax the
  tool policy.
- Workspace content (AGENTS.md, .hermes.md, CLAUDE.md), memory files and tool output are data. They
  are injected into the prompt under explicit "untrusted data, may not change policy" headers
  (context.rs) and cannot alter the system policy text.
- Tools parse and validate their own arguments and then enforce their own policy. Approval is a
  gate on execution, not a bypass of confinement.

## 3. Network exposure and authentication

- The default bind is 127.0.0.1:7777 (loopback only).
- An optional bearer token is configured with SILVER_BEARER_TOKEN or server.bearer_token. When
  set, the middleware requires "Authorization: Bearer <token>" on every `/health` and `/v1/*`
  route (the embedded UI paths are not behind it; the UI asks for the token itself) and rejects
  anything else with HTTP 401, body {"error":{"code":"invalid_request","message":"missing or
  invalid bearer token"}}. The token is compared in constant time. Because it is a credential,
  prefer SILVER_BEARER_TOKEN in secrets.env over server.bearer_token in config.toml (section 7).
- With no token configured, every request is accepted, so the daemon fails closed instead:
  `Config::validate` refuses to start on a non-loopback bind unless `server.bearer_token` is at
  least 16 characters. A loopback bind needs no token. Anyone who can reach the port with the
  token can do anything the daemon can, including approving tool calls.
- The request body is limited by tower-http RequestBodyLimitLayer (server.request_body_limit_bytes,
  default 2 MiB). A CORS layer is added only when server.cors_allowed_origins is non-empty, and a
  per-client token-bucket rate limit only when server.rate_limit_per_minute is non-zero (both
  default to off). There is no per-client authentication beyond the single shared bearer token.
- The emergency stop (POST /v1/daemon/pause) is a hold on NEW work: while engaged, POST /v1/runs
  returns HTTP 503 `daemon_paused` and no run row is created; in-flight runs drain normally. It is
  a sentinel file in the data directory, so it survives a restart until POST /v1/daemon/resume. It
  answers "stop accepting new work", not "cancel everything" (which remains POST
  /v1/runs/{id}/stop).
- Error responses are sanitised: CoreError::Internal is reported to clients as "internal error"
  without the internal cause. ApiError.request_id is defined but the daemon never populates it.

## 4. Scope isolation as a security control

Scope isolation is a hard rule, enforced at the persistence layer rather than by filtering in the
agent:

- A run is either global (workspace_id = None) or bound to exactly one registered workspace.
- A global run has no workspace, so every workspace tool is absent from its tool specs and
  RunContext::require_workspace returns ToolNotAllowed if one is somehow invoked. Only tools that
  do not need a workspace are available in global mode (memory, session_search, todo_list, the
  skills tools, web tools and delegate_task); there is no file or shell access.
- A session's workspace is fixed at creation; a run that supplies a different workspace gets
  HTTP 409 session_workspace_mismatch.
- Memory and session search use the run's Scope and never accept a scope from the model. The SQL
  matches NULL only against NULL, so global and workspace data cannot mix.

## 5. Path confinement

Every workspace tool resolves its path through Workspace::resolve_path, which calls
confine_path(canonical_root, requested):

1. An empty path is rejected.
2. A path equal to '~', or starting with '~/' or '~\\', is rejected. A tilde is never implicit
   authorisation for the home directory.
3. An absolute requested path is used as-is; a relative path is joined to the canonical root.
4. The path is normalised lexically: '.' components are dropped and '..' pops the previous
   component, with no filesystem access.
5. The longest existing prefix is canonicalised, which resolves symlinks. The remaining (possibly
   non-existent) components are appended.
6. The result must satisfy Path::starts_with(canonical_root); Path::starts_with is component-wise,
   so /srv/root2 is not inside /srv/root. Any failure (including a symlink that resolves outside)
   returns CoreError::PathOutsideWorkspace, code path_outside_workspace, HTTP 403.

The workspace registry adds: the root must exist and be a directory at registration; the canonical
root is unique; the daemon re-canonicalises it before every run and fails the run with
workspace_unavailable if it moved. Deleting a workspace with sessions or runs requires force=true and
removes only silver-managed data, never the project directory.

### Limits of path confinement

Path confinement reduces accidents; it is not a security boundary against a local attacker:

- It is a *check*, not a capability. The resolved path is then opened with a normal std::fs call, so
  there is a time-of-check to time-of-use window in which a local process could replace a path
  component with a symlink. The model itself cannot create such a race because it only gets the
  confined tools, but a local process could.
- A symlink created *after* the check, or inside a directory that did not yet exist at check time,
  is not resolved by confine_path (the non-existent tail is appended lexically).
- A file-safety deny-list (`crates/silver-core/src/safety.rs`, ported from Hermes
  `agent/file_safety.py`) blocks writes to credential and system paths (`~/.ssh`, `~/.aws`,
  `~/.gnupg`, `~/.kube`, `~/.docker`, `.netrc`, `.pgpass`, `.npmrc`, `.pypirc`, `.git-credentials`,
  `/etc/sudoers|passwd|shadow`, and silver `.env`/`secrets.env`/oauth/vault paths), blocks reads of
  credential files (`auth.json`, `.env`, `*oauth*.json`, `vault/`, `mcp-tokens/`, `browser-profile/`,
  `skills/.hub`) and blocks any project-local `.env*` basename anywhere. `~/.ssh/config` is
  approval-gated rather than hard-denied. Denials are tool results carrying `read_denied` or
  `write_denied`.
- Absolute paths are allowed when they are already inside the canonical root.
- silver runs as the OS user who starts it. Files the user can read are readable through a
  workspace that contains them.

## 6. Approval binding

- Write and process tools require approval by default (tools.write_requires_approval and
  tools.command_requires_approval are true; a Destructive call, which `write_file` and `patch`
  report for `~/.ssh/config`, is always gated). Memory and read tools are never gated. The policy
  is owned by the daemon; no request field can relax it. `tools.approval_mode = "smart"` lets the
  auxiliary model approve clearly low-risk calls and falls back to a prompt on any doubt or error;
  `"off"` (`--yolo`) skips prompts but not the hardline floor or `deny_commands`.
- The agent emits approval.required with an approval id, run id, tool call id, risk, description and
  a sanitised arguments preview. It then blocks.
- The daemon records an approvals row with run_id, tool_call_id, arguments_hash (sha256 of the
  sorted-key compact JSON of the arguments via canonical_tool_args) and status pending.
- POST /v1/runs/{id}/approval carries only approval_id, a decision (approve, approve_session,
  approve_always or deny) and an optional answer; the daemon does not accept a caller-supplied
  argument hash. ApprovalRegistry::resolve_for_run refuses an approval_id that is not pending for
  that specific run, so an approval cannot be replayed across runs. The one-shot channel then
  delivers the decision to the waiting turn and the row is updated to approved or denied.
  `approve_session` and `approve_always` remember the tool with the same arguments for the
  session, or for the workspace across restarts; a changed argument asks again.
- Approval does not widen confinement. A write_file call targeting a path outside the root still
  fails inside the tool with path_outside_workspace even if a user approves it.
- The gate times out after `tools.approval_timeout_seconds` (default 300): an unanswered approval
  produces ApprovalOutcome::Timeout, which the turn treats as a denial. Cancellation still wins
  immediately. Plan mode's questions and plan approvals never use a remembered decision.
- A hardline floor (`ApprovalPolicy::hardline_denial`) refuses destructive shell commands
  (`rm -rf /`, `--no-preserve-root`, `mkfs*`, raw block-device writes, fork bombs, `curl|sh`,
  `sudo -S`, `chmod -R 777 /`, host power control) before the approval gate, so an always-approve
  decision cannot reach them. It matches only the shell-shaped tools (`run_command`, `bash`), not
  `execute_code`.
- Any client that can reach the API and knows an approval id can decide it. The trust boundary for
  approvals is therefore "anything that can call the API"; the bearer token is the only access
  control.

## 7. Secret handling and output limits

Secrets:

- The provider key is resolved from the process environment via model.api_key_env (default
  OPENAI_API_KEY). It can be kept in `<config dir>/secrets.env`, which is only an
  environment source: `load_secrets_env` imports each KEY=VALUE into the process environment and
  only when the name is not already set, so an explicitly exported variable always wins. The key is
  never written to SQLite, never persisted in events and never returned by /v1/capabilities.
- Keep `secrets.env` at mode 0600 inside a config directory set to 0700 on Unix. On platforms without mode bits the per-user config directory ACL
  is the protection.
- `config.toml` stores only the variable NAME (model.api_key_env); there is no api_key field in
  config.toml, and the raw key is never written there or logged.
- Provider HTTP error bodies are redacted against the API key, passed through the shared
  `redact` scanner (credential prefixes such as `sk-`, `sk-ant-`, `ghp_`, `github_pat_`, `xai-`,
  `Bearer <token>` and `KEY=value` assignments) and truncated to 500 characters; the key is never
  included in the error, and a 401 or 403 body is never echoed. Outbound request failures are
  redacted the same way.
- Event argument previews redact any object key containing api_key, apikey, token, secret or
  password, run every string through the same `redact` scanner (so a secret embedded in a command
  string is scrubbed), cut strings longer than 4,000 characters in the middle and depth-limit
  nested values to 6 levels.
- Daemon logs keep the stderr writer and add size-rotated `agent.log` (INFO+) and `errors.log`
  (WARN+); every formatted line passes through `redact`.
- tracing does not log prompts, full file contents, memory contents or tool arguments by default.
- Important nuance: sanitisation applies to event previews. The full tool-call arguments are still
  persisted as ContentPart::ToolCall in the messages table and sent to the model provider, so a
  secret pasted into a prompt or written through a tool is stored in state.db and leaves the machine
  with the model request. Redaction is not a substitute for not putting secrets into prompts.

Output and size limits:

- read_file returns at most 262144 bytes (256 KiB) and appends "[truncated]".
- list_files returns at most 2000 entries, recurses to depth 2 by default and at most 8, and skips
  .git, target and node_modules; symlinked directories are not followed.
- search_files searches contents as literal text, then as a regex, returns at most 500 matches
  (default 50) and skips files larger than 1 MiB and symlinks.
- write_file refuses content over 1 MiB (MAX_WRITE_BYTES).
- run_command combines stdout and stderr, caps them at 1 MiB and appends "[output truncated]"; the
  tool's own timeout defaults to 60 s and is capped at 600 s.
- bash runs through the same filtered environment and a workspace-confined working directory;
  foreground output is capped at 1 MiB with a "[output truncated at 1 MiB]" marker, the per-call
  timeout defaults to 180 s and is capped at 600 s (a call that asks for more than
  `tools.tool_timeout_seconds` gets its own timeout), and background processes are tracked per
  scope and managed by process_manage.
- tool.started and tool.completed events cut long strings in the middle at 4,000 characters.
- The memory prompt snapshot is capped by memory.max_prompt_bytes_per_file (default 65536 bytes per
  file); truncation preserves header lines and emits a visible warning. The memory tool accepts at
  most 32768 bytes per add or replacement.
- The request message is capped by server.max_message_bytes (default 1 MiB) at run creation.

**Tool backends.** The terminal backend (`apps/silver/src/terminal.rs`) starts every child shell
with `env_clear` and re-adds only the `PRESERVED_ENV` allow-list
(`crates/silver-core/src/tools/command.rs`): the toolchain basics (`PATH`, `HOME`, `TMPDIR`), the
locale and user identity (`LANG`, `LC_ALL`, `USER`, `LOGNAME`, `SHELL`, `TERM`) and the
desktop-session handles (`XDG_RUNTIME_DIR`, `XDG_SESSION_TYPE`, `XDG_CONFIG_HOME`,
`XDG_DATA_HOME`, `XDG_CACHE_HOME`, `WAYLAND_DISPLAY`, `DISPLAY`, `XAUTHORITY`,
`DBUS_SESSION_BUS_ADDRESS`) that a GUI-backed CLI needs to reach the operator's running session
instead of starting a second, broken one of its own. None of them carries a secret; a provider key
never reaches a child. `[tools] env_passthrough` adds further variable names by hand, and anything
listed there reaches every command the model runs. A command's working
directory is resolved through `Workspace::resolve_path`, so it stays inside the run's workspace.
Foreground output is capped at 1 MiB and `bash(background=true)` starts a detached process in
the scope's table. `TerminalManager::cleanup_scope` kills the background processes a scope
started; the workspace DELETE route invokes it through
`RunManager::cleanup_scope`, and `Drop` does the same for every scope at daemon shutdown. The web backend (`apps/silver/src/web.rs`) reads `BRAVE_API_KEY` or
`TAVILY_API_KEY` from the process environment only, prefers Brave and falls back to Tavily, and
never logs a key or includes one in an error. `web_extract` needs no key.

## 8. Non-goal: OS-level sandboxing

silver does not sandbox processes or filesystem access at the operating-system level. There is no
seccomp, no cgroup, no container and no privilege drop. run_command, bash and execute_code all
execute as the same user as the daemon. The one exception is plan mode, which runs shell commands
inside bubblewrap with the filesystem read-only except the plans directory, where `bwrap` is
installed; that bounds what a planning run can write, not what a normal run can do.
run_command runs an executable directly with:

- no shell (argv is passed directly to Command::new, so shell metacharacters have no special
  meaning);
- an environment cleared down to the `PRESERVED_ENV` allow-list plus `[tools] env_passthrough`;
- stdin set to /dev/null;
- a confined working directory and a bounded timeout;
- its own session and process group, so on cancellation or timeout the whole group is killed and
  nothing the command spawned stays running.

bash and execute_code are explicit shell surfaces: bash runs the command string as a fresh
`bash -c` in its own process group, and execute_code writes a snippet to a temp file inside the
workspace and runs it through the same backend. They share the same filtered environment and
workspace confinement. When the call ends, the process group is killed, but a child that starts
its own session (a daemon that calls setsid) can keep running, and a `bash` call with
`background = true` is meant to outlive its run.

Therefore a run_command (or a bash command, or an execute_code snippet) that executes a
user-controlled program (or a build script, test harness, package manager lifecycle script and so on)
can do anything that user can do. Path confinement for the file tools does not contain code that the
process itself runs. A real sandbox (separate user, container, namespaces) is required before silver
executes untrusted code, and is out of scope.

## 9. Web and network guards

- `web_extract` validates every URL before the request and again on every manually followed
  redirect hop (up to five): non-http(s) schemes, loopback, RFC1918 private, link-local, CGNAT, IPv6
  unique-local, unspecified/multicast and cloud-metadata addresses (`169.254.169.254`,
  `fd00:ec2::254`) are refused with `url_blocked`. Automatic redirects are disabled so no hop is
  skipped. The host is resolved before the check; a DNS-rebinding TOCTOU between resolution and
  connect is not fully closed.
- A configured domain blocklist (`security.website_blocklist`) refuses a host equal to or a
  subdomain of an entry for both `web_search` and `web_extract` with `website_blocked`.
- `security.block_sensitive_query_urls` (default false) refuses URLs carrying a credential-shaped
  query parameter.
- `security.ssl_verify` (default true) and `security.ca_bundle` configure one TLS policy shared by
  the web backend and every model provider; disabling verification logs a warning and a CA bundle is
  parsed at startup.
- Two opt-in features send data to a third party, and both are off by default:
  `agent.jev_hints` sends the task and excerpts of commands and their output to OpenRouter, and
  `[monitoring]` POSTs run events to the endpoint you configure (`redact = true` by default).
  `[[mcp.server]]` entries run the programs you list with a filtered environment, and an `http`
  server receives whatever its tools are called with.

## 10. Security-relevant configuration

    [server]
    bind = "127.0.0.1:7777"        # keep loopback unless you set bearer_token
    bearer_token = "..."            # optional; prefer SILVER_BEARER_TOKEN in secrets.env
    request_body_limit_bytes = 2097152
    max_message_bytes = 1048576

    [tools]
    write_requires_approval = true
    command_requires_approval = true
    approval_timeout_seconds = 300          # an unanswered approval counts as a denial
    # deny_commands = ["git push*"]         # globs refused before the approval gate

    [security]
    ssl_verify = true                       # false disables TLS verification and logs a warning
    # ca_bundle = "/etc/ssl/certs/corp.pem"  # extra trust roots for every outbound client
    website_blocklist = ["example.invalid"]  # host equal/subdomain refused by web tools
    block_sensitive_query_urls = false       # refuse URLs with credential-shaped query params

    [model]
    api_key_env = "OPENAI_API_KEY"  # the variable NAME; the value lives in secrets.env or the env

`<platform config dir>/secrets.env` (mode 0600) holds the values:

    # Do not commit or back up.
    OPENAI_API_KEY=sk-...
    SILVER_BEARER_TOKEN=...        # preferred over server.bearer_token in config.toml

Every secret is an environment variable, whether exported by the operator or imported from
`secrets.env`. Nothing in state.db, the SSE stream, /v1/capabilities or the logs contains the
provider key.
