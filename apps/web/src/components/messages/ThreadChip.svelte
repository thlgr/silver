<!-- Under a message that has a thread: who replied, how many, and how recently. -->
<script>
  import { botById } from '../../lib/chat.svelte.js'
  import { day } from '../../lib/when.js'
  import Avatar from './Avatar.svelte'
  import IconChevron from '~icons/lucide/chevron-right'

  let { summary, onopen } = $props()
  const bots = $derived(summary.authors.map(botById).filter(Boolean).slice(0, 3))
  const replies = $derived(summary.count === 1 ? '1 reply' : `${summary.count} replies`)
</script>

<button type="button" class="thread press" title="View thread" aria-label="View thread, {replies}{summary.unread ? `, ${summary.unread} new` : ''}" onclick={onopen}>
  <span class="faces">
    {#each bots as bot (bot.id)}<Avatar shape={bot.avatar_shape} color={bot.avatar_color} size={18} />{/each}
  </span>
  <span class="count">{replies}</span>
  {#if summary.unread > 0}
    <span class="new">{summary.unread} new</span>
  {:else}
    <span class="when">{day(summary.last_at)}</span>
  {/if}
  <IconChevron />
</button>

<style>
  .thread { display: inline-flex; align-items: center; gap: 6px; padding: 5px 10px; border: 0; border-radius: 999px; background: var(--m-surface); color: inherit; font-size: var(--text-xs); }
  .faces { display: flex; }
  .faces :global(canvas + canvas) { margin-left: -6px; }
  .count { color: var(--m-text); font-weight: 600; }
  .new { color: var(--m-text); font-weight: 600; }
  .when { color: var(--m-tertiary); }
  .thread :global(svg) { width: 12px; height: 12px; color: var(--m-tertiary); }
</style>
