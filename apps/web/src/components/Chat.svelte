<script>
  import { app, currentWorkspace, newSession, updateSession, deleteSession, rewind, download, transcriptMarkdown } from '../lib/state.svelte.js'
  import { runCommand } from '../lib/commands.js'
  import { sessionLabel } from '../lib/format.js'
  import Popover from './Popover.svelte'
  import ConfirmButton from './ConfirmButton.svelte'
  import Composer from './Composer.svelte'
  import Turn from './Turn.svelte'
  import IconPanelLeft from '~icons/lucide/panel-left'
  import IconPanelRight from '~icons/lucide/panel-right'
  import IconMore from '~icons/lucide/ellipsis'
  import IconFolder from '~icons/lucide/folder'
  import IconDown from '~icons/lucide/chevron-down'
  import IconCheck from '~icons/lucide/check'
  import IconMessageSquare from '~icons/lucide/message-square'

  let { sidebarOpen, onExpand } = $props()
  let scroller = $state()
  let content = $state()
  let pinned = true
  let editing = $state(false)

  const workspace = $derived(currentWorkspace())
  const empty = $derived(!app.session && !app.turns.length)
  const lastTurn = $derived(app.turns.findLast((t) => !t.note))
  const needsModel = $derived(app.providers.length > 0 && !app.providers.some((p) => p.authenticated || p.configured))

  // Follow new output while the reader is at the bottom. Scroll events arrive a frame late, when
  // more output may already have landed, so only moving above our own last position unpins.
  let followed = 0
  $effect(() => {
    if (!content) return
    const follow = () => {
      if (!pinned) return
      scroller.scrollTo({ top: scroller.scrollHeight })
      followed = scroller.scrollTop
    }
    const observer = new ResizeObserver(follow)
    observer.observe(content)
    observer.observe(scroller)
    return () => observer.disconnect()
  })
  // Only the very bottom re-pins: any slack there would drag a reader who just started
  // scrolling up straight back down on the next chunk.
  const onscroll = () => {
    pinned = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 2 || scroller.scrollTop >= followed - 2
  }
  // A new prompt, a command's output or another session always starts at the bottom.
  $effect(() => {
    app.turns.length
    pinned = true
  })

  // An automatic title is the first prompt clipped with "…"; renaming starts from the whole prompt.
  function fullTitle() {
    const title = app.session.title ?? ''
    const prompt = app.turns.find((t) => t.user)?.user.split(/\s+/).join(' ')
    return title.endsWith('…') && prompt?.startsWith(title.slice(0, -1)) ? prompt : title
  }

  const selectAll = (node) => node.select()

  function rename(e) {
    editing = false
    const title = e.target.value.trim()
    if (title && title !== e.target.defaultValue) updateSession({ title })
  }
</script>

<main class="chat" class:empty>
  <header>
    {#if !sidebarOpen}
      <button class="icon-btn" title="Expand sidebar" aria-label="Expand sidebar" onclick={onExpand}><IconPanelLeft /></button>
    {/if}
    {#if app.session}
      {#if editing}
        <!-- svelte-ignore a11y_autofocus -->
        <input class="field title-input" value={fullTitle()} autofocus use:selectAll onblur={rename} onkeydown={(e) => e.key === 'Enter' && e.target.blur()} />
      {:else}
        <button class="title" title="Rename" onclick={() => (editing = true)}>{sessionLabel(app.session)}</button>
      {/if}
      <span class="muted">{workspace?.name ?? 'No workspace'}</span>
    {/if}
    <span class="spacer"></span>
    {#if app.session}
      <button class="icon-btn" title="Workspace panel" aria-label="Workspace panel" aria-expanded={!!app.panel} onclick={() => (app.panel = app.panel ? null : 'changes')}><IconPanelRight /></button>
      <Popover class="icon-btn" label="More" right>
        {#snippet trigger()}<IconMore />{/snippet}
        {#snippet children(close)}
          <button class="menu-item" onclick={() => (editing = true, close())}>Rename</button>
          <button class="menu-item" onclick={() => (runCommand('/copy'), close())}>Copy last reply</button>
          <button class="menu-item" onclick={() => (download(`${app.session.title || 'silver'}.md`, transcriptMarkdown()), close())}>Export as Markdown</button>
          <button class="menu-item" disabled={!!app.run} onclick={() => (rewind(1), close())}>Undo last turn</button>
          <div class="menu-rule"></div>
          <ConfirmButton class="menu-item error" ask="Delete this session and its history?" onconfirm={() => (deleteSession(app.session.id), close())}>Delete session</ConfirmButton>
        {/snippet}
      </Popover>
    {/if}
    <button class="icon-btn" title="Messages" aria-label="Messages" onclick={() => (app.settings.messaging = true)}><IconMessageSquare /></button>
  </header>

  {#if empty}
    <div class="hero">
      <h1>{workspace?.name ?? 'silver'}</h1>
      <p class="muted">{workspace?.path ?? 'No workspace. The agent has no project files, only past sessions.'}</p>
      {#if needsModel}
        <p class="muted">Connect a model to start. <button class="link" onclick={() => (app.settingsTab = 'providers')}>Open Settings</button></p>
      {/if}
    </div>
  {:else}
    <div class="scroller" bind:this={scroller} {onscroll}>
      <div class="column transcript" bind:this={content}>
        {#each app.turns as turn (turn)}<Turn {turn} last={turn === lastTurn} />{/each}
      </div>
    </div>
  {/if}

  <div class="column bottom">
    {#if empty}
      <Popover label="Workspace">
        {#snippet trigger()}<IconFolder /> {workspace?.name ?? 'No workspace'} <IconDown />{/snippet}
        {#snippet children(close)}
          {#each [...app.workspaces, { id: null, name: 'No workspace' }] as w (w.id)}
            <button class="menu-item" onclick={() => (newSession(w.id), close())}>
              {w.name}
              {#if w.id === app.scope}<IconCheck class="hint" />{/if}
            </button>
          {/each}
        {/snippet}
      </Popover>
    {/if}
    <Composer />
  </div>
</main>

<style>
  .chat { position: relative; display: flex; flex-direction: column; height: 100vh; min-width: 0; }
  header {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: 52px;
    padding: 0 var(--space-4);
    flex: none;
  }
  .title { max-width: 50%; padding: var(--space-1) var(--space-2); border: 0; border-radius: var(--radius-sm); background: none; font-weight: 500; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .title:hover { background: var(--bg-hover); }
  .title-input { max-width: 360px; height: 30px; }
  header .muted { font-size: var(--text-sm); min-width: 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .hero .link { padding: 0; border: 0; background: none; color: var(--accent); font: inherit; text-decoration: underline; }
  .spacer { flex: 1; }

  .column { width: 100%; max-width: calc(var(--column-width) + 2 * 80px); margin: 0 auto; padding: 0 var(--space-6); }
  .scroller { flex: 1; min-height: 0; overflow-y: auto; }
  .transcript { display: flex; flex-direction: column; gap: var(--space-8); padding-top: var(--space-4); padding-bottom: var(--space-8); }
  .bottom { display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--space-2); padding-bottom: var(--space-2); }
  .bottom :global(.popover) { justify-self: start; }


  .empty { justify-content: center; }
  .empty header { position: absolute; top: 0; }
  .hero { width: 100%; max-width: calc(var(--column-width) + 2 * 80px); margin: 0 auto var(--space-6); padding: 0 var(--space-6); }
  .hero h1 { margin: 0 0 var(--space-1); font-size: var(--text-xl); font-weight: 600; letter-spacing: -0.02em; }
  .hero p { margin: 0; font-size: var(--text-sm); }
  .empty .bottom { padding-bottom: 12vh; }
</style>
