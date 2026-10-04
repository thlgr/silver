<!-- A row that picks one of a few things, Codync's value row with a pop-up: the label, the current
     choice and up/down arrows; a click opens the list under it. -->
<script>
  import Menu from './Menu.svelte'
  import IconUpDown from '~icons/lucide/chevrons-up-down'

  /** options: [{ value, label, hint }]; `extra`: entries after a rule that are actions, not values. */
  let { label, value, options, extra = [], onchange } = $props()
  let el = $state()
  let open = $state(false)
  const current = $derived(options.find((option) => option.value === value))
  const items = $derived([
    ...options.map((option) => ({ label: option.label, hint: option.hint, selected: option.value === value, run: () => onchange(option.value) })),
    ...extra.map((action, i) => ({ label: action.label, icon: action.icon, rule: i === 0, run: action.run })),
  ])
</script>

<button type="button" class="choice" bind:this={el} aria-haspopup="listbox" aria-expanded={open} onclick={() => (open = !open)}>
  <span class="label">{label}</span>
  <span class="value">{current?.label ?? ''}</span>
  <IconUpDown />
</button>
{#if open}<Menu anchor={el} align="right" {items} onclose={() => (open = false)} />{/if}

<style>
  .choice { display: flex; align-items: center; gap: 12px; width: 100%; min-height: 44px; padding: 0 14px; border: 0; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .choice:hover, .choice[aria-expanded='true'] { background: var(--bg-hover); }
  .label { flex: none; width: 92px; color: var(--m-secondary); }
  .value { flex: 1; min-width: 0; overflow: hidden; text-align: right; text-overflow: ellipsis; white-space: nowrap; }
  .choice :global(svg) { flex: none; width: 14px; height: 14px; color: var(--m-tertiary); }
</style>
