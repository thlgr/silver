// Application state and every action that talks to silver. Components read `app` and call
// these functions; nothing else holds daemon state.
import { api, subscribe, watchLink } from './api.js'
import { perByte, sessionLabel } from './format.js'
import { describe, diff } from './tools.js'

const SETTINGS_KEY = 'silver.settings'
const DRAFTS_KEY = 'silver.drafts'

function load(key = SETTINGS_KEY) {
  try { return JSON.parse(localStorage.getItem(key)) ?? {} } catch { return {} }
}

export const app = $state({
  workspaces: [],
  commands: [], // slash-command catalog from GET /v1/commands
  sessions: [],
  scope: null, // workspace id used for a new session; null = no workspace
  session: null, // SessionView of the open session
  turns: [], // { user, steps[], running, runId, usage, cost, duration, error } | { note }
  run: null, // { id } of the streaming run
  approval: null, // the approval.required the card shows: the oldest of approvalQueue
  approvalQueue: [], // every pending approval; parallel subagents can ask for several at once
  queued: null, // { prompt, goalBudget } that starts when the active run ends
  context: null, // ContextUsage of the session's latest model request
  tokensPerByte: 1 / 3, // from the provider's latest token count; the daemon's guess until one
  rate: { bytes: 0, ms: 0 }, // this run's streamed text bytes and the time between its deltas
  providers: [], // /v1/auth providers
  activeProvider: null,
  model: null, // model the daemon routes to (session override or provider default)
  defaultModel: null, // model a new session runs on: the active provider's last pick
  effort: null, // per-chat reasoning effort; null = the daemon default
  autoEffort: null, // daemon-configured reasoning effort, from GET /v1/models
  pendingYolo: false,
  pendingPlan: false, // /plan in a chat with no session yet; the first run enters plan mode
  approvals: null, // { mode, frozen }
  advisor: null, // { enabled, has_key, questions } from GET /v1/advisor
  loop: null, // { prompt, interval, times, fired, status, next }
  heartbeat: null, // { prompt, interval, status, next, fired }
  history: [],
  draft: null, // text to put back in the composer (a failed send, an undone turn)
  attachments: [], // { name, path, bytes, preview } already stored in the workspace
  panel: null, // 'changes' | 'checkpoints' | 'worktrees' | 'agents' | 'usage'
  agent: null, // { tool, index }: the delegate_task call and task the Agents tab shows
  settingsTab: null,
  notice: null, // { text, error }
  link: 'up', // 'up' | 'down' (the server does not answer) | 'locked' (it wants a bearer token)
  presets: [], // preset catalog from GET /v1/presets
  agents: [], // subagent definitions from GET /v1/agents
  settings: { theme: 'system', verbose: 'all', focus: false, messaging: false, goalBudget: 20, collapsed: [], order: [], preset: 'minimal', ...load() },
})

$effect.root(() => {
  $effect(() => localStorage.setItem(SETTINGS_KEY, JSON.stringify(app.settings)))
  $effect(() => {
    const theme = app.settings.theme
    if (theme === 'system') delete document.documentElement.dataset.theme
    else document.documentElement.dataset.theme = theme
  })
  $effect(() => {
    document.title = `${app.approval ? '● Needs approval · ' : app.run ? '… ' : ''}${app.session ? sessionLabel(app.session) : 'silver'}`
  })
})

let source = null
let noticeTimer = null

export function notify(text, error = false, action = null) {
  app.notice = { text, error, action }
  clearTimeout(noticeTimer)
  noticeTimer = setTimeout(() => (app.notice = null), error ? 8000 : 4000)
}

/** Local, unsaved line in the transcript (command output). */
export function note(text) {
  app.turns.push({ note: text })
}

async function attempt(fn) {
  try { return await fn() } catch (e) { notify(e.message, true) }
}

export const currentWorkspace = () =>
  app.workspaces.find((w) => w.id === (app.session ? app.session.workspace_id : app.scope))

// What the composer holds per chat: one draft per session, one per workspace for a new session.
const drafts = load(DRAFTS_KEY)

export const draftKey = () => app.session?.id ?? `new:${app.scope}`
export const getDraft = (key) => drafts[key] ?? ''

export function setDraft(key, text) {
  if (text.trim()) drafts[key] = text
  else delete drafts[key]
  const kept = Object.entries(drafts).filter(([, t]) => t.length <= 2000)
  try { localStorage.setItem(DRAFTS_KEY, JSON.stringify(Object.fromEntries(kept))) } catch { /* storage blocked or full */ }
}

// ---------------------------------------------------------------- boot & lists

const loadWorkspaces = () => attempt(async () => {
  const [workspaces, approvals, advisor] = await Promise.all([api('/v1/workspaces'), api('/v1/approvals'), api('/v1/advisor').catch(() => null)])
  // The sidebar order the user dragged; workspaces added since then go last.
  const { order } = app.settings
  const rank = (w) => (order.includes(w.id) ? order.indexOf(w.id) : order.length)
  app.workspaces = workspaces.sort((a, b) => rank(a) - rank(b))
  app.approvals = approvals
  app.advisor = advisor
  app.scope ??= workspaces[0]?.id ?? null
})

export async function boot() {
  watchLink((link) => {
    const away = app.link === 'down'
    app.link = link
    if (link === 'up' && away) {
      notify('Reconnected')
      loadWorkspaces()
      loadSessions()
      loadProviders()
    }
  })
  await loadWorkspaces()
  await Promise.all([loadSessions(), loadProviders(), loadPresets()])
  const id = location.hash.slice(1)
  if (id) await openSession(id)
  // A session link pasted into this tab (replaceState never fires this).
  addEventListener('hashchange', () => {
    const next = location.hash.slice(1)
    if (next && next !== app.session?.id) openSession(next)
  })
  setInterval(tickTimers, 1000)
}

let finishedRun = null // the run this view just finished; the list may still call it active

/** Sessions of every scope the sidebar shows; `q` keeps those whose messages match it. */
async function everySession(q) {
  const scopes = [{ scope: 'global' }, ...app.workspaces.map((w) => ({ workspace_id: w.id }))]
  const lists = await Promise.all(scopes.map((scope) => api('/v1/sessions', { query: { ...scope, q, limit: 200 } })))
  return lists.flat()
}

/** Ids of the sessions whose messages contain `q`. */
export async function searchSessions(q) {
  return new Set((await attempt(() => everySession(q)))?.map((s) => s.id))
}

export async function loadSessions() {
  await attempt(async () => {
    app.sessions = (await everySession()).sort((a, b) => b.updated_at.localeCompare(a.updated_at))
  })
  // The daemon started a run here on its own (a /goal continuation): follow it.
  const open = app.sessions.find((s) => s.id === app.session?.id)
  if (open?.active_run && open.active_run !== finishedRun && !app.run && !app.turns.at(-1)?.running) openSession(open.id)
}

export async function loadProviders() {
  await attempt(async () => {
    const [auth, models] = await Promise.all([api('/v1/auth'), api('/v1/models')])
    app.providers = auth.providers
    app.activeProvider = auth.active ?? models.provider
    app.defaultModel = models.default
    app.autoEffort = models.effort ?? null
    app.model = app.session?.model_override ?? models.default
  })
}

export const listModels = (provider) => api('/v1/models', { query: { provider } })

/** The subagents this workspace can delegate to. A project file needs a workspace, so the
 *  listing is scoped like the rest of the panel. */
export async function loadAgents() {
  await attempt(async () => {
    const { agents } = await api('/v1/agents', { query: { workspace_id: app.scope } })
    app.agents = agents
  })
}

export async function agentMarkdown(name) {
  const { markdown } = await api(`/v1/agents/${name}`, { query: { workspace_id: app.scope } })
  return markdown
}

export async function saveAgent(name, markdown, scope = 'project') {
  const query = { scope, workspace_id: scope === 'project' ? app.scope : undefined }
  const saved = name
    ? await attempt(() => api(`/v1/agents/${name}`, { method: 'PUT', query, body: { markdown } }))
    : await attempt(() => api('/v1/agents', { method: 'POST', query: { ...query, name }, body: { markdown } }))
  if (saved) await loadAgents()
  return saved
}

export async function deleteAgent(name, scope = 'project') {
  const gone = await attempt(() =>
    api(`/v1/agents/${name}`, { method: 'DELETE', query: { scope, workspace_id: scope === 'project' ? app.scope : undefined } }))
  if (gone) await loadAgents()
  return gone
}

/** Put a task for this agent in the composer, so the reader can shape the brief. */
export function tryAgent(agent) {
  app.draft = `Use the ${agent.name} agent: `
  notify(`Describe what the ${agent.name} agent should do`)
}

export async function loadPresets() {
  await attempt(async () => {
    app.presets = (await api('/v1/presets')).presets ?? []
  })
}

export function currentPreset() {
  const id = app.session ? (app.session.preset ?? 'minimal') : app.settings.preset
  return app.presets.find((p) => p.id === id) ?? app.presets[0]
}

export async function setPreset(id) {
  app.settings.preset = id
  if (app.session) await updateSession({ preset: id })
}

export async function savePreset(preset) {
  const body = { name: preset.name, tools: preset.tools, skills: preset.skills }
  if (!preset.id) {
    const saved = await attempt(() => api('/v1/presets', { method: 'POST', body }))
    if (saved) await loadPresets()
    return saved
  }
  const i = app.presets.findIndex((p) => p.id === preset.id)
  if (i >= 0) app.presets[i] = preset
  try {
    const saved = await api(`/v1/presets/${preset.id}`, { method: 'PUT', body })
    const j = app.presets.findIndex((p) => p.id === saved.id)
    if (j >= 0) app.presets[j] = saved
    return saved
  } catch (e) {
    notify(e.message, true)
    await loadPresets()
  }
}

export async function deletePreset(preset) {
  await attempt(() => api(`/v1/presets/${preset.id}`, { method: 'DELETE' }))
  await loadPresets()
  if (app.settings.preset === preset.id) app.settings.preset = 'minimal'
  if (app.session?.preset === preset.id) app.session = await api(`/v1/sessions/${app.session.id}`)
}

/** Ask silver to open the machine's folder dialog; the chosen absolute path, or null when the
 *  user cancelled. Throws when the machine running silver has no dialog to open, so the caller
 *  can offer to type the path instead. */
export async function openFolderDialog() {
  return (await api('/v1/workspaces/pick', { method: 'POST' })).path
}

/** Throws on a bad path, so the form can show why beside its field. */
export async function addWorkspace(path, name) {
  const folder = path.split('/').filter(Boolean).at(-1) ?? path
  const w = await api('/v1/workspaces', { method: 'POST', body: { path, name: name || folder } })
  app.workspaces.push(w)
  return w
}

export async function removeWorkspace(id) {
  await attempt(async () => {
    await api(`/v1/workspaces/${id}`, { method: 'DELETE', query: { force: true } })
    app.workspaces = app.workspaces.filter((w) => w.id !== id)
    if (app.scope === id) newSession(null)
    await loadSessions()
  })
}

// ---------------------------------------------------------------- sessions

export function newSession(scope = app.scope) {
  source?.close()
  Object.assign(app, { session: null, turns: [], run: null, approval: null, approvalQueue: [], queued: null, context: null, agent: null, scope })
  Object.assign(app, { model: app.defaultModel, effort: null, pendingYolo: false, pendingPlan: false })
  lastSent.clear()
  history.replaceState(null, '', location.pathname)
}

export async function openSession(id) {
  source?.close()
  const loaded = await attempt(() => Promise.all([
    api(`/v1/sessions/${id}`),
    api(`/v1/sessions/${id}/messages`, { query: { limit: 500 } }),
    api(`/v1/sessions/${id}/usage`),
    api(`/v1/sessions/${id}/injected`).catch(() => []),
  ]))
  if (!loaded) {
    // A stale link (a deleted session) must not fail again on every reload.
    if (location.hash === `#${id}`) history.replaceState(null, '', location.pathname + location.search)
    return
  }
  const [session, messages, usage, injected] = loaded
  // A resumed run ends by reopening its session.
  const agent = session.id === app.session?.id ? app.agent : null
  Object.assign(app, { session, scope: session.workspace_id ?? null, run: null, approval: null, approvalQueue: [], queued: null, agent, context: usage.context ?? null, tokensPerByte: perByte(usage.context), effort: session.reasoning_effort ?? null })
  app.turns = buildTurns(messages)
  lastSent.clear()
  // These are events, not transcript: each goes before the first step persisted after it. A
  // subagent's own events belong inside the delegate_task call that started it, not beside it.
  for (const e of injected) {
    const turn = app.turns.find((t) => t.runId === e.run_id)
    if (!turn) continue
    if (e.type?.startsWith('subagent.')) {
      applySubagent(turn.steps.find((s) => s.kind === 'tool' && s.id === e.tool_call_id), e)
      continue
    }
    const step = eventStep(e)
    if (!step) continue
    const at = Date.parse(e.created_at)
    const next = turn.steps.findIndex((s) => s.at > at)
    turn.steps.splice(next < 0 ? turn.steps.length : next, 0, step)
  }
  for (const run of usage.runs) {
    const turn = app.turns.find((t) => t.runId === run.run_id)
    if (turn && run.total_tokens) Object.assign(turn, { usage: run, cost: run.cost_usd })
    if (turn && run.error) turn.error = run.error
    if (turn && run.status === 'cancelled') turn.stopped = true
  }
  app.model = session.model_override ?? app.defaultModel
  history.replaceState(null, '', `#${id}`)
  // Re-attach to a run that is still going (page reload mid-run, a /goal continuation).
  const run = session.active_run
  if (run) {
    // Keep what was persisted (reasoning is never replayed) and let the replay fill the rest;
    // apply() skips tools, texts and notes the turn already has.
    const turn = app.turns.findLast((t) => t.runId === run) ?? pushTurn('', run)
    Object.assign(turn, { running: true, resumed: true })
    stream(run, turn)
  }
}

/** The daemon titles a session after its first run; pull that and the list back in. */
async function refreshSession() {
  if (app.session) app.session = (await attempt(() => api(`/v1/sessions/${app.session.id}`))) ?? app.session
  await loadSessions()
}

export async function updateSession(patch) {
  if (!app.session) return
  const view = await attempt(() => api(`/v1/sessions/${app.session.id}`, { method: 'PATCH', body: patch }))
  if (view) app.session = view
  await loadSessions()
  return view
}

/** Pick the per-chat reasoning effort. A fresh chat holds it locally until the first run
 *  pins it to the session; an existing chat persists it at once, reverting on failure. */
export async function setEffort(level) {
  const previous = app.effort
  app.effort = level || null
  if (!app.session) return
  const view = await attempt(() => updateSession({ reasoning_effort: level || '' }))
  if (view) app.effort = view.reasoning_effort ?? null
  else app.effort = previous
}

export async function deleteSession(id) {
  await attempt(async () => {
    await api(`/v1/sessions/${id}`, { method: 'DELETE' })
    setDraft(id, '')
    if (app.session?.id === id) newSession()
    await loadSessions()
  })
}

/** An attachment rides its own text part, so it stays out of the words the user wrote and out
 *  of the session title. The conversation shows it as a chip; the model reads it as a path. */
const MARKER = /^\[attached: (.+?) → (.+?)\]$/

export function splitAttachments(content) {
  const words = []
  const files = []
  for (const part of content ?? []) {
    const match = part.type === 'text' ? MARKER.exec(part.text.trim()) : null
    if (match) files.push({ name: match[1], path: match[2] })
    else if (part.type === 'text' && part.text.trim()) words.push(part.text)
  }
  return { words: words.join('\n'), files }
}

/** A chat message's words and the files it names, from the marker lines attachments ride on. */
export function splitMessage(text) {
  const files = []
  const words = text
    .split('\n')
    .filter((line) => {
      const match = MARKER.exec(line.trim())
      if (match) files.push({ name: match[1], path: match[2] })
      return !match
    })
    .join('\n')
    .trim()
  return { words, files }
}

/** Group persisted messages into turns: one per run, opened by its first user message. */
export function buildTurns(messages) {
  const turns = []
  const tools = new Map()
  for (const m of messages) {
    let turn = turns.at(-1)
    const { words, files } = splitAttachments(m.content)
    if (m.role === 'user' && (!turn || turn.runId !== m.run_id)) {
      turns.push((turn = { user: words, attachments: files, steps: [], runId: m.run_id }))
      continue
    }
    if (!turn) continue
    // A user message inside a run is a steer; label it as the live view does.
    const at = Date.parse(m.created_at)
    if (m.role === 'user') turn.steps.push({ kind: 'note', text: `Steer: ${words}`, at })
    for (const part of m.role === 'user' ? [] : m.content) {
      if (part.type === 'reasoning') turn.steps.push({ kind: 'reasoning', text: part.text, at })
      if (part.type === 'text' && part.text) turn.steps.push({ kind: 'text', text: part.text, at })
      if (part.type === 'tool_call') {
        const step = { kind: 'tool', id: part.id, name: part.name, args: part.arguments, output: '', status: '', at, start: at }
        tools.set(part.id, step)
        turn.steps.push(step)
      }
      if (part.type === 'tool_result' && tools.has(part.tool_call_id)) {
        const step = tools.get(part.tool_call_id)
        // A denied call is a policy decision, not a tool failure; the live stream already
        // reads it this way (approval.resolved sets 'denied' directly).
        step.status = part.is_error ? (part.content.startsWith('operation denied') ? 'denied' : 'failed') : 'completed'
        step.output = part.content
        step.ms = at - step.start
      }
    }
  }
  return turns
}

const text = (m) => m.content.filter((p) => p.type === 'text').map((p) => p.text).join('\n')

const lastSent = new Map() // label -> text of the open session's newest system prompt or loaded file

/** A step from an advisor.checked or context.injected event, or none for a repeat. The system
 *  prompt and the files loaded into it go out on every run; a turn shows them when they are new
 *  to the session, then only the lines that changed. A subagent's are shown whole. */
function eventStep(e, nested = false) {
  const id = `${e.run_id}:${e.event_id}`
  if (e.type === 'advisor.checked') return { kind: 'advisor', id, point: e.point, answers: e.answers, hints: e.hints }
  const step = { kind: 'injected', id, label: e.label, text: e.text }
  if (nested || (e.label !== 'System prompt' && !e.label.startsWith('Loaded '))) return step
  const before = lastSent.get(e.label)
  lastSent.set(e.label, e.text)
  if (before === undefined) return step
  const changed = lineDiff(before, e.text)
  return changed ? { ...step, label: `${e.label} (changed)`, text: changed } : null
}

/** The lines `after` dropped from `before` (-) and added to it (+). */
function lineDiff(before, after) {
  const changed = diff(before, after).filter((row) => row.sign !== ' ')
  return changed.map((row) => `${row.sign} ${row.text}`).join('\n')
}

// ---------------------------------------------------------------- runs

/** How a stored attachment is named in the prompt: the model reads that path with a tool. */
export const attachmentNote = (a) => `[attached: ${a.name} → ${a.path}]`

/** Where a stored attachment's picture is served from; null names a non-image, shown as a file
 *  chip instead. Only images: a PDF is named but never rendered. */
export const attachmentSrc = (path) => {
  const id = currentWorkspace()?.id
  return id && /\.(png|jpe?g|gif|webp)$/i.test(path)
    ? `/v1/workspaces/${id}/files?path=${encodeURIComponent(path)}`
    : null
}

function toBase64(bytes) {
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}

/** Store one file in a workspace, so a tool can read it like any file there; resolves to
 *  `{ name, path, bytes, preview }`, where `preview` is a data: URL for a picture (the page's CSP
 *  does not allow blob: images). */
export async function storeAttachment(workspace, file) {
  const data = toBase64(new Uint8Array(await file.arrayBuffer()))
  const stored = await api(`/v1/workspaces/${workspace}/attachments`, { method: 'POST', body: { name: file.name, data } })
  return { ...stored, preview: file.type.startsWith('image/') ? `data:${file.type};base64,${data}` : null }
}

/** Store files the user attached. Uploading on attach rather than on send is what makes a bad
 * file fail next to the chip, not after a long run. A workspace-less chat has nowhere to put a
 * file, so it says so. */
export async function attachFiles(files) {
  const workspace = app.session ? app.session.workspace_id : app.scope
  if (!workspace) return notify('Pick a workspace before attaching a file', true)
  app.notice = null
  for (const file of files) {
    try {
      app.attachments.push(await storeAttachment(workspace, file))
      // A later file that lands clears the error an earlier one left.
      app.notice = null
    } catch (e) {
      notify(e.message, true)
    }
  }
}

export function dropAttachment(index) {
  app.attachments.splice(index, 1)
}

function pushTurn(user, runId = null, attachments = []) {
  app.turns.push({ user, attachments, steps: [], running: true, runId })
  return app.turns.at(-1)
}

/** Composer entry point: queue while a run is active, otherwise start one. A goal budget
 *  makes the prompt the session's /goal. */
export function submit(prompt, goalBudget) {
  // The attachments are consumed here, not in start(): a queued prompt keeps its own notes,
  // and anything the user attaches meanwhile belongs to the next message.
  const sent = app.attachments.splice(0)
  // Words first, markers after, each its own part. A bare attachment still needs an
  // instruction, or the model gets a path and nothing to do with it.
  const content = [
    { type: 'text', text: prompt || 'Look at the attached file.' },
    ...sent.map((a) => ({ type: 'text', text: attachmentNote(a) })),
  ]
  const message = splitAttachments(content)
  if (app.run) {
    if (app.queued) return notify('A prompt is already queued')
    app.queued = { content, goalBudget }
    return
  }
  return start(content, goalBudget)
}

export async function start(prompt, goalBudget) {
  const content = typeof prompt === 'string' ? [{ type: 'text', text: prompt }] : prompt
  const { words, files } = splitAttachments(content)
  const turn = pushTurn(words, null, files)
  const fresh = !app.session
  const post = () => api('/v1/runs', {
    method: 'POST',
    body: {
      workspace_id: app.session ? app.session.workspace_id : app.scope,
      session_id: app.session?.id,
      message: { content },
      reasoning_effort: app.effort ?? undefined,
      yolo: fresh && app.pendingYolo ? true : undefined,
      plan_mode: fresh && app.pendingPlan ? true : undefined,
      preset: fresh ? currentPreset()?.id : undefined,
      goal_budget: goalBudget,
    },
  })
  // A prompt sent as a run ends can beat the daemon letting go of the session; try once more.
  const created = await attempt(() => post().catch((e) =>
    /already has an active run/.test(e.message) ? new Promise((r) => setTimeout(r, 500)).then(post) : Promise.reject(e)))
  if (!created) {
    app.turns.splice(app.turns.indexOf(turn), 1)
    app.draft = words
    return
  }
  turn.runId = created.run_id
  if (fresh) {
    app.session = await api(`/v1/sessions/${created.session_id}`)
    app.session.preview ??= words
    history.replaceState(null, '', `#${created.session_id}`)
    loadSessions()
  }
  stream(created.run_id, turn)
}

function stream(runId, turn) {
  app.run = { id: runId }
  turn.start ??= Date.now()
  source?.close()
  let last = 0
  source = subscribe(runId, (e) => {
    // Deltas are never stored, so those sent before this stream connected are gone; a gap in
    // the ids marks the turn so its completed text replaces the partial one.
    if (e.event_id > last + 1) turn.gapped = true
    last = Math.max(last, e.event_id)
    apply(turn, e)
  })
}

let audio = null

/** Beep when an approval waits and the page is not in front. A replayed approval is resolved a
 *  moment later, so the beep waits that long; browsers block audio before the first click. */
function beepIfAway(id) {
  setTimeout(() => {
    if (document.hasFocus() || !app.approvalQueue.some((a) => a.approval_id === id)) return
    if (navigator.userActivation && !navigator.userActivation.hasBeenActive) return
    audio ??= new AudioContext()
    const tone = audio.createOscillator()
    const volume = audio.createGain()
    const now = audio.currentTime
    tone.frequency.value = 880
    volume.gain.setValueAtTime(0.2, now)
    volume.gain.exponentialRampToValueAtTime(0.001, now + 0.3)
    tone.connect(volume).connect(audio.destination)
    tone.start(now)
    tone.stop(now + 0.3)
  }, 500)
}

function lastStep(steps, kind) {
  const step = steps.at(-1)
  if (step?.kind === kind) return step
  steps.push({ kind, text: '' })
  return steps.at(-1)
}

function addNote(steps, text) {
  if (!steps.some((s) => s.kind === 'note' && s.text === text)) steps.push({ kind: 'note', text })
}

// Longest silence still counted as generation. A local server pauses to think, a tool call
// arrives as one chunk after a long silence, and a tool then runs for seconds: none of that is
// the model writing tokens, so a gap over the cap stays out of the denominator instead of
// dragging the rate down.
const GENERATION_GAP_MS = 1_000
const encoder = new TextEncoder() // the server counts real bytes, so the ratio applies to real bytes
let lastDelta = 0

/** Count one content delta: its bytes, and the gap since the previous one. */
function generated(delta) {
  const now = Date.now()
  if (now - lastDelta <= GENERATION_GAP_MS) app.rate.ms += now - lastDelta
  lastDelta = now
  app.rate.bytes += encoder.encode(delta).length
}

/** The run-level events that only mean something at the top: a subagent's copy is dropped. */
const RUN_ONLY = new Set([
  'run.started', 'run.completed', 'run.failed', 'run.cancelled', 'context.updated',
  'plan_mode.exited', 'steer.delivered', 'replay.gap', 'advisor.checked',
  // The subagent's own reply arrives with subagent.completed; a nested text.completed would
  // be the same words twice.
  'text.completed',
])

/**
 * Apply one event to a list of steps. `steps` is a turn's own, or a subagent's: a delegated
 * task runs a real agent turn, so its events go through the same reducer, and only the
 * run-level ones are dropped.
 */
function applyTo(steps, e, turn, nested = false) {
  if (nested && RUN_ONLY.has(e.type)) return
  const tool = () => steps.find((s) => s.kind === 'tool' && s.id === e.tool_call_id)
  switch (e.type) {
    case 'run.started':
      app.model = e.model
      // Each run reports its own rate; the previous run's stays on screen until this one starts.
      // From now, so the wait for the first token counts as generation instead of dropping it.
      Object.assign(app.rate, { bytes: 0, ms: 0 })
      lastDelta = Date.now()
      break
    case 'context.updated':
      app.context = e.context
      if (e.context.prompt_tokens) app.tokensPerByte = perByte(e.context)
      break
    case 'reasoning.delta': lastStep(steps, 'reasoning').text += e.delta; if (!nested) generated(e.delta); break
    case 'text.delta': lastStep(steps, 'text').text += e.delta; if (!nested) generated(e.delta); break
    // Like the TUI: streamed deltas win; the completed text (which may join continuation
    // segments) only fills in when no delta arrived, or when some were missed. A completed
    // text that no longer holds the streamed one was rewritten by the server, so it wins.
    case 'text.completed': {
      const last = steps.at(-1)
      if (last?.kind === 'text' && (turn?.gapped || (last.text && !e.text.includes(last.text.trim())))) last.text = e.text
      else if (!(last?.kind === 'text' && last.text) && !steps.some((s) => s.text === e.text))
        steps.push({ kind: 'text', text: e.text })
      break
    }
    case 'tool.started':
      if (!tool()) steps.push({ kind: 'tool', id: e.tool_call_id, name: e.name, args: e.preview, output: '', status: 'running', start: Date.parse(e.created_at) })
      break
    case 'tool.output': { const t = tool(); if (t) t.output += e.chunk; break }
    case 'tool.completed': {
      const t = tool()
      if (t) Object.assign(t, { status: e.status, output: e.summary, ms: Date.parse(e.created_at) - t.start })
      break
    }
    case 'approval.required': {
      // Parallel subagents can ask at once, so every pending approval is kept and the card
      // shows the oldest; each decision names its own tool call.
      app.approvalQueue = [...app.approvalQueue.filter((a) => a.approval_id !== e.approval_id), { ...e }]
      app.approval = app.approvalQueue[0]
      const t = tool()
      if (t) t.status = 'waiting'
      beepIfAway(e.approval_id)
      break
    }
    case 'approval.resolved': {
      app.approvalQueue = app.approvalQueue.filter((a) => a.approval_id !== e.approval_id)
      app.approval = app.approvalQueue[0] ?? null
      const t = steps.find((s) => s.id === e.tool_call_id && s.status === 'waiting')
        ?? steps.find((s) => s.status === 'waiting')
      if (t) t.status = e.decision === 'deny' ? 'denied' : 'running'
      break
    }
    case 'plan_mode.exited':
      turn.planApproved = true
      if (app.session) app.session.plan_mode = false
      break
    case 'run.waiting': addNote(steps, e.reason); break
    case 'steer.delivered':
      if (turn?.pendingSteer === e.text) turn.pendingSteer = null
      addNote(steps, `Steer: ${e.text}`)
      break
    case 'advisor.checked':
    case 'context.injected': {
      const step = eventStep(e, nested)
      if (step && !steps.some((s) => s.id === step.id)) steps.push(step)
      break
    }
    case 'replay.gap': notify('Some live output was missed; reopen the session for the full history'); break
    case 'subagent.started':
    case 'subagent.step':
    case 'subagent.completed': {
      const host = steps.find((s) => s.kind === 'tool' && s.id === e.tool_call_id)
      applySubagent(host, e)
      break
    }
    case 'run.completed':
      Object.assign(turn, { usage: e.usage, cost: e.cost_usd, duration: e.duration_ms })
      return finish(turn)
    case 'run.failed': turn.error = e.message; return finish(turn)
    case 'run.cancelled': turn.stopped = true; return finish(turn)
  }
}

function apply(turn, e) {
  applyTo(turn.steps, e, turn)
}

/** One task of a delegate_task batch, as the transcript card shows it. */
function subagentStep(host, index) {
  host.subagents ??= []
  let entry = host.subagents.find((s) => s.index === index)
  if (!entry) host.subagents.push((entry = { index, steps: [], status: 'running' }))
  return entry
}

export function openAgent(tool, index) {
  Object.assign(app, { agent: { tool, index }, panel: 'agents' })
}

/** A subagent's own turn, filed under the delegate_task call that started it. */
function applySubagent(host, e) {
  if (!host) return
  const entry = subagentStep(host, e.index)
  if (e.type === 'subagent.started') {
    Object.assign(entry, { agent: e.agent, description: e.description, model: e.model, root: e.worktree, start: Date.parse(e.created_at) })
    return
  }
  if (e.type === 'subagent.completed') {
    Object.assign(entry, {
      status: e.status,
      report: e.summary,
      toolUses: e.tool_uses,
      durationMs: e.duration_ms,
      worktree: e.worktree ?? null,
    })
    return
  }
  // The wrapped event has no id or time of its own. `agent` names the task on an approval card.
  const { run_id, event_id, created_at } = e
  applyTo(entry.steps, { ...e.event, run_id, event_id, created_at, agent: entry.description }, null, true)
}

function finish(turn) {
  source?.close()
  source = null
  finishedRun = turn.runId
  turn.running = false
  app.run = null
  app.approval = null
  app.approvalQueue = []
  if (turn.stopped && app.queued) restoreQueued()
  // A steer the run never picked up goes back where the user typed it.
  if (turn.pendingSteer) {
    const held = turn.pendingSteer
    turn.pendingSteer = null
    app.draft = app.draft ? `${held}\n${app.draft}` : held
    notify('The run ended before your steer was delivered; it is back in the composer')
  }
  // An approved plan ends its run, and the work starts as its own message: a retry of the
  // work, or of a stuck run, then keeps the plan.
  const work = turn.planApproved && !turn.error && !turn.stopped
    ? () => (turn.freshPlan ? implementInNewSession(turn.freshPlan) : start('Implement the approved plan.'))
    : null
  // Text streamed before a reload is never replayed; the transcript has all of it now.
  if (turn.resumed) return Promise.all([loadSessions(), openSession(app.session.id)]).then(work)
  backfill(turn)
  // A refresh landing after the work opened a new session would put this one back.
  const refreshed = refreshSession()
  if (work) refreshed.then(work)
  else if (!turn.error && !turn.stopped) afterTurn()
  else if (app.queued) releaseQueued()
}

/** Replace a just-finished turn's live (cut) tool args/output with the full persisted ones,
 *  the same shape a reload would show. Headers (status, name, duration) stay put; only an
 *  open body can grow. */
async function backfill(turn) {
  if (!app.session || !turn.runId) return
  await attempt(async () => {
    const messages = await api(`/v1/sessions/${app.session.id}/messages`, { query: { limit: 500 } })
    const full = buildTurns(messages).find((t) => t.runId === turn.runId)
    if (!full) return
    const byId = new Map(full.steps.filter((s) => s.kind === 'tool').map((s) => [s.id, s]))
    for (const step of turn.steps) {
      if (step.kind !== 'tool') continue
      const match = byId.get(step.id)
      if (match) Object.assign(step, { args: match.args, output: match.output })
    }
  })
}

function releaseQueued() {
  const { content, goalBudget } = app.queued
  app.queued = null
  start(content, goalBudget)
}

/** Stop means stop: the queued prompt goes back to the composer instead of starting. */
function restoreQueued() {
  const { words, files } = splitAttachments(app.queued.content)
  app.queued = null
  app.attachments.push(...files)
  app.draft = words
}

/** A queued prompt wins, then per-turn loops (TUI order). The daemon continues a /goal. */
function afterTurn() {
  if (app.queued) return releaseQueued()
  const loop = app.loop
  if (loop?.status === 'running' && !loop.interval) fireLoop()
}

function fireLoop() {
  const loop = app.loop
  if (loop.times && loop.fired >= loop.times) {
    loop.status = 'done'
    return notify(`Loop finished after ${loop.fired} runs`)
  }
  loop.fired++
  if (loop.interval) loop.next = Date.now() + loop.interval
  start(`[/loop wakeup #${loop.fired}] ${loop.prompt}\n\nThis is an automatic /loop wakeup. Re-check the current state before acting and report concisely what changed.`)
}

let ticks = 0

function tickTimers() {
  // Keep the sidebar's working sessions current, and catch /goal continuations starting.
  const busy = app.sessions.some((s) => s.active_run || s.goal?.status === 'active')
  if (++ticks % 2 === 0 && busy && !document.hidden) loadSessions()
  if (app.run || app.queued) return
  const now = Date.now()
  if (app.loop?.status === 'running' && app.loop.interval && now >= app.loop.next) return fireLoop()
  const beat = app.heartbeat
  if (beat?.status === 'running' && now >= beat.next) {
    beat.fired++
    beat.next = now + beat.interval
    start(`[Heartbeat - recurring instruction, fires every ${formatInterval(beat.interval)}]\n${beat.prompt}\n\nIf there is nothing meaningful to do or report for this instruction right now, reply briefly that nothing has changed and stop - do not invent work.`)
  }
}

export const stop = () => app.run && attempt(() => api(`/v1/runs/${app.run.id}/stop`, { method: 'POST' }))

export async function steer(message) {
  if (!app.run) return notify('No active run to steer')
  const turn = app.turns.findLast((t) => t.running)
  if (turn) turn.pendingSteer = message
  const sent = await attempt(() => api(`/v1/runs/${app.run.id}/steer`, { method: 'POST', body: { message } }))
  // A rejected steer goes back to the composer rather than vanishing.
  if (!sent && turn) {
    turn.pendingSteer = null
    app.draft = app.draft ? `${message}\n${app.draft}` : message
  }
}

/** Answer the approval on the card; an `answer` replies to an ask_user_question card. The next
 *  one, if any, takes its place right away. */
export async function decide(decision, answer) {
  const a = app.approval
  if (!a) return
  app.approvalQueue = app.approvalQueue.filter((p) => p.approval_id !== a.approval_id)
  app.approval = app.approvalQueue[0] ?? null
  await attempt(() => api(`/v1/runs/${a.run_id}/approval`, { method: 'POST', body: { approval_id: a.approval_id, decision, answer } }))
}

/** Approve the plan and do the work in a new session, so the planning talk stays out of its
 *  context. */
export function approveInNewSession() {
  const turn = app.turns.find((t) => t.runId === app.approval?.run_id)
  if (turn) turn.freshPlan = app.approval.description
  return decide('approve')
}

/** A new session on the same workspace and YOLO setting, whose first message is the plan. */
function implementInNewSession(plan) {
  const { workspace_id, yolo_mode } = app.session
  newSession(workspace_id)
  app.pendingYolo = yolo_mode
  return start(`Implement this approved plan:\n\n${plan}`)
}

/** Remove the newest N user turns; `retry` resubmits the removed prompt. */
export async function rewind(turns = 1, retry = false) {
  if (app.run) return notify('Stop the active run first')
  if (!app.session) {
    const prompt = app.turns.findLast((t) => t.user)?.user
    return retry && prompt ? start(prompt) : notify('Nothing to undo')
  }
  const res = await attempt(() => api(`/v1/sessions/${app.session.id}/rewind`, { method: 'POST', query: { turns } }))
  if (!res) return
  await openSession(app.session.id)
  if (retry && res.removed_user_text) return start(res.removed_user_text)
  if (res.removed_user_text) app.draft = res.removed_user_text
  const n = res.turns_undone
  notify(
    res.files_changed
      ? `Removed ${n} turn${n === 1 ? '' : 's'}. Its file changes stay on disk; Checkpoints can restore them.`
      : `Removed ${n} turn${n === 1 ? '' : 's'}`,
  )
}

// ---------------------------------------------------------------- settings actions

export async function setApprovalMode(mode) {
  // Show the new mode at once, so a quick second Shift+Tab cycles on from it.
  const previous = app.approvals
  app.approvals = { ...previous, mode }
  app.approvals = (await attempt(() => api('/v1/approvals', { method: 'POST', body: { mode } }))) ?? previous
}

export async function setAdvisor(enabled) {
  const saved = await attempt(() => api('/v1/advisor', { method: 'POST', body: { enabled } }))
  if (saved) app.advisor = saved
}

export async function setYolo(on) {
  if (!app.session) return (app.pendingYolo = on)
  await updateSession({ yolo_mode: on })
}

export const planMode = () => (app.session ? app.session.plan_mode : app.pendingPlan)

export async function setPlanMode(on) {
  if (app.session && !(await updateSession({ plan_mode: on }))) return
  if (!app.session) app.pendingPlan = on
  notify(on ? 'Plan mode: silver explores read-only and writes a plan; nothing changes until you approve it' : 'Left plan mode')
}

/** The session's plan file, as a transcript note. */
export async function showPlan() {
  if (!app.session) return notify('No plan yet: send a message to start planning')
  const plan = await attempt(() => api(`/v1/sessions/${app.session.id}/plan`))
  if (plan) note(plan.content ? `Plan, ${plan.path}\n\n${plan.content}` : `No plan yet. It will be written to ${plan.path}`)
}

/** Route new runs through `provider` (activating it when it changes), then pick `model`.
 *  The provider stores the pick as its model, so new sessions and restarts keep it. */
export async function setModel(model, provider = app.activeProvider) {
  const saved = await attempt(() => model
    ? api(`/v1/auth/${provider}`, { method: 'POST', body: { model, activate: provider !== app.activeProvider } })
    : api(`/v1/auth/${provider}/activate`, { method: 'POST' }))
  if (saved) await routeTo(provider, saved.model)
}

async function routeTo(provider, model) {
  app.activeProvider = provider
  app.model = app.defaultModel = model
  if (app.session) await updateSession({ model })
}

/** Store what the user typed for `provider` (key, endpoint; either may be absent) and route
 *  new runs through it. The daemon picks a model from the endpoint's own listing. */
export async function connect(provider, body) {
  const saved = await attempt(() => api(`/v1/auth/${provider}`, { method: 'POST', body: { ...body, activate: true } }))
  await loadProviders()
  if (!saved) return
  if (!saved.model) {
    notify(`Saved, but ${saved.base_url} listed no models. Is the server running?`, true)
  } else {
    await routeTo(provider, saved.model)
    notify(`Using ${saved.model}`)
  }
  return saved
}

export async function logout(provider) {
  await attempt(() => Promise.all([
    api(`/v1/auth/${provider}`, { method: 'DELETE' }),
    api(`/v1/oauth/${provider}/logout`, { method: 'POST' }).catch(() => {}),
  ]))
  await loadProviders()
}

export const beginLogin = (provider) => api(`/v1/oauth/${provider}/login`, { method: 'POST' })
export const pollLogin = (provider) => api(`/v1/oauth/${provider}/poll`)

// ---------------------------------------------------------------- helpers

export function formatInterval(ms) {
  const s = Math.round(ms / 1000)
  const parts = [[Math.floor(s / 3600), 'h'], [Math.floor((s % 3600) / 60), 'm'], [s % 60, 's']]
  return parts.filter(([n]) => n).map(([n, u]) => n + u).join('') || '0s'
}

export function parseInterval(text) {
  const m = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/i.exec(text ?? '')
  const ms = m ? ((+m[1] || 0) * 3600 + (+m[2] || 0) * 60 + (+m[3] || 0)) * 1000 : 0
  return ms || null
}

export function transcriptMarkdown() {
  const out = [`# ${app.session?.title || 'silver session'}`]
  for (const t of app.turns) {
    if (t.note) continue
    out.push(`## User\n\n${t.user}`)
    for (const s of t.steps) {
      if (s.kind === 'text') out.push(`## Assistant\n\n${s.text}`)
      if (s.kind === 'tool') {
        const info = describe(s, currentWorkspace()?.path)
        out.push(`> ${info.verb} \`${info.target}\`${info.meta ? ` · ${info.meta}` : ''}`)
      }
    }
    if (t.error) out.push(`> ${t.error}`)
  }
  return out.join('\n\n') + '\n'
}

export function download(name, body) {
  const a = Object.assign(document.createElement('a'), {
    href: URL.createObjectURL(new Blob([body], { type: 'text/markdown' })),
    download: name,
  })
  a.click()
  URL.revokeObjectURL(a.href)
}
