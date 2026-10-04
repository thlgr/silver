# Tools and agent features

What the agent can do, how calls are approved, and the modes that change its behaviour. For the
Hermes decision matrix behind this tool set see [tool-port-matrix.md](tool-port-matrix.md); for the
code layout see [architecture.md](architecture.md).

## Tool set

One `ToolRegistry` (`crates/silver-core/src/tools/mod.rs`) holds every tool. The daemon registers
**23 tools**: 20 built in, `delegate_task`, which exists only while `[delegation] enabled` is true,
and the two team tools, which only a bot's session in [Messages](messages.md) is given. `GET /v1/tools` lists them all with their toolset and whether the [filters](#which-tools-a-run-sees)
leave them `enabled`; `GET /v1/capabilities` lists only the enabled ones.

| Tool | Risk | Needs workspace | What it does |
| --- | --- | --- | --- |
| `read_file` | read | yes | Text with line numbers, paged by `offset`/`limit`; extracts a PDF's text when the bytes are not UTF-8 |
| `list_files` | read | yes | Directory listing; directories end in `/` |
| `search_files` | read | yes | Text in file contents (literal first, then regex; case-insensitive when the pattern is all lowercase), optionally narrowed by `file_glob`; `target = "files"` matches file and directory names by glob |
| `view_image` | read | yes | Shows a png/jpg/gif/webp (up to 5 MB) to the model |
| `search_documents` | read | yes | Full-text search over documents indexed in this workspace |
| `write_file` | write | yes | Replace or create a file atomically (1 MiB cap) |
| `patch` | write | yes | Exact find-and-replace edit (`path`, `old_string`, `new_string`, `replace_all`) |
| `run_command` | process | yes | One executable with an argv list, no shell |
| `bash` | process | yes | A fresh `bash -c`; `background = true` starts a detached process |
| `process_manage` | process | yes | `list`, `read`, `wait` or `kill` a background process |
| `execute_code` | process | yes | Write a snippet to a temp file in the workspace and run it |
| `lsp` | read (`rename`: write) | yes | Language-server `diagnostics`, `hover`, `definition`, `references`, `symbols`, `rename` |
| `memory` | memory | no | Add, replace or remove entries in `MEMORY.md` / `USER.md` |
| `session_search` | read | no | Search past sessions of the same scope |
| `todo_list` | memory | no | The session's checklist |
| `skills_list`, `skill_view` | read | no | Discover and load skills |
| `skill_manage` | write | no | Create, update or delete a skill |
| `web_search`, `web_extract` | read | no | Search, and fetch a page as text |
| `delegate_task` | read | no | Hand independent tasks to [subagents](#subagents) |
| `list_bots` | read | no | The user's other [bots](messages.md#bots-asking-each-other): name, what each is for, status, folder |
| `ask_bot` | read | no | Put a self-contained request to another bot and wait for its final reply (up to ten minutes) |

A global (workspace-less) run sees only the tools that do not need a workspace; a workspace run
sees the rest too. Pick a workspace in the web UI, or pass `workspace_id` to `POST /v1/runs`, to
give the model files and a shell.

`write_file` and `patch` report `destructive` risk for credential-like paths such as
`~/.ssh/config`, which is always gated. Details that matter to a model:

- `patch` matches `old_string` through a nine-strategy fuzzy chain (exact, line-trimmed,
  whitespace-normalised, indentation-flexible, escape-normalised, trimmed-boundary,
  unicode-normalised, block-anchor, context-aware) and re-indents on a non-exact hit. A miss
  answers with a "Did you mean?" snippet. Only the replace fields are advertised, but
  `mode = "patch"` with a unified diff is still accepted. CRLF files keep their line endings and
  edits keep file permissions.
- `search_files` and `list_files` say when a result was cut at `limit`.
- A tool never receives a scope from the model: memory, session search and documents use the
  run's own scope.

### Which tools a run sees

Two filters apply in order, and together they define the built-in **Minimal** preset:

1. **Toolsets** (`core`, `files`, `terminal`, `web`, `memory`, `skills`, `delegation`, `mcp`):
   `[tools] default_toolsets` selects them (empty selects all) and `disabled_toolsets`
   subtracts. The default disables `web`, which keeps the `web_search`/`web_extract` schemas out of
   the prompt until you set `disabled_toolsets = []`.
2. **Tool names**: `enabled` (allow-list, empty means all) and `disabled` (deny-list, deny wins).
   The default `disabled` trims the roster for small models: `run_command`, `execute_code`,
   `process_manage`, `skill_manage` and `skills_list`. `list_files` stays on: without it a small
   model lists a directory with `bash find`, which is a process-risk call and an approval prompt.

A coding agent with file edit, `bash`, `lsp`, `memory` and skill loading costs about 4.2k prompt
tokens. Edit `config.toml` and restart to change the lists
([configuration.md](configuration.md#tools)).

## Approvals

```toml
[tools]
approval_mode = "manual"        # manual (default) | smart | off
write_requires_approval = true
command_requires_approval = true
approval_timeout_seconds = 300
deny_commands = []              # globs refused before the gate
```

`write_requires_approval` gates `write` risk (`write_file`, `patch`, `skill_manage`, `lsp`
rename) and `command_requires_approval` gates `process` risk (`bash`, `run_command`,
`process_manage`, `execute_code`). `read` and `memory` calls are never gated; `destructive` is
always gated.

`approval_mode` picks who decides:

- `manual` prompts for every gated call.
- `smart` asks the auxiliary model (`[auxiliary]`) to approve clearly low-risk calls and prompts
  for the rest. Without an auxiliary route, or if the review errors or times out, it behaves like
  `manual`.
- `off` skips prompts (YOLO). The hardline floor and `deny_commands` still apply because they run
  before the gate.

A **hardline floor** refuses destructive shell commands (`rm -rf /`, `mkfs*`, raw block-device
writes, fork bombs, `curl | sh`, `sudo -S`, `chmod -R 777 /`, host power control) in `bash` and
`run_command` whatever the mode or any remembered decision.

Change the mode at runtime from the web UI's approval menu, `/approvals smart`, or
`POST /v1/approvals`; the daemon writes it to `config.toml`. `silver --yolo` or
`SILVER_YOLO_MODE=1` pins the mode to `off` for the process and refuses runtime changes. `/yolo`
(or `yolo_mode` on `PATCH /v1/sessions/{id}`) toggles the bypass for one session; the flag is
stored with the session. `GET /v1/approvals` reports the effective mode.

A decision is one of `approve`, `approve_session` (same tool and arguments for the rest of the
session), `approve_always` (same, in this workspace or the global scope, across restarts) or
`deny`. An approval nobody answers within `approval_timeout_seconds` counts as a denial, and a
denial turns tools off for the rest of the run so the model answers instead of retrying another
way. Approval gates execution only: it never widens path confinement.

## Presets

A preset is a named tool and skill selection a chat runs with. Two are built in: **Minimal**
follows `[tools]` in `config.toml`, and **Pi** gives the agent one shell tool (`bash`) and nothing
else. Pick one per chat from the wrench menu or `/preset [name]`; it applies from the next
message. Settings → Presets creates and edits custom ones.

A custom preset's tool list is the whole selection: the `[tools]` filters define Minimal only.
Skills are a blacklist ("all except unchecked", new skills on) or a whitelist ("only checked",
new skills off). The system prompt follows the tools: without `memory` there is no MEMORY or USER
PROFILE block, a shell-only chat gets shell read/edit lines, and a chat with no file or shell
tools is told it cannot see files. Approvals, `deny_commands` and path confinement hold for every
preset. Deleting a preset moves its sessions to Minimal.

## Plan mode

`/plan` puts the chat in plan mode (`/plan <description>` also sends the description); the
approval chip reads **Plan mode** while it lasts. The model explores with read-only tools, asks
questions, and writes a plan to `<data_dir>/plans/<session_id>.md`, outside the workspace. Plan
mode needs a workspace.

- The shell still runs for looking around (`ls`, `grep`, `git log`, tests). A command that
  changes files or state is refused with a note naming the part that does: `rm`, `mv`, `cp`,
  `mkdir`, `touch`, `tee`, `chmod`, a `>` into a file, `sed -i`, `find -delete`, git commits,
  checkouts and pushes, package installs, `cargo fmt`, any `--write` or `--fix`. This is a
  blacklist: a program it does not know runs.
- Writing the plan needs no approval, with `write_file`, `patch` or `cat > <path> <<'EOF'`.
  Only the plan file counts; any other change is refused.
- Where `bwrap` (bubblewrap) is installed, every shell command also runs with the filesystem
  read-only except the plans directory, so the kernel refuses the write whichever program asked
  for it. Without it (macOS, Windows, user namespaces disabled) only the classification applies.
- `/plan` again shows the plan; `/plan off` leaves plan mode.

When the plan is ready the model calls `exit_plan_mode` and the plan appears in an approval card:
**Approve and start** ends plan mode and sends "Implement the approved plan."; **Approve in new
session** (`s`) starts a fresh session on the same workspace whose first message is the plan;
**Keep planning** lets the model ask what to change. Questions come as a card too
(`ask_user_question`): pick an option (`1`-`4`) or type an answer. Both cards always wait for you,
whatever the approval mode or YOLO flag. The two tools exist only in plan-mode runs.

The prompts are opencode's plan-mode ones with silver's tool names and one addition, **Plan
Style**: the plan is terse technical English whatever language you write in, with no pronouns and
every line naming the `path:line` and symbol it touches.

## Subagents

`delegate_task` takes a `tasks` list and runs each task as a subagent: another agent turn with its
own system prompt and tool list and no memory of the parent conversation. Tasks of one call run
concurrently; the parent keeps each report. Subagents never get `delegate_task`, so delegation
does not nest. Their tool calls go through the same approvals, path confinement and checkpoints as
the parent, and a stop cancels them. A task may set `isolation: "worktree"` to work in a temporary
git worktree, removed again if it left it clean.

One subagent is built in: `general-purpose`, with every tool but delegation, for research or a
multi-step task end to end. A task that names no agent runs it. Anything narrower, such as a
read-only search or a reviewer, is a definition of your own. Write it as Markdown in
`~/.silver/agents/<name>.md` (all projects; `SILVER_AGENTS_DIR` overrides the directory) or
`<workspace>/.silver/agents/<name>.md` (this project, and it wins):

```markdown
---
name: migration-reviewer
description: Reviews a migration for safety under concurrent writes
tools: [read_file, search_files, bash]
model: fast-model
max_turns: 30
---

You review one migration file. Report what breaks under concurrency, with paths.
```

`name` and `description` are required; `tools`, `disallowed_tools`, `model`, `max_turns` and
`isolation: worktree` are optional; the body is the system prompt. It is scanned for prompt
injection like project instructions. A file the parser rejects is skipped with a log line. The web
UI shows a delegation as a card with one row per agent, and the Agents tab (`/agents`) lists every
agent a chat started and edits the custom definitions; built-ins are read-only.

```toml
[delegation]
enabled = true          # false leaves a run with no way to spawn work
max_iterations = 50     # per subagent turn
timeout_seconds = 600   # wall clock per subagent turn
max_concurrent = 4      # tasks per call, and how many run at once
```

A subagent's transcript is discarded; a reopened session shows its `subagent.*` events and the
report in the tool result. Its tokens are not added to the parent run's usage or cost.

## Images and documents

The composer takes attachments: drop, paste or **Attach**. The file is stored at
`<workspace>/.silver/attachments/<name>` (a repeated name becomes `<name>-2`), `.silver/.gitignore`
(`*`) keeps it out of `git status`, and the prompt carries
`[attached: shot.png → .silver/attachments/shot.png]`. The model then opens the path with the
tools above. Pictures (png, jpg, gif, webp) and PDFs are accepted on their bytes; any other file
must be valid UTF-8 text; the rest is refused by name. Uploads are bounded by
`server.request_body_limit_bytes` (2 MiB with base64, roughly a 1.4 MB PDF) and need a workspace.

- `view_image` returns a one-line receipt to the transcript; the bytes ride the next request only,
  so messages never grow by megabytes of base64. The UI renders the picture from
  `GET /v1/workspaces/{id}/files?path=…`, confined to the workspace root.
- A picture to a DeepSeek model on OpenCode Zen/Go rides the relay's Anthropic surface, because the
  relay's chat/completions shim rejects inline images. If a route refuses a picture anyway, the
  request is retried once without it and the receipt says why.
- `search_documents` takes `query` and an optional `path` to index one document first. It searches
  two FTS5 indexes (`unicode61` for words, `trigram` for substrings in any language) and never
  leaves the run's workspace. A document with unchanged size and mtime is not re-read.

## Backends

- **Shell and processes** (`apps/silver/src/terminal.rs`): each `bash` call is a fresh `bash -c`
  with stdin closed, so `cd` and exports do not carry over. Output is capped at 1 MiB; on timeout
  or cancel the whole process group is killed. Commands have no controlling terminal, so a prompt
  (an ssh passphrase, a git username) fails at once; for git over SSH set
  `env_passthrough = ["SSH_AUTH_SOCK"]`. `bash(background=true)` returns a `session_id` for
  `process_manage` and lives until it is killed or the daemon exits; deleting a workspace kills its
  background processes.
- **Child environment**: every spawned process starts from `env_clear` and gets back only `PATH`,
  `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `USER`, `LOGNAME`, `SHELL`, `TERM` and the desktop-session
  handles (`XDG_*`, `WAYLAND_DISPLAY`, `DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`).
  Provider keys never reach a child. `[tools] env_passthrough = ["NVM_DIR"]` adds names; do not
  list a secret, since everything listed reaches every command.
- **Todos**: in memory per session, lost on restart.
- **Skills** (`apps/silver/src/skills.rs`): Markdown files with YAML frontmatter from two overlaid
  stores, global `~/.agents/skills` (`SILVER_SKILLS_DIR`) and the workspace's `.agents/skills`.
  The newer file wins on a name clash; `skill_manage` writes to the project store when there is a
  workspace. There is no hub or marketplace.
- **Web** (`apps/silver/src/web.rs`): `web_search` uses Brave with `BRAVE_API_KEY`, else Tavily with
  `TAVILY_API_KEY`; `web_extract` is a plain fetch with HTML-to-text and needs no key. Keys come
  from the process environment only.
- **LSP** (`[lsp]`, on by default): stdio language servers for rust-analyzer, pyright/pylsp,
  typescript-language-server, gopls and clangd, found in the workspace or named in `servers`.
  After `write_file` or `patch`, up to five errors on the lines the edit changed are appended to
  the result.
- **Checkpoints** (`[checkpoints]`, on by default): before `write_file` or `patch` changes a path,
  the old bytes are saved under `<data_dir>/checkpoints/<id>/` (50 snapshots, 64 MiB).
  `/rollback`, `GET /v1/checkpoints` and `POST /v1/checkpoints/{id}/restore` bring them back.
- **MCP client** (`[[mcp.server]]`, stdio or http): on startup silver connects, lists tools and
  registers each as `<server>__<tool>` in the `mcp` toolset. They are approval-gated unless the
  server marks them `readOnlyHint`. stdio children get a filtered environment. Server mode
  (exposing silver over MCP) is not implemented.

## Not implemented

- `clarify` (Hermes core tool): plan mode's `ask_user_question` already makes the round trip;
  a general `clarify` would reuse that path outside plan mode.
- Hermes tools with no backend here: `browser_*`, `image_generate`, `video_*`, `text_to_speech`,
  `kanban_*`, `cronjob_manage`, `ha_*`, `computer_use`, `manage_connections`. The reason for each
  is in [tool-port-matrix.md](tool-port-matrix.md).
