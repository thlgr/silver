<!-- The roster: bots under the workspace they work in, then those without one, then groups; in
     each, pinned on top, then the newest conversation. -->
<script>
  import { app } from '../../lib/state.svelte.js'
  import { chat, deleteBot, markRead, newSession, roster, select, setPinned } from '../../lib/chat.svelte.js'
  import BotRow from './BotRow.svelte'
  import Menu from './Menu.svelte'
  import Dialog from './Dialog.svelte'
  import IconSearch from '~icons/lucide/search'
  import IconPlus from '~icons/lucide/plus'
  import IconPin from '~icons/lucide/pin'
  import IconPinOff from '~icons/lucide/pin-off'
  import IconRead from '~icons/lucide/check-check'
  import IconEdit from '~icons/lucide/pencil'
  import IconReset from '~icons/lucide/rotate-ccw'
  import IconTrash from '~icons/lucide/trash-2'
  import IconBot from '~icons/lucide/bot'
  import IconMessage from '~icons/lucide/square-pen'
  import IconUsers from '~icons/lucide/users'
  import IconTerminal from '~icons/lucide/terminal'
  import IconSettings from '~icons/lucide/settings'
  import IconFolder from '~icons/lucide/folder'
  import IconFolderX from '~icons/lucide/folder-x'

  let menu = $state(null) // { x, y, items }
  let ask = $state(null) // a Dialog's props
  // Workspaces keep the workbench's order. One without bots still shows, so a bot can be started
  // there, except while searching.
  const sections = $derived.by(() => {
    const all = roster()
    const agents = all.filter((bot) => bot.kind === 'agent')
    const searching = chat.query.trim() !== ''
    return [
      ...app.workspaces.map((w) => ({ key: w.id, name: w.name, path: w.path, icon: IconFolder, add: `New bot in ${w.name}`, editor: { bot: null, workspace: w.id }, bots: agents.filter((bot) => bot.workspace_id === w.id) })),
      { key: 'none', name: 'No workspace', icon: IconFolderX, add: 'New bot without a workspace', editor: { bot: null, workspace: '' }, bots: agents.filter((bot) => !app.workspaces.some((w) => w.id === bot.workspace_id)) },
      { key: 'groups', name: 'Groups', icon: IconUsers, add: 'New group chat', editor: { group: null }, bots: all.filter((bot) => bot.kind === 'group') },
    ].filter((section) => section.bots.length || (section.path && !searching))
  })
  const rows = $derived(sections.flatMap((section) => section.bots))

  function botMenu(event, bot) {
    const group = bot.kind === 'group'
    const items = [
      { label: bot.pinned ? 'Unpin' : 'Pin', icon: bot.pinned ? IconPinOff : IconPin, run: () => setPinned(bot) },
      ...(bot.unread ? [{ label: 'Mark as read', icon: IconRead, run: () => markRead(bot.id) }] : []),
      { label: group ? 'Edit group' : 'Edit bot', icon: IconEdit, run: () => (chat.editor = group ? { group: bot } : { bot }) },
      ...(group ? [] : [{
        label: 'New session',
        icon: IconReset,
        run: () => (ask = { title: 'Start a new session?', message: 'The conversation stays here, but the bot starts with a fresh context.', action: { label: 'New session', run: () => newSession(bot.id) } }),
      }]),
      {
        label: group ? 'Delete group' : 'Delete bot',
        icon: IconTrash,
        danger: true,
        rule: true,
        run: () => (ask = { title: `Delete ${bot.name}?`, message: group ? 'The group and its messages go; its bots stay.' : 'The bot, its conversation and its sessions go. This cannot be undone.', danger: true, action: { label: 'Delete', run: () => deleteBot(bot.id) } }),
      },
    ]
    menu = { x: event.clientX, y: event.clientY, items }
  }

  function newMenu(event) {
    const box = event.currentTarget.getBoundingClientRect()
    menu = {
      x: box.left,
      y: box.bottom + 6,
      items: [
        { label: 'New message', icon: IconMessage, run: () => (chat.composing = true) },
        { label: 'New bot', icon: IconBot, run: () => (chat.editor = { bot: null }) },
        { label: 'New group chat', icon: IconUsers, run: () => (chat.editor = { group: null }) },
      ],
    }
  }

  // Up and down walk the list, like the Mac roster.
  function keys(event) {
    if (event.target.matches('input') || !['ArrowDown', 'ArrowUp'].includes(event.key) || !rows.length) return
    event.preventDefault()
    const at = rows.findIndex((bot) => bot.id === chat.selected)
    select(rows[Math.max(0, Math.min(rows.length - 1, at + (event.key === 'ArrowDown' ? 1 : -1)))].id)
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside class="roster" onkeydown={keys}>
  <header>
    <label class="search">
      <IconSearch />
      <input type="search" placeholder="Search" aria-label="Search bots" bind:value={chat.query} />
    </label>
    <button type="button" class="round press" title="New" aria-label="New" onclick={newMenu}><IconPlus /></button>
  </header>

  <div class="list">
    {#each sections as section (section.key)}
      <div class="section" class:empty={!section.bots.length} title={section.path}>
        <section.icon />
        <span class="name">{section.name}</span>
        <button type="button" class="add press" title={section.add} aria-label={section.add} onclick={() => (chat.editor = section.editor)}><IconPlus /></button>
      </div>
      {#each section.bots as bot (bot.id)}
        <BotRow {bot} selected={bot.id === chat.selected} onselect={() => select(bot.id)} oncontext={(e) => botMenu(e, bot)} />
      {/each}
    {/each}
    {#if chat.loaded && !rows.length}
      <div class="none">
        <strong>{chat.query ? 'No matching bots' : 'No bots yet'}</strong>
        <span>{chat.query ? 'Try another name or message.' : 'Use + to start a new chat.'}</span>
      </div>
    {/if}
  </div>

  <footer>
    <button type="button" class="foot press" onclick={() => (app.settings.messaging = false)}><IconTerminal /> Workbench</button>
    <button type="button" class="round press" title="Settings" aria-label="Settings" onclick={() => (app.settingsTab = 'general')}><IconSettings /></button>
  </footer>
</aside>

{#if menu}<Menu {...menu} onclose={() => (menu = null)} />{/if}
{#if ask}<Dialog {...ask} onclose={() => (ask = null)} />{/if}

<style>
  .roster { display: flex; flex-direction: column; min-height: 0; background: var(--m-surface); }
  header { display: flex; align-items: center; gap: 8px; flex: none; padding: 12px 12px 8px; }
  .search { display: flex; align-items: center; flex: 1; gap: 8px; min-width: 0; height: 34px; padding: 0 12px; border-radius: 999px; background: var(--m-bg); color: var(--m-tertiary); }
  .search :global(svg) { flex: none; width: 15px; height: 15px; }
  .search input { flex: 1; min-width: 0; height: 100%; padding: 0; border: 0; outline: 0; background: none; color: var(--m-text); font-size: var(--text-md); }
  .search input::placeholder { color: var(--m-tertiary); }
  .search input::-webkit-search-cancel-button { display: none; }
  header .round { background: var(--m-bg); }
  .list { display: flex; flex-direction: column; gap: 2px; flex: 1; min-height: 0; padding: 4px 8px; overflow-y: auto; }
  .section { display: flex; align-items: center; gap: 6px; padding: 12px 6px 2px 10px; color: var(--m-secondary); font-size: var(--text-xs); font-weight: 600; }
  .section:first-child { padding-top: 2px; }
  .section.empty { color: var(--m-tertiary); }
  .section > :global(svg) { flex: none; width: 13px; height: 13px; }
  .section .name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .add { display: grid; flex: none; place-items: center; width: 22px; height: 22px; padding: 0; border: 0; border-radius: 50%; background: none; color: var(--m-tertiary); }
  .add:hover { background: var(--bg-hover); color: var(--m-text); }
  @media (hover: hover) { .add { opacity: 0; } .section:hover .add, .add:focus-visible { opacity: 1; } }
  .add :global(svg) { width: 13px; height: 13px; }
  .none { display: grid; gap: 4px; margin: auto; padding: 24px; text-align: center; }
  .none strong { font-size: var(--text-md); font-weight: 600; }
  .none span { color: var(--m-secondary); font-size: var(--text-sm); }
  footer { display: flex; align-items: center; gap: 8px; flex: none; padding: 10px 12px 12px; }
  .foot { display: inline-flex; align-items: center; gap: 8px; height: 34px; padding: 0 12px; border: 0; border-radius: 999px; background: none; color: var(--m-secondary); font-size: var(--text-sm); }
  .foot:hover { background: var(--bg-hover); color: var(--m-text); }
  .foot :global(svg) { width: 15px; height: 15px; }
  footer .round { margin-left: auto; background: none; color: var(--m-secondary); }
</style>
