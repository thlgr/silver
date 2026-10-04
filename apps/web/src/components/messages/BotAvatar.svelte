<!-- A bot's or group's avatar with the roster's status mark: amber "!" when it needs the user. -->
<script>
  import Avatar from './Avatar.svelte'
  import GroupAvatar from './GroupAvatar.svelte'
  import { membersOf } from '../../lib/chat.svelte.js'
  import { moodOf } from './mood.js'

  let { bot, size = 44, badge = true } = $props()
  const needs = $derived(bot.status === 'needs_input')
</script>

<span class="avatar" style:width="{size}px" style:height="{size}px">
  {#if bot.kind === 'group'}
    <GroupAvatar members={membersOf(bot)} {size} />
  {:else}
    <Avatar shape={bot.avatar_shape} color={bot.avatar_color} {size} mood={moodOf(bot)} />
  {/if}
  {#if badge && needs}
    <span class="mark needs" style:width="{size * 0.36}px" style:height="{size * 0.36}px" style:font-size="{size * 0.22}px">!</span>
  {/if}
</span>

<style>
  .avatar { position: relative; display: inline-block; flex: none; }
  .mark { position: absolute; right: -2px; bottom: -2px; box-sizing: border-box; border: 2px solid var(--m-bg); border-radius: 50%; }
  .needs { display: grid; place-items: center; background: var(--m-warning); color: #fff; font-weight: 900; line-height: 1; }
</style>
