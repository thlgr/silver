<!-- One subagent's task, steps and report, in the panel. -->
<script>
  import { app, currentWorkspace } from '../lib/state.svelte.js'
  import { agents, activity, agentName } from '../lib/tools.js'
  import { markdown, copyCode } from '../lib/markdown.js'
  import { seconds } from '../lib/format.js'
  import ToolCall from './ToolCall.svelte'
  import Step from './Step.svelte'
  import IconBack from '~icons/lucide/arrow-left'
  import IconMap from '~icons/lucide/map'

  const host = $derived(app.turns.flatMap((t) => t.steps ?? []).find((s) => s.kind === 'tool' && s.id === app.agent?.tool))
  const agent = $derived(host && agents(host)[app.agent.index])
  // An undone turn takes its agents with it.
  $effect(() => {
    if (!agent) app.agent = null
  })

  const root = $derived(agent?.root ?? currentWorkspace()?.path)
  const meta = $derived(agent && [
    `${agentName(agent.agent)} agent`,
    agent.model && agent.model !== app.model ? agent.model : '',
    agent.durationMs ? seconds(agent.durationMs) : '',
  ].filter(Boolean).join(' · '))
  const long = $derived((agent?.prompt ?? '').split('\n').length > 6 || (agent?.prompt ?? '').length > 480)
  let full = $state(false)
</script>

{#if agent}
  <div class="view">
    <button class="back" onclick={() => (app.agent = null)}><IconBack /> All agents</button>

    <header>
      <h3>{agent.description}</h3>
      <p class="muted">{meta}</p>
      <p class="status {agent.status}">{activity(agent, root)}</p>
    </header>

    <section>
      <h4>Task</h4>
      <div class="task" class:clamped={long && !full}>{agent.prompt || agent.description}</div>
      {#if long}<button class="more" onclick={() => (full = !full)}>{full ? 'Show less' : 'Show more'}</button>{/if}
    </section>

    <section>
      <h4>Steps</h4>
      {#each agent.steps as step, i (step.id ?? i)}
        {#if step.kind === 'tool'}<ToolCall {step} {root} />{:else}<Step {step} />{/if}
      {:else}
        <p class="muted">{agent.status === 'running' ? 'Starting' : 'No steps.'}</p>
      {/each}
    </section>

    {#if agent.status !== 'running' && agent.status !== 'waiting'}
      <section>
        <h4>{agent.status === 'completed' ? 'Report' : 'Why it stopped'}</h4>
        {#if agent.worktree}
          <p class="muted note"><IconMap /> Its changes are in the worktree <span class="mono">{agent.worktree}</span></p>
        {/if}
        {#if agent.status === 'completed' && agent.report}
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
          <div class="prose" onclick={copyCode}>{@html markdown(agent.report)}</div>
        {:else}
          <p class={agent.status === 'stopped' ? 'muted' : 'error'}>
            {agent.report || (agent.status === 'stopped' ? 'The run stopped before this agent could report back.' : 'It gave no reason.')}
          </p>
        {/if}
      </section>
    {/if}
  </div>
{/if}

<style>
  .view { display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--space-6); }
  .back { display: inline-flex; align-items: center; gap: var(--space-2); justify-self: start; padding: 0; border: 0; background: none; color: var(--ink-2); font-size: var(--text-sm); }
  .back:hover { color: var(--ink); }
  .back :global(svg) { width: 16px; height: 16px; }
  header { display: grid; gap: var(--space-1); }
  h3 { margin: 0; font-size: var(--text-lg); font-weight: 600; line-height: 1.4; overflow-wrap: anywhere; }
  h4 { margin: 0 0 var(--space-2); color: var(--ink-3); font-size: var(--text-xs); font-weight: 500; }
  p { margin: 0; }
  .status { color: var(--ink-2); font-size: var(--text-sm); }
  .status.waiting { color: var(--ink); font-weight: 500; }
  .status.completed { color: var(--success); }
  .status.failed, .status.denied, .status.blocked { color: var(--danger); }
  .task {
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-md);
    background: var(--bg-bubble);
    font-size: var(--text-sm);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .task.clamped { display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 6; line-clamp: 6; overflow: hidden; }
  .more { margin-top: var(--space-1); padding: 0; border: 0; background: none; color: var(--ink-3); font-size: var(--text-xs); }
  .more:hover { color: var(--ink-2); }
  .note { display: flex; align-items: center; gap: var(--space-2); margin-bottom: var(--space-2); font-size: var(--text-xs); }
  .note :global(svg) { flex: none; width: 14px; height: 14px; }
  .error { color: var(--danger); }
</style>
