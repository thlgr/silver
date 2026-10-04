<!-- What silver says about an action ("Copied", why a save failed), above the composer. -->
<script>
  import { app } from '../../lib/state.svelte.js'
</script>

{#if app.notice}
  <div class="toast" class:error={app.notice.error} role="status">
    <button type="button" aria-label="Dismiss" onclick={() => (app.notice = null)}>{app.notice.text}</button>
    {#if app.notice.action}
      <button type="button" class="act" onclick={() => (app.notice.action.run(), (app.notice = null))}>{app.notice.action.label}</button>
    {/if}
  </div>
{/if}

<style>
  .toast { position: fixed; bottom: 84px; left: 50%; z-index: 80; max-width: min(520px, calc(100vw - 32px)); border-radius: 999px; background: var(--m-fill); color: var(--m-on-fill); transform: translateX(-50%); box-shadow: 0 8px 28px rgb(0 0 0 / 30%); animation: rise 0.2s var(--m-spring); }
  .toast button { padding: 9px 16px; border: 0; background: none; color: inherit; font-size: var(--text-sm); font-weight: 500; }
  .toast .act { border-left: 1px solid color-mix(in srgb, currentColor 30%, transparent); font-weight: 700; }
  .toast.error { background: var(--m-danger); color: #fff; }
  @keyframes rise { from { opacity: 0; transform: translate(-50%, 8px); } }
  @media (prefers-reduced-motion: reduce) { .toast { animation: none; } }
</style>
