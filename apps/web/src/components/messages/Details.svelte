<!-- The inspector beside a conversation: for a bot, who it is and how it is set up; for a group,
     who is in it. -->
<script>
  import { app } from '../../lib/state.svelte.js'
  import { botById, chat, membersOf, newSession, openTrace, select, setPinned } from '../../lib/chat.svelte.js'
  import BotAvatar from './BotAvatar.svelte'
  import Dialog from './Dialog.svelte'
  import Orb from './Orb.svelte'
  import IconSettings from '~icons/lucide/settings'
  import IconClose from '~icons/lucide/chevrons-right'
  import IconList from '~icons/lucide/list'
  import IconReset from '~icons/lucide/rotate-ccw'
  import IconPin from '~icons/lucide/pin'
  import IconPinOff from '~icons/lucide/pin-off'

  let { botId, onclose } = $props()
  const bot = $derived(botById(botId))
  const group = $derived(bot.kind === 'group')
  const workspace = $derived(app.workspaces.find((w) => w.id === bot.workspace_id))
  const provider = $derived(app.providers.find((p) => p.id === bot.provider))
  let ask = $state(null)
  const edit = () => (chat.editor = group ? { group: bot } : { bot })
</script>

<aside class="details">
  <header>
    <strong>{group ? 'Members' : 'Details'}</strong>
    <span class="end">
      <button type="button" class="round press" title={group ? 'Edit group' : 'Bot settings'} aria-label={group ? 'Edit group' : 'Bot settings'} onclick={edit}><IconSettings /></button>
      <button type="button" class="round press" title="Close details" aria-label="Close details" onclick={onclose}><IconClose /></button>
    </span>
  </header>

  <div class="scroll">
    {#if group}
      <p class="count">{bot.members.length} {bot.members.length === 1 ? 'bot' : 'bots'}</p>
      {#each membersOf(bot) as member (member.id)}
        <button type="button" class="member press" title="Open {member.name}'s own chat" onclick={() => select(member.id)}>
          <BotAvatar bot={member} size={30} badge={false} />
          <span class="who">
            <strong>{member.name}</strong>
            <small>
              {#if member.status === 'working'}<Orb size={12} /> {member.activity || 'Working…'}
              {:else}{app.workspaces.find((w) => w.id === member.workspace_id)?.name ?? 'No workspace'}{/if}
            </small>
          </span>
        </button>
      {/each}
      {#if bot.description}<p class="about">{bot.description}</p>{/if}
    {:else}
      <div class="who-card">
        <BotAvatar {bot} size={72} badge={false} />
        <h2>{bot.name}</h2>
        {#if bot.description}<p>{bot.description}</p>{/if}
      </div>

      <h3 class="card-title">Setup</h3>
      <div class="card info">
        <div class="where"><span>Workspace</span><b>{workspace?.name ?? 'None'}</b>{#if workspace}<small>{workspace.path}</small>{/if}</div>
        <div><span>Agent</span><b>{provider?.label ?? bot.provider ?? 'Default'}</b></div>
        <div><span>Model</span><b title={bot.model}>{bot.model || 'Default'}</b></div>
        <div><span>Permissions</span><b>{bot.yolo ? 'Approve automatically' : 'Ask me'}</b></div>
      </div>
      {#if !workspace}<p class="card-note">No workspace: it can chat and remember, but not read files or run commands.</p>{/if}

      <h3 class="card-title">Actions</h3>
      <div class="card actions">
        <button type="button" class="press" onclick={() => openTrace(bot.id)}><IconList /> Full conversation</button>
        <button type="button" class="press" onclick={() => setPinned(bot)}>{#if bot.pinned}<IconPinOff /> Unpin{:else}<IconPin /> Pin{/if}</button>
        <button type="button" class="press" onclick={() => (ask = { title: 'Start a new session?', message: 'The conversation stays here, but the bot starts with a fresh context.', action: { label: 'New session', run: () => newSession(bot.id) } })}><IconReset /> New session</button>
      </div>
    {/if}
  </div>
</aside>

{#if ask}<Dialog {...ask} onclose={() => (ask = null)} />{/if}

<style>
  .details { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--m-bg); }
  header { display: flex; align-items: center; justify-content: space-between; flex: none; height: 54px; padding: 0 12px 0 20px; }
  header strong { font-size: 15px; }
  .end { display: flex; gap: 8px; }
  .scroll { flex: 1; min-height: 0; padding: 4px 16px 24px; overflow-y: auto; }
  .who-card { display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 8px 0 22px; text-align: center; }
  .who-card h2 { margin: 6px 0 0; font-size: 18px; font-weight: 650; }
  .who-card p { margin: 0; color: var(--m-secondary); font-size: var(--text-sm); }
  .info > div { display: flex; justify-content: space-between; gap: 12px; padding: 10px 14px; font-size: var(--text-sm); }
  .info span { flex: none; color: var(--m-secondary); }
  .info b { min-width: 0; overflow: hidden; font-weight: 400; text-overflow: ellipsis; white-space: nowrap; }
  .where { flex-wrap: wrap; row-gap: 2px; }
  .where small { flex-basis: 100%; color: var(--m-tertiary); font-size: var(--text-xs); overflow-wrap: anywhere; }
  .actions button { display: flex; align-items: center; gap: 10px; width: 100%; min-height: 40px; padding: 0 14px; border: 0; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .actions button:hover { background: var(--bg-hover); }
  .actions :global(svg) { width: 15px; height: 15px; color: var(--m-secondary); }
  .count { margin: 4px 4px 8px; font-size: 13px; font-weight: 600; }
  .member { display: flex; align-items: center; gap: 10px; width: 100%; padding: 6px 4px; border: 0; border-radius: 10px; background: none; color: inherit; text-align: left; }
  .member:hover { background: var(--bg-hover); }
  .who { display: flex; flex-direction: column; min-width: 0; }
  .who strong { font-size: 13px; font-weight: 500; }
  .who small { display: flex; align-items: center; gap: 6px; overflow: hidden; color: var(--m-tertiary); font-size: 11px; text-overflow: ellipsis; white-space: nowrap; }
  .about { margin: 16px 4px 0; color: var(--m-secondary); font-size: var(--text-sm); }
</style>
