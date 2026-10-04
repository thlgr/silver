<!-- One exchange: the user's prompt, the process steps, the final reply and its actions. -->
<script>
  import { untrack } from 'svelte'
  import { app, rewind, notify, attachmentSrc } from '../lib/state.svelte.js'
  import { authImage } from '../lib/api.js'
  import { markdown, copyCode } from '../lib/markdown.js'
  import { tokens, seconds, cost } from '../lib/format.js'
  import { isStandalone, finalStep, groupParts, summary } from '../lib/process.js'
  import Step from './Step.svelte'
  import ToolCall from './ToolCall.svelte'
  import AgentList from './AgentList.svelte'
  import IconChevron from '~icons/lucide/chevron-right'
  import IconCopy from '~icons/lucide/copy'
  import IconRetry from '~icons/lucide/rotate-ccw'
  import IconLoader from '~icons/lucide/loader-circle'
  import IconCompass from '~icons/lucide/compass'

  let { turn, last } = $props()

  // Seconds since the run started, so a slow model shows "Thinking…" instead of a bare spinner.
  let now = $state(Date.now())
  $effect(() => {
    if (!turn.running) return
    const id = setInterval(() => (now = Date.now()), 1000)
    return () => clearInterval(id)
  })
  const idle = $derived(
    turn.running && turn.start && !turn.steps.some((s) => s.kind === 'text' || s.kind === 'reasoning' || s.kind === 'tool')
      ? Math.floor((now - turn.start) / 1000)
      : 0,
  )

  const final = $derived(finalStep(turn))
  const process = $derived(turn.steps.filter((s) => s !== final))
  const hidden = $derived(app.settings.focus || app.settings.verbose === 'off')
  // Only the newest tool step keeps showing while a run streams in 'new' verbosity; the rest
  // of the process then groups exactly as it does once finished.
  const parts = $derived.by(() => {
    const lastTool = process.findLastIndex((s) => s.kind !== 'text')
    const shown = turn.running && app.settings.verbose === 'new' ? process.filter((s, i) => isStandalone(s) || i === lastTool) : process
    return groupParts(shown)
  })
  // What the reader watched stream stays open when the run ends, so finishing never reflows
  // the transcript under them; turns loaded from history start collapsed.
  const watched = untrack(() => turn.running)
  let opened = $state({})
  let exploreOpen = $state({})
  const isOpen = (i) => opened[i] ?? (watched && app.settings.verbose === 'all')

  function copy(text) {
    navigator.clipboard.writeText(text)
    notify('Copied')
  }
</script>

{#if turn.note}
  <pre class="note">{turn.note}</pre>
{:else}
  <section class="turn">
    <div class="user">
      <div class="bubble">
        {turn.user}
        {#each turn.attachments ?? [] as file (file.path)}
          {@const src = attachmentSrc(file.path)}
          {#if src}<img class="attachment" use:authImage={src} alt={file.name} />{:else}<span class="attachment file" title={file.path}>{file.name}</span>{/if}
        {/each}
      </div>
      <button class="icon-btn" title="Copy prompt" aria-label="Copy prompt" onclick={() => copy(turn.user)}><IconCopy /></button>
    </div>

    {#if !hidden}
      {#each parts as part, i (i)}
        {#if Array.isArray(part)}
          {@const s = summary(part)}
          <div class="process" class:live={turn.running}>
            {#if !turn.running}
              <button class="summary" class:open={isOpen(i)} onclick={() => (opened[i] = !isOpen(i))}>
                {s.head}{#if s.failed}&nbsp;· <span class="danger">{s.failed} failed</span>{/if}{#if s.denied}&nbsp;· {s.denied} denied{/if}{#if s.hints}&nbsp;· {s.hints} Jev hint{s.hints === 1 ? '' : 's'}{/if}
                <IconChevron />
              </button>
            {/if}
            {#if turn.running || isOpen(i)}
              {#each part as item, j (j)}
                {#if item.kind === 'explore'}
                  {@const groupKey = `${i}-${j}`}
                  {@const running = item.steps.some((step) => step.status === 'running')}
                  <div class="explore">
                    <button class="row" aria-expanded={!!exploreOpen[groupKey]} onclick={() => (exploreOpen[groupKey] = !exploreOpen[groupKey])}>
                      <IconChevron class="chevron" style={exploreOpen[groupKey] ? 'transform: rotate(90deg)' : ''} />
                      <IconCompass class="tool-icon" />
                      <span class="verb">{running ? 'Exploring' : 'Explored'}</span>
                      <span class="target"></span>
                      <span class="meta"></span>
                    </button>
                    {#if exploreOpen[groupKey]}
                      {#each item.steps.filter((s) => s.kind === 'tool') as sub (sub.id)}
                        <ToolCall step={sub} root={currentWorkspace()?.path} sub />
                      {/each}
                    {/if}
                  </div>
                {:else if item.kind === 'tool'}
                  <ToolCall step={item} root={currentWorkspace()?.path} />
                {:else}
                  <Step step={item} />
                {/if}
              {/each}
            {/if}
          </div>
        {:else if part.kind === 'tool'}
          <AgentList step={part} />
        {:else}
          <div class="prose">{@html markdown(part.text)}</div>
        {/if}
      {/each}
    {/if}

    {#if final}
      <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
      <div class="prose answer" onclick={copyCode}>{@html markdown(final.text)}</div>
    {/if}
    {#if turn.pendingSteer}<p class="muted pending">Steer: {turn.pendingSteer} · waiting</p>{/if}
    {#if turn.running && !app.approval}
      <span class="working"><IconLoader class="spin" />{#if idle >= 2}<span class="thinking">Thinking… {idle}s</span>{/if}</span>
    {/if}
    {#if turn.error}<p class="error">{turn.error}</p>{/if}
    {#if turn.stopped}<p class="muted stopped">Stopped</p>{/if}

    {#if !turn.running}
      <div class="meta">
        {#if final}<button class="icon-btn" title="Copy reply" aria-label="Copy reply" onclick={() => copy(final.text)}><IconCopy /></button>{/if}
        {#if last}<button class="icon-btn" title="Retry" aria-label="Retry" onclick={() => rewind(1, true)}><IconRetry /></button>{/if}
        {#if turn.usage}<span title="Sent and generated over every request of this reply; each request resends the context">{tokens(turn.usage.total_tokens)} tokens</span>{/if}
        {#if turn.cost}<span>{cost(turn.cost)}</span>{/if}
        {#if turn.duration}<span>{seconds(turn.duration)}</span>{/if}
      </div>
    {/if}
  </section>
{/if}

<style>
  .turn { display: flex; flex-direction: column; gap: var(--space-3); }
  .user .attachment { max-width: 220px; max-height: 160px; margin-top: var(--space-1); border-radius: var(--radius-sm); border: 1px solid var(--line); }
  .user .attachment.file { display: block; padding: 2px var(--space-2); font-size: var(--text-xs); color: var(--ink-3); background: var(--bg-sunken); }

  .user { display: flex; flex-direction: column; align-items: flex-end; gap: var(--space-1); }
  .bubble {
    max-width: 80%;
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-lg);
    background: var(--bg-bubble);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .user .icon-btn { visibility: hidden; font-size: 14px; }
  .user:hover .icon-btn { visibility: visible; }

  .process { padding-bottom: var(--space-2); border-bottom: 1px solid var(--line); }
  .process.live { border-bottom: 0; }
  .summary {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    height: 30px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--ink-2);
    font-size: var(--text-sm);
  }
  .summary :global(svg) { transition: transform 0.15s; }
  .summary.open :global(svg) { transform: rotate(90deg); }
  .summary .danger { color: var(--danger); }

  /* Column gap lives on the outer grid: a subgrid row's own gap override is not reliable across
     browsers, but the ancestor's is always inherited. */
  .explore { display: grid; grid-template-columns: 16px 16px max-content minmax(0, 1fr) max-content; gap: 2px var(--space-2); }
  .explore > .row {
    grid-column: 1 / -1;
    display: grid;
    grid-template-columns: subgrid;
    align-items: center;
    min-height: 30px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--ink-3);
    text-align: left;
  }
  .explore > .row:hover { color: var(--ink-2); }
  .explore :global(.chevron), .explore :global(.tool-icon) { width: 16px; height: 16px; color: var(--ink-3); }
  .explore :global(.chevron) { transition: transform 0.15s; }
  .explore .verb { color: var(--ink); font-size: var(--text-sm); font-weight: 500; }

  /* As tall as the .meta row that replaces it, so the reply does not move when the run ends. */
  .turn :global(.working) { display: inline-flex; align-items: center; gap: var(--space-2); flex: none; height: 28px; color: var(--ink-3); font-size: var(--text-sm); }
  .thinking { font-size: var(--text-xs); }
  .pending { margin: 0; font-size: var(--text-sm); }
  .error, .stopped { margin: 0; font-size: var(--text-sm); }

  .meta { display: flex; align-items: center; gap: var(--space-3); color: var(--ink-3); font-size: var(--text-xs); }
  .meta .icon-btn { font-size: 14px; margin-right: calc(-1 * var(--space-2)); }

  .note {
    margin: 0;
    padding: var(--space-3) var(--space-4);
    border-left: 2px solid var(--line-strong);
    color: var(--ink-2);
    font-size: var(--text-xs);
    line-height: 1.7;
    white-space: pre-wrap;
  }
</style>
