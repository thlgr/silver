<!--
  Prompt input. Enter sends (queued while a run is active), Ctrl/Cmd+Enter steers the active
  run, Shift+Enter adds a line, Up recalls history, Shift+Tab cycles the approval mode.
-->
<script>
  import { tick } from 'svelte'
  import { app, notify, submit, steer, stop, setApprovalMode, setYolo, formatInterval, currentPreset, setPreset, planMode, setPlanMode, attachFiles, dropAttachment, splitAttachments, draftKey, getDraft, setDraft } from '../lib/state.svelte.js'
  import { matchCommands, runCommand } from '../lib/commands.js'
  import { tokens, cost, percent, contextTokens, perSecond } from '../lib/format.js'
  import Popover from './Popover.svelte'
  import ModelPicker from './ModelPicker.svelte'
  import Approval from './Approval.svelte'
  import IconUp from '~icons/lucide/arrow-up'
  import IconStop from '~icons/lucide/square'
  import IconDown from '~icons/lucide/chevron-down'
  import IconShield from '~icons/lucide/shield'
  import IconCheck from '~icons/lucide/check'
  import IconWrench from '~icons/lucide/wrench'
  import IconPaperclip from '~icons/lucide/paperclip'
  import IconFileText from '~icons/lucide/file-text'
  import IconX from '~icons/lucide/x'

  let text = $state('')
  let input
  let picker
  let pick = $state(0)
  let recall = -1
  let dropping = $state(false)

  const MODES = { manual: 'Ask before changes', smart: 'Ask for risky changes', off: 'Never ask' }
  const commands = $derived(/^\/\S*$/.test(text) ? matchCommands(text) : [])
  const yolo = $derived(app.session ? app.session.yolo_mode : app.pendingYolo)
  const plan = $derived(planMode())
  const preset = $derived(currentPreset())
  const spent = $derived(app.turns.reduce((sum, t) => sum + (t.cost ?? 0), 0))
  const speed = $derived(perSecond(app.rate.bytes, app.rate.ms, app.tokensPerByte))
  const summary = (p) => (!p || !p.tools.length ? 'no tools' : p.tools.length === 1 ? p.tools[0] : `${p.tools.length} tools`)
  const signedOut = $derived(app.providers.length > 0 && !app.providers.some((p) => p.id === app.activeProvider && p.authenticated))

  const key = $derived(draftKey())
  let shown // the chat whose draft is in the box: a chat change swaps the text before it is saved
  $effect(() => {
    if (key === shown) return
    shown = key
    text = getDraft(key)
    app.attachments = []
  })
  $effect(() => {
    if (key === shown) setDraft(key, text)
  })
  $effect(() => {
    commands.length
    pick = 0
  })
  $effect(() => {
    if (app.draft == null) return
    text = text.trim() ? `${app.draft}\n\n${text}` : app.draft
    app.draft = null
    input.focus()
  })
  $effect(() => {
    text
    // Text set by code reaches the box after this effect, so measure once it is there.
    tick().then(() => {
      input.style.height = 'auto'
      input.style.height = `${Math.min(input.scrollHeight, 280)}px`
    })
  })

  /** During a drag the browser hides the files, so the `Files` type is the only honest test:
   *  answering true for dragged text would swallow a drop the textarea should handle itself. */
  function isFileDrag(e) {
    return [...(e.dataTransfer?.types ?? [])].includes('Files')
  }

  function ondragover(e) {
    if (!isFileDrag(e)) return
    e.preventDefault()
    dropping = true
  }

  function ondragleave(e) {
    // Moving onto a child of the box fires this too; only leaving the box clears the outline.
    if (!e.currentTarget.contains(e.relatedTarget)) dropping = false
  }

  function ondrop(e) {
    dropping = false
    if (!isFileDrag(e)) return
    e.preventDefault()
    attachFiles([...e.dataTransfer.files])
  }

  function onpaste(e) {
    const files = [...(e.clipboardData?.files ?? [])]
    if (!files.length) return
    e.preventDefault()
    attachFiles(files)
  }

  function send(steering = false) {
    // An attachment with no words is a complete message: the marker names the file, and the
    // model opens it. Requiring text here would be a dead end.
    const prompt = text.trim()
    if (!prompt && !app.attachments.length) return
    app.history.push(prompt)
    recall = -1
    text = ''
    if (prompt.startsWith('/')) runCommand(prompt)
    else if (steering && app.run) steer(prompt)
    else submit(prompt)
  }

  function complete(command, enter = false) {
    // Enter on a fully typed command whose argument is optional runs it; otherwise the name
    // is filled in so an argument can follow.
    const typed = text.slice(1).toLowerCase()
    const exact = typed === command.name || command.aliases.includes(typed)
    const run = !command.usage || (enter && exact && command.usage.startsWith('['))
    text = `/${command.name}${run ? '' : ' '}`
    if (run) send()
    input.focus()
  }

  function onkeydown(e) {
    if (commands.length) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault()
        pick = (pick + (e.key === 'ArrowDown' ? 1 : -1) + commands.length) % commands.length
        return
      }
      if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
        e.preventDefault()
        return complete(commands[pick], e.key === 'Enter')
      }
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault()
      return send(e.ctrlKey || e.metaKey)
    }
    if (e.key === 'Tab' && e.shiftKey) {
      e.preventDefault()
      const order = ['manual', 'smart', 'off']
      const mode = order[(order.indexOf(app.approvals?.mode) + 1) % 3]
      notify(`Approvals: ${MODES[mode]}, all sessions`)
      return setApprovalMode(mode)
    }
    const browsing = recall >= 0 || !text
    if (browsing && (e.key === 'ArrowUp' || e.key === 'ArrowDown') && app.history.length) {
      e.preventDefault()
      recall = Math.max(-1, Math.min(app.history.length - 1, recall + (e.key === 'ArrowUp' ? 1 : -1)))
      text = recall < 0 ? '' : app.history.at(-1 - recall)
    }
  }
</script>

<div class="dock">
  {#if app.approval}<Approval />{/if}
  {#if app.queued}
    <div class="bar">
      <span class="muted">Queued</span> <span class="text">{splitAttachments(app.queued.content).words}</span>
      <button class="chip" onclick={() => (app.queued = null)}>Cancel</button>
    </div>
  {/if}
  {#if signedOut}
    <div class="bar">
      <span class="text">No model is connected yet.</span>
      <button class="btn primary" onclick={() => (app.settingsTab = 'providers')}>Connect a model</button>
    </div>
  {/if}

  <div class="composer">
    {#if commands.length}
      <div class="menu commands" role="listbox">
        <div class="menu-label">Commands</div>
        {#each commands as command, i (command.name)}
          <button class="menu-item" class:active={i === pick} role="option" aria-selected={i === pick} onpointerenter={() => (pick = i)} onclick={() => complete(command)}>
            <span class="name">/{command.name}</span>
            <span class="muted">{command.usage}</span>
            <span class="hint">{command.summary}</span>
          </button>
        {/each}
      </div>
    {/if}

    {#if app.attachments.length}
      <div class="attachments">
        {#each app.attachments as attachment, i (attachment.path)}
          <span class="attachment">
            {#if attachment.preview}<img src={attachment.preview} alt="" />{:else}<IconFileText />{/if}
            <span class="name">{attachment.name}</span>
            <button class="drop" title="Remove" onclick={() => dropAttachment(i)}><IconX /></button>
          </span>
        {/each}
      </div>
    {/if}

    <div class="input" class:dropping {ondragover} {ondragleave} {ondrop}>
      <textarea
        bind:this={input}
        bind:value={text}
        {onkeydown}
        {onpaste}
        rows="1"
        placeholder={app.run ? 'Queue a follow-up, or Ctrl+Enter to steer the run' : plan ? 'Describe what to plan, /plan shows the plan' : 'Message silver, / for commands'}
      ></textarea>
    </div>

    <div class="toolbar">
      <input bind:this={picker} type="file" multiple hidden
        onchange={(e) => (attachFiles([...e.target.files]), (e.target.value = ''))} />
      <button class="chip" title="Attach files" onclick={() => picker.click()}>
        <IconPaperclip /> <span class="label">Attach</span>
      </button>
      <Popover label="Approval mode" up>
        {#snippet trigger()}<IconShield /> <span class="label">{plan ? 'Plan mode' : `${MODES[app.approvals?.mode] ?? 'Approvals'}${yolo ? ', YOLO' : ''}`}</span> <IconDown />{/snippet}
        {#snippet children(close)}
          <div class="menu-label">All sessions{app.approvals?.frozen ? ' (locked by config)' : ''}</div>
          {#each Object.entries(MODES) as [mode, label] (mode)}
            <button class="menu-item" disabled={app.approvals?.frozen} onclick={() => (setApprovalMode(mode), close())}>
              {label}
              {#if app.approvals?.mode === mode}<IconCheck class="hint" />{/if}
            </button>
          {/each}
          <div class="menu-rule"></div>
          <div class="menu-label">This session</div>
          <button class="menu-item" onclick={() => (setYolo(!yolo), close())}>
            YOLO, skip every approval
            {#if yolo}<IconCheck class="hint" />{/if}
          </button>
          <button class="menu-item" onclick={() => (setPlanMode(!plan), close())}>
            Plan mode, change nothing until a plan is approved
            {#if plan}<IconCheck class="hint" />{/if}
          </button>
        {/snippet}
      </Popover>
      <Popover label="Preset" up data-preset-picker>
        {#snippet trigger()}<IconWrench /> <span class="label">{preset?.name ?? 'Minimal'}</span> <IconDown />{/snippet}
        {#snippet children(close)}
          <div class="menu-label">Tools and skills for this chat</div>
          {#each app.presets as p (p.id)}
            <button class="menu-item" onclick={() => (setPreset(p.id), close())}>
              <span>{p.name}</span> <span class="muted">{summary(p)}</span>
              {#if preset?.id === p.id}<IconCheck class="hint" />{/if}
            </button>
          {/each}
          <div class="menu-rule"></div>
          <button class="menu-item" onclick={() => ((app.settingsTab = 'presets'), close())}>Edit presets…</button>
        {/snippet}
      </Popover>
      <div class="end">
        <ModelPicker />
        {#if app.run}
          <button class="send" title="Stop" aria-label="Stop" onclick={stop}><IconStop /></button>
        {:else}
          <button class="send" title="Send" aria-label="Send" disabled={!text.trim() && !app.attachments.length} onclick={() => send()}><IconUp /></button>
        {/if}
      </div>
    </div>
  </div>

  <div class="status">
    {#if app.notice}
      <span class:error={app.notice.error}>{app.notice.text}</span>
    {:else}
      {#if app.context}<button onclick={() => (app.panel = 'usage')}>Context {percent(app.context)}%, {app.context.prompt_tokens ? '' : '~'}{tokens(contextTokens(app.context, app.tokensPerByte))} tokens</button>{/if}
      {#if speed}<span title="Text tokens per second, at the provider's own token ratio. Only the time the provider streamed counts: a tool call, a wait for approval or a stall does not.">{speed}</span>{/if}
      {#if spent}<span>{cost(spent)}</span>{/if}
      {#if app.session?.goal}{@const goal = app.session.goal}<span>Goal {goal.used}/{goal.max}{goal.status === 'active' ? '' : `, ${goal.status}`}</span>{/if}
      {#if app.loop}<span>Loop {app.loop.interval ? `every ${formatInterval(app.loop.interval)}` : 'per turn'}, {app.loop.fired} run{app.loop.fired === 1 ? '' : 's'}{app.loop.status === 'running' ? '' : `, ${app.loop.status}`}</span>{/if}
      {#if app.heartbeat}<span>Heartbeat every {formatInterval(app.heartbeat.interval)}{app.heartbeat.status === 'running' ? '' : ', paused'}</span>{/if}
    {/if}
  </div>
</div>

<style>
  .dock { display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--space-2); }
  .bar { display: flex; align-items: center; gap: var(--space-2); padding: 0 var(--space-2); font-size: var(--text-sm); }
  .bar .text { flex: 1; min-width: 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }

  .composer {
    position: relative;
    padding: var(--space-3) var(--space-3) var(--space-2);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-raised);
  }
  .composer:focus-within { border-color: var(--ink-3); }
  textarea {
    display: block;
    width: 100%;
    min-height: 48px;
    padding: 0 var(--space-1);
    border: 0;
    outline: 0;
    background: none;
    resize: none;
    line-height: 1.6;
  }
  textarea::placeholder { color: var(--ink-3); }
  /* On a narrow window the chips wrap onto a second row rather than truncating to "M…";
     the model and send button stay together on the right. */
  .attachments { display: flex; flex-wrap: wrap; gap: var(--space-1); margin-bottom: var(--space-2); }
  .attachment {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    max-width: 220px;
    padding: 2px var(--space-1) 2px 2px;
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: var(--bg-sunken);
    font-size: var(--text-xs);
    color: var(--ink-2);
  }
  .attachment :global(img), .attachment :global(svg) { flex: none; width: 18px; height: 18px; }
  .attachment :global(img) { width: 22px; height: 22px; border-radius: 2px; object-fit: cover; }
  .attachment .name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .attachment .drop { display: grid; place-items: center; flex: none; padding: 0; border: 0; background: none; color: var(--ink-3); }
  .attachment .drop:hover { color: var(--danger); }

  /* A file dragged over the box is the one gesture with no visible target, so the box says so. */
  .input { border-radius: var(--radius-sm); transition: box-shadow 120ms; }
  .input.dropping { box-shadow: inset 0 0 0 2px var(--accent); background: var(--bg-sunken); }

  .toolbar { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-1); margin-top: var(--space-2); }
  .end { display: flex; align-items: center; gap: var(--space-1); min-width: 0; margin-left: auto; }
  .toolbar :global(.popover) { min-width: 0; }
  .toolbar :global(.chip) { max-width: 100%; }
  .send {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    margin-left: var(--space-1);
    border: 0;
    border-radius: 50%;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 16px;
  }
  .send:disabled { background: var(--bg-active); color: var(--ink-3); opacity: 1; }

  .commands { left: 0; right: 0; bottom: calc(100% + var(--space-2)); }
  .commands .name { min-width: 88px; font-weight: 500; }
  .commands .hint { text-align: right; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }

  .status {
    display: flex;
    justify-content: center;
    gap: var(--space-4);
    min-height: 20px;
    color: var(--ink-3);
    font-size: var(--text-xs);
  }
  .status button { padding: 0; border: 0; background: none; color: inherit; font-size: inherit; }
  .status button:hover { color: var(--ink); }
</style>
