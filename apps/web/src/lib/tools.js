// Turns a tool call step into what a transcript row shows: a verb, a target, a meta value and
// which body renderer applies. One switch per tool, so a row reads as an action on a target
// with an outcome rather than a function name and a JSON dump.

const READ_ONLY_EXPLORE = new Set(['read_file', 'list_files', 'search_files'])

const KNOWN_TOOLS = new Set([
  'read_file', 'list_files', 'search_files', 'patch', 'write_file',
  'bash', 'run_command', 'execute_code', 'todo_list',
  'web_search', 'web_extract', 'session_search', 'skill_view', 'skills_list', 'skill_manage', 'lsp',
  'ask_user_question', 'exit_plan_mode', 'delegate_task',
])

/** The one argument that best identifies a tool call (path, command, query...), with the
 * workspace root stripped from paths. Used for the fallback "any other tool" row. */
export function toolArg(args, root) {
  if (!args || typeof args !== 'object') return ''
  const key = ['path', 'command', 'pattern', 'query', 'url', 'name', 'action'].find((k) => typeof args[k] === 'string')
  const value = key ? args[key] : JSON.stringify(args).slice(0, 80)
  return strip(value, root)
}

function strip(path, root) {
  // A workspace added as "/repo/" keeps that trailing slash.
  const base = root?.replace(/\/+$/, '')
  return typeof path === 'string' && base && path.startsWith(`${base}/`) ? path.slice(base.length + 1) : path
}

/** ask = approval-card title (also a call the user denied, which never ran), running = present
 * participle, done = past tense. A call waiting for approval has not started yet. */
function verbFor([ask, running, done], status) {
  if (status === 'ask' || status === 'denied') return ask
  if (status === 'waiting') return `Wants to ${ask[0].toLowerCase()}${ask.slice(1)}`
  if (status === 'running') return running
  return done
}

/** Parse a tool result: a JSON envelope on its first line (guard notes may follow after a
 * blank line), or plain text. Used for both the live (cut) summary and the full reloaded
 * content, since both share this shape. */
export function result(text) {
  if (!text) return { text: '' }
  if (text[0] === '{') {
    const nl = text.indexOf('\n')
    const head = nl === -1 ? text : text.slice(0, nl)
    try {
      const envelope = JSON.parse(head)
      const extra = nl === -1 ? '' : text.slice(nl + 1).replace(/^\n+/, '')
      const body = envelope.output ?? envelope.content ?? envelope.text ?? envelope.result ?? ''
      return {
        envelope,
        text: extra ? `${body}\n\n${extra}` : body,
        exitCode: envelope.exit_code,
      }
    } catch {
      // Not actually an envelope; fall through to plain text.
    }
  }
  return { text }
}

/** Head/tail split of long text: everything within `max` lines, else the first two lines, a
 * hidden-line count, and the last three (the tail matters more: build/test output ends in its
 * verdict). */
export function preview(text, max = 6) {
  const lines = (text ?? '').split('\n')
  if (lines.length <= max) return { head: lines, hidden: 0, tail: [] }
  const head = lines.slice(0, 2)
  const tail = lines.slice(-3)
  return { head, hidden: lines.length - head.length - tail.length, tail }
}

/** Line-level LCS diff of two texts: `{ sign: '-' | '+' | ' ', text }[]`. */
export function diff(before, after) {
  const a = (before ?? '').split('\n')
  const b = (after ?? '').split('\n')
  // Both texts end in a real trailing newline (the common case for old_string/new_string
  // copied verbatim): drop the spurious blank line it would otherwise add to the diff.
  if (a.length > 1 && b.length > 1 && a.at(-1) === '' && b.at(-1) === '') { a.pop(); b.pop() }
  const n = a.length
  const m = b.length
  const dp = Array.from({ length: n + 1 }, () => new Array(m + 1).fill(0))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1])
    }
  }
  const rows = []
  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (a[i] === b[j]) { rows.push({ sign: ' ', text: a[i] }); i++; j++ }
    else if (dp[i + 1][j] >= dp[i][j + 1]) { rows.push({ sign: '-', text: a[i] }); i++ }
    else { rows.push({ sign: '+', text: b[j] }); j++ }
  }
  while (i < n) { rows.push({ sign: '-', text: a[i] }); i++ }
  while (j < m) { rows.push({ sign: '+', text: b[j] }); j++ }
  return rows
}

/** Number `diff()` rows from a starting line, old-file numbering for context/deletions and
 * new-file numbering for insertions (they agree until the two texts diverge). */
export function withLineNumbers(rows, startLine) {
  let oldLine = startLine
  let newLine = startLine
  return rows.map((row) => {
    if (row.sign === '-') return { ...row, line: oldLine++ }
    if (row.sign === '+') return { ...row, line: newLine++ }
    const line = oldLine
    oldLine++
    newLine++
    return { ...row, line }
  })
}

/** Split git's unified diff into files for the changes panel: `{ path, status, add, del, rows }`,
 * each row `{ text, cls }` from the first hunk on; the header lines only feed `status`. */
export function diffFiles(text) {
  const files = []
  for (const line of (text ?? '').split('\n')) {
    const head = /^diff --git a\/.+ b\/(.+)$/.exec(line)
    if (head) {
      files.push({ path: head[1], status: '', add: 0, del: 0, rows: [] })
      continue
    }
    const file = files.at(-1)
    if (!file) continue
    if (!file.rows.length && !line.startsWith('@@') && !line.startsWith('Binary')) {
      if (line.startsWith('new file')) file.status = 'new'
      if (line.startsWith('deleted file')) file.status = 'deleted'
      continue
    }
    const cls = line.startsWith('@@') ? 'hunk' : line[0] === '+' ? 'add' : line[0] === '-' ? 'del' : ''
    if (cls === 'add') file.add++
    if (cls === 'del') file.del++
    file.rows.push({ text: line || ' ', cls })
  }
  return files
}

/** Read-only exploration: consecutive calls to these collapse into one "Explored" group. */
export function explores(step) {
  if (step.kind !== 'tool') return false
  if (READ_ONLY_EXPLORE.has(step.name)) return true
  return step.name === 'lsp' && step.args?.action !== 'rename'
}

function matchMeta(step) {
  const text = result(step.output).text
  if (!step.output) return ''
  if (!text || /^no (text )?matches/.test(text.trim())) return 'no matches'
  const n = text.split('\n').filter(Boolean).length
  return `${n} match${n === 1 ? '' : 'es'}`
}

function patchMeta(step) {
  // A failed edit changed nothing, so its row says "failed" rather than +/- counts.
  if (!step.output || step.status === 'failed') return ''
  const text = result(step.output).text ?? ''
  if (text.startsWith('no change')) return 'no change'
  const rows = diff(step.args?.old_string, step.args?.new_string)
  const add = rows.filter((r) => r.sign === '+').length
  const del = rows.filter((r) => r.sign === '-').length
  const match = /replaced (\d+) occurrences?/.exec(text)
  const count = match ? Number(match[1]) : 1
  return `+${add} −${del}${count > 1 ? ` ×${count}` : ''}`
}

function commandMeta(step) {
  if (!step.output) return ''
  const r = result(step.output)
  if (r.exitCode != null && r.exitCode !== 0) return `exit ${r.exitCode}`
  return ''
}

/** How a batch ended: how many subagents reported, and how long the slowest one took. */
function delegateMeta(step) {
  const tasks = step.subagents ?? []
  if (!tasks.length) return ''
  const done = tasks.filter((t) => t.status === 'completed').length
  const failed = tasks.length - done
  const ms = Math.max(0, ...tasks.map((t) => t.durationMs ?? 0))
  const counts = [done && `${done} done`, failed && `${failed} failed`].filter(Boolean).join(' · ')
  return [counts, ms >= 1000 ? `${Math.round(ms / 1000)}s` : ''].filter(Boolean).join(' ')
}

function todoItems(step) {
  return result(step.output).envelope?.todos ?? step.args?.todos ?? []
}

function todoMeta(step) {
  const items = todoItems(step)
  if (!items.length) return ''
  const done = items.filter((t) => t.status === 'completed').length
  return `${done} of ${items.length} done`
}

function lspTarget(args) {
  const at = args.path ? `${args.path}${args.line ? `:${args.line}` : ''}` : ''
  return [args.action, at].filter(Boolean).join(' ')
}

/** `{ verb, target, meta, body, open, icon }` for one tool step: verb and target name the
 * action, meta is the outcome (exit code, match count, +/-, ...), body picks the ToolBody
 * renderer, icon picks the lucide icon (by name) shown before the verb. */
export function describe(step, root) {
  const { name, args = {}, status } = step
  const path = strip(args.path, root)

  switch (name) {
    case 'read_file': {
      const meta = args.offset || args.limit
        ? `lines ${args.offset ?? 1}–${(args.offset ?? 1) + (args.limit ?? 0) - 1}`
        : ''
      return { verb: verbFor(['Read', 'Reading', 'Read'], status), target: path, meta, body: 'output', open: false, icon: 'file-text' }
    }
    case 'list_files': {
      const meta = args.depth ? `depth ${args.depth}` : ''
      return { verb: verbFor(['List', 'Listing', 'Listed'], status), target: path || '.', meta, body: 'output', open: false, icon: 'folder' }
    }
    case 'search_files': {
      let target = args.pattern ? `"${args.pattern}"` : ''
      if (args.path) target += ` in ${strip(args.path, root)}`
      if (args.file_glob) target += ` ${args.file_glob}`
      return { verb: verbFor(['Search', 'Searching', 'Searched'], status), target, meta: matchMeta(step), body: 'output', open: false, icon: 'search' }
    }
    case 'patch':
      return { verb: verbFor(['Edit', 'Editing', 'Edited'], status), target: path, meta: patchMeta(step), body: 'diff', open: true, icon: 'file-diff' }
    case 'write_file': {
      const lines = typeof args.content === 'string' ? args.content.split('\n').length : 0
      return { verb: verbFor(['Write', 'Writing', 'Wrote'], status), target: path, meta: lines ? `${lines} lines` : '', body: 'content', open: true, icon: 'file-plus' }
    }
    case 'bash':
    case 'run_command':
    case 'execute_code': {
      const command = (args.command ?? args.code ?? '').split('\n')[0]
      return { verb: verbFor(['Run', 'Running', 'Ran'], status), target: command, meta: commandMeta(step), body: 'output', open: true, icon: 'terminal' }
    }
    case 'todo_list': {
      const hasTodos = Array.isArray(args.todos) && args.todos.length > 0
      const forms = hasTodos
        ? ['Update todos', 'Updating todos', 'Updated todos']
        : ['Read todos', 'Reading todos', 'Read todos']
      return { verb: verbFor(forms, status), target: '', meta: todoMeta(step), body: 'todos', open: hasTodos, icon: 'list-checks' }
    }
    case 'web_search':
      return { verb: verbFor(['Search the web', 'Searching the web', 'Searched the web'], status), target: args.query ? `"${args.query}"` : '', meta: '', body: 'output', open: false, icon: 'globe' }
    case 'web_extract': {
      const url = (args.url ?? args.urls?.[0] ?? '').replace(/^https?:\/\//, '')
      return { verb: verbFor(['Fetch', 'Fetching', 'Fetched'], status), target: url, meta: '', body: 'output', open: false, icon: 'link' }
    }
    case 'view_image':
      return { verb: verbFor(['View image', 'Viewing image', 'Viewed image'], status), target: path, meta: matchMeta(step), body: 'image', open: true, icon: 'image' }
    case 'search_documents': {
      let target = args.query ? `"${args.query}"` : ''
      if (args.path) target += ` in ${strip(args.path, root)}`
      return { verb: verbFor(['Search documents', 'Searching documents', 'Searched documents'], status), target, meta: matchMeta(step), body: 'output', open: false, icon: 'file-text' }
    }
    case 'session_search':
      return { verb: verbFor(['Search sessions', 'Searching sessions', 'Searched sessions'], status), target: args.query ? `"${args.query}"` : '', meta: '', body: 'output', open: false, icon: 'messages-square' }
    case 'skill_view':
      return { verb: verbFor(['Read skill', 'Reading skill', 'Read skill'], status), target: args.name ?? '', meta: '', body: 'output', open: false, icon: 'puzzle' }
    case 'skills_list':
      return { verb: verbFor(['List skills', 'Listing skills', 'Listed skills'], status), target: '', meta: '', body: 'output', open: false, icon: 'puzzle' }
    case 'skill_manage':
      return { verb: verbFor(['Update skill', 'Updating skill', 'Updated skill'], status), target: args.name ?? '', meta: args.action ?? '', body: 'output', open: false, icon: 'puzzle' }
    case 'lsp':
      return { verb: verbFor(['Look up', 'Looking up', 'Looked up'], status), target: lspTarget(args), meta: '', body: 'output', open: false, icon: 'crosshair' }
    // Only the Markdown export reads this; the transcript renders a delegation as AgentList.
    case 'delegate_task':
      return {
        verb: verbFor(['Delegate', 'Delegating', 'Delegated'], status),
        target: `${step.subagents?.length ?? args.tasks?.length ?? 0} tasks`,
        meta: delegateMeta(step),
        body: 'raw',
        open: false,
        icon: 'users',
      }
    case 'ask_user_question':
      return { verb: verbFor(['Ask', 'Asking', 'Asked'], status), target: args.question ?? '', meta: '', body: 'output', open: false, icon: 'circle-help' }
    case 'exit_plan_mode':
      return { verb: verbFor(['Present plan', 'Presenting plan', 'Presented plan'], status), target: '', meta: '', body: 'output', open: false, icon: 'map' }
    default:
      return { verb: verbFor(['Call', 'Calling', 'Called'], status), target: `${name} ${toolArg(args, root)}`.trim(), meta: '', body: 'raw', open: false, icon: 'wrench' }
  }
}

/** The finished-process summary sentence, e.g. "Read 3 files, edited 2 files, ran 2 commands".
 * A denied call never ran, so it is not counted. */
export function phrase(steps) {
  const tools = steps.filter((s) => s.kind === 'tool' && s.status !== 'denied')
  if (!tools.length) return null
  const byName = (name) => tools.filter((s) => s.name === name)
  const distinctPaths = (names) => new Set(tools.filter((s) => names.includes(s.name)).map((s) => s.args?.path)).size

  const parts = []
  const read = distinctPaths(['read_file'])
  if (read) parts.push(`read ${read} file${read === 1 ? '' : 's'}`)
  const listed = byName('list_files').length
  if (listed) parts.push(`listed ${listed} director${listed === 1 ? 'y' : 'ies'}`)
  const searched = byName('search_files').length
  if (searched) parts.push(`searched ${searched} pattern${searched === 1 ? '' : 's'}`)
  const edited = distinctPaths(['patch', 'write_file'])
  if (edited) parts.push(`edited ${edited} file${edited === 1 ? '' : 's'}`)
  const ran = tools.filter((s) => ['bash', 'run_command', 'execute_code'].includes(s.name)).length
  if (ran) parts.push(`ran ${ran} command${ran === 1 ? '' : 's'}`)
  if (byName('todo_list').length) parts.push('updated todos')
  if (byName('web_search').length) parts.push('searched the web')
  const fetched = byName('web_extract').length
  if (fetched) parts.push(`fetched ${fetched} page${fetched === 1 ? '' : 's'}`)
  if (byName('session_search').length) parts.push('searched past sessions')
  const skills = tools.filter((s) => ['skill_view', 'skills_list', 'skill_manage'].includes(s.name)).length
  if (skills) parts.push(`used ${skills} skill${skills === 1 ? '' : 's'}`)
  const lsp = byName('lsp').length
  if (lsp) parts.push(`ran ${lsp} code lookup${lsp === 1 ? '' : 's'}`)
  const other = tools.filter((s) => !KNOWN_TOOLS.has(s.name)).length
  if (other) parts.push(`called ${other} tool${other === 1 ? '' : 's'}`)

  if (!parts.length) return null
  const sentence = parts.join(', ')
  return sentence.charAt(0).toUpperCase() + sentence.slice(1)
}

const FINISHED = ['completed', 'failed', 'denied', 'blocked']

/** A delegate_task call's tasks: its arguments merged with what each subagent reported. */
export function agents(step) {
  const asked = Array.isArray(step.args?.tasks) ? step.args.tasks : []
  const live = step.subagents ?? []
  const count = Math.max(asked.length, ...live.map((s) => s.index + 1))
  return Array.from({ length: count }, (_, index) => {
    const task = asked[index] ?? {}
    const entry = live.find((s) => s.index === index) ?? { steps: [], status: 'running' }
    let status = entry.status
    // The batch ended under it: a stop, or a daemon restart.
    if (status === 'running' && FINISHED.includes(step.status)) status = 'stopped'
    else if (status === 'running' && entry.steps.some((s) => s.status === 'waiting')) status = 'waiting'
    return {
      ...entry,
      index,
      status,
      agent: entry.agent ?? task.agent ?? 'general-purpose',
      description: entry.description ?? (task.description || task.prompt?.split('\n')[0] || 'Task'),
      prompt: task.prompt ?? '',
    }
  })
}

/** A subagent's steps are only its tool calls and injected context (nested_event). */
export function activity(agent, root) {
  const tools = agent.steps.filter((s) => s.kind === 'tool')
  if (agent.status === 'waiting') return 'Needs your approval'
  if (agent.status === 'stopped') return 'Stopped before it finished'
  if (agent.status === 'completed') {
    const n = agent.toolUses ?? tools.length
    return `Finished · ${n} step${n === 1 ? '' : 's'}`
  }
  if (agent.status !== 'running') return `Failed: ${agent.report?.split('\n')[0] || 'no reason given'}`
  if (!tools.length) return 'Starting'
  const info = describe(tools.at(-1), root)
  return `${info.verb} ${info.target}`.trim()
}

export const agentName = (name) => name.replace(/-/g, ' ').replace(/^./, (c) => c.toUpperCase())
