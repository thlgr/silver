<!-- The Messages mode: the roster on the left, a conversation beside it, and a panel (a thread,
     or the details of the chat) on the right. On a phone-width window it is one pane at a time. -->
<script>
  import { onMount } from 'svelte'
  import { app } from '../../lib/state.svelte.js'
  import { botById, chat, startChat, stopChat, unreadTotal } from '../../lib/chat.svelte.js'
  import Avatar from './Avatar.svelte'
  import BotEditor from './BotEditor.svelte'
  import Conversation from './Conversation.svelte'
  import Details from './Details.svelte'
  import GroupEditor from './GroupEditor.svelte'
  import NewChat from './NewChat.svelte'
  import Toast from './Toast.svelte'
  import Replies from './Replies.svelte'
  import Roster from './Roster.svelte'
  import Trace from './Trace.svelte'
  import './messages.css'

  const phone = matchMedia('(max-width: 720px)')
  let narrow = $state(phone.matches)
  let width = $state(1000) // of the area beside the roster
  let details = $state(false)
  const bot = $derived(chat.selected ? botById(chat.selected) : null)
  // A panel sits beside the conversation when there is room, and over it when there is not.
  const wide = $derived(width >= 680 && !narrow)
  const panel = $derived(bot ? (chat.thread ? 'thread' : details ? 'details' : null) : null)

  onMount(() => {
    const onchange = () => {
      narrow = phone.matches
      if (narrow) details = false
    }
    phone.addEventListener('change', onchange)
    startChat()
    return () => {
      phone.removeEventListener('change', onchange)
      stopChat()
    }
  })

  // The unread count rides in the tab title, like a messaging app's.
  $effect(() => {
    const unread = unreadTotal()
    document.title = `${unread ? `(${unread}) ` : ''}silver · Messages`
  })

  // A thread belongs to the chat it was opened in.
  $effect(() => {
    chat.selected
    chat.thread = null
    if (narrow) details = false
  })

  // The roster's width is the user's to set, and this browser remembers it.
  const widthOf = () => Math.min(420, Math.max(240, app.settings.rosterWidth ?? 296))
  function resize(event) {
    const start = { x: event.clientX, width: widthOf() }
    const move = (e) => (app.settings.rosterWidth = Math.round(start.width + e.clientX - start.x))
    const stop = () => {
      removeEventListener('pointermove', move)
      removeEventListener('pointerup', stop)
    }
    addEventListener('pointermove', move)
    addEventListener('pointerup', stop)
  }

  const closePanel = () => (chat.thread ? (chat.thread = null) : (details = false))
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && chat.thread && !chat.editor && !chat.trace && (chat.thread = null)} />

<div class="messages" class:narrow style:--side="{widthOf()}px">
  {#if !narrow || (!bot && !chat.composing)}
    <div class="side">
      <Roster />
      {#if !narrow}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="grip" title="Drag to resize" onpointerdown={resize} ondblclick={() => (app.settings.rosterWidth = 296)}></div>
      {/if}
    </div>
  {/if}
  {#if !narrow || bot || chat.composing}
    <div class="stage" class:panel={panel && wide} style:--panel="{panel === 'thread' ? 'min(420px, 45%)' : '292px'}" bind:clientWidth={width}>
      <main>
        {#if chat.composing}
          <NewChat onclose={() => (chat.composing = false)} />
        {:else if bot}
          {#key bot.id}
            <Conversation botId={bot.id} details={panel === 'details'} ontoggle={() => ((chat.thread = null), (details = !details))} onback={narrow ? () => (chat.selected = null) : undefined} />
          {/key}
        {:else}
          <div class="welcome">
            <div class="crew">
              <Avatar shape="blob" color="blue" size={60} mood="working" />
              <Avatar shape="squircle" color="orange" size={60} />
              <Avatar shape="teardrop" color="violet" size={60} mood="working" />
            </div>
            <h1>Your coding agents, as teammates.</h1>
            <p>Pick a bot, or make one for each kind of work and give it a workspace.</p>
            <button type="button" class="pill press" onclick={() => (chat.editor = { bot: null })}>New bot</button>
          </div>
        {/if}
      </main>
      {#if panel && !chat.composing}
        <div class="aside" class:over={!wide}>
          {#if panel === 'thread'}
            {#key chat.thread}<Replies botId={bot.id} root={chat.thread} onclose={closePanel} />{/key}
          {:else}
            <Details botId={bot.id} onclose={closePanel} />
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  {#if chat.editor}
    {#if 'group' in chat.editor}
      <GroupEditor group={chat.editor.group} onclose={() => (chat.editor = null)} />
    {:else}
      <BotEditor bot={chat.editor.bot} workspace={chat.editor.workspace} onclose={() => (chat.editor = null)} />
    {/if}
  {/if}
  <Toast />
  {#if chat.trace}<Trace {...chat.trace} onclose={() => (chat.trace = null)} />{/if}
</div>

<style>
  .messages { display: grid; grid-template-columns: var(--side) minmax(0, 1fr); height: 100vh; overflow: hidden; }
  .messages.narrow { grid-template-columns: minmax(0, 1fr); }
  .side { position: relative; display: grid; grid-template: minmax(0, 1fr) / minmax(0, 1fr); min-width: 0; min-height: 0; overflow: hidden; border-right: 1px solid var(--m-border); }
  .grip { position: absolute; top: 0; right: 0; bottom: 0; z-index: 10; width: 6px; cursor: col-resize; }
  .grip:hover { background: var(--bg-active); }
  .stage { position: relative; display: grid; grid-template-columns: minmax(0, 1fr); min-width: 0; min-height: 0; transition: grid-template-columns 0.3s var(--m-spring); }
  .stage.panel { grid-template-columns: minmax(0, 1fr) var(--panel); }
  main { min-width: 0; min-height: 0; }
  .aside { min-width: 0; min-height: 0; overflow: hidden; border-left: 1px solid var(--m-border); }
  .aside.over { position: absolute; inset: 0; z-index: 20; border-left: 0; animation: slide 0.25s var(--m-spring); }
  @keyframes slide { from { transform: translateX(32px); opacity: 0; } }
  .welcome { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 14px; height: 100%; padding: 32px; text-align: center; }
  .crew { display: flex; }
  .crew > :global(canvas + canvas) { margin-left: -10px; }
  h1 { margin: 0; font-size: 22px; font-weight: 650; }
  .welcome p { max-width: 420px; margin: 0; color: var(--m-secondary); }
  @media (prefers-reduced-motion: reduce) { .stage { transition: none; } .aside.over { animation: none; } }
</style>
