<!-- One tool call row: a verb, a target, an outcome, and its body once opened. `sub` renders a
     24px sub-row inside an Explored group, sharing that group's column widths via subgrid. -->
<script>
  import { untrack } from 'svelte'
  import { describe } from '../lib/tools.js'
  import { seconds } from '../lib/format.js'
  import { toolIcon } from '../lib/tool-icons.js'
  import ToolBody from './ToolBody.svelte'
  import IconChevron from '~icons/lucide/chevron-right'

  let { step, root, sub = false } = $props()

  const info = $derived(describe(step, root))
  const Icon = $derived(toolIcon(info.icon))
  let open = $state(untrack(() => info.open))
  // describe() only decides the default once; a reader's own toggle must not be overridden by
  // later re-renders (e.g. the row's meta changing when the result arrives).
  let openedByUser = false
  // A call waiting for approval shows its command/diff on the card, not twice: stay collapsed.
  $effect(() => { if (!openedByUser) open = info.open && step.status !== 'waiting' })

  let now = $state(Date.now())
  $effect(() => {
    if (step.status !== 'running' || !step.start) return
    const id = setInterval(() => (now = Date.now()), 1000)
    return () => clearInterval(id)
  })

  const duration = $derived(
    step.status === 'running' && step.start ? now - step.start
    : step.ms,
  )
  const durationLabel = $derived(duration >= 1000 ? seconds(duration) : '')

  const meta = $derived(
    step.status === 'denied' ? 'denied'
    : step.status === 'blocked' ? 'blocked'
    : step.status === 'waiting' ? (step.name === 'ask_user_question' ? 'needs your answer' : '')
    : step.status === 'failed' ? (info.meta || 'failed')
    : info.meta,
  )
  const failed = $derived(['blocked', 'failed'].includes(step.status))
  // An edit's "+2 −10 ×3": the counts take the diff's green and red.
  const counts = $derived(/^(\+\d+) (−\d+)(.*)$/.exec(meta ?? ''))

  function toggle() {
    openedByUser = true
    open = !open
  }
</script>

<div class="call" class:sub>
  <button class="row" class:sub aria-expanded={open} onclick={toggle}>
    <IconChevron class="chevron" style={open ? 'transform: rotate(90deg)' : ''} />
    <Icon class="tool-icon" />
    <span class="verb" class:running={step.status === 'running'}>{info.verb}</span>
    <span class="target mono">{info.target}</span>
    <span class="meta" class:danger={failed}>
      {#if counts}<span class="add">{counts[1]}</span><span class="del">{counts[2]}</span>{counts[3]}{:else if meta}{meta}{/if}
      {#if durationLabel}<span class="dur">{durationLabel}</span>{/if}
    </span>
  </button>
  {#if open}
    <div class="body">
      <ToolBody {step} kind={info.body} />
    </div>
  {/if}
</div>

<style>
  .call.sub { display: contents; }
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
    text-align: left;
  }
  .row.sub { grid-column: 1 / -1; grid-template-columns: subgrid; min-height: 24px; }
  .row :global(.chevron), .row :global(.tool-icon) { width: 16px; height: 16px; flex: none; color: var(--ink-3); }
  .row :global(.chevron) { transition: transform 0.15s; }
  .row.sub :global(.chevron) { visibility: hidden; }
  .verb { color: var(--ink); font-size: var(--text-sm); font-weight: 500; }
  .verb.running { animation: breathe 1.6s ease-in-out infinite; }
  @media (prefers-reduced-motion: reduce) { .verb.running { animation: none; } }
  .target { min-width: 0; overflow: hidden; color: var(--ink-2); font-size: var(--text-xs); white-space: nowrap; text-overflow: ellipsis; }
  .meta { flex: none; display: flex; gap: var(--space-2); margin-left: auto; color: var(--ink-3); font-size: var(--text-xs); font-variant-numeric: tabular-nums; }
  .meta.danger { color: var(--danger); }
  .meta .add { color: var(--success); }
  .meta .del { color: var(--danger); }
  @keyframes breathe { 50% { opacity: 0.4; } }

  .body { margin: var(--space-1) 0 var(--space-3) var(--space-6); color: var(--ink-2); font-size: var(--text-sm); }
  .call.sub > .body { grid-column: 1 / -1; }
</style>
