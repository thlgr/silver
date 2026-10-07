<!-- One endless conversation with a bot or a group. Only deliberate messages show here; the
     tools, thinking and plans are in "Full conversation". The title floats over the messages,
     which fade out under it as they scroll. -->
<script>
  import { tick } from 'svelte'
  import { app } from '../../lib/state.svelte.js'
  import { botById, chat, laneKey, loadOlder, markRead, openThread, openTrace, workingIn } from '../../lib/chat.svelte.js'
  import { separator } from '../../lib/when.js'
  import { buildItems } from './items.js'
  import BotAvatar from './BotAvatar.svelte'
  import ChatRow from './ChatRow.svelte'
  import Composer from './Composer.svelte'
  import ContextBar from './ContextBar.svelte'
  import Intro from './Intro.svelte'
  import LimitBar from './LimitBar.svelte'
  import WorkingIndicator from './WorkingIndicator.svelte'
  import IconBack from '~icons/lucide/chevron-left'
  import IconBrain from '~icons/lucide/brain'
  import IconDetails from '~icons/lucide/chevrons-left'
  import IconList from '~icons/lucide/list'
  import IconFolder from '~icons/lucide/folder'

  let { botId, details, ontoggle, onmemory, onback } = $props()
  const bot = $derived(botById(botId))
  const found = $derived(chat.lanes[laneKey(botId)])
  const entries = $derived(found?.entries ?? [])
  const working = $derived(bot ? workingIn(bot) : false)
  const named = $derived(app.workspaces.find((w) => w.id === bot?.workspace_id)?.name)
  // A bot always names its workspace; a group names the folder it was filed under, if any.
  const workspace = $derived(bot?.kind === 'agent' || bot?.workspace_id ? (named ?? 'No workspace') : null)
  let scroller = $state()
  let content = $state()
  let pinned = true
  let focused = $state(document.hasFocus())

  const items = $derived(buildItems(entries))
  // The approval card already says what the bot waits for.
  const waiting = $derived(entries.some((entry) => entry.permission?.status === 'pending'))

  // Follow new text while the reader is at the bottom; a fresh chat opens there.
  $effect(() => {
    if (!content) return
    const follow = () => pinned && scroller.scrollTo({ top: scroller.scrollHeight })
    const observer = new ResizeObserver(follow)
    observer.observe(content)
    observer.observe(scroller)
    return () => observer.disconnect()
  })
  const onscroll = () => (pinned = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 48)
  $effect(() => {
    botId
    pinned = true
  })
  const toBottom = () => {
    pinned = true
    tick().then(() => scroller?.scrollTo({ top: scroller.scrollHeight }))
  }

  // Having the chat open reads it, while the window is in front.
  $effect(() => {
    if (focused && !document.hidden && bot?.unread) {
      const timer = setTimeout(() => markRead(botId), 250)
      return () => clearTimeout(timer)
    }
  })
</script>

<svelte:window onfocus={() => (focused = true)} onblur={() => (focused = false)} />

<section class="convo">
  <div class="top">
    <div class="bar">
      <span class="side">
        {#if onback}<button type="button" class="round glass press" title="Back" aria-label="Back" onclick={onback}><IconBack /></button>{/if}
      </span>
      <button type="button" class="title" title="Conversation details" aria-label="View conversation details" onclick={ontoggle}>
        <BotAvatar {bot} size={22} badge={false} />
        <span>{bot.name}</span>
        {#if workspace}<small><IconFolder />{workspace}</small>{/if}
      </button>
      {#if bot.limits?.length || bot.context}
        <span class="usage">
          {#if bot.limits?.length}<LimitBar windows={bot.limits} />{/if}
          {#if bot.context}<ContextBar context={bot.context} />{/if}
        </span>
      {/if}
      <span class="side end">
        {#if bot.workspace_id}<button type="button" class="round glass press" title="Workspace memory" aria-label="Workspace memory" onclick={onmemory}><IconBrain /></button>{/if}
        <button type="button" class="round glass press" title="Full conversation" aria-label="Full conversation" onclick={() => openTrace(botId)}><IconList /></button>
        {#if !details}<button type="button" class="round glass press" title="Conversation details" aria-label="Conversation details" onclick={ontoggle}><IconDetails /></button>{/if}
      </span>
    </div>
  </div>

  <div class="scroller" bind:this={scroller} {onscroll}>
    <div class="column" bind:this={content}>
      {#if found && !found.complete && entries.length >= 50}
        <button type="button" class="older" onclick={() => loadOlder(botId)}>Load earlier messages</button>
      {/if}
      {#if found && !items.length}<Intro {bot} />{/if}
      {#each items as item (item.id)}
        {#if item.separator}
          <div class="sep">{separator(item.separator)}</div>
        {:else}
          <ChatRow entry={item.entry} start={item.start} {bot} {working} onthread={(entry) => openThread(botId, entry.id)} />
        {/if}
      {/each}
      {#if working && !waiting}<WorkingIndicator {bot} />{/if}
      <div class="end-pad"></div>
    </div>
  </div>

  <Composer {botId} onsent={toBottom} />
</section>

<style>
  .convo { position: relative; display: flex; flex-direction: column; height: 100%; min-width: 0; min-height: 0; background: var(--m-bg); }
  .top { position: absolute; top: 0; right: 0; left: 0; z-index: 5; padding: 10px 16px 28px; pointer-events: none; background: linear-gradient(var(--m-bg) 0%, color-mix(in srgb, var(--m-bg) 65%, transparent) 40%, transparent); }
  .bar { position: relative; display: flex; align-items: center; justify-content: center; gap: 8px; }
  .bar > * { pointer-events: auto; }
  .side { position: absolute; top: 0; display: flex; gap: 8px; left: 0; }
  .side.end { right: 0; left: auto; }
  .title { display: inline-flex; align-items: center; gap: 8px; max-width: 60%; padding: 5px 16px 5px 10px; border: 0; border-radius: 999px; background: var(--m-bg); color: var(--m-text); font-size: 14px; font-weight: 600; box-shadow: 0 4px 16px var(--m-shadow); }
  .title span, .title small { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .title small { display: inline-flex; align-items: center; gap: 4px; min-width: 0; color: var(--m-secondary); font-size: var(--text-xs); font-weight: 400; }
  .title small :global(svg) { flex: none; width: 12px; height: 12px; }
  .usage { display: inline-flex; align-items: center; gap: 10px; padding: 0 12px; border-radius: 999px; background: var(--m-bg); box-shadow: 0 4px 16px var(--m-shadow); }
  .usage :global(.card) { left: 50%; translate: -50% 0; }
  .scroller { flex: 1; min-height: 0; overflow-y: auto; padding-top: 60px; }
  .column { width: 100%; max-width: 852px; margin: 0 auto; padding: 8px 16px 0; }
  .sep { padding: 18px 0 6px; color: var(--m-tertiary); font-size: var(--text-xs); text-align: center; }
  .older { display: block; margin: 0 auto; padding: 12px; border: 0; background: none; color: var(--m-secondary); font-size: var(--text-xs); }
  .end-pad { height: 8px; }
</style>
