<!-- A button that asks once: the first click arms it and shows `ask`, a second click within a
     few seconds runs `onconfirm`. Replaces the browser's blocking confirm(). -->
<script>
  let { onconfirm, ask = 'Sure?', class: cls = 'btn', title, children } = $props()
  let armed = $state(false)
  let timer

  const disarm = () => (clearTimeout(timer), (armed = false))

  function click(e) {
    e.preventDefault()
    e.stopPropagation()
    if (!armed) {
      armed = true
      timer = setTimeout(disarm, 3000)
      return
    }
    disarm()
    onconfirm()
  }
</script>

<button type="button" class={cls} class:armed {title} aria-label={armed ? ask : title} onclick={click} onblur={disarm}>
  {#if armed}{ask}{:else}{@render children?.()}{/if}
</button>

<style>
  .armed { color: var(--danger); border-color: var(--danger); white-space: nowrap; }
</style>
