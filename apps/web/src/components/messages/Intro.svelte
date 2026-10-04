<!-- The top of an empty chat: who this is and how it works. -->
<script>
  import { app } from '../../lib/state.svelte.js'
  import { membersOf } from '../../lib/chat.svelte.js'
  import BotAvatar from './BotAvatar.svelte'

  let { bot } = $props()
  const group = $derived(bot.kind === 'group')
  const workspace = $derived(app.workspaces.find((w) => w.id === bot.workspace_id))
</script>

<div class="intro">
  <BotAvatar {bot} size={72} badge={false} />
  <h2>{bot.name}</h2>
  {#if group}
    <p class="meta">{membersOf(bot).map((member) => member.name).join(' · ')}</p>
  {:else}
    <p class="meta code">{bot.model || 'Default model'} · {workspace?.path ?? 'no workspace'}</p>
  {/if}
  {#if bot.description}<p class="about">{bot.description}</p>{/if}
  <p class="hint">
    {#if group}Everyone answers in turn. @mention a bot to ask just that one.
    {:else}Tell it what you need. You'll see it work, and it will say when it needs you.{/if}
  </p>
</div>

<style>
  .intro { display: flex; flex-direction: column; align-items: center; gap: 12px; padding: 40px 24px 0; text-align: center; }
  h2 { margin: 0; font-size: 22px; font-weight: 650; }
  p { margin: 0; }
  .meta { color: var(--m-tertiary); font-size: var(--text-sm); }
  .code { font-family: ui-monospace, 'SF Mono', Menlo, monospace; font-size: var(--text-xs); overflow-wrap: anywhere; }
  .about { color: var(--m-secondary); font-size: var(--text-md); }
  .hint { max-width: 340px; margin-top: 4px; color: var(--m-tertiary); font-size: var(--text-xs); }
</style>
