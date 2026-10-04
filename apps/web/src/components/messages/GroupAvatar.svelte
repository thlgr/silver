<!-- A group's face, clustered like iMessage: two bots tucked diagonally, three in a triangle, four
     in a grid; past four the last cell counts the rest. -->
<script>
  import Avatar from './Avatar.svelte'
  import { moodOf } from './mood.js'

  let { members, size = 40 } = $props()
  const shown = $derived(members.length > 4 ? members.slice(0, 3) : members)
  const side = $derived(members.length === 2 ? 0.66 : members.length === 3 ? 0.55 : 0.5)
  const corners = $derived(
    { 2: ['bl', 'tr'], 3: ['t', 'bl', 'br'] }[members.length] ?? ['tl', 'tr', 'bl', 'br'],
  )
</script>

<div class="group" style:width="{size}px" style:height="{size}px" aria-hidden="true">
  {#each shown as bot, i (bot.id)}
    <span class="cell {corners[i]}"><Avatar shape={bot.avatar_shape} color={bot.avatar_color} size={Math.round(size * side)} mood={moodOf(bot)} /></span>
  {/each}
  {#if members.length > 4}
    <span class="cell br more" style:font-size="{size * 0.24}px" style:width="{size * 0.5}px" style:height="{size * 0.5}px">+{members.length - 3}</span>
  {/if}
  {#if !members.length}<span class="empty" style:font-size="{size * 0.4}px">·</span>{/if}
</div>

<style>
  .group { position: relative; flex: none; }
  .cell { position: absolute; display: block; }
  .tl { top: 0; left: 0; } .tr { top: 0; right: 0; } .bl { bottom: 0; left: 0; } .br { bottom: 0; right: 0; }
  .t { top: 0; left: 50%; transform: translateX(-50%); }
  .more { display: grid; place-items: center; color: var(--m-secondary); font-weight: 700; }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; color: var(--m-secondary); }
</style>
