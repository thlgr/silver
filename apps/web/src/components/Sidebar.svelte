<script>
  import { tick } from 'svelte'
  import { app, newSession, openSession, deleteSession, addWorkspace, removeWorkspace, openFolderDialog, searchSessions } from '../lib/state.svelte.js'
  import { ago, sessionLabel } from '../lib/format.js'
  import ConfirmButton from './ConfirmButton.svelte'
  import IconPen from '~icons/lucide/square-pen'
  import IconPanel from '~icons/lucide/panel-left'
  import IconSearch from '~icons/lucide/search'
  import IconFolderPlus from '~icons/lucide/folder-plus'
  import IconFolder from '~icons/lucide/folder'
  import IconChevron from '~icons/lucide/chevron-down'
  import IconChevronRight from '~icons/lucide/chevron-right'
  import IconPlus from '~icons/lucide/plus'
  import IconTrash from '~icons/lucide/trash-2'
  import IconSettings from '~icons/lucide/settings'
  import IconLoader from '~icons/lucide/loader-circle'

  let { onCollapse } = $props()
  let searching = $state(false)
  let adding = $state(false)
  let query = $state('')
  let path = $state('')
  let name = $state('')
  let problem = $state('')
  let picking = $state(false)
  let found = $state(new Set()) // sessions whose messages match the query, from the daemon
  let tip = $state(null) // { group, top, left }: workspace card shown beside the hovered row
  let dragged = $state(null) // id of the workspace being dragged
  let over = $state(null) // id of the workspace it would land on

  const working = (s) => s.active_run || (app.run && app.session?.id === s.id)
  const waiting = (s) => app.approval && app.session?.id === s.id
  const selected = $derived(app.session ? (app.session.workspace_id ?? null) : app.scope)
  const index = (id) => app.workspaces.findIndex((w) => w.id === id)
  const needle = $derived(query.trim().toLowerCase())
  const matches = (s) => !needle || sessionLabel(s).toLowerCase().includes(needle) || found.has(s.id)
  const groups = $derived(
    [...app.workspaces, { id: null, name: 'No workspace' }].map((g) => ({
      ...g,
      working: app.sessions.some((s) => (s.workspace_id ?? null) === g.id && working(s)),
      sessions: app.sessions.filter((s) => (s.workspace_id ?? null) === g.id && matches(s)),
    })),
  )
  const nothing = $derived(needle && !groups.some((g) => g.sessions.length))

  $effect(() => {
    const q = needle
    found = new Set()
    if (!q) return
    const timer = setTimeout(async () => {
      const ids = await searchSessions(q)
      if (q === needle) found = ids
    }, 200)
    return () => clearTimeout(timer)
  })

  const focus = (node) => node.focus()
  const focusComposer = () => tick().then(() => document.querySelector('.composer textarea')?.focus())
  const fresh = (scope) => (newSession(scope), focusComposer())

  function closeSearch() {
    searching = false
    query = ''
  }

  // Workspace ids the user folded; null (no workspace) is stored as 'none'.
  const key = (id) => id ?? 'none'
  const open = (g) => query || !app.settings.collapsed.includes(key(g.id))
  function toggle(g) {
    const c = app.settings.collapsed
    app.settings.collapsed = open(g) ? [...c, key(g.id)] : c.filter((k) => k !== key(g.id))
  }

  // Fixed positioning so the card is not clipped by the scrolling list.
  function showTip(e, group) {
    const r = e.currentTarget.getBoundingClientRect()
    tip = group.id ? { group, top: r.top, left: r.right + 12 } : null
  }

  // Takes the target's place, so it lands below the target when moving down and above when up.
  function drop() {
    const list = [...app.workspaces]
    list.splice(index(over), 0, ...list.splice(index(dragged), 1))
    app.workspaces = list
    app.settings.order = list.map((w) => w.id)
  }

  async function save() {
    problem = ''
    try {
      newSession((await addWorkspace(path.trim(), name.trim())).id)
    } catch (err) {
      // The form stays open with the typed path, so a typo is a quick fix.
      problem = err.message
      return
    }
    adding = false
    path = name = ''
    focusComposer()
  }

  // A browser never reveals a folder's absolute path, so silver opens the system's own dialog.
  // The form is the fallback, shown only when the machine running silver has none to open.
  async function pickFolder() {
    problem = ''
    picking = true
    try {
      const picked = await openFolderDialog()
      if (picked) {
        path = picked
        await save()
      }
    } catch (err) {
      problem = err.message
      adding = true
    }
    picking = false
  }
</script>

<aside class="sidebar">
  <header>
    <span class="wordmark">silver</span>
    <button class="icon-btn" title="Collapse sidebar" aria-label="Collapse sidebar" onclick={onCollapse}><IconPanel /></button>
  </header>

  <button class="btn new" onclick={() => fresh()}><IconPen /> New session</button>

  <div class="section">
    <span>Workspaces</span>
    <button class="icon-btn" title="Search sessions" aria-label="Search sessions" aria-expanded={searching} onclick={() => (searching ? closeSearch() : (searching = true))}><IconSearch /></button>
    <button class="icon-btn" title="Add workspace" aria-label="Add workspace" aria-expanded={adding} disabled={picking} onclick={pickFolder}>
      {#if picking}<IconLoader class="spin spinner" />{:else}<IconFolderPlus />{/if}
    </button>
  </div>

  {#if searching}
    <input class="field" data-session-search placeholder="Search sessions" bind:value={query} use:focus onkeydown={(e) => e.key === 'Escape' && closeSearch()} />
  {/if}
  {#if adding}
    <form class="add" onsubmit={(e) => (e.preventDefault(), save())}>
      <button type="button" class="btn" disabled={picking} onclick={pickFolder}><IconFolder /> {picking ? 'Waiting for the folder dialog…' : 'Choose folder…'}</button>
      <input class="field" placeholder="or type /path/to/project" bind:value={path} oninput={() => (problem = '')} required use:focus onkeydown={(e) => e.key === 'Escape' && (adding = false)} />
      <input class="field" placeholder="Name (optional)" bind:value={name} />
      {#if problem}<p class="error" role="alert">{problem}</p>{/if}
      <button class="btn primary" disabled={!path.trim()}>Add workspace</button>
    </form>
  {/if}
  {#if nothing}<p class="muted empty">No sessions match “{query.trim()}”.</p>{/if}

  <nav>
    {#each groups as group (group.id)}
      {#if (group.id && !needle) || group.sessions.length}
        <div
          class="workspace"
          class:selected={selected === group.id}
          class:current={selected === group.id && !app.session}
          class:dragged={dragged === group.id}
          class:above={over === group.id && index(dragged) > index(over)}
          class:below={over === group.id && index(dragged) < index(over)}
          role="group"
          draggable={!!group.id}
          onmouseenter={(e) => showTip(e, group)}
          onmouseleave={() => (tip = null)}
          ondragstart={(e) => (e.dataTransfer.setData('text/plain', group.name), (dragged = group.id), (tip = null))}
          ondragover={(e) => dragged && group.id && (e.preventDefault(), (over = group.id))}
          ondragleave={() => (over = null)}
          ondrop={drop}
          ondragend={() => (dragged = over = null)}
        >
          <button class="toggle" aria-expanded={open(group)} onclick={() => toggle(group)}>
            <span class="folder">{#if group.working}<IconLoader class="spin spinner" />{:else}<IconFolder />{/if}</span>
            <span class="chevron">{#if open(group)}<IconChevron />{:else}<IconChevronRight />{/if}</span>
            <span class="name">{group.name}</span>
          </button>
          <button class="icon-btn" title="New session in {group.name}" aria-label="New session in {group.name}" onclick={() => fresh(group.id)}><IconPlus /></button>
          {#if group.id}
            <ConfirmButton class="icon-btn" title="Remove workspace (files on disk stay)" ask="Remove?" onconfirm={() => removeWorkspace(group.id)}><IconTrash /></ConfirmButton>
          {/if}
        </div>
        {#each open(group) ? group.sessions : [] as s (s.id)}
          <a
            class="session"
            class:active={app.session?.id === s.id}
            href="#{s.id}"
            onclick={(e) => (e.preventDefault(), openSession(s.id))}
          >
            {#if working(s)}<IconLoader class="spin spinner" />{/if}
            <span class="name">{sessionLabel(s)}</span>
            <span class="time" class:waiting={waiting(s)}>{waiting(s) ? 'Needs approval' : working(s) ? 'Working' : ago(s.updated_at)}</span>
            <ConfirmButton class="icon-btn" title="Delete session" ask="Delete?" onconfirm={() => deleteSession(s.id)}><IconTrash /></ConfirmButton>
          </a>
        {/each}
      {/if}
    {/each}
  </nav>

  {#if tip}
    <div class="tip" style:top="{tip.top}px" style:left="{tip.left}px">
      <strong>{tip.group.name}</strong>
      <span>{tip.group.path}</span>
      <span>Created {new Date(tip.group.created_at).toLocaleString()}</span>
    </div>
  {/if}

  <footer>
    <button class="chip" onclick={() => (app.settingsTab = 'general')}><IconSettings /> Settings</button>
  </footer>
</aside>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    height: 100%;
    padding: var(--space-3);
    border-right: 1px solid var(--line);
    background: var(--bg-sidebar);
    min-width: 0;
  }
  header { display: flex; align-items: center; justify-content: space-between; height: 36px; padding-left: var(--space-1); }
  .wordmark { font-size: var(--text-lg); font-weight: 650; letter-spacing: -0.02em; }
  .new { width: 100%; margin: var(--space-1) 0 var(--space-2); }
  .section { display: flex; align-items: center; gap: var(--space-1); padding-left: var(--space-1); color: var(--ink-3); font-size: var(--text-sm); }
  .section span { flex: 1; }
  .add { display: grid; gap: var(--space-2); }
  .add .error { margin: 0; font-size: var(--text-xs); overflow-wrap: anywhere; }
  nav { flex: 1; overflow-y: auto; margin: 0 calc(-1 * var(--space-1)); padding: 0 var(--space-1); }

  .workspace, .session {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: 34px;
    padding: 0 var(--space-1) 0 var(--space-2);
    border-radius: var(--radius-sm);
    font-size: var(--text-sm);
  }
  .workspace { margin-top: var(--space-3); border-block: 2px solid transparent; color: var(--ink-2); }
  .workspace:first-child { margin-top: 0; }
  .toggle { display: flex; flex: 1; min-width: 0; align-items: center; gap: var(--space-2); height: 100%; padding: 0; border: 0; background: none; color: inherit; font: inherit; cursor: pointer; text-align: left; }
  .workspace:hover { background: var(--bg-hover); }
  .workspace.selected { color: var(--ink); font-weight: 600; }
  .workspace.current { background: var(--bg-active); }
  .workspace.dragged { opacity: 0.4; }
  .workspace.above { border-top-color: var(--accent); }
  .workspace.below { border-bottom-color: var(--accent); }
  .folder { display: contents; }
  .chevron, .workspace:hover .folder { display: none; }
  .workspace:hover .chevron { display: contents; }
  .tip {
    position: fixed;
    z-index: 10;
    display: grid;
    gap: var(--space-2);
    width: 260px;
    padding: var(--space-3) var(--space-4);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-md);
    background: var(--bg-raised);
    font-size: var(--text-sm);
    pointer-events: none;
    animation: fade 120ms 300ms both;
  }
  .tip span { color: var(--ink-2); font-size: var(--text-xs); overflow-wrap: anywhere; }
  @keyframes fade { from { opacity: 0; } }
  /* Titles line up with the workspace name; the spinner sits in the icon column below the folder. */
  .session { position: relative; padding-left: calc(2 * var(--space-2) + 1.2em); color: var(--ink); text-decoration: none; }
  .session:hover { background: var(--bg-hover); }
  .session.active { background: var(--bg-active); }
  .name { flex: 1; min-width: 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .time { color: var(--ink-3); font-size: var(--text-xs); }
  .time.waiting { color: var(--ink); font-weight: 500; }
  .empty { margin: var(--space-2) var(--space-1); font-size: var(--text-sm); }
  .sidebar :global(.spinner) { flex: none; color: var(--accent); }
  .session :global(.spinner) { position: absolute; left: var(--space-2); }
  .workspace :global(.icon-btn), .session :global(.icon-btn) { display: none; width: 24px; height: 24px; font-size: 14px; font-weight: 400; }
  .workspace :global(.icon-btn.armed), .session :global(.icon-btn.armed) { width: auto; padding: 0 var(--space-2); font-size: var(--text-xs); }
  .workspace:hover :global(.icon-btn), .session:hover :global(.icon-btn) { display: inline-grid; }
  .session:hover .time { display: none; }
  footer { padding-top: var(--space-2); }
</style>
