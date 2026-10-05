// The Messages mode: the bot roster, the chats and threads, and every action on them. The server
// routes and decides everything (who answers, who is mentioned, what counts as unread); this
// only holds what it was told and what is on screen. Components read `chat` and call these.
import { api, subscribeChat } from './api.js'
import { buildTurns, notify, storeAttachment } from './state.svelte.js'

const PAGE = 50

export const QUICK_REACTIONS = ['👍', '❤️', '😂', '🎉', '👀', '✅']

export const chat = $state({
  bots: [], // BotView[], as the server sent them
  loaded: false,
  selected: null, // the open bot's id
  thread: null, // the open thread's root entry id, in the selected chat
  memory: false, // the workspace memory panel is open beside the conversation
  lanes: {}, // lane key -> { entries, complete }, for every chat or thread that was opened
  files: {}, // lane key -> attached files, stored in the bot's folder, going out with the next message
  query: '', // the roster search
  editor: null, // { bot, workspace } (null bot: a new one, in `workspace` when given) or { group } (null group: a new one)
  trace: null, // { title, turns } while "Full conversation" is open
  composing: false, // the new-chat page is open
})

export const laneKey = (botId, thread = null) => (thread ? `${botId}/${thread}` : botId)

const lane = (key) => (chat.lanes[key] ??= { entries: [], complete: false })

// The files attached to a chat or thread, stored in its bot's folder.
const laneFiles = (key) => (chat.files[key] ??= [])

/** Add files already stored in the bot's folder to what the next message carries. */
export function addFiles(key, list) {
  laneFiles(key).push(...list)
}

export function removeFile(key, at) {
  laneFiles(key).splice(at, 1)
}

/** During a drag the browser hides the files, so the `Files` type is the only honest test:
 *  answering true for dragged text would swallow a drop the textarea should handle itself. */
export const isFileDrag = (e) => [...(e.dataTransfer?.types ?? [])].includes('Files')

/** Store files into a workspace's .silver/attachments; a file that fails reports itself and
 *  is left out of the returned list. */
export async function storeFiles(workspace, list) {
  const stored = []
  for (const file of list) {
    try {
      stored.push(await storeAttachment(workspace, file))
    } catch (e) {
      notify(e.message, true)
    }
  }
  return stored
}

/** A chat a dropped file cannot land in: say why, and point at the way out. */
export function noWorkspace(bot) {
  if (bot?.kind === 'group') return notify('Groups take no files', true)
  notify('Attachments need a workspace — open the bot settings to give it one', true, {
    label: 'Bot settings',
    run: () => (chat.editor = { bot }),
  })
}

export const botById = (id) => chat.bots.find((bot) => bot.id === id)

export const membersOf = (bot) => bot.members.map(botById).filter(Boolean)

/** The roster as shown: pinned first, then the newest conversation, narrowed by the search. */
export function roster() {
  const needle = chat.query.trim().toLowerCase()
  return chat.bots
    .filter((bot) => !needle || bot.name.toLowerCase().includes(needle) || bot.last_message?.toLowerCase().includes(needle))
    .toSorted((a, b) => Number(b.pinned) - Number(a.pinned) || b.last_at - a.last_at)
}

export const unreadTotal = () => chat.bots.reduce((sum, bot) => sum + bot.unread, 0)

/** Whether a turn is going in this chat or thread (not elsewhere, like another group). */
export const workingIn = (bot, thread = null) =>
  bot.status !== 'idle' && bot.status !== 'error' && bot.working_chat === bot.id && (bot.working_thread ?? null) === thread

// ---------------------------------------------------------------- stream

let stream = null
/** The last finished reply per bot chat, thread replies included; the bot view's own
 *  `last_message` only ever holds the main chat, not a thread. */
const lastReply = new Map()

export function startChat() {
  if (stream) return
  // Connect first and load once connected, so nothing is missed between the two; entries are
  // upserted by id, so one seen twice is harmless.
  stream = subscribeChat(apply, reload)
}

export function stopChat() {
  stream?.close()
  stream = null
}

async function reload() {
  try {
    const { bots } = await api('/v1/chat/bots')
    chat.bots = bots
    chat.loaded = true
    if (chat.selected && !botById(chat.selected)) chat.selected = chat.thread = null
    await Promise.all(Object.keys(chat.lanes).map((key) => fetchLane(key)))
  } catch (e) {
    notify(e.message, true)
  }
}

function apply(event) {
  if (event.type === 'bot') upsertBot(event.bot)
  else if (event.type === 'bot_removed') dropBot(event.id)
  else if (event.type === 'entry') upsertEntry(event.entry)
  else if (event.type === 'resync') reload()
}

function upsertBot(bot) {
  const at = chat.bots.findIndex((have) => have.id === bot.id)
  if (at < 0) {
    chat.bots.push(bot)
    return
  }
  alert(chat.bots[at], bot)
  chat.bots[at] = bot
}

/** A reply as a one-line notification body, clipped like the server's roster preview. */
const preview = (text) => {
  const line = text.split(/\s+/).filter(Boolean).join(' ')
  return line.length > 120 ? `${line.slice(0, 120)}…` : line
}

/** A desktop notification when a bot needs the user, fails or finishes while the window is not
 *  in front, as Codync's push does. A group says only when it needs you or fails: it goes idle
 *  between its members' turns. */
function alert(before, bot) {
  if (document.hasFocus() || typeof Notification === 'undefined' || Notification.permission !== 'granted') return
  // The reply that just landed, thread or not; the bot's own `last_message` only holds the main chat.
  const last = lastReply.get(bot.id)
  const reply = last && (!before.started_at || last.at >= before.started_at) ? preview(last.text) : null
  const body =
    bot.status === 'needs_input' && before.status !== 'needs_input'
      ? bot.activity || 'Needs your approval'
      : bot.status === 'error' && before.status !== 'error'
        ? bot.activity || 'Something went wrong'
        : bot.status === 'idle' && before.status === 'working' && bot.kind !== 'group'
          ? (reply ?? bot.last_message)
          : null
  if (!body) return
  const note = new Notification(bot.name, { body, tag: bot.id })
  // Clicking it brings the window forward and opens the chat it is about.
  note.onclick = () => {
    window.focus()
    select(bot.id)
  }
}

function dropBot(id) {
  chat.bots = chat.bots.filter((bot) => bot.id !== id)
  lastReply.delete(id)
  for (const key of Object.keys(chat.lanes)) {
    if (key === id || key.startsWith(`${id}/`)) delete chat.lanes[key]
    if (key === id || key.startsWith(`${id}/`)) delete chat.files[key]
  }
  if (chat.selected === id) chat.selected = chat.thread = null
}

function upsertEntry(entry) {
  // Kept even for a chat that is not open, so its reply can name the notification. A thread
  // entry also re-publishes the root it hangs off, so only a newer reply may replace the last.
  if (entry.kind === 'agent' && entry.text) {
    const have = lastReply.get(entry.chat_id)
    if (!have || entry.created_at > have.at) lastReply.set(entry.chat_id, { text: entry.text, at: entry.created_at })
  }
  const found = chat.lanes[laneKey(entry.chat_id, entry.thread_id)]
  if (!found) return
  const { entries } = found
  // The server's copy of a message this tab sent replaces the local one it was shown as.
  const at = entries.findIndex((have) => have.id === entry.id || (entry.nonce && have.nonce === entry.nonce))
  if (at >= 0) {
    entries[at] = { ...entry, thread: entry.thread ?? entries[at].thread }
    return
  }
  const next = entries.findIndex((have) => !have.local && have.seq > entry.seq)
  entries.splice(next < 0 ? entries.findLastIndex((have) => !have.local) + 1 : next, 0, entry)
}

// ---------------------------------------------------------------- lanes

async function fetchLane(key) {
  const [botId, thread] = key.split('/')
  const { entries } = await api(`/v1/chat/bots/${botId}/entries`, { query: { thread, limit: PAGE } })
  const found = lane(key)
  // A message still on its way, or one that failed, stays until the server answers for it.
  const local = found.entries.filter((entry) => entry.local && !entries.some((have) => have.nonce === entry.nonce))
  found.entries = [...entries, ...local]
  found.complete = entries.length < PAGE
}

/** Show a chat or thread: its newest messages, kept current by the stream from here on. */
export async function openLane(botId, thread = null) {
  try {
    await fetchLane(laneKey(botId, thread))
  } catch (e) {
    notify(e.message, true)
  }
}

export async function loadOlder(botId) {
  const found = lane(botId)
  const first = found.entries.find((entry) => !entry.local)
  if (!first || found.complete) return
  try {
    const { entries } = await api(`/v1/chat/bots/${botId}/entries`, { query: { limit: PAGE, before: first.seq } })
    found.entries.unshift(...entries)
    found.complete = entries.length < PAGE
  } catch (e) {
    notify(e.message, true)
  }
}

export function select(id) {
  chat.composing = false
  if (chat.selected === id) return
  chat.selected = id
  chat.thread = null
  if (id) openLane(id)
}

export function openThread(botId, root) {
  chat.thread = root
  openLane(botId, root)
}

/** Tell the server the chat (or thread) on screen has been seen. */
export function markRead(botId, thread = null) {
  api(`/v1/chat/bots/${botId}/read`, { method: 'POST', body: { thread_id: thread } }).catch(() => {})
}

// ---------------------------------------------------------------- messages

/** Send a message: it shows at once, and turns into the server's copy when that arrives. */
export async function send(botId, text, thread = null, nonce = crypto.randomUUID()) {
  // Sending is a gesture, so this is the moment the browser lets us ask to notify.
  if (typeof Notification !== 'undefined' && Notification.permission === 'default') Notification.requestPermission()
  const entries = lane(laneKey(botId, thread)).entries
  const local = { id: `local-${nonce}`, local: true, seq: Number.MAX_SAFE_INTEGER, chat_id: botId, thread_id: thread, kind: 'user', text, status: 'sending', nonce, reactions: [], created_at: Date.now() }
  entries.push(local)
  try {
    upsertEntry(await api(`/v1/chat/bots/${botId}/send`, { method: 'POST', body: { text, thread_id: thread, nonce } }))
  } catch (e) {
    const failed = entries.find((entry) => entry.nonce === nonce && entry.local)
    if (failed) Object.assign(failed, { status: 'failed', error: e.message })
  }
}

export function resend(entry) {
  discard(entry)
  return send(entry.chat_id, entry.text, entry.thread_id, entry.nonce)
}

export function discard(entry) {
  const entries = lane(laneKey(entry.chat_id, entry.thread_id)).entries
  const at = entries.findIndex((have) => have.id === entry.id)
  if (at >= 0) entries.splice(at, 1)
}

const call = (path, body) => api(path, { method: 'POST', body }).catch((e) => notify(e.message, true))

export const stop = (botId) => call(`/v1/chat/bots/${botId}/stop`)

export const react = (entry, emoji) => call(`/v1/chat/entries/${entry.id}/react`, { emoji })

/** Answer an approval card (`decision`: approve, approve_session, approve_always or deny). */
export const answer = (entry, decision, text) => call(`/v1/chat/entries/${entry.id}/answer`, { decision, answer: text })

export const newSession = (botId) => call(`/v1/chat/bots/${botId}/new-session`)

// ---------------------------------------------------------------- bots

/** Create a bot (`id` null) or change one; resolves to the bot, or null after telling the user why not. */
export async function saveBot(id, fields) {
  try {
    const bot = await api(id ? `/v1/chat/bots/${id}` : '/v1/chat/bots', { method: id ? 'PATCH' : 'POST', body: fields })
    upsertBot(bot)
    return bot
  } catch (e) {
    notify(e.message, true)
    return null
  }
}

export const setPinned = (bot) => saveBot(bot.id, { pinned: !bot.pinned })

export async function deleteBot(id) {
  try {
    await api(`/v1/chat/bots/${id}`, { method: 'DELETE' })
    dropBot(id)
  } catch (e) {
    notify(e.message, true)
  }
}

// ---------------------------------------------------------------- trace

/** The turns "Full conversation" shows: what a bot did in a chat or thread, or in one reply. */
export async function openTrace(botId, thread = null, only = null) {
  const bot = botById(botId)
  const entries = lane(laneKey(botId, thread)).entries
  const runs = new Set(entries.filter((entry) => entry.run_id).map((entry) => entry.run_id))
  const sessions = [...new Set(entries.filter((entry) => entry.session_id && (!only || entry.run_id === only)).map((entry) => entry.session_id))]
  try {
    const all = await Promise.all(sessions.map((id) => api(`/v1/sessions/${id}/messages`, { query: { limit: 500 } })))
    // A group's runs sit among each member's own conversations; keep the ones this lane shows.
    const turns = all
      .flatMap((messages) => buildTurns(messages))
      .filter((turn) => (only ? turn.runId === only : bot?.kind !== 'group' || runs.has(turn.runId)))
    chat.trace = { title: only ? 'What it did' : 'Full conversation', turns }
  } catch (e) {
    notify(e.message, true)
  }
}
