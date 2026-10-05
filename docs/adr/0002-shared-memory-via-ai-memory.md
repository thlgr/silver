# 0002. Shared memory via ai-memory

Status: accepted

## Context

silver's memory is harness-local. It is Markdown (`MEMORY.md`, `USER.md`) under the data
directory, one file set per scope (`memory/global/`, `workspaces/ws_<id>/memory/`), edited through
the `memory` tool and injected into the run's system prompt (`crates/silver-core/src/memory.rs`,
`apps/silver/src/memory_fs.rs`). For silver's own loop it works: a bot whose session is pinned to a
workspace reads and writes the same files as any other in that workspace (`Scope::Workspace`).

It stops at the harness boundary. A run whose model is an external agent over ACP
(`apps/silver/src/acp.rs`: Claude Code, OpenCode, Cursor, and the rest) is a provider-shaped call,
not silver's loop. Silver's tools are not offered to that agent (`docs/provider-setup.md`: it "runs
its own loop"), so it cannot call `memory` and cannot write silver's files; silver's prompt,
including the memory blocks, reaches it only once as text in a *fresh* ACP session, and only the
newest message is sent after that. Two bots in one workspace — one native, one Claude — therefore
share nothing.

ai-memory (akitaonrails/ai-memory; analysis clone in `ref/ai-memory`) is built for exactly this:
one memory of record per project, fed by lifecycle hooks and read and written over MCP, with
first-party installs for twenty-plus harnesses. Markdown-in-git is its source of truth, SQLite+FTS5
its derived index, and it works with no LLM and no API key. Its unit of sharing — a project
resolved from the working directory — matches a silver workspace, which is the directory a bot's
session runs in.

## Decision

Retire silver's Markdown memory and make ai-memory the memory of record.

- silver **manages an ai-memory server** beside itself: if nothing is listening on the configured
  loopback bind it starts `ai-memory serve --transport http`, and it stops the one it started on
  shutdown. A server the operator already runs is adopted, not duplicated.
- silver registers that server as an **MCP client**, so every run — workbench or bot — gets
  ai-memory's `memory_*` tools. MCP is already how silver reaches external tools
  (`apps/silver/src/mcp/`).
- The server's data directory lives under silver's own, so the store travels with an install.
  ai-memory resolves the project from each run's workspace, which is what makes every harness in
  one workspace share one memory.
- External harnesses share the same server by the operator's own choice: running ai-memory's
  installers (`ai-memory install-mcp --client claude-code --apply`, `install-hooks --agent
  claude-code --apply`, …) points them at it. silver does not touch their configs; it does not
  reimplement a harness's memory protocol either.
- A run on a native provider has no harness hooks, so silver is its **lifecycle producer**: it
  posts the run's start, prompt, tool calls and end to ai-memory's `/hook/batch`
  (`MemoryHooks`, `apps/silver/src/ai_memory.rs`), scoped to the workspace's `(workspace, project)`
  and tagged `extension=silver`. On the wire it reports as `agent=claude-code`: ai-memory keeps a
  tool call's content only for the agent kinds it has a verified payload shape for, silver posts
  exactly Claude Code's shape, and `extension=silver` carries the truth until silver has a kind of
  its own upstream. One session can span several idle periods: after a quiet
  `session-end` ai-memory keeps the session's observations and rewrites its summary on the next
  end, so a later run needs no new start. The ten-minute quiet before that `session-end` is what
  lets one bot's consecutive turns share a session and one summary, instead of fragmenting a
  conversation into a session per run. A session ends in ai-memory once it has had no run for
  ten minutes, or at shutdown, which is when ai-memory writes its summary page and handoff. An ACP
  agent keeps its own hooks and is not captured here, so nothing is stored twice; a run with no
  workspace has no project and is not captured. A tool call made inside a subagent reaches the run
  as `subagent.step` and is not captured; only the run's own calls are.
- silver's `memory` tool, its `MEMORY.md`/`USER.md` store, the prompt's memory blocks and the
  `[memory] max_prompt_bytes_per_file` cap are removed. A workspace with no reaching server has no
  memory, rather than a second, invisible one.

## Consequences

- One memory of record. A fact a Claude bot learns is readable by a silver bot in the same
  workspace, and the reverse, because both go through the same server.
- silver gains a runtime dependency: the `ai-memory` binary, looked up on `PATH` like an ACP
  mode's CLI. When it is missing, memory is off with a clear notice and the rest of silver runs
  unchanged (`[memory] enabled = false` turns it off deliberately).
- A second process. This is a deliberate exception to "one binary": ai-memory is a server with its
  own store and schema, and vendoring it into silver would add its crates, a git backend and a
  SQLite database to silver's build. The managed-server boundary keeps silver small.
- Capture becomes automatic through ai-memory's hooks (and silver's own for native runs),
  retrieval becomes `memory_query` and `memory_briefing`, and consolidation is opt-in. silver stops
  growing memory itself.
- ai-memory keeps a tool call's content only for the harnesses it has a verified payload shape
  for; silver is not one, so a native run's tool calls are stored as counts, not content (the
  posted body carries the redacted argument preview and the result summary, ready for the day
  silver has a shape upstream). Delivery is best-effort without holding up a run: a batch the
  server does not take is retried once, then logged and dropped (one retry covers a server that
  restarts mid-batch; a 4xx will not heal, so more would only delay the queue), and a burst of
  tool calls cannot evict a lifecycle event because the queue keeps a reserve for it (start and
  end carry the session summary; dropping them loses the session even though every call survived).
- Memory no longer sits in the run's system prompt; it arrives through a tool call, so prompts
  shrink and the `memory.changed` event goes away.
