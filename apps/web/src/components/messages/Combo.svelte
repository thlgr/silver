<!-- A text field that suggests from a list as you type, in place of the browser's datalist: a model
     id can be picked or written, and nothing is red-underlined. -->
<script>
  import Menu from './Menu.svelte'
  import IconUpDown from '~icons/lucide/chevrons-up-down'

  let { label, value = $bindable(''), suggestions, placeholder = '' } = $props()
  let row = $state()
  let box = $state()
  let open = $state(false)
  let active = $state(-1)
  let typed = $state(false) // the text was edited since it opened, so it narrows the list
  const menuId = crypto.randomUUID()

  const shown = $derived(
    (typed && value.trim() ? suggestions.filter((id) => id.toLowerCase().includes(value.trim().toLowerCase())) : suggestions).slice(0, 60),
  )
  const items = $derived(shown.map((id) => ({ label: id, selected: id === value, run: () => pick(id) })))

  function pick(id) {
    value = id
    open = false
    typed = false
  }

  function keydown(event) {
    if (!open && event.key === 'ArrowDown') {
      open = true
      return
    }
    if (!open || !shown.length) return
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      active = (active + (event.key === 'ArrowDown' ? 1 : -1) + shown.length) % shown.length
    } else if (event.key === 'Enter' && active >= 0) {
      event.preventDefault()
      pick(shown[active])
    }
  }
</script>

<div class="combo" bind:this={row}>
  <span class="label">{label}</span>
  <input
    bind:this={box}
    bind:value
    {placeholder}
    spellcheck="false"
    autocomplete="off"
    autocapitalize="off"
    role="combobox"
    aria-expanded={open}
    aria-controls={menuId}
    aria-label={label}
    onfocus={() => ((open = suggestions.length > 0), (active = -1))}
    oninput={() => ((typed = true), (open = suggestions.length > 0), (active = -1))}
    onkeydown={keydown}
  />
  <button type="button" class="more" tabindex="-1" aria-label="Show models" onclick={() => ((open = !open), box.focus())}><IconUpDown /></button>
</div>
{#if open && shown.length}<Menu id={menuId} anchor={row} align="right" {items} {active} onclose={() => (open = false)} />{/if}

<style>
  .combo { display: flex; align-items: center; gap: 12px; min-height: 44px; padding: 0 14px; font-size: var(--text-md); }
  .label { flex: none; width: 92px; color: var(--m-secondary); }
  input { flex: 1; min-width: 0; height: 44px; padding: 0; border: 0; outline: 0; background: none; color: var(--m-text); font: inherit; text-align: right; }
  input::placeholder { color: var(--m-tertiary); }
  .more { display: grid; flex: none; place-items: center; width: 14px; height: 44px; padding: 0; border: 0; background: none; color: var(--m-tertiary); }
  .more:hover { color: var(--m-text); }
  .more :global(svg) { width: 14px; height: 14px; }
</style>
