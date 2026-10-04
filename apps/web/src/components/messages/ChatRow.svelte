<!-- One thing in a chat or thread: the user's message, a bot's reply, an approval card or a notice.
     On a desktop pointer a message's actions appear beside its time while it is hovered; the same
     choices are in the menu (right click, or press and hold on a touch screen). -->
<script>
  import { botById, discard, openTrace, react, resend, QUICK_REACTIONS } from '../../lib/chat.svelte.js'
  import { app, notify, splitMessage } from '../../lib/state.svelte.js'
  import { authImage } from '../../lib/api.js'
  import { copyCode, markdown } from '../../lib/markdown.js'
  import { hexOf } from '../../lib/avatar.js'
  import { time } from '../../lib/when.js'
  import Avatar from './Avatar.svelte'
  import Menu from './Menu.svelte'
  import PermissionCard from './PermissionCard.svelte'
  import Reactions from './Reactions.svelte'
  import ThreadChip from './ThreadChip.svelte'
  import IconReply from '~icons/lucide/corner-up-left'
  import IconList from '~icons/lucide/list'
  import IconCopy from '~icons/lucide/copy'
  import IconResend from '~icons/lucide/rotate-cw'
  import IconTrash from '~icons/lucide/trash-2'
  import IconWarn from '~icons/lucide/triangle-alert'
  import IconRequest from '~icons/lucide/arrow-left-right'

  /** `bot` is the chat's bot or group; `onthread(entry)` opens the thread on a message. */
  let { entry, start = false, bot, working = false, inThread = false, onthread } = $props()
  const group = $derived(bot.kind === 'group')
  const author = $derived(botById(entry.author))
  const stamp = $derived(time(entry.created_at))
  // A user's message and the files it names, which are stored in the bot's folder.
  const sent = $derived(entry.kind === 'user' ? splitMessage(entry.text) : null)
  const picture = (path) => (bot.workspace_id && /\.(png|jpe?g|gif|webp)$/i.test(path) ? `/v1/workspaces/${bot.workspace_id}/files?path=${encodeURIComponent(path)}` : null)
  const canThread = $derived(!inThread && !entry.local && entry.kind !== 'notice' && onthread)
  let menu = $state(null)
  let hold = null

  function copy() {
    navigator.clipboard.writeText(entry.text)
    notify('Copied')
  }

  const items = $derived([
    { label: 'Copy', icon: IconCopy, run: copy },
    ...(canThread ? [{ label: 'Reply in thread', icon: IconReply, run: () => onthread(entry) }] : []),
    ...(entry.kind === 'agent' && entry.run_id ? [{ label: 'Show what it did', icon: IconList, run: () => trace() }] : []),
  ])

  const trace = () => openTrace(entry.chat_id, entry.thread_id, entry.run_id)

  function open(event) {
    if (entry.local) return
    event.preventDefault()
    menu = { x: event.clientX, y: event.clientY }
  }

  // Press and hold opens the menu on a touch screen, where there is no hover or right click.
  const press = (event) => {
    if (event.pointerType !== 'touch') return
    const { clientX, clientY } = event
    hold = setTimeout(() => (menu = { x: clientX, y: clientY }), 450)
  }
  const release = () => clearTimeout(hold)
</script>

{#snippet actions()}
  {#if !entry.local}
    <span class="actions">
      {#each QUICK_REACTIONS as emoji (emoji)}
        <button type="button" class="act emoji" class:chosen={entry.reactions?.includes(emoji)} title={entry.reactions?.includes(emoji) ? `Remove ${emoji}` : `React ${emoji}`} onclick={() => react(entry, emoji)}>{emoji}</button>
      {/each}
      <span class="bar"></span>
      {#if canThread}<button type="button" class="act" title="Reply in thread" aria-label="Reply in thread" onclick={() => onthread(entry)}><IconReply /></button>{/if}
      {#if entry.kind === 'agent' && entry.run_id}<button type="button" class="act" title="Show what it did" aria-label="Show what it did" onclick={trace}><IconList /></button>{/if}
      <button type="button" class="act" title="Copy" aria-label="Copy" onclick={copy}><IconCopy /></button>
    </span>
  {/if}
{/snippet}

{#if entry.kind === 'user'}
  <div class="row user" class:start>
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    {#each sent.files as file (file.path)}
      {@const src = picture(file.path)}
      {#if src}<img class="picture" use:authImage={src} alt={file.name} />{:else}<span class="attachment" title={file.path}>{file.name}</span>{/if}
    {/each}
    {#if sent.words}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div class="bubble user" oncontextmenu={open} onpointerdown={press} onpointerup={release} onpointercancel={release} onpointermove={release}>{sent.words}</div>
    {/if}
    <div class="foot">
      {#if !entry.status || entry.status === 'sent'}{@render actions()}<span class="time">{stamp}</span>
      {:else if entry.status === 'sending'}<span class="state">Sending…</span>
      {:else if entry.status === 'queued'}<span class="state">{working ? 'Queued until this response finishes' : 'Queued'}</span>
      {:else if entry.status === 'cancelled'}<span class="state">Not sent — stopped</span>
      {:else if entry.status === 'failed'}
        <span class="failed" title={entry.error}>Failed to send
          <button type="button" class="act" title="Resend" aria-label="Resend" onclick={() => resend(entry)}><IconResend /></button>
          <button type="button" class="act" title="Delete" aria-label="Delete" onclick={() => discard(entry)}><IconTrash /></button>
        </span>
      {/if}
    </div>
    <Reactions {entry} />
    {#if entry.thread && !inThread}<ThreadChip summary={entry.thread} onopen={() => onthread(entry)} />{/if}
  </div>
{:else if entry.kind === 'agent'}
  <div class="row agent" class:start>
    {#if group && start}
      <span class="author">
        {#if author}<Avatar shape={author.avatar_shape} color={author.avatar_color} size={26} />{/if}
        <span class="who" style:color={author ? hexOf(author.avatar_color) : undefined}>{author?.name ?? 'A bot that left'}</span>
      </span>
    {/if}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="bubble agent prose" class:live={!entry.final} oncontextmenu={open} onpointerdown={press} onpointerup={release} onpointercancel={release} onpointermove={release} onclick={copyCode}>{@html markdown(entry.text)}</div>
    <div class="foot"><span class="time">{stamp}</span>{@render actions()}</div>
    <div class="under" class:indent={group}>
      <Reactions {entry} />
      {#if entry.thread && !inThread}<ThreadChip summary={entry.thread} onopen={() => onthread(entry)} />{/if}
    </div>
  </div>
{:else if entry.kind === 'permission'}
  <div class="row agent start">
    {#if group}
      <span class="author">
        {#if author}<Avatar shape={author.avatar_shape} color={author.avatar_color} size={26} />{/if}
        <span class="who" style:color={author ? hexOf(author.avatar_color) : undefined}>{author?.name ?? 'A bot that left'}</span>
      </span>
    {/if}
    <PermissionCard {entry} />
  </div>
{:else if entry.style === 'divider'}
  <div class="divider"><span></span><em>{entry.text}</em><span></span></div>
{:else if entry.style === 'error'}
  <div class="alert">
    <IconWarn /><span>{entry.text}</span>
    {#if /credential|sign in|\/login/i.test(entry.text)}<button type="button" class="fix" onclick={() => (app.settingsTab = 'providers')}>Open Settings</button>{/if}
  </div>
{:else if entry.style === 'request'}
  {@const [heading, ...detail] = entry.text.split('\n')}
  <div class="request" class:bad={entry.status === 'failed' || entry.status === 'cancelled'}>
    <IconRequest />
    <span><strong>{heading}</strong>{#if detail.length}<small>{detail.join('\n')}</small>{/if}</span>
  </div>
{:else}
  <div class="plain">{entry.text}</div>
{/if}

{#if menu}<Menu {...menu} {items} reactions={entry.reactions ?? []} onreact={(emoji) => react(entry, emoji)} onclose={() => (menu = null)} />{/if}

<style>
  .row { display: flex; flex-direction: column; gap: 4px; padding-top: 4px; min-width: 0; }
  .row.start { padding-top: 12px; }
  .row.user { align-items: flex-end; padding-left: 56px; }
  .row.agent { align-items: flex-start; padding-right: 64px; }
  .bubble { max-width: 100%; padding: 8px 14px; border-radius: 22px; font-size: 15px; overflow-wrap: anywhere; -webkit-touch-callout: none; }
  .bubble.user { background: var(--m-user); white-space: pre-wrap; user-select: text; }
  .bubble.agent { background: var(--m-agent); user-select: text; }
  .picture { max-width: min(280px, 100%); max-height: 220px; border-radius: 16px; object-fit: cover; }
  .attachment { padding: 6px 12px; border-radius: 999px; background: var(--m-user); font-size: var(--text-sm); }
  .bubble.agent :global(> :last-child) { margin-bottom: 0; }
  .bubble.agent :global(> :first-child) { margin-top: 0; }
  .author { display: flex; align-items: center; gap: 8px; }
  .who { font-size: var(--text-sm); font-weight: 500; }
  .foot { display: flex; align-items: center; gap: 6px; min-height: 26px; padding: 0 4px; }
  .agent .foot { padding-left: 12px; }
  .time { color: var(--m-tertiary); font-size: 10.5px; }
  .state { color: var(--m-tertiary); font-size: 11px; }
  .failed { display: inline-flex; align-items: center; gap: 8px; color: var(--m-danger); font-size: 11px; font-weight: 700; }
  .actions { display: inline-flex; align-items: center; opacity: 0; transition: opacity 0.12s var(--m-ease); }
  .row:hover .actions, .row:focus-within .actions { opacity: 1; }
  @media (hover: none) { .actions { display: none; } }
  .act { display: inline-grid; place-items: center; width: 26px; height: 26px; padding: 0; border: 0; border-radius: 8px; background: none; color: var(--m-secondary); }
  .act:hover { background: var(--bg-active); color: var(--m-text); }
  .act :global(svg) { width: 14px; height: 14px; }
  .emoji { font-size: 14px; transition: transform 0.12s var(--m-ease); }
  .emoji:hover { transform: scale(1.15); }
  .emoji.chosen { background: var(--bg-active); }
  .bar { width: 1px; height: 12px; margin: 0 4px; background: var(--m-border); }
  .under { display: flex; flex-direction: column; align-items: flex-start; gap: 4px; }
  .under.indent { padding-left: 34px; }
  .row.user > :global(.thread), .row.user > :global(.reactions) { align-self: flex-end; }

  .divider { display: flex; align-items: center; gap: 10px; padding: 14px 0 6px; }
  .divider span { flex: 1; height: 1px; background: var(--m-border); }
  .divider em { max-width: 260px; color: var(--m-tertiary); font-size: 11px; font-style: normal; text-align: center; }
  .plain { padding-top: 10px; color: var(--m-secondary); font-size: var(--text-sm); text-align: center; }
  .alert { display: flex; align-items: flex-start; gap: 10px; margin-top: 12px; padding: 12px; border-radius: 12px; background: color-mix(in srgb, var(--m-danger) 12%, transparent); font-size: var(--text-sm); overflow-wrap: anywhere; user-select: text; }
  .alert { flex-wrap: wrap; }
  .fix { margin-left: 26px; padding: 0; border: 0; background: none; color: var(--m-text); font-size: var(--text-sm); font-weight: 600; text-decoration: underline; text-underline-offset: 2px; }
  .alert :global(svg) { flex: none; width: 16px; height: 16px; margin-top: 1px; color: var(--m-danger); }
  .request { display: flex; align-items: flex-start; gap: 10px; margin-top: 12px; padding: 10px 12px; border-radius: 14px; background: var(--m-surface); color: var(--m-secondary); font-size: var(--text-sm); }
  .request :global(svg) { flex: none; width: 15px; height: 15px; margin-top: 2px; }
  .request strong { display: block; color: var(--m-text); font-weight: 600; overflow-wrap: anywhere; }
  .request small { display: block; margin-top: 2px; font-size: var(--text-xs); white-space: pre-wrap; overflow-wrap: anywhere; }
  .request.bad { color: var(--m-danger); }
  @media (max-width: 720px) {
    .row.user { padding-left: 40px; }
    .row.agent { padding-right: 40px; }
    .bubble { padding: 10px 16px; font-size: 16px; }
  }
</style>
