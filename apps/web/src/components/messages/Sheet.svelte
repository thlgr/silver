<!-- A modal card: centered on a desktop window, rising from the bottom on a phone. Escape or a click
     outside closes it. -->
<script>
  import IconX from '~icons/lucide/x'

  let { title, onclose, width = 520, actions, children } = $props()
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onclose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="scrim" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="sheet" style:--width="{width}px" role="dialog" aria-modal="true" aria-label={title}>
    <header>
      <button type="button" class="round press" title="Close" aria-label="Close" onclick={onclose}><IconX /></button>
      <h2>{title}</h2>
      <span class="end">{@render actions?.()}</span>
    </header>
    <div class="body">{@render children()}</div>
  </div>
</div>

<style>
  .scrim { position: fixed; inset: 0; z-index: 50; display: grid; place-items: center; padding: 24px; background: var(--scrim); animation: fade 0.12s var(--m-ease); }
  .sheet { display: flex; flex-direction: column; width: min(var(--width), 100%); max-height: min(720px, 100%); overflow: hidden; border-radius: 22px; background: var(--m-bg); box-shadow: 0 24px 80px rgb(0 0 0 / 40%), 0 0 0 1px var(--m-border); animation: rise 0.22s var(--m-spring); }
  header { display: grid; grid-template-columns: 1fr auto 1fr; align-items: center; flex: none; padding: 12px 14px; }
  h2 { margin: 0; font-size: 15px; font-weight: 650; }
  .end { justify-self: end; display: flex; gap: 8px; }
  .body { flex: 1; min-height: 0; padding: 4px 20px 24px; overflow-y: auto; }
  @keyframes fade { from { opacity: 0; } }
  @keyframes rise { from { opacity: 0; transform: translateY(8px) scale(0.985); } }
  @media (max-width: 720px) {
    .scrim { align-items: end; padding: 0; }
    .sheet { width: 100%; max-height: 92%; border-radius: 22px 22px 0 0; animation-name: slide; }
    @keyframes slide { from { transform: translateY(40px); opacity: 0; } }
  }
  @media (prefers-reduced-motion: reduce) { .scrim, .sheet { animation: none; } }
</style>
