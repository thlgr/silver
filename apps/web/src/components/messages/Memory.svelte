<!-- What every bot and external harness in this workspace remembers, read from the ai-memory
     server. Read-only: the agents write memory themselves, this only shows it. -->
<script>
  import { api } from '../../lib/api.js'
  import IconClose from '~icons/lucide/chevrons-right'

  let { workspaceId, onclose } = $props()

  let view = $state(null) // MemoryView once it loads
  let error = $state(null) // memory off, or the server unreachable
  let loaded = $state(false)
  let query = $state('')
  let open = $state(null) // the page being read, body included
  let timer

  async function load(text) {
    loaded = true
    error = null
    open = null
    try {
      view = await api(`/v1/workspaces/${workspaceId}/memory`, { query: { q: text.trim() || undefined } })
    } catch (failure) {
      view = null
      error = failure.message ?? String(failure)
    }
  }

  async function read(path) {
    try {
      open = await api(`/v1/workspaces/${workspaceId}/memory/page`, { query: { path } })
    } catch (failure) {
      error = failure.message ?? String(failure)
    }
  }

  // Filter without a request per keystroke.
  function search() {
    clearTimeout(timer)
    timer = setTimeout(() => load(query), 300)
  }

  // A different workspace is a different memory.
  $effect(() => {
    void workspaceId
    query = ''
    load('')
  })
</script>

<aside class="memory">
  <header>
    <strong>Memory</strong>
    <span class="end">
      <button type="button" class="round press" title="Close memory" aria-label="Close memory" onclick={onclose}><IconClose /></button>
    </span>
  </header>

  <div class="find">
    <input type="search" placeholder="Search this workspace's memory" aria-label="Search memory" bind:value={query} oninput={search} />
  </div>

  <div class="scroll">
    {#if !loaded}
      <p class="muted">Loading…</p>
    {:else if error}
      <p class="muted">{error}</p>
    {:else if open}
      <button type="button" class="back press" onclick={() => (open = null)}>← All pages</button>
      <h2>{open.title}</h2>
      <p class="meta">{open.kind} · {open.path}</p>
      <pre class="body">{open.body_markdown}</pre>
    {:else if !view?.pages.length}
      <p class="muted">{query.trim() ? 'Nothing matched.' : 'Nothing remembered here yet; the bots fill this as they work.'}</p>
    {:else}
      <p class="where">{view.workspace} / {view.project}</p>
      {#each view.pages as page (page.path)}
        <button type="button" class="row press" onclick={() => read(page.path)}>
          <span class="title">{page.title}</span>
          <span class="kind">{page.kind}</span>
        </button>
      {/each}
    {/if}
  </div>
</aside>

<style>
  .memory { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--m-bg); }
  header { display: flex; align-items: center; justify-content: space-between; flex: none; height: 54px; padding: 0 12px 0 20px; }
  header strong { font-size: 15px; }
  .end { display: flex; gap: 8px; }
  .find { flex: none; padding: 0 16px 10px; }
  .find input { width: 100%; height: 34px; padding: 0 12px; border: 1px solid var(--m-border); border-radius: 10px; background: var(--bg-hover); color: var(--m-text); font-size: var(--text-sm); }
  .find input:focus { outline: none; border-color: var(--m-secondary); }
  .scroll { flex: 1; min-height: 0; padding: 2px 12px 24px; overflow-y: auto; }
  .muted { margin: 8px 4px; color: var(--m-secondary); font-size: var(--text-sm); }
  .where { margin: 0 4px 8px; color: var(--m-tertiary); font-size: var(--text-xs); }
  .row { display: flex; align-items: center; gap: 10px; width: 100%; padding: 9px 8px; border: 0; border-radius: 10px; background: none; color: inherit; text-align: left; }
  .row:hover { background: var(--bg-hover); }
  .row .title { flex: 1; min-width: 0; overflow: hidden; font-size: 13px; text-overflow: ellipsis; white-space: nowrap; }
  .row .kind { flex: none; padding: 1px 7px; border-radius: 999px; background: var(--bg-hover); color: var(--m-tertiary); font-size: 10px; letter-spacing: 0.04em; text-transform: uppercase; }
  .back { margin: 0 0 6px 4px; padding: 4px 0; border: 0; background: none; color: var(--m-secondary); font-size: var(--text-xs); }
  h2 { margin: 0 4px 2px; font-size: 15px; }
  .meta { margin: 0 4px 12px; color: var(--m-tertiary); font-size: var(--text-xs); }
  .body { margin: 0; padding: 0 4px; color: var(--m-text); font-family: ui-monospace, monospace; font-size: 12px; line-height: 1.5; overflow-wrap: anywhere; white-space: pre-wrap; }
</style>
