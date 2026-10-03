<!-- A trigger button with an anchored menu. `children` receives a close() callback. -->
<script>
  let { trigger, children, class: cls = 'chip', label, up = false, right = false, ...rest } = $props()
  let open = $state(false)
  let root

  const close = () => (open = false)
  const outside = (e) => open && !root.contains(e.target) && close()
</script>

<svelte:window onpointerdown={outside} onkeydown={(e) => e.key === 'Escape' && close()} />

<div class="popover" bind:this={root}>
  <button type="button" class={cls} aria-label={label} title={label} aria-expanded={open} onclick={() => (open = !open)} {...rest}>
    {@render trigger()}
  </button>
  {#if open}
    <div class="menu" class:up class:right role="menu">{@render children(close)}</div>
  {/if}
</div>

<style>
  .popover { position: relative; }
  .menu { top: calc(100% + var(--space-1)); left: 0; }
  .menu.up { top: auto; bottom: calc(100% + var(--space-1)); }
  .menu.right { left: auto; right: 0; }
</style>
