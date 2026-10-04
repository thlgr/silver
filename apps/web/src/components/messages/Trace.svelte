<!-- "Full conversation": everything a bot did, turn by turn — reasoning, tool calls with their
     output, plans and the replies. The chat keeps only the messages; this is where the rest is. -->
<script>
  import { currentWorkspace } from '../../lib/state.svelte.js'
  import { markdown } from '../../lib/markdown.js'
  import { groupParts } from '../../lib/process.js'
  import Step from '../Step.svelte'
  import ToolCall from '../ToolCall.svelte'
  import AgentList from '../AgentList.svelte'
  import Sheet from './Sheet.svelte'

  let { title, turns, onclose } = $props()
  const root = $derived(currentWorkspace()?.path)
  const rows = (turn) => groupParts(turn.steps)
  const reply = (turn) => turn.steps.findLast((step) => step.kind === 'text')
</script>

<Sheet {title} width={620} {onclose}>
  {#each turns as turn, t (t)}
    {@const final = reply(turn)}
    <h3 class="card-title turn">{turn.user || `Turn ${t + 1}`}</h3>
    <div class="card trace">
      {#each rows(turn) as part, i (i)}
        {#if Array.isArray(part)}
          {#each part as item, j (j)}
            {#if item.kind === 'explore'}
              <div class="group">
                <div class="label">Explored</div>
                {#each item.steps.filter((s) => s.kind === 'tool') as sub (sub.id)}<ToolCall step={sub} {root} sub />{/each}
              </div>
            {:else if item.kind === 'tool'}
              <ToolCall step={item} {root} />
            {:else if item.kind === 'text'}
              <div class="said"><span>{item === final ? 'Reply' : 'Said'}</span><div class="prose">{@html markdown(item.text)}</div></div>
            {:else}
              <Step step={item} />
            {/if}
          {/each}
        {:else if part.kind === 'tool'}
          <AgentList step={part} />
        {:else if part.kind === 'text'}
          <div class="said"><span>{part === final ? 'Reply' : 'Said'}</span><div class="prose">{@html markdown(part.text)}</div></div>
        {:else}
          <Step step={part} />
        {/if}
      {/each}
      {#if turn.error}<div class="said bad">{turn.error}</div>{/if}
      {#if turn.stopped}<div class="said"><span>Stopped</span></div>{/if}
    </div>
  {:else}
    <p class="none">Nothing yet.</p>
  {/each}
</Sheet>

<style>
  .turn { overflow: hidden; font-size: var(--text-sm); text-overflow: ellipsis; white-space: nowrap; }
  .trace { padding: 6px 4px; }
  .trace > :global(*) { border-top: 0; }
  .group { margin-left: 8px; }
  .label { padding: 4px 8px; color: var(--m-tertiary); font-size: var(--text-xs); font-weight: 500; }
  .said { display: flex; flex-direction: column; gap: 2px; padding: 8px 12px; font-size: var(--text-sm); }
  .said > span { color: var(--m-tertiary); font-size: 11px; font-weight: 700; }
  .said.bad { color: var(--m-danger); }
  .none { margin: 16px 4px; color: var(--m-tertiary); }
</style>
