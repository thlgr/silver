<!-- The user's reactions under a message; clicking one takes it back. -->
<script>
  import { react } from '../../lib/chat.svelte.js'

  let { entry } = $props()
</script>

{#if entry.reactions?.length}
  <div class="reactions">
    {#each entry.reactions as emoji (emoji)}
      <button type="button" class="tag press" title="Remove {emoji}" aria-label="Remove reaction {emoji}" onclick={() => react(entry, emoji)}>{emoji}</button>
    {/each}
  </div>
{/if}

<style>
  .reactions { display: flex; gap: 4px; }
  .tag { padding: 2px 8px; border: 0; border-radius: 999px; background: var(--m-surface); font-size: 14px; line-height: 1.4; animation: pop 0.15s var(--m-spring); }
  @keyframes pop { from { opacity: 0; transform: scale(0.7); } }
  @media (prefers-reduced-motion: reduce) { .tag { animation: none; } }
</style>
