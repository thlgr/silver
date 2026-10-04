<!-- One roster row: the avatar with its status mark, the name, and what the bot is doing now or
     what the chat last said. -->
<script>
  import BotAvatar from './BotAvatar.svelte'
  import Orb from './Orb.svelte'
  import { day } from '../../lib/when.js'
  import IconPin from '~icons/lucide/pin'

  let { bot, selected = false, onselect, oncontext } = $props()
  const narrow = matchMedia('(max-width: 720px)').matches

  const working = $derived(bot.status === 'working')
  const needs = $derived(bot.status === 'needs_input')
  const failed = $derived(bot.status === 'error')
</script>

<button type="button" class="row press" class:selected aria-current={selected} onclick={onselect} oncontextmenu={(e) => (e.preventDefault(), oncontext(e))}>
  <BotAvatar {bot} size={narrow ? 46 : 38} />
  <span class="text">
    <span class="top">
      {#if bot.pinned}<IconPin class="pin" />{/if}
      <span class="name">{bot.name}</span>
      <span class="when" class:fresh={bot.unread > 0}>{day(bot.last_at)}</span>
    </span>
    <span class="bottom">
      <span class="preview" class:warn={needs} class:bad={failed}>
        {#if needs}
          <Orb kind="listening" size={16} color="var(--m-warning)" />
          <span>{bot.activity || 'Needs your approval'}</span>
        {:else if working}
          <Orb size={14} />
          <span>{bot.activity || 'Working…'}</span>
        {:else if failed}
          <span>{bot.activity || bot.last_message || 'Something went wrong'}</span>
        {:else}
          <span>{bot.last_message ?? (bot.kind === 'group' ? `${bot.members.length} bots` : 'No messages yet')}</span>
        {/if}
      </span>
      {#if bot.unread > 0}<span class="unread">{bot.unread}</span>{/if}
    </span>
  </span>
</button>

<style>
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 7px 10px;
    border: 0;
    border-radius: 12px;
    background: none;
    color: inherit;
    text-align: left;
  }
  .row:hover { background: var(--bg-hover); }
  .row.selected { background: var(--bg-active); }
  .text { display: flex; flex-direction: column; gap: 1px; min-width: 0; flex: 1; }
  .top, .bottom { display: flex; align-items: baseline; gap: 6px; min-width: 0; }
  .name { flex: 1; min-width: 0; overflow: hidden; color: var(--m-text); font-size: var(--text-md); font-weight: 600; text-overflow: ellipsis; white-space: nowrap; }
  .top :global(.pin) { flex: none; align-self: center; width: 10px; height: 10px; color: var(--m-tertiary); }
  .when { flex: none; color: var(--m-tertiary); font-size: 11.5px; }
  .when.fresh { color: var(--m-text); }
  .preview { display: flex; flex: 1; align-items: center; gap: 6px; min-width: 0; color: var(--m-secondary); font-size: var(--text-sm); }
  .preview span:not(:empty) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .preview.warn { color: var(--m-warning); }
  .preview.bad { color: var(--m-danger); }
  .unread { flex: none; align-self: center; min-width: 18px; padding: 0 6px; border-radius: 999px; background: var(--m-fill); color: var(--m-on-fill); font-size: 11px; font-weight: 700; line-height: 18px; text-align: center; }
</style>
