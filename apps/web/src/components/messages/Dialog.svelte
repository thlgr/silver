<!-- A confirmation: a title, a sentence and the choice. Escape or a click outside cancels. -->
<script>
  let { title, message, action, danger = false, onclose } = $props()

  // Escape closes the dialog, not the sheet under it.
  function escape(event) {
    if (event.key !== 'Escape') return
    event.stopPropagation()
    onclose()
  }

  function confirm() {
    // Closing takes the props away, so the action is taken first.
    const { run } = action
    onclose()
    run()
  }
</script>

<svelte:window onkeydowncapture={escape} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="scrim" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="dialog" role="alertdialog" aria-label={title}>
    <h2>{title}</h2>
    {#if message}<p>{message}</p>{/if}
    <div class="actions">
      <button type="button" class="pill quiet press" onclick={onclose}>Cancel</button>
      <button type="button" class="pill press" class:danger onclick={confirm}>{action.label}</button>
    </div>
  </div>
</div>

<style>
  .scrim { position: fixed; inset: 0; z-index: 70; display: grid; place-items: center; padding: 24px; background: var(--scrim); animation: fade 0.12s var(--m-ease); }
  .dialog { width: min(340px, 100%); padding: 20px; border-radius: 20px; background: var(--m-surface); box-shadow: 0 20px 60px rgb(0 0 0 / 35%); animation: rise 0.2s var(--m-spring); }
  h2 { margin: 0 0 6px; font-size: 16px; font-weight: 650; }
  p { margin: 0 0 18px; color: var(--m-secondary); font-size: var(--text-sm); }
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  p + .actions { margin-top: 0; }
  .pill.danger { background: var(--m-danger); color: #fff; }
  @keyframes fade { from { opacity: 0; } }
  @keyframes rise { from { opacity: 0; transform: translateY(6px) scale(0.98); } }
  @media (prefers-reduced-motion: reduce) { .scrim, .dialog { animation: none; } }
</style>
