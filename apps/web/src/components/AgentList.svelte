<!-- The agents one delegate_task call started; a row opens its work in the panel. -->
<script>
  import { app, openAgent, currentWorkspace } from '../lib/state.svelte.js'
  import { agents, activity, agentName } from '../lib/tools.js'
  import { seconds } from '../lib/format.js'
  import IconUsers from '~icons/lucide/users'
  import IconLoader from '~icons/lucide/loader-circle'
  import IconCheck from '~icons/lucide/circle-check'
  import IconX from '~icons/lucide/circle-x'
  import IconHand from '~icons/lucide/hand'
  import IconStop from '~icons/lucide/circle-stop'
  import IconChevron from '~icons/lucide/chevron-right'

  let { step } = $props()

  const MARKS = { running: IconLoader, waiting: IconHand, completed: IconCheck, stopped: IconStop }
  const list = $derived(agents(step))
  const working = $derived(list.filter((a) => a.status === 'running' || a.status === 'waiting').length)
  const summary = $derived.by(() => {
    const plural = (n) => `${n} agent${n === 1 ? '' : 's'}`
    if (working) return `${plural(working)} working`
    const count = (status) => list.filter((a) => a.status === status).length
    const failed = list.length - count('completed') - count('stopped')
    const parts = [[count('completed'), 'finished'], [failed, 'failed'], [count('stopped'), 'stopped']].filter(([n]) => n)
    if (parts.length === 1) return `${plural(list.length)} ${parts[0][1]}`
    return parts.map(([n, word]) => `${n} ${word}`).join(', ')
  })

  let now = $state(Date.now())
  $effect(() => {
    if (!working) return
    const id = setInterval(() => (now = Date.now()), 1000)
    return () => clearInterval(id)
  })
  const time = (a) =>
    a.durationMs ? seconds(a.durationMs)
    : a.start && (a.status === 'running' || a.status === 'waiting') ? seconds(Math.max(0, now - a.start))
    : ''
</script>

<div class="agents">
  <div class="head"><IconUsers /> {summary}</div>
  <div class="card">
    {#each list as a (a.index)}
      {@const Mark = MARKS[a.status] ?? IconX}
      <button
        class="agent {a.status}"
        aria-current={app.agent?.tool === step.id && app.agent.index === a.index}
        title="See what this agent did"
        onclick={() => openAgent(step.id, a.index)}
      >
        <Mark class={a.status === 'running' ? 'mark spin' : 'mark'} />
        <span class="task">{a.description}</span>
        <span class="kind">{agentName(a.agent)}</span>
        <IconChevron class="chevron" />
        <span class="now">{activity(a, a.root ?? currentWorkspace()?.path)}</span>
        <span class="time">{time(a)}</span>
      </button>
    {/each}
  </div>
</div>

<style>
  .agents { display: grid; gap: var(--space-2); }
  .head { display: flex; align-items: center; gap: var(--space-2); color: var(--ink-2); font-size: var(--text-sm); font-weight: 500; }
  .head :global(svg) { width: 16px; height: 16px; color: var(--ink-3); }
  .card { display: grid; border: 1px solid var(--line); border-radius: var(--radius-md); overflow: hidden; }
  .agent {
    display: grid;
    grid-template-columns: 16px minmax(0, 1fr) max-content 16px;
    grid-template-areas: 'mark task kind chevron' '. now time chevron';
    align-items: center;
    gap: 2px var(--space-3);
    padding: var(--space-2) var(--space-3);
    border: 0;
    background: none;
    text-align: left;
  }
  .agent + .agent { border-top: 1px solid var(--line); }
  .agent:hover, .agent[aria-current='true'] { background: var(--bg-hover); }
  .agent :global(.mark) { grid-area: mark; width: 16px; height: 16px; color: var(--ink-3); }
  .agent :global(.chevron) { grid-area: chevron; width: 16px; height: 16px; color: var(--ink-3); }
  .running :global(.mark), .waiting :global(.mark) { color: var(--accent); }
  .completed :global(.mark) { color: var(--success); }
  .failed :global(.mark), .denied :global(.mark), .blocked :global(.mark) { color: var(--danger); }
  .task { grid-area: task; overflow: hidden; color: var(--ink); font-size: var(--text-sm); font-weight: 500; text-overflow: ellipsis; white-space: nowrap; }
  .kind { grid-area: kind; color: var(--ink-3); font-size: var(--text-xs); }
  .now { grid-area: now; overflow: hidden; color: var(--ink-2); font-size: var(--text-xs); text-overflow: ellipsis; white-space: nowrap; }
  .waiting .now { color: var(--ink); }
  .failed .now, .denied .now, .blocked .now { color: var(--danger); }
  .time { grid-area: time; justify-self: end; color: var(--ink-3); font-size: var(--text-xs); font-variant-numeric: tabular-nums; }
</style>
