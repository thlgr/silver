<!-- How much of the provider's usage limit was used when a bot wrote a reply: the session window as
     a thin bar (amber near the end, red where the bot stops), and every window in a card while it
     is hovered or focused, which is how a touch screen opens it. `up` opens the card above. -->
<script>
  import { time } from '../../lib/when.js'

  let { windows, up = false } = $props()
  const session = $derived(windows[0])
  const percent = (window) => Math.round(Math.min(100, Math.max(0, window.percent)))
  const level = (window) => (window.percent >= 90 ? 'stop' : window.percent >= 75 ? 'warn' : '')

  function resets(window) {
    if (!window.resets_at) return ''
    const at = new Date(window.resets_at)
    const today = at.toDateString() === new Date().toDateString()
    const when = today ? time(window.resets_at) : `${at.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${time(window.resets_at)}`
    return `${window.resets_at < Date.now() ? 'reset' : 'resets'} ${when}`
  }
</script>

{#snippet track(window)}
  <span class="track"><span class="fill" style:width="{percent(window)}%"></span></span>
{/snippet}

<button type="button" class="limit {level(session)}" aria-label="Usage limits: {session.name} {percent(session)}% used">
  {@render track(session)}
  <span class="card" class:up role="tooltip">
    {#each windows as window (window.name)}
      <span class="line {level(window)}">
        <b>{window.name}</b>
        {@render track(window)}
        <i>{percent(window)}%</i>
        <small>{resets(window)}</small>
      </span>
    {/each}
  </span>
</button>

<style>
  .limit { position: relative; display: inline-flex; flex: none; align-items: center; align-self: center; padding: 6px 0; border: 0; background: none; color: var(--m-secondary); font: inherit; cursor: default; }
  .warn, .line.warn { color: var(--m-warning); }
  .stop, .line.stop { color: var(--m-danger); }
  .track { flex: none; width: 34px; height: 4px; overflow: hidden; border-radius: 999px; background: var(--m-dim); }
  .fill { display: block; height: 100%; border-radius: inherit; background: currentColor; }
  .card { position: absolute; top: calc(100% - 2px); left: 0; z-index: 8; display: none; flex-direction: column; gap: 7px; min-width: 250px; padding: 10px 12px; border: 1px solid var(--m-border); border-radius: 12px; background: var(--m-bg); box-shadow: 0 6px 20px var(--m-shadow); color: var(--m-secondary); text-align: left; white-space: nowrap; }
  .card.up { top: auto; bottom: calc(100% - 2px); }
  .limit:hover .card, .limit:focus .card { display: flex; }
  .line { display: grid; grid-template-columns: 92px 56px 34px 1fr; align-items: center; gap: 8px; color: var(--m-secondary); font-size: var(--text-xs); }
  .line b { overflow: hidden; color: var(--m-text); font-weight: 500; text-overflow: ellipsis; }
  .line .track { width: 56px; }
  .line i { color: var(--m-text); font-style: normal; font-variant-numeric: tabular-nums; text-align: right; }
  .line small { color: var(--m-tertiary); font-size: 11px; }
</style>
