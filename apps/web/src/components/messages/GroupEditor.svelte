<!-- Create a group chat or change one: a name, what the room is for, which bots are in it, and the
     workspace it is filed under. -->
<script>
  import { app } from '../../lib/state.svelte.js'
  import { chat, deleteBot, saveBot, select } from '../../lib/chat.svelte.js'
  import Avatar from './Avatar.svelte'
  import Choice from './Choice.svelte'
  import Dialog from './Dialog.svelte'
  import Sheet from './Sheet.svelte'
  import IconCheck from '~icons/lucide/check'

  let { group = null, onclose } = $props()
  const agents = $derived(chat.bots.filter((bot) => bot.kind === 'agent'))
  const workspaces = $derived([{ value: '', label: 'None' }, ...app.workspaces.map((w) => ({ value: w.id, label: w.name }))])
  // The form starts from the bot it was opened with and is not re-seeded while open.
  // svelte-ignore state_referenced_locally
  const draft = $state({ name: group?.name ?? '', description: group?.description ?? '', members: [...(group?.members ?? [])], workspace_id: group ? (group.workspace_id ?? '') : (app.scope ?? app.workspaces[0]?.id ?? '') })
  let saving = $state(false)
  let ask = $state(null)
  const valid = $derived(draft.name.trim() && draft.members.length > 0)

  const toggle = (id) => (draft.members = draft.members.includes(id) ? draft.members.filter((m) => m !== id) : [...draft.members, id])

  async function save() {
    if (!valid || saving) return
    saving = true
    // A new group may carry no workspace ("" would not parse); an existing one sends "" to clear it.
    const workspace_id = group ? draft.workspace_id : draft.workspace_id || undefined
    const saved = await saveBot(group?.id ?? null, { ...draft, name: draft.name.trim(), kind: group ? undefined : 'group', workspace_id })
    saving = false
    if (!saved) return
    if (!group) select(saved.id)
    onclose()
  }
</script>

<Sheet title={group ? 'Edit group' : 'New group chat'} width={420} {onclose}>
  {#snippet actions()}
    <button type="button" class="round press save" class:ready={valid} disabled={!valid || saving} title={group ? 'Save' : 'Create'} aria-label={group ? 'Save' : 'Create'} onclick={save}><IconCheck /></button>
  {/snippet}

  <h3 class="card-title">Group</h3>
  <div class="card">
    <label class="row"><span>Name</span><input placeholder="e.g. Release crew" maxlength="60" bind:value={draft.name} /></label>
    <label class="col"><span>About</span><textarea rows="2" placeholder="What this room is for. Its bots read it." bind:value={draft.description}></textarea></label>
    <Choice label="Workspace" value={draft.workspace_id} options={workspaces} onchange={(id) => (draft.workspace_id = id)} />
  </div>

  <h3 class="card-title">Bots</h3>
  <div class="card">
    {#each agents as bot (bot.id)}
      <button type="button" class="member press" role="checkbox" aria-checked={draft.members.includes(bot.id)} onclick={() => toggle(bot.id)}>
        <Avatar shape={bot.avatar_shape} color={bot.avatar_color} size={30} />
        <span>{bot.name}</span>
        <span class="tick" class:on={draft.members.includes(bot.id)}>{#if draft.members.includes(bot.id)}<IconCheck />{/if}</span>
      </button>
    {:else}
      <p class="empty">Create a bot first.</p>
    {/each}
  </div>
  <p class="card-note">Everyone answers in turn. @mention a bot to ask just that one.</p>

  {#if group}
    <button type="button" class="pill danger delete press" onclick={() => (ask = { title: `Delete ${group.name}?`, message: 'The group and its messages go; its bots stay.', danger: true, action: { label: 'Delete', run: async () => (await deleteBot(group.id), onclose()) } })}>Delete group</button>
  {/if}
</Sheet>

{#if ask}<Dialog {...ask} onclose={() => (ask = null)} />{/if}

<style>
  .row, .col { display: flex; align-items: center; gap: 12px; padding: 0 14px; min-height: 44px; font-size: var(--text-md); }
  .col { flex-direction: column; align-items: stretch; gap: 4px; padding: 10px 14px; }
  .row > span:first-child { flex: none; width: 70px; color: var(--m-secondary); }
  .col > span { color: var(--m-secondary); font-size: var(--text-xs); }
  input, textarea { flex: 1; min-width: 0; border: 0; outline: 0; background: none; color: var(--m-text); font: inherit; }
  .row input { text-align: right; }
  textarea { resize: vertical; line-height: 1.45; }
  input::placeholder, textarea::placeholder { color: var(--m-tertiary); }
  .member { display: flex; align-items: center; gap: 12px; width: 100%; min-height: 48px; padding: 0 14px; border: 0; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .member:hover { background: var(--bg-hover); }
  .member > span:nth-child(2) { flex: 1; }
  .tick { display: grid; place-items: center; width: 20px; height: 20px; border-radius: 50%; box-shadow: inset 0 0 0 1.5px var(--m-dim); }
  .tick.on { background: var(--m-fill); color: var(--m-on-fill); box-shadow: none; }
  .tick :global(svg) { width: 12px; height: 12px; stroke-width: 3; }
  .empty { margin: 0; padding: 16px; color: var(--m-tertiary); text-align: center; }
  .save { background: var(--m-dim); color: var(--m-tertiary); }
  .save.ready { background: var(--m-fill); color: var(--m-on-fill); }
  .delete { display: flex; margin: 24px auto 0; }
</style>
