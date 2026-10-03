<!-- Right-hand workspace panel: changes (diff), checkpoints (rollback), worktrees, the agents
     (this chat's and the catalogue), usage & context. -->
<script>
  import { api } from '../lib/api.js'
  import { app, notify, currentWorkspace } from '../lib/state.svelte.js'
  import Agents from './Agents.svelte'
  import AgentList from './AgentList.svelte'
  import AgentView from './AgentView.svelte'
  import ConfirmButton from './ConfirmButton.svelte'
  import { ago, tokens, cost, percent, approxTokens, contextTokens } from '../lib/format.js'
  import { diffFiles } from '../lib/tools.js'
  import IconX from '~icons/lucide/x'
  import IconRefresh from '~icons/lucide/refresh-cw'
  import IconChevron from '~icons/lucide/chevron-right'

  const TABS = { changes: 'Changes', checkpoints: 'Checkpoints', worktrees: 'Worktrees', agents: 'Agents', usage: 'Usage' }
  const SCOPES = ['working', 'staged', 'all', 'session']
  const NOT_GIT = /not (inside )?a git repository/i
  const NOT_GIT_MESSAGE = 'Not a git repository.'

  let scope = $state('working')
  let data = $state(null)
  // The tab `data` was loaded for: a tab switch renders before the effect clears `data`,
  // and one tab's reply read as another's crashes the view.
  let dataTab = $state('')
  let error = $state('')
  let branch = $state('')
  let reload = $state(0)
  let opened = $state({})
  let restored = $state({})
  const CHECKPOINT_KIND = { create: 'Created', replace: 'Edited', delete: 'Deleted' }

  // Changes and worktrees belong to the workspace, so they work before the first message.
  const ws = $derived(currentWorkspace()?.id)
  const sid = $derived(app.session?.id)
  const delegations = $derived(app.turns.flatMap((t) => t.steps?.filter((s) => s.name === 'delegate_task') ?? []))

  const LOADERS = {
    changes: () => api('/v1/diff', { query: { workspace_id: ws, scope } }),
    checkpoints: () => api('/v1/checkpoints', { query: { session_id: sid, limit: 100 } }),
    worktrees: () => api('/v1/worktrees', { query: { workspace_id: ws } }),
    usage: () => api(`/v1/sessions/${sid}/usage`),
    // The agents tab renders its own list; there is nothing to load here.
    agents: () => Promise.resolve({ agents: [] }),
  }

  $effect(() => {
    const tab = app.panel
    reload, scope, app.run // refetch when the tab, scope or run state changes
    // Agents and usage work anywhere; the rest describe one workspace.
    const needsWorkspace = tab !== 'usage' && tab !== 'checkpoints' && tab !== 'agents'
    data = null
    const blocked = needsWorkspace
      ? ws ? '' : 'Pick a workspace to see its changes and worktrees.'
      : tab === 'agents' ? ''
      : sid ? '' : 'Send a message first.'
    error = blocked
    if (blocked) return
    let current = true // drop a slower reply from a tab the reader already left
    LOADERS[tab]().then((d) => current && ((data = d), (dataTab = tab)), (e) => current && (error = NOT_GIT.test(e.message) ? NOT_GIT_MESSAGE : e.message))
    return () => (current = false)
  })

  const refresh = () => reload++

  async function act(fn, message) {
    try {
      await fn()
      notify(message)
    } catch (e) {
      notify(e.message, true)
    }
    refresh()
  }

  async function restore(c) {
    try {
      await api(`/v1/checkpoints/${c.id}/restore`, { method: 'POST' })
      restored = { ...restored, [c.id]: true }
      notify(`Restored ${c.path}`)
    } catch (e) {
      notify(e.message, true)
    }
    refresh()
  }

  const createWorktree = (e) => {
    e.preventDefault()
    act(() => api('/v1/worktrees', { method: 'POST', body: { workspace_id: ws, name: branch.trim() || undefined } }), 'Worktree created')
    branch = ''
  }

  const removeWorktree = (w) =>
    act(() => api(`/v1/worktrees/${encodeURIComponent(w.name)}`, { method: 'DELETE', query: { workspace_id: ws } }), 'Worktree removed')
</script>

<aside class="panel">
  <header>
    <nav>
      {#each Object.entries(TABS) as [id, label] (id)}
        <button class:active={app.panel === id} onclick={() => (app.panel = id)}>{label}</button>
      {/each}
    </nav>
    <button class="icon-btn" title="Refresh" aria-label="Refresh" onclick={refresh}><IconRefresh /></button>
    <button class="icon-btn" title="Close panel" aria-label="Close panel" onclick={() => (app.panel = null)}><IconX /></button>
  </header>

  <div class="body">
    {#if app.panel === 'changes' && !error}
      <div class="scopes">
        {#each SCOPES as s (s)}<button class="chip" aria-expanded={scope === s} onclick={() => (scope = s)}>{s}</button>{/each}
      </div>
    {/if}

    {#if app.panel === 'agents' && app.agent}
      {#key `${app.agent.tool}:${app.agent.index}`}<AgentView />{/key}
    {:else if app.panel === 'agents'}
      {#if delegations.length}
        <h3>In this chat</h3>
        <div class="delegations">
          {#each delegations as step (step.id)}<AgentList {step} />{/each}
        </div>
        <h3>Available agents</h3>
      {/if}
      <Agents />
    {:else if error}
      <p class="muted">{error}</p>
    {:else if !data || dataTab !== app.panel}
      <p class="muted">Loading</p>
    {:else if app.panel === 'changes'}
      {@const files = diffFiles(data.diff ?? '')}
      {#if data.error}
        <p class={NOT_GIT.test(data.error) ? 'muted' : 'error'}>{data.error}</p>
      {:else if data.empty && !data.untracked?.length}
        <p class="muted">No changes.</p>
      {:else}
        {#each files as f (f.path)}
          {@const show = opened[f.path] ?? files.length <= 5}
          <div class="change">
            <button class="change-head" aria-expanded={show} onclick={() => (opened[f.path] = !show)}>
              <IconChevron class="chevron {show ? 'open' : ''}" />
              <span class="grow mono">{f.path}</span>
              {#if f.add}<span class="add">+{f.add}</span>{/if}
              {#if f.del}<span class="del">−{f.del}</span>{/if}
              {#if f.status}<span class="tag">{f.status}</span>{/if}
            </button>
            {#if show}
              <div class="diff">{#each f.rows as row, i (i)}<span class={row.cls}>{row.text}</span>{/each}</div>
            {/if}
          </div>
        {/each}
        {#each data.untracked ?? [] as path (path)}
          <div class="change-head"><span class="grow mono">{path}</span><span class="tag">new</span></div>
        {/each}
      {/if}
    {:else if app.panel === 'checkpoints'}
      {#each data as c (c.id)}
        <div class="row">
          <div class="grow"><div class="mono">{c.path.replace(`${currentWorkspace()?.path}/`, '')}</div><div class="muted small">{CHECKPOINT_KIND[c.kind] ?? c.kind} · {ago(c.created_at)}{#if restored[c.id]} · Restored{/if}</div></div>
          <ConfirmButton title="Put the file back as it was before this change" ask="Restore?" onconfirm={() => restore(c)}>Restore</ConfirmButton>
        </div>
      {:else}
        <p class="muted">No file checkpoints in this session yet. They are taken before each write.</p>
      {/each}
    {:else if app.panel === 'worktrees'}
      <form class="row" onsubmit={createWorktree}>
        <input class="field" placeholder="Name (optional)" bind:value={branch} />
        <button class="btn primary">New worktree</button>
      </form>
      {#each data.worktrees as w, i (w.path)}
        <div class="row">
          <div class="grow"><div>{w.name} <span class="muted">{w.branch ?? 'detached'}</span></div><div class="muted small mono">{w.path}</div></div>
          {#if i > 0}<ConfirmButton class="btn danger" ask="Remove?" onconfirm={() => removeWorktree(w)}>Remove</ConfirmButton>{/if}
        </div>
      {/each}
    {:else if app.panel === 'usage'}
      {@const c = app.context}
      {@const ratio = app.tokensPerByte}
      <h3>Session, all requests</h3>
      <dl>
        <dt>Prompt</dt><dd>{tokens(data.prompt_tokens)}</dd>
        <dt>Completion</dt><dd>{tokens(data.completion_tokens)}</dd>
        <dt>Total</dt><dd>{tokens(data.total_tokens)}</dd>
        {#if data.cost_usd}<dt>Cost</dt><dd>{cost(data.cost_usd)}</dd>{/if}
        <dt>Runs</dt><dd>{data.runs.length}</dd>
      </dl>
      {#if c}
        <h3>Context, {percent(c)}% used</h3>
        <div class="bar">
          {#each [['system', c.system_prompt_bytes], ['tools', c.tool_schema_bytes], ['conversation', c.conversation_bytes]] as [k, v] (k)}
            <span class={k} style:width="{(v * 100) / c.budget_bytes}%"></span>
          {/each}
        </div>
        <dl>
          <dt><i class="system"></i>System prompt</dt><dd>~{approxTokens(c.system_prompt_bytes, ratio)}</dd>
          <dt><i class="tools"></i>Tool schemas</dt><dd>~{approxTokens(c.tool_schema_bytes, ratio)}</dd>
          <dt><i class="conversation"></i>Conversation</dt><dd>~{approxTokens(c.conversation_bytes, ratio)}</dd>
          <dt>In context</dt><dd>{c.prompt_tokens ? '' : '~'}{tokens(contextTokens(c, ratio))}</dd>
          <dt>Compacts at</dt><dd>~{approxTokens(c.budget_bytes, ratio)}</dd>
          <dt>Window</dt><dd>{c.window_tokens ? tokens(c.window_tokens) : 'unknown'}</dd>
        </dl>
      {/if}
    {/if}
  </div>
</aside>

<style>
  .panel { display: flex; flex-direction: column; height: 100%; min-width: 0; border-left: 1px solid var(--line); background: var(--bg); }
  header { display: flex; align-items: center; gap: var(--space-1); height: 52px; padding: 0 var(--space-3); border-bottom: 1px solid var(--line); }
  nav { display: flex; flex: 1; gap: var(--space-2); min-width: 0; height: 100%; overflow-x: auto; scrollbar-width: thin; }
  nav button { flex: none; padding: 0; border: 0; border-bottom: 2px solid transparent; background: none; color: var(--ink-3); font-size: var(--text-xs); white-space: nowrap; }
  nav button:hover { color: var(--ink); }
  nav button.active { color: var(--ink); border-bottom-color: var(--ink); }
  .body { flex: 1; overflow: auto; padding: var(--space-4); font-size: var(--text-sm); }
  .scopes { display: flex; gap: var(--space-1); margin-bottom: var(--space-3); text-transform: capitalize; }

  .change { border-bottom: 1px solid var(--line); }
  .change-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: var(--space-2) 0;
    border: 0;
    background: none;
    color: var(--ink);
    font-size: var(--text-sm);
    text-align: left;
  }
  .change-head :global(.chevron) { flex: none; width: 14px; height: 14px; color: var(--ink-3); transition: transform 0.15s; }
  .change-head :global(.chevron.open) { transform: rotate(90deg); }
  .change-head .add { color: var(--success); font-variant-numeric: tabular-nums; }
  .change-head .del { color: var(--danger); font-variant-numeric: tabular-nums; }
  .change .tag, .change-head .tag { flex: none; padding: 0 var(--space-1); border: 1px solid var(--line); border-radius: var(--radius-sm); color: var(--ink-3); font-size: var(--text-xs); }
  .diff { overflow-x: auto; padding: 0 0 var(--space-2); font-size: var(--text-xs); line-height: 1.65; }
  .diff span { display: block; padding: 0 var(--space-2); white-space: pre; }
  .diff .hunk { color: var(--ink-3); }
  .diff .add { background: var(--diff-add); }
  .diff .del { background: var(--diff-del); }

  .row { display: flex; align-items: center; gap: var(--space-3); padding: var(--space-3) 0; border-bottom: 1px solid var(--line); }
  form.row { padding-top: 0; }
  .grow { flex: 1; min-width: 0; overflow-wrap: anywhere; }
  .small { font-size: var(--text-xs); }

  h3 { margin: var(--space-4) 0 var(--space-2); font-size: var(--text-sm); font-weight: 600; }
  h3:first-child { margin-top: 0; }
  .delegations { display: grid; gap: var(--space-4); margin-bottom: var(--space-6); }
  dl { display: grid; grid-template-columns: 1fr auto; gap: var(--space-2) var(--space-4); margin: 0; }
  dt { color: var(--ink-2); }
  dd { margin: 0; text-align: right; font-variant-numeric: tabular-nums; }
  .bar { display: flex; height: 8px; margin-bottom: var(--space-3); border-radius: 4px; background: var(--bg-sunken); overflow: hidden; }
  dt i { display: inline-block; width: 8px; height: 8px; margin-right: var(--space-2); border-radius: 2px; }
  .system { background: var(--ink); }
  .tools { background: var(--ink-3); }
  .conversation { background: var(--accent); }
</style>
