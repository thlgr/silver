<!-- Provider → model menu. Providers set up in Settings only; picking one on another provider switches it.
     The search box filters a long catalog and takes any model id, listed or not. -->
<script>
  import { app, listModels, setEffort, setModel } from '../lib/state.svelte.js'
  import Popover from './Popover.svelte'
  import IconDown from '~icons/lucide/chevron-down'
  import IconRight from '~icons/lucide/chevron-right'
  import IconLeft from '~icons/lucide/chevron-left'
  import IconUp from '~icons/lucide/chevron-up'
  import IconCheck from '~icons/lucide/check'
  import IconSearch from '~icons/lucide/search'

  let provider = $state(null)
  let catalog = $state(null)
  let error = $state('')
  let searching = $state(false)
  let query = $state('')
  let effortOpen = $state(false)

  // Default plus the daemon's accepted levels, weakest to strongest.
  const EFFORTS = [
    { value: null, label: 'Default' },
    { value: 'none', label: 'None' },
    { value: 'minimal', label: 'Minimal' },
    { value: 'low', label: 'Low' },
    { value: 'medium', label: 'Medium' },
    { value: 'high', label: 'High' },
    { value: 'xhigh', label: 'X-High' },
    { value: 'max', label: 'Max' },
  ]
  // The daemon's fallback set, shown when the catalog has nothing for the model.
  const DEFAULT_EFFORTS = ['none', 'minimal', 'low', 'medium', 'high']
  // The selected model's supported levels, from /v1/models efforts, so the menu never offers a
  // level the model cannot honor (it would be clamped to the nearest lower one on the run).
  const modelEfforts = $derived(
    catalog?.efforts?.[app.model] ?? catalog?.efforts?.[app.defaultModel] ?? DEFAULT_EFFORTS,
  )
  const effortOptions = $derived(EFFORTS.filter((level) => level.value === null || modelEfforts.includes(level.value)))
  const effortLabel = $derived(
    app.effort ?? (app.autoEffort ? `Default (${app.autoEffort})` : 'Default'),
  )

  function pickEffort(level) {
    const already = (level.value ?? null) === app.effort
    setEffort(level.value)
    if (already) effortOpen = false
  }

  // Same rule as the TUI: hide embedding and reranker models.
  const chat = $derived(
    (catalog?.models ?? []).filter((id) => {
      const kind = catalog.details?.find((d) => d.id === id)?.kind?.toLowerCase()
      return kind ? !['embedding', 'embeddings', 'reranker'].includes(kind) : !/embed|rerank/i.test(id)
    }),
  )
  const typed = $derived(query.trim())
  const shown = $derived(typed ? chat.filter((id) => id.toLowerCase().includes(typed.toLowerCase())) : chat)
  // With nothing to pick from, typing an id is the only way forward, so the box starts open.
  const search = $derived(searching || !!error || (catalog && !chat.length))
  const usable = $derived(app.providers.filter((p) => p.configured || p.active || p.id === app.activeProvider))
  const label = $derived(app.providers.find((p) => p.id === provider)?.label ?? provider)
  const loaded = (id) => catalog?.details?.some((d) => d.id === id && d.loaded)

  async function browse(id) {
    provider = id
    catalog = null
    error = ''
    searching = false
    query = ''
    try {
      catalog = await listModels(id)
    } catch (e) {
      error = e.message
    }
  }

  function pick(model) {
    setModel(model, provider)
    // Stay open: the effort selector sits at the bottom of this same menu, so the user can
    // set the model and the effort in one visit instead of reopening and drilling down again.
  }

  // Enter takes the first row: the best match, or the typed id when nothing matches.
  function enter(e) {
    if (e.key === 'Enter' && typed) pick(shown[0] ?? typed)
  }

  // The menu opens upward, so hold its height while the list filters: the box stays under the cursor.
  const focus = (node) => {
    const menu = node.closest('.menu')
    menu.style.minHeight = `${menu.offsetHeight}px`
    node.focus()
    return { destroy: () => (menu.style.minHeight = '') }
  }
</script>

<Popover label="Model" up right data-model-picker>
  {#snippet trigger()}<span class="label">{app.model ?? 'Model'}</span><span class="effort-tag"><span class="dot">·</span>{effortLabel}</span> <IconDown />{/snippet}
  {#snippet children(close)}
    {#if !provider}
      <div class="menu-label">Provider</div>
      {#each usable as p (p.id)}
        <button class="menu-item" onclick={() => browse(p.id)}>
          {p.label || p.id}
          <span class="hint">{p.id === app.activeProvider ? 'Active' : ''}</span>
          <IconRight />
        </button>
      {/each}
      <div class="menu-rule"></div>
      <button class="menu-item" onclick={() => ((app.settingsTab = 'providers'), close())}>Add a provider…</button>
    {:else}
      <div class="head">
        <button class="menu-item back" aria-label="Back to providers" onclick={() => (provider = null)}>
          <IconLeft />{#if !search}{label}{/if}
        </button>
        {#if search}
          <input class="field" bind:value={query} placeholder="Search or type a model id" aria-label="Model id" use:focus onkeydown={(e) => enter(e)} />
        {:else}
          <button class="icon-btn" title="Search models" aria-label="Search models" onclick={() => (searching = true)}><IconSearch /></button>
        {/if}
      </div>
      <div class="menu-rule"></div>
      {#if error}<p class="empty error">{error}</p>{/if}
      {#if !catalog && !error}<p class="empty">Loading models</p>{/if}
      {#if catalog && !chat.length && !typed}<p class="empty">This provider lists no models. Type an id above.</p>{/if}
      {#each shown as model (model)}
        <button class="menu-item" onclick={() => pick(model)}>
          {model}
          {#if loaded(model)}<span class="hint">loaded</span>{/if}
          {#if model === app.model && provider === app.activeProvider}<IconCheck class="hint" />{/if}
        </button>
      {/each}
      {#if typed && !chat.includes(typed)}
        <button class="menu-item" onclick={() => pick(typed)}>Use “{typed}”<span class="hint">custom</span></button>
      {/if}
      <div class="effort">
        <button class="menu-item bar" aria-expanded={effortOpen} onclick={() => (effortOpen = !effortOpen)}>
          Effort
          <span class="hint">{effortLabel}</span>
          {#if effortOpen}<IconUp class="hint" />{:else}<IconDown class="hint" />{/if}
        </button>
        {#if effortOpen}
          {#each effortOptions as level (level.value)}
            <button class="menu-item" onclick={() => pickEffort(level)}>
              {level.label}
              {#if level.value === null && app.autoEffort}<span class="hint">({app.autoEffort})</span>{/if}
              {#if app.effort === level.value}<IconCheck class="hint" />{/if}
            </button>
          {/each}
        {/if}
      </div>
    {/if}
  {/snippet}
</Popover>

<style>
  .effort-tag .dot { margin: 0 var(--space-1); color: var(--ink-3); }
  .effort-tag { color: var(--ink-3); }
  .empty { margin: 0; padding: var(--space-2); color: var(--ink-3); font-size: var(--text-sm); }
  .back { color: var(--ink-2); }
  .menu-item :global(.hint + .hint) { margin-left: 0; }
  /* A fixed width, so filtering never squeezes the search box or long model ids. */
  :global(.menu:has(> .head)) { width: min(320px, 90vw); }
  /* Pinned so the search box stays in reach while a long catalog scrolls under it. */
  .head { position: sticky; top: calc(-1 * var(--space-1)); z-index: 1; display: flex; align-items: center; gap: var(--space-1); background: var(--bg-raised); }
  .head .back { width: auto; flex: 1; }
  .head:has(input) .back { flex: none; }
  .head .field { flex: 1; min-width: 0; height: 30px; }
  /* Pinned so effort stays in reach while a long catalog scrolls under it. */
  .effort { position: sticky; bottom: calc(-1 * var(--space-1)); z-index: 1; margin: var(--space-1) calc(-1 * var(--space-1)) calc(-1 * var(--space-1)); padding: 0 var(--space-1); background: var(--bg-raised); border-top: 1px solid var(--line); }
  .effort .bar { margin-top: var(--space-1); }
</style>
