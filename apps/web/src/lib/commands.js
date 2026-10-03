// Slash commands. The catalog (name, aliases, usage, summary) comes from silver's
// GET /v1/commands; this file only maps names to web behaviour. Catalog
// entries without a handler here (e.g. /quit) are not offered.
import { api } from './api.js'
import { approxTokens, contextTokens, percent, tokens } from './format.js'
import {
  app, note, notify, submit, steer, stop, rewind, setYolo, setApprovalMode, setModel, setPreset, updateSession,
  download, transcriptMarkdown, formatInterval, parseInterval, logout, currentPreset, planMode, setPlanMode, showPlan,
} from './state.svelte.js'

const on = (arg, current) => (arg === 'on' ? true : arg === 'off' ? false : !current)

/** status/pause/clear shared by /loop and /heartbeat; anything else starts one. */
function schedule(kind, arg, make) {
  const item = app[kind]
  switch (arg) {
    case '':
    case 'status': return note(item ? describe(kind) : `No active ${kind}`)
    case 'pause': return item ? ((item.status = 'paused'), notify(`${kind} paused`)) : notify(`No ${kind}`)
    case 'clear':
    case 'stop': app[kind] = null; return notify(`${kind} cleared`)
  }
  return make()
}

function describe(kind) {
  const g = app[kind]
  const every = g.interval ? `every ${formatInterval(g.interval)}` : 'after each turn'
  return `${kind === 'loop' ? 'Loop' : 'Heartbeat'} (${g.status}, ${every}, fired ${g.fired}${g.times ? `/${g.times}` : ''}): ${g.prompt}`
}

const HANDLERS = {
  help: () => note(app.commands.map((c) => `/${c.name} ${c.usage}`.trimEnd() + `\n    ${c.summary}`).join('\n')),
  copy: () => {
    const reply = app.turns.findLast((t) => t.steps?.some((s) => s.kind === 'text'))?.steps.findLast((s) => s.kind === 'text').text
    if (!reply) return notify('Nothing to copy')
    navigator.clipboard.writeText(reply)
    notify('Copied the last reply')
  },
  clear: () => (app.turns = []),
  status: () => {
    const c = app.context
    note([
      `Model      ${app.model ?? 'default'} via ${app.activeProvider ?? 'default provider'}`,
      `Preset     ${currentPreset()?.name ?? 'Minimal'}`,
      `Session    ${app.session ? `${app.session.title || 'untitled'} (${app.session.id})` : 'new'}`,
      `Run        ${app.run ? `active ${app.run.id}` : 'idle'}${app.queued ? ', 1 queued' : ''}`,
      `Approvals  ${app.approvals?.mode ?? 'unknown'}${app.session?.yolo_mode || app.pendingYolo ? ', YOLO on' : ''}${planMode() ? ', plan mode' : ''}`,
      `Context    ${c ? `${percent(c)}%, ${c.prompt_tokens ? '' : '~'}${tokens(contextTokens(c, app.tokensPerByte))} of ~${approxTokens(c.budget_bytes, app.tokensPerByte)} tokens before compaction` : 'no run yet'}`,
    ].join('\n'))
  },
  sessions: () => document.querySelector('[data-session-search]')?.focus() ?? document.querySelector('[aria-label="Search sessions"]')?.click(),
  usage: () => (app.panel = 'usage'),
  context: () => (app.panel = 'usage'),
  title: (arg) => (arg ? updateSession({ title: arg }) : note(`Title: ${app.session?.title || 'untitled'}`)),
  model: (arg) => {
    if (!arg) return document.querySelector('[data-model-picker]')?.click()
    const [head, ...rest] = arg.split(':')
    const known = app.providers.some((p) => p.id === head.toLowerCase())
    return known ? setModel(rest.join(':'), head.toLowerCase()) : setModel(arg)
  },
  yolo: (arg) => setYolo(on(arg, app.session?.yolo_mode ?? app.pendingYolo)),
  // /plan enters plan mode (and sends a description as the first prompt); inside it, /plan
  // shows the plan and /plan off leaves.
  plan: async (arg) => {
    if (arg === 'off') return planMode() ? setPlanMode(false) : notify('Not in plan mode')
    if (!planMode()) await setPlanMode(true)
    else if (!arg || arg === 'open') return showPlan()
    if (arg && arg !== 'open' && planMode()) submit(arg)
  },
  preset: (arg) => {
    if (!arg) return document.querySelector('[data-preset-picker]')?.click()
    const found = app.presets.find((p) => p.name.toLowerCase() === arg.toLowerCase())
    if (found) return setPreset(found.id)
    return notify(`No preset named "${arg}". /preset opens the list.`, true)
  },
  approvals: (arg) => (['manual', 'smart', 'off'].includes(arg) ? setApprovalMode(arg) : note(`Approvals: ${app.approvals?.mode}`)),
  export: () => download(`${app.session?.title || 'silver'}.md`, transcriptMarkdown()),
  retry: () => rewind(1, true),
  undo: (arg) => rewind(Math.min(Math.max(+arg || 1, 1), 100)),
  steer: (arg) => (arg ? steer(arg) : notify('Usage: /steer <prompt>')),
  stop: () => (app.run ? stop() : notify('No active run')),
  // The daemon holds the session's goal and starts every continuation.
  goal: async (arg) => {
    const [verb, n] = arg.split(/\s+/)
    const goal = app.session?.goal
    if (verb === 'budget') {
      app.settings.goalBudget = Math.max(+n || 20, 1)
      if (goal) await updateSession({ goal: { budget: app.settings.goalBudget } })
      return notify(`Goal budget: ${app.settings.goalBudget} continuations`)
    }
    if (!arg || arg === 'status') return note(goal ? `Goal (${goal.status}, ${goal.used}/${goal.max} continuations): ${goal.objective}` : 'No goal in this session')
    if (['pause', 'resume', 'clear', 'stop'].includes(arg)) {
      if (!goal) return notify('No goal in this session')
      const done = { pause: 'paused', resume: 'resumed', clear: 'cleared', stop: 'cleared' }[arg]
      if (await updateSession({ goal: arg === 'stop' ? 'clear' : arg })) notify(`Goal ${done}`)
      return
    }
    return submit(arg, app.settings.goalBudget)
  },
  loop: (arg) => {
    if (arg === 'resume' && app.loop) {
      if (app.loop.status === 'done') app.loop.fired = 0
      return Object.assign(app.loop, { status: 'running', next: Date.now() + (app.loop.interval ?? 0) })
    }
    return schedule('loop', arg, () => {
      const times = +(/--times\s+(\d+)/.exec(arg)?.[1] ?? 0) || null
      const words = arg.replace(/--times\s+\d+/, '').trim().replace(/^every\s+/i, '').split(/\s+/)
      const interval = parseInterval(words[0])
      const prompt = (interval ? words.slice(1) : words).join(' ')
      if (!prompt) return notify('Usage: /loop [interval] <prompt> [--times N]')
      app.loop = { prompt, interval, times, fired: 1, status: 'running', next: Date.now() + (interval ?? 0) }
      submit(prompt)
    })
  },
  heartbeat: (arg) => {
    if (arg === 'resume' && app.heartbeat) return Object.assign(app.heartbeat, { status: 'running', next: Date.now() + app.heartbeat.interval })
    return schedule('heartbeat', arg, () => {
      const [first, ...rest] = arg.replace(/^every\s+/i, '').split(/\s+/)
      const interval = parseInterval(first)
      if (!interval || !rest.length) return notify('Usage: /heartbeat every <interval> <prompt>')
      app.heartbeat = { prompt: rest.join(' '), interval, status: 'running', next: Date.now() + interval, fired: 0 }
      notify(`Heartbeat every ${formatInterval(interval)}`)
    })
  },
  focus: (arg) => (app.settings.focus = on(arg, app.settings.focus)),
  verbose: (arg) => (['off', 'new', 'all'].includes(arg) ? (app.settings.verbose = arg) : note(`Verbose: ${app.settings.verbose}`)),
  theme: (arg) =>
    (app.settings.theme = arg === 'dark' || arg === 'light' ? arg : arg === 'auto' ? 'system' : app.settings.theme === 'dark' ? 'light' : 'dark'),
  rollback: () => (app.panel = 'checkpoints'),
  diff: () => (app.panel = 'changes'),
  worktree: () => (app.panel = 'worktrees'),
  agents: () => (app.panel = 'agents'),
  login: () => (app.settingsTab = 'providers'),
  logout: (arg) => (arg ? logout(arg) : (app.settingsTab = 'providers')),
}

export async function loadCommands() {
  try {
    app.commands = (await api('/v1/commands')).filter((c) => HANDLERS[c.name])
  } catch (e) {
    notify(`Commands unavailable: ${e.message}`, true)
  }
}

const find = (name) => app.commands.find((c) => c.name === name || c.aliases.includes(name))

/** Commands whose name starts with, then contains, the typed prefix. */
export function matchCommands(input) {
  const q = input.slice(1).split(/\s/)[0].toLowerCase()
  const starts = app.commands.filter((c) => c.name.startsWith(q) || c.aliases.includes(q))
  return starts.concat(app.commands.filter((c) => !starts.includes(c) && c.name.includes(q)))
}

export function runCommand(input) {
  const [, name, arg = ''] = /^\/(\S*)\s*([\s\S]*)$/.exec(input.trim())
  const command = find(name.toLowerCase())
  if (!command) return notify(`Unknown command /${name}, /help lists them`)
  return HANDLERS[command.name](arg.trim())
}
