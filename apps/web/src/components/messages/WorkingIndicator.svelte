<!-- What the bot is doing right now: a turning orb, the step, and for how long. Clicking it
     unfolds what the bot is thinking, when its model shares that. -->
<script>
  import { elapsed } from '../../lib/when.js'
  import Orb from './Orb.svelte'
  import IconChevron from '~icons/lucide/chevron-down'

  let { bot } = $props()
  let open = $state(false)
  let now = $state(Date.now())
  let thought = $state()
  const needs = $derived(bot.status === 'needs_input')

  $effect(() => {
    const tick = setInterval(() => (now = Date.now()), 1000)
    return () => clearInterval(tick)
  })
  // Follow the newest thought.
  $effect(() => {
    bot.thinking
    if (thought) thought.scrollTop = thought.scrollHeight
  })
</script>

<div class="row">
  <button type="button" class="working press" disabled={!bot.thinking} aria-expanded={bot.thinking ? open : undefined} onclick={() => (open = !open)}>
    <span class="line" class:needs>
      <Orb kind={needs ? 'listening' : 'working'} size={16} color={needs ? 'var(--m-warning)' : 'var(--m-secondary)'} />
      <span class="what">{bot.activity || 'Working…'}</span>
      {#if bot.started_at}<span class="timer">{elapsed(now - bot.started_at)}</span>{/if}
      {#if bot.thinking}<IconChevron class="chev" style="transform: rotate({open ? 180 : 0}deg)" />{/if}
    </span>
    {#if open && bot.thinking}<span class="thought" bind:this={thought}>{bot.thinking}</span>{/if}
  </button>
</div>

<style>
  .row { padding: 6px 64px 0 0; }
  .working { display: flex; flex-direction: column; gap: 8px; max-width: 100%; padding: 12px 16px; border: 0; border-radius: 22px; background: var(--m-agent); color: inherit; text-align: left; }
  .working:disabled { opacity: 1; cursor: default; }
  .line { display: flex; align-items: center; gap: 8px; min-width: 0; color: var(--m-secondary); font-size: var(--text-md); }
  .line.needs { color: var(--m-warning); }
  .what { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .timer { flex: none; color: var(--m-tertiary); font-size: var(--text-xs); font-variant-numeric: tabular-nums; }
  .line :global(.chev) { flex: none; width: 12px; height: 12px; color: var(--m-tertiary); transition: transform 0.2s var(--m-spring); }
  .thought { max-height: 180px; overflow-y: auto; color: var(--m-secondary); font-size: var(--text-sm); white-space: pre-wrap; overflow-wrap: anywhere; user-select: text; }
  @media (max-width: 720px) { .row { padding-right: 40px; } }
</style>
