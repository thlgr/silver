<!-- A menu floating at a point or under an element: a row of quick reactions on top when given,
     then the choices. A choice with `selected` set (true or false) is a pick-list entry and the
     current one is checked. Escape closes only the menu, not the sheet under it. -->
<script>
  import { QUICK_REACTIONS } from '../../lib/chat.svelte.js'
  import IconCheck from '~icons/lucide/check'

  /** items: [{ label, hint, icon, run, danger, rule, selected }]. `anchor` (an element) places
   *  the menu under it, with its right edge just inside the element's when `align` is right;
   *  otherwise it opens at (x, y). It is as wide as its choices, never wider than the window. `reactions`: the emoji already chosen, or undefined
   *  for a menu without the row. `active` is the keyboard-highlighted index. */
  let { id = '', x = 0, y = 0, anchor, align = 'left', items, reactions, active = -1, onreact, onclose } = $props()
  let menu = $state()
  let at = $state(null)
  const picking = $derived(items.some((item) => item.selected !== undefined))

  // Keep the menu on screen: flip to the other side of its anchor when it would run off.
  $effect(() => {
    if (!menu) return
    const { width, height } = menu.getBoundingClientRect()
    const box = anchor?.getBoundingClientRect()
    const [left, below, above] = box ? [align === 'right' ? box.right - 8 - width : box.left, box.bottom + 6, box.top - 6] : [x, y, y]
    at = {
      left: Math.max(8, Math.min(left, innerWidth - width - 8)),
      top: Math.max(8, below + height > innerHeight - 8 ? above - height : below),
    }
  })

  // The highlighted choice stays in view while arrow keys walk a long list.
  $effect(() => {
    if (active >= 0) menu?.querySelectorAll('.item')[active]?.scrollIntoView({ block: 'nearest' })
  })

  const choose = (run) => {
    onclose()
    run()
  }

  function outside(event) {
    if (menu && !menu.contains(event.target) && !anchor?.contains(event.target)) onclose()
  }

  function escape(event) {
    if (event.key !== 'Escape') return
    event.stopPropagation()
    onclose()
  }
</script>

<svelte:window onpointerdown={outside} onkeydowncapture={escape} onblur={onclose} />

<div class="float" {id} bind:this={menu} style:left="{at?.left ?? x}px" style:top="{at?.top ?? y}px" role={picking ? 'listbox' : 'menu'}>
  {#if reactions}
    <div class="reactions">
      {#each QUICK_REACTIONS as emoji (emoji)}
        <button type="button" class="emoji" class:chosen={reactions.includes(emoji)} title={reactions.includes(emoji) ? `Remove ${emoji}` : `React ${emoji}`} onclick={() => choose(() => onreact(emoji))}>{emoji}</button>
      {/each}
    </div>
  {/if}
  <div class="list">
    {#each items as item, i (item.label)}
      {#if item.rule}<div class="rule"></div>{/if}
      <button type="button" class="item press" class:danger={item.danger} class:active={i === active} role={picking ? 'option' : 'menuitem'} aria-selected={picking ? item.selected : undefined} onclick={() => choose(item.run)}>
        {#if picking}<span class="mark">{#if item.selected}<IconCheck />{/if}</span>{/if}
        {#if item.icon}<item.icon />{/if}
        <span class="text">{item.label}</span>
        {#if item.hint}<span class="hint">{item.hint}</span>{/if}
      </button>
    {/each}
  </div>
</div>

<style>
  .float {
    position: fixed;
    z-index: 60;
    min-width: 220px;
    max-width: min(340px, calc(100vw - 16px));
    padding: 6px;
    border-radius: 14px;
    background: color-mix(in srgb, var(--m-surface) 92%, transparent);
    backdrop-filter: blur(24px) saturate(1.5);
    box-shadow: 0 12px 40px rgb(0 0 0 / 28%), 0 0 0 1px var(--m-border);
    animation: pop 0.12s var(--m-ease);
  }
  @keyframes pop { from { opacity: 0; transform: scale(0.97); } }
  .list { max-height: min(320px, 50vh); overflow-y: auto; }
  .reactions { display: flex; justify-content: space-between; padding: 2px 2px 6px; }
  .emoji { width: 34px; height: 34px; padding: 0; border: 0; border-radius: 50%; background: none; font-size: 20px; transition: transform 0.12s var(--m-ease), background-color 0.12s; }
  .emoji:hover { transform: scale(1.15); background: var(--bg-hover); }
  .emoji.chosen { background: var(--bg-active); }
  .item { display: flex; align-items: center; gap: 10px; width: 100%; min-height: 32px; padding: 0 10px; border: 0; border-radius: 8px; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .item:hover, .item.active { background: var(--bg-active); }
  .item.danger { color: var(--m-danger); }
  .item :global(svg) { flex: none; width: 15px; height: 15px; color: var(--m-secondary); }
  .item.danger :global(svg) { color: var(--m-danger); }
  .mark { display: grid; flex: none; place-items: center; width: 15px; }
  .mark :global(svg) { color: var(--m-text); }
  .text { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .hint { flex: none; color: var(--m-tertiary); font-size: var(--text-xs); }
  .rule { height: 1px; margin: 4px 8px; background: var(--m-border); }
  @media (prefers-reduced-motion: reduce) { .float { animation: none; } }
</style>
