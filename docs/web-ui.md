# Web UI

The browser client in `apps/web` (Svelte 5 + Vite). It holds presentation state only and talks to
silver through the [HTTP API](api.md); it is compiled into the binary and served on the same port
(`http://127.0.0.1:7777`). Design notes are in [architecture.md](architecture.md#11-web-ui).

## What it does

- **Sessions per workspace**, with global (workspace-less) sessions beside them. The sidebar
  marks every session with a run in progress, and the workspace folder holding it, with a spinner;
  it highlights the selected workspace, and workspaces drag into any order this browser remembers.
  The search box matches titles and message text. **Add workspace** opens the desktop's own folder
  dialog on the machine running silver.
- **Streaming replies** whose tool calls render as an action on a target with an outcome: a diff
  with line numbers for an edit, a command's exit code and output tail, a checklist for a todo
  update, and read-only calls folded into one "Explored" group. The same rendering is used live
  and after a reload. A delegation is a card with one row per subagent.
- **Inline approvals** (`y` once, `a` session, `A` always, `n` deny) show the same diff as the
  row. The page beeps when an approval, question or plan arrives while it is not in front: another
  tab or window, once you have clicked or typed in it.
- **Queue and steer** while a run is active: a message sent mid-run is steered into it.
- **Menus** for model, approval mode and preset, and **provider sign-in** (Settings → Providers).
- **Side panel**: working-tree diff, file checkpoints, worktrees, usage and the Agents tab.
- **Status line** under the composer: context percentage and tokens, what the run has spent, and
  the generation rate (`~139 tok/s`). The rate counts only the time the provider spent streaming
  text, so stalls, silent tool calls, approval waits and tool runtime are left out; it stays as
  the last run's rate until the next one starts.
- **Transparency**: every run's steps show, word for word, what silver gave the model besides your
  message and tool output: the system prompt, each file loaded into it (`AGENTS.md`, `CLAUDE.md`),
  an `AGENTS.md` found in a subdirectory, and every loop notice
  (verification required, tool guard, iteration budget, reply cut off, context summary). The
  prompt and loaded files show on a session's first run, then only as the lines that changed, and
  all of it survives a reopen. Jev hint checks show as Jev steps.
- **Goals**: `/goal <objective>` gives the open session a standing goal. The daemon keeps it: after
  each completed run it starts another toward the goal until the budget (default 20
  continuations) is spent, so it keeps going while you work in another session. Stopping a run
  pauses its goal.
- **Messages**: the header button (or Settings → General) switches the whole window to a
  messaging app: a roster of bots with their own chats, group chats, threads, reactions, approval
  cards and bots that ask each other for help, in Codync's black-and-white look. See
  [messages.md](messages.md). The mode persists per browser in `app.settings.messaging`; its code is
  `src/components/messages/` and `src/lib/chat.svelte.js`.

When `server.bearer_token` is set the UI asks for it on first load, keeps it in `localStorage` and
sends it with every call; Settings has Sign out. An unreachable server shows a "Connection lost"
pill, and the UI reloads its lists once silver is back.

## Slash commands

The catalog is `GET /v1/commands`; the UI offers the commands it has a handler for.

| Command | Does |
| --- | --- |
| `/help` (`/h`) | Show the command list |
| `/status`, `/usage`, `/context` | Model, session and run status; token usage; the context window breakdown |
| `/sessions` | Search sessions by title or content |
| `/title [text]` | Show or set the session title |
| `/model [name]` | Open the model menu, name a model (`provider:model`), or switch provider |
| `/preset [name]` | Open the preset menu or switch this chat to a preset |
| `/agents` | List the subagents you can delegate to and edit the custom ones |
| `/plan [off\|<description>]` | [Plan mode](tools.md#plan-mode) |
| `/yolo [on\|off]` | Bypass approvals for this session |
| `/approvals [manual\|smart\|off]` | Show or set the daemon's approval mode (Shift+Tab cycles it) |
| `/goal [text\|status\|pause\|resume\|clear\|budget N]` | Set and drive a standing objective |
| `/steer <prompt>`, `/stop`, `/retry` | Steer the active run, stop it, or re-submit the last prompt |
| `/undo [N]`, `/rollback [N]` (`/checkpoints`), `/compress` | Drop the newest turns; restore a file checkpoint; compress the context |
| `/diff [staged\|all\|session]`, `/worktree` (`/wt`) | Working-tree diff; list, create or remove git worktrees |
| `/login [provider]` (`/auth`), `/logout <provider>` | Provider menu, or sign in or out by name |
| `/loop`, `/heartbeat` (`/hb`) | Re-fire a prompt after each turn, or on a timer into an idle session. These are timers in the page, not in the daemon |
| `/copy`, `/export`, `/clear`, `/focus`, `/verbose`, `/theme` | Copy the last reply, download the transcript as Markdown, clear the view, hide tool lines, set tool-progress verbosity, switch theme |

## Serving

`apps/silver/src/api/ui.rs` embeds `apps/web/dist` with `rust-embed`. Known files are served as
they are, fingerprinted `assets/*` get a one-year immutable cache and a miss is a 404, and any
other path returns `index.html` so client-side routes load. UI responses carry their own
same-origin Content-Security-Policy. A debug build reads `apps/web/dist` from disk instead, so a
rebuilt UI is picked up without recompiling; a binary built with no `dist` still serves the API
and `/` explains how to build the UI.

## Developing the UI

    cd apps/web
    npm install
    npm run dev       # http://localhost:5173 with HMR, proxying /v1 to silver
    npm run build     # refresh dist; a debug silver serves it without recompiling

Vite proxies `/v1` to `SILVER_URL` (default `http://127.0.0.1:7777`) and adds
`SILVER_BEARER_TOKEN` as the `Authorization` header when set, so the token never reaches the
browser. Run a debug `silver` in another terminal (`cargo run -p silver`).

Layout: `src/lib/state.svelte.js` holds all state and every API call, `src/lib/commands.js` maps
catalog names to handlers, `src/app.css` defines the design tokens, and each component in
`src/components/` owns one region (sidebar, chat, composer, turn, panel, settings). Run events
arrive over a `fetch` stream, not `EventSource`, because `EventSource` cannot send the bearer
token; it reconnects with `Last-Event-ID`.
