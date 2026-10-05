<!-- A thread (Slack's "reply in thread"): the message it started on, its replies, and a box to
     continue there. In a bot's chat the thread is a branch with a session of its own; in a group
     the room answers inside it. -->
<script>
  import { botById, chat, laneKey, markRead, workingIn } from '../../lib/chat.svelte.js'
  import { buildItems } from './items.js'
  import ChatRow from './ChatRow.svelte'
  import Composer from './Composer.svelte'
  import WorkingIndicator from './WorkingIndicator.svelte'
  import IconX from '~icons/lucide/x'

  let { botId, root, onclose } = $props()
  const bot = $derived(botById(botId))
  const rootEntry = $derived(chat.lanes[laneKey(botId)]?.entries.find((entry) => entry.id === root))
  const entries = $derived(chat.lanes[laneKey(botId, root)]?.entries ?? [])
  const working = $derived(bot ? workingIn(bot, root) : false)
  const items = $derived(buildItems(entries).filter((item) => !item.separator))
  const replies = $derived(entries.filter((entry) => entry.kind === 'user' || entry.kind === 'agent').length)
  let scroller = $state()
  let content = $state()
  let focused = $state(document.hasFocus())

  $effect(() => {
    if (!content) return
    const follow = () => scroller.scrollTo({ top: scroller.scrollHeight })
    const observer = new ResizeObserver(follow)
    observer.observe(content)
    return () => observer.disconnect()
  })

  // A thread is read on its own: having it open reads its replies.
  $effect(() => {
    if (focused && !document.hidden && rootEntry?.thread?.unread) {
      const timer = setTimeout(() => markRead(botId, root), 250)
      return () => clearTimeout(timer)
    }
  })
</script>

<svelte:window onfocus={() => (focused = true)} onblur={() => (focused = false)} />

<section class="replies">
  <header>
    <span class="heading"><strong>Thread</strong><small>{bot?.name}</small></span>
    <button type="button" class="round press" title="Close thread" aria-label="Close thread" onclick={onclose}><IconX /></button>
  </header>
  <div class="scroller" bind:this={scroller}>
    <div class="column" bind:this={content}>
      {#if rootEntry}<ChatRow entry={rootEntry} start {bot} inThread />{/if}
      <div class="count"><span>{replies ? (replies === 1 ? '1 reply' : `${replies} replies`) : 'No replies yet'}</span><i></i></div>
      {#each items as item (item.id)}
        <ChatRow entry={item.entry} start={item.start} {bot} {working} inThread />
      {/each}
      {#if working && !entries.some((entry) => entry.permission?.status === 'pending')}<WorkingIndicator {bot} />{/if}
    </div>
  </div>
  <Composer {botId} thread={root} />
</section>

<style>
  .replies { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--m-bg); }
  header { display: flex; align-items: center; justify-content: space-between; flex: none; height: 54px; padding: 0 12px 0 16px; border-bottom: 1px solid var(--m-border); }
  .heading { display: flex; flex-direction: column; line-height: 1.2; }
  .heading strong { font-size: var(--text-md); }
  .heading small { color: var(--m-tertiary); font-size: var(--text-xs); }
  header .round { background: none; color: var(--m-secondary); }
  .scroller { flex: 1; min-height: 0; overflow-y: auto; }
  .column { padding: 8px 16px 8px; }
  .count { display: flex; align-items: center; gap: 10px; padding: 12px 0; color: var(--m-tertiary); font-size: var(--text-xs); }
  .count span { flex: none; }
  .count i { flex: 1; height: 1px; background: var(--m-border); }
</style>
