<!-- One process row inside a turn: a reasoning block, an intermediate reply, a note, an advisor
     (Jev) check with its answers and the hints it injected, or other text silver gave the
     model (the system prompt, a loaded AGENTS.md, a nudge). Tool calls render as ToolCall. -->
<script>
  import { markdown } from '../lib/markdown.js'
  import { app } from '../lib/state.svelte.js'
  import IconChevron from '~icons/lucide/chevron-right'
  import IconLightbulb from '~icons/lucide/lightbulb'
  import IconMessageCircle from '~icons/lucide/message-circle'
  import IconStickyNote from '~icons/lucide/sticky-note'
  import IconSparkles from '~icons/lucide/sparkles'
  import IconScrollText from '~icons/lucide/scroll-text'
  import IconFileDown from '~icons/lucide/file-down'
  import IconMegaphone from '~icons/lucide/megaphone'

  let { step } = $props()
  let open = $state(false)

  const STATUS = { hint: 'Hint injected' }
  const POINT = { start: 'At the start', tools: 'After the tools', answer: 'Before the answer' }
  const ICONS = { reasoning: IconLightbulb, text: IconMessageCircle, note: IconStickyNote, advisor: IconSparkles }
  const title = $derived({ reasoning: 'Thought', text: 'Reply', note: 'Note', advisor: 'Jev', injected: step.label }[step.kind])
  // Injected context varies: the system prompt, a loaded file, or a loop notice (tool guard,
  // iteration budget, verification required, ...) each read differently.
  const Icon = $derived(
    step.kind !== 'injected' ? ICONS[step.kind]
    : step.label?.startsWith('System prompt') ? IconScrollText
    : step.label?.startsWith('Loaded ') ? IconFileDown
    : IconMegaphone,
  )
  const detail = $derived(
    step.kind === 'advisor' ? (step.hints[0]?.replace(/^Hint: /, '') ?? `${POINT[step.point]} · no hint`)
    // A leading wrapper tag (<system-reminder>) says nothing; show the line after it.
    : step.text?.replace(/^<[^>\n]+>\n/, '').split('\n')[0],
  )
  const status = $derived(step.kind === 'advisor' ? (step.hints.length ? 'hint' : '') : '')
  // The decisive answers first; the thresholds match the daemon's (yes at 0.8, no at 0.3).
  const answers = $derived(step.kind === 'advisor' ? Object.entries(step.answers).sort((a, b) => b[1] - a[1]) : [])
  const verdict = (p) => (p >= 0.8 ? 'yes' : p <= 0.3 ? 'no' : 'unsure')
</script>

<div class="step" class:open>
  <button class="row" onclick={() => (open = !open)}>
    <IconChevron class="chevron" />
    <Icon class="tool-icon" />
    <span class="title">{title}</span>
    <span class="detail">{detail}</span>
    {#if STATUS[status]}<span class="status {status}">{STATUS[status]}</span>{/if}
  </button>
  {#if open}
    <div class="body">
      {#if step.kind === 'text'}
        <div class="prose">{@html markdown(step.text)}</div>
      {:else if step.kind === 'injected'}
        <div class="io"><pre>{step.text}</pre></div>
      {:else if step.kind === 'advisor'}
        <p class="muted">{POINT[step.point]}, Jev answered (hover a name for its question):</p>
        <div class="answers">
          {#each answers as [name, p] (name)}
            <span class="name mono" title={app.advisor?.questions?.[name]}>{name}</span>
            <span class="bar {verdict(p)}" title={app.advisor?.questions?.[name]}><span style:width="{p * 100}%"></span></span>
            <span class="mono {verdict(p)}">{Math.round(p * 100)}%</span>
          {/each}
        </div>
        {#each step.hints as hint, i (i)}
          <div class="io"><span>Sent to the model</span><pre>{hint}</pre></div>
        {/each}
      {:else}
        <p>{step.text}</p>
      {/if}
    </div>
  {/if}
</div>

<style>
  /* Same fixed chevron and icon columns as ToolCall/the explore group, so a reasoning or text
     row lines up with a tool row beside it regardless of icon/font metrics. */
  .row {
    display: grid;
    grid-template-columns: 16px 16px max-content minmax(0, 1fr) max-content;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    min-height: 30px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--ink-3);
    font-size: var(--text-sm);
    text-align: left;
  }
  .row:hover { color: var(--ink-2); }
  .row :global(.chevron), .row :global(.tool-icon) { width: 16px; height: 16px; color: var(--ink-3); }
  .row :global(.chevron) { transition: transform 0.15s; }
  .open .row :global(.chevron) { transform: rotate(90deg); }
  .title { color: var(--ink-2); font-weight: 500; }
  .detail { min-width: 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
  .status { font-size: var(--text-xs); }
  .status.hint { color: var(--accent); }

  .body { margin: var(--space-1) 0 var(--space-3) var(--space-6); color: var(--ink-2); font-size: var(--text-sm); }
  .body p { margin: 0; white-space: pre-wrap; }
  .io { display: grid; gap: var(--space-1); padding: var(--space-2) var(--space-3); border-radius: var(--radius-sm); background: var(--bg-sunken); }
  .io + .io { margin-top: 2px; }
  .io span { color: var(--ink-3); font-size: var(--text-xs); line-height: 20px; }
  .answers { display: grid; grid-template-columns: max-content minmax(60px, 1fr) 40px; gap: 2px var(--space-3); align-items: center; margin: var(--space-2) 0; font-size: var(--text-xs); }
  .answers .name { cursor: help; }
  .bar { height: 6px; border-radius: 3px; background: var(--bg-sunken); overflow: hidden; }
  .bar span { display: block; height: 100%; background: var(--ink-3); }
  .bar.yes span { background: var(--accent); }
  .answers .yes { color: var(--ink); }
  .answers .no { color: var(--ink-3); }
  .muted { color: var(--ink-3); }
  .io pre { margin: 0; max-height: 280px; overflow: auto; white-space: pre-wrap; word-break: break-word; font-size: var(--text-xs); line-height: 20px; }
</style>
