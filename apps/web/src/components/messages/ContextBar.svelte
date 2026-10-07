<!-- How full the agent's context window is: a thin bar filling with the share of the compact budget
     it holds. A colour of its own, apart from the usage-limit bar beside it (`--m-context`). -->
<script>
  let { context } = $props()
  const percent = $derived(
    context?.budget_bytes ? Math.min(100, Math.max(0, Math.round((context.total_bytes * 100) / context.budget_bytes))) : 0,
  )
</script>

<span class="context" title="Context {percent}% full; silver compacts near the end">
  <span class="track"><span class="fill" style:width="{percent}%"></span></span>
</span>

<style>
  .context { display: inline-flex; flex: none; align-items: center; align-self: center; padding: 6px 0; }
  .track { flex: none; width: 34px; height: 4px; overflow: hidden; border-radius: 999px; background: var(--m-dim); }
  .fill { display: block; height: 100%; border-radius: inherit; background: var(--m-context); }
</style>
