<!-- Create a bot or change one: its face, what it is for, which agent runs it and where, and
     whether it asks before acting. Sections use Codync's card look. -->
<script>
  import { addWorkspace, app, listModels } from '../../lib/state.svelte.js'
  import { api } from '../../lib/api.js'
  import { COLORS, SHAPES } from '../../lib/avatar.js'
  import { deleteBot, saveBot, select } from '../../lib/chat.svelte.js'
  import Avatar from './Avatar.svelte'
  import Choice from './Choice.svelte'
  import Combo from './Combo.svelte'
  import Dialog from './Dialog.svelte'
  import Sheet from './Sheet.svelte'
  import IconCheck from '~icons/lucide/check'
  import IconShuffle from '~icons/lucide/shuffle'
  import IconFolderPlus from '~icons/lucide/folder-plus'

  /** `workspace`: where a new bot starts, '' for none; absent, the workbench's current one. */
  let { bot = null, workspace, onclose } = $props()
  const pick = (list) => list[Math.floor(Math.random() * list.length)]
  // The form starts from the bot it was opened with and is not re-seeded while open.
  // svelte-ignore state_referenced_locally
  const draft = $state({
    name: bot?.name ?? '',
    description: bot?.description ?? '',
    instructions: bot?.instructions ?? '',
    avatar_shape: bot?.avatar_shape ?? pick(SHAPES),
    avatar_color: bot?.avatar_color ?? pick(COLORS).id,
    provider: bot?.provider ?? '',
    model: bot?.model ?? '',
    workspace_id: bot ? (bot.workspace_id ?? '') : (workspace ?? app.scope ?? app.workspaces[0]?.id ?? ''),
    yolo: bot?.yolo ?? false,
  })
  let models = $state([])
  let saving = $state(false)
  let ask = $state(null)
  let problem = $state('')
  let typing = $state(false) // the folder dialog could not open, so the path is typed
  let path = $state('')
  const focus = (node) => node.focus()

  // An agent whose CLI is installed here is there to pick without being set up first.
  const usable = $derived(app.providers.filter((p) => p.configured || p.active || p.installed || p.id === bot?.provider))
  const valid = $derived(draft.name.trim().length > 0)
  const providers = $derived([
    { value: '', label: `Default${app.activeProvider ? ` (${app.providers.find((p) => p.id === app.activeProvider)?.label ?? app.activeProvider})` : ''}` },
    ...usable.map((provider) => ({ value: provider.id, label: provider.label || provider.id })),
  ])
  const workspaces = $derived([{ value: '', label: 'None' }, ...app.workspaces.map((w) => ({ value: w.id, label: w.name }))])
  const chosen = $derived(app.workspaces.find((w) => w.id === draft.workspace_id))
  // A session never changes workspace, so moving the bot starts it a fresh one.
  const moved = $derived(bot && draft.workspace_id !== (bot.workspace_id ?? ''))
  const PERMISSIONS = [
    { value: 'ask', label: 'Ask me' },
    { value: 'auto', label: 'Approve automatically' },
  ]
  const chosenProvider = $derived(draft.provider || app.activeProvider)
  // An external agent brings its own tools, so "no workspace" does not mean it cannot touch files.
  const external = $derived(app.providers.find((p) => p.id === chosenProvider)?.kind === 'acp')

  // The model suggestions follow the provider.
  $effect(() => {
    const provider = chosenProvider
    models = []
    if (provider) listModels(provider).then((catalog) => (models = catalog.models ?? [])).catch(() => {})
  })

  async function save() {
    if (!valid || saving) return
    saving = true
    const saved = await saveBot(bot?.id ?? null, {
      ...draft,
      name: draft.name.trim(),
      workspace_id: bot ? draft.workspace_id : draft.workspace_id || undefined,
    })
    saving = false
    if (!saved) return
    if (!bot) select(saved.id)
    onclose()
  }

  // A browser never reveals a folder's path, so silver opens the machine's own dialog; when it
  // cannot, the path is typed instead.
  async function addFolder(typed) {
    problem = ''
    try {
      const picked = typed ?? (await api('/v1/workspaces/pick', { method: 'POST' })).path
      if (!picked) return
      draft.workspace_id = (await addWorkspace(picked)).id
      typing = false
    } catch (e) {
      problem = e.message
      typing = true
    }
  }
</script>

<Sheet title={bot ? 'Bot settings' : 'New bot'} {onclose}>
  {#snippet actions()}
    <button type="button" class="round press save" class:ready={valid} disabled={!valid || saving} title={bot ? 'Save' : 'Create'} aria-label={bot ? 'Save' : 'Create'} onclick={save}><IconCheck /></button>
  {/snippet}

  <div class="face">
    <Avatar shape={draft.avatar_shape} color={draft.avatar_color} size={96} mood="working" />
    <div class="shapes">
      {#each SHAPES as shape (shape)}
        <button type="button" class="shape press" class:on={draft.avatar_shape === shape} title={shape} aria-label={shape} onclick={() => (draft.avatar_shape = shape)}>
          <Avatar {shape} color={draft.avatar_color} size={30} />
        </button>
      {/each}
    </div>
    <div class="colors">
      {#each COLORS as color (color.id)}
        <button type="button" class="swatch press" class:on={draft.avatar_color === color.id} style:background={color.hex} title={color.label} aria-label={color.label} onclick={() => (draft.avatar_color = color.id)}></button>
      {/each}
      <button type="button" class="swatch dice press" title="Surprise me" aria-label="Surprise me" onclick={() => ((draft.avatar_shape = pick(SHAPES)), (draft.avatar_color = pick(COLORS).id))}><IconShuffle /></button>
    </div>
  </div>

  <h3 class="card-title">Profile</h3>
  <div class="card">
    <label class="row"><span>Name</span><input placeholder="e.g. Reviewer" maxlength="60" bind:value={draft.name} /></label>
    <label class="col"><span>About</span><textarea rows="2" placeholder="e.g. Reviews PRs. Never pushes without asking." bind:value={draft.description}></textarea></label>
  </div>

  <h3 class="card-title">Setup</h3>
  <div class="card">
    <Choice label="Workspace" value={draft.workspace_id} options={workspaces} extra={[{ label: 'Add workspace…', icon: IconFolderPlus, run: () => addFolder() }]} onchange={(id) => (draft.workspace_id = id)} />
    {#if typing}
      <form class="row" onsubmit={(e) => (e.preventDefault(), addFolder(path.trim()))}><span>Path</span><input placeholder="/path/to/project" bind:value={path} use:focus /></form>
    {/if}
    <Choice label="Agent" value={draft.provider} options={providers} onchange={(id) => ((draft.provider = id), (draft.model = ''))} />
    <Combo label="Model" bind:value={draft.model} suggestions={models} placeholder="Default" />
    <Choice label="Permissions" value={draft.yolo ? 'auto' : 'ask'} options={PERMISSIONS} onchange={(id) => (draft.yolo = id === 'auto')} />
  </div>
  <p class="card-note">
    {#if problem}{problem}
    {:else if chosen}Works in <span class="path">{chosen.path}</span>.
    {:else if external}This agent runs its own tools. Without a workspace it works in the directory silver was started in, so pick one.
    {:else}Without a workspace the bot can chat, remember and ask other bots, but it cannot read or change files or run commands.{/if}
    {#if moved}It starts a new session there; this chat stays.{/if}
  </p>

  <h3 class="card-title">Instructions</h3>
  <div class="card">
    <label class="col"><textarea rows="5" placeholder="How this bot should work, in your words. It reads this on every turn." bind:value={draft.instructions}></textarea></label>
  </div>

  {#if bot}
    <button type="button" class="pill danger delete press" onclick={() => (ask = { title: `Delete ${bot.name}?`, message: 'The bot, its conversation and its sessions go. This cannot be undone.', danger: true, action: { label: 'Delete', run: async () => (await deleteBot(bot.id), onclose()) } })}>Delete bot</button>
  {/if}
</Sheet>

{#if ask}<Dialog {...ask} onclose={() => (ask = null)} />{/if}

<style>
  .face { display: flex; flex-direction: column; align-items: center; gap: 14px; padding: 8px 0 22px; }
  .shapes, .colors { display: flex; flex-wrap: wrap; justify-content: center; gap: 8px; }
  .shape { display: grid; place-items: center; width: 44px; height: 44px; padding: 0; border: 0; border-radius: 12px; background: var(--m-surface); }
  .shape.on { box-shadow: inset 0 0 0 2px var(--m-text); }
  .swatch { display: grid; place-items: center; width: 24px; height: 24px; padding: 0; border: 0; border-radius: 50%; color: var(--m-text); }
  .swatch.on { box-shadow: 0 0 0 2px var(--m-bg), 0 0 0 4px var(--m-text); }
  .swatch.dice { background: var(--m-surface); }
  .swatch.dice :global(svg) { width: 13px; height: 13px; }
  .path { color: var(--m-secondary); overflow-wrap: anywhere; }
  .row, .col { display: flex; align-items: center; gap: 12px; padding: 0 14px; min-height: 44px; font-size: var(--text-md); }
  .col { flex-direction: column; align-items: stretch; gap: 4px; padding: 10px 14px; }
  .row > span:first-child, .col > span { flex: none; width: 92px; color: var(--m-secondary); }
  .col > span { width: auto; font-size: var(--text-xs); }
  input, textarea { flex: 1; min-width: 0; border: 0; outline: 0; background: none; color: var(--m-text); font: inherit; }
  .row input { text-align: right; }
  textarea { resize: vertical; line-height: 1.45; }
  input::placeholder, textarea::placeholder { color: var(--m-tertiary); }
  .save { background: var(--m-dim); color: var(--m-tertiary); }
  .save.ready { background: var(--m-fill); color: var(--m-on-fill); }
  .delete { display: flex; margin: 24px auto 0; }
</style>
