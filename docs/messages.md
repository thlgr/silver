# Messages

Messages is a second way to use silver: a roster of **bots** you message like teammates, in a
Telegram-style window. It follows [Codync](https://github.com/leepokai/Codync) (itself modelled on
Grok Bot) in look and behaviour: a black-and-white window, dotted halftone avatars, one endless chat
per bot, group chats, reply threads, approval cards, and bots that ask each other for help. Switch
to it with the message icon in the workbench header (or Settings → General → Messages); the
roster's **Workbench** button switches back. Everything is served by the same binary on the same
port; nothing here needs a second process.

## Bots

A bot is a persistent, named teammate with its own chat:

| Field | Meaning |
| --- | --- |
| name, about | What the roster shows; the about text is what the bot, and the other bots, know it for |
| avatar | One of 8 shapes in one of 11 colours, drawn as a halftone grid with hollow eyes that glance and blink while the bot works |
| agent, model | Which [provider](provider-setup.md) answers, and which model. Empty follows the daemon's default. Any provider works, including the [external agent modes](provider-setup.md#external-agent-modes-acp) |
| workspace | One of the workbench's [workspaces](tools.md), or **Add workspace…** to register a folder (typed by path when silver cannot open the machine's folder dialog). Without one a bot can chat, remember and ask other bots, but it has no file or shell tools |
| permissions | *Ask me* (silver's normal approvals) or *Approve automatically* |
| instructions | Standing instructions, given to the bot on every turn |

The roster lists bots under the workspace they work in, in the workbench's order, then the bots
with no workspace, then groups; within each, pinned first, then the newest conversation. Every
workspace shows, even one with no bots yet, and the **+** beside its name creates a bot there (the
roster's own **+ → New bot** starts in the workbench's current workspace). The conversation's title
names the bot's workspace, and the details panel and an empty chat show its path. Change a bot from
the details panel's gear, and pin, edit or delete it from the roster's context menu. Removing a
workspace in the workbench leaves its bots without one.

### How a bot runs

A bot is not a new kind of agent. Each of its turns is an ordinary silver run, in a session the
daemon keeps for it (source `chat`, never listed in the workbench). The session is pinned to the
bot's model and workspace; the instructions go in through the run's external context. So the loop,
tools, approvals, memory and providers are exactly the workbench's. **New session** gives the bot a
fresh context and leaves the chat alone; changing its workspace does the same, because a session
never changes workspace.

What the chat shows is only your messages, each turn's **final reply**, approval cards and
notices. While a bot works its row and a bubble show what it is doing right now, with a timer, and
tapping the bubble unfolds its latest thinking. Every tool call, diff, plan and thought is behind
**Full conversation** (a bot's header button, or *Show what it did* on one reply).

A message sent while the bot is busy is queued and answered, together with any others queued
behind it, as one turn after the current one. Stop ends the turn and drops what was queued.

## Groups

A group holds several bots and you. Create one with **+ → New group chat**; the same members share
one group. A group has a transcript but no agent. When you write in it the server starts a *room
turn*:

- Up to 3 rounds and 10 replies. Each round, the bots answer one at a time, each in **its own**
  session, told who is in the room and what was said since it last spoke.
- Everyone answers, unless the new messages @-mention someone (`@alice`, `@alice chen` or `@all`):
  then only the bots named. Typing `@` in the box suggests the members.
- A bot with nothing to add replies `(pass)`, which says nothing; a round where nobody speaks ends
  the turn. A bot can @-mention another to bring it into the next round.
- A newer message in the same chat, or Stop, ends a running room turn before its next speaker.
  Approval cards from a member appear in the group.

## Threads

*Reply in thread* on any message branches a conversation under it, shown in a panel beside the
chat. In a bot's chat a thread is its own session, started from the message and the lines before
it; nothing said there reaches the main chat. In a group the room answers inside the thread and
sees only the thread. Threads are flat. The message shows how many replies, who made them and how
many are new.

## Bots asking each other

Every bot's session has two extra tools, `list_bots` and `ask_bot` (see [tools.md](tools.md)).
Tell a bot *"ask Reviewer to check these changes"* and it can find its teammates and put a
self-contained request to one, then wait for its final reply. The asked bot answers in its own
session with its own workspace, tools and approvals; its permission cards appear in *its* chat. Both
chats get a notice that follows the request (*Asked Reviewer…* / *Request from Alice…*), and the
answer is not a message in the asked bot's chat.

A bot cannot ask itself or one it already waits on, nor close a cycle of bots waiting on each
other; 64 requests can wait at once, and one waits at most ten minutes, queue time included.
Stopping the asking bot withdraws its requests; stopping the asked bot releases the asker with an
error. A failed request is an error, never a made-up answer, and is not retried.

## Approvals, notifications, attachments

- A tool that needs approval shows an **approval card** in the chat — *Wants to run a command*, with
  the command and **Allow once / Always allow / Deny**. A question the bot asks you is the same
  card with its options. The roster row turns amber while a bot waits for you.
- When the window is not in front, the browser can notify you when a bot needs you, fails or
  finishes. It asks for permission the first time you send a message.
- Bots with a workspace take files: the paperclip, dropping files on the box, or pasting an image
  stores them under `.silver/attachments/` and names them in the message, as in the workbench.

## Reading and unread

Each chat and each thread is read separately, and a bot's unread badge is their sum. Having a chat
open (while the window is in front) reads it; a reply in a thread you are not looking at keeps
saying "N new" under its message.

## The window

Wide windows show the roster, the chat, and a panel that is an open thread, or the bot's details
(or a group's members) when you tap the conversation's title. Under 680 pixels beside the roster a panel opens over the chat; at
phone width the roster and the chat take turns. On a message, right click (or press and hold) for
reactions, copy, reply in thread and *Show what it did*; on a desktop pointer the same actions
appear beside its time while it is hovered. The window follows the system light or dark
theme.

## Data

Bots, chat entries and read marks live in `state.db` (`bots`, `chat_entries`, `chat_reads`,
migration `0002_chat.sql`); the sessions behind them are ordinary rows. Chat entries are the chat
view only: deleting a bot deletes its entries and its sessions. After a restart half-written
replies are finished as they stood and unanswered cards expire. The HTTP routes, and the stream
that keeps a window current, are in [api.md](api.md#bot-chat).

## Other coding agents

A bot's agent is whatever provider you pick for it, so Messages is not tied to silver's own loop.

- **Any model silver can call** (hosted APIs, local servers, the Codex sign-in) runs on silver's own
  loop, with silver's tools, approvals, memory and the team tools.
- **External coding agents** run as their own process over the Agent Client Protocol:
  **Claude Code** (through its adapter, which needs Node.js and a signed-in `claude`), OpenCode, Grok
  Build, Gemini CLI, Cursor, Kimi, Qwen Code and the rest of the
  [agent modes](provider-setup.md#external-agent-modes-acp). Each one whose CLI is installed on the
  machine running silver is in the editor's **Agent** list; there is nothing to set up first, and
  silver downloads nothing but Claude's adapter.
- Each bot's agent session starts in the bot's workspace, several bots can share one agent at once,
  and such a bot can be asked by other bots and join groups like any other.

An external agent keeps its own loop and tools; silver relays what it says. When it asks leave to
act (to edit a file, run a command), the request is an **approval card** in the bot's chat: *Allow
once*, *Always allow* or *Deny*. A bot set to *Approve automatically* says yes itself. A card left
open when the turn ends, by Stop or otherwise, expires and the agent is told no. Stopping a turn
also tells the agent to stop. Silver's own tools and the team tools are not the agent's, so
`ask_bot` is not available to it, and "Full conversation" shows its tool activity as text.

A bot using one with no workspace works in the directory silver was started in; give it a workspace.

## Not included

Codync's voice calls, remote screen, relay and phone pairing, push notifications through a relay,
routines, the memory keeper, the marketplace and widgets depend on its phone and cloud
infrastructure and are not part of this mode.

## Testing

`scripts/chat_e2e.py` starts the mock model server and a daemon and exercises replies, read marks,
retry, reactions, threads, group rooms and mentions, approval cards, `ask_bot`, Stop and deletion,
and an external agent's permission requests (against `scripts/fake_acp_agent.py`, a stand-in for
Claude Code) as cards, automatic approval and expiry (`cargo build -p silver` first). It needs no
network or credentials.
