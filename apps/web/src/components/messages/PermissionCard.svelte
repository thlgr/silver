<!-- An approval the bot is waiting on, in Codync's choice-card style: what it wants, where it
     runs and the answers as a list of rows. A question the bot asked the user is the same card
     with its options. -->
<script>
  import { currentWorkspace } from '../../lib/state.svelte.js'
  import { answer } from '../../lib/chat.svelte.js'
  import { describe } from '../../lib/tools.js'
  import { markdown } from '../../lib/markdown.js'
  import IconChevron from '~icons/lucide/chevron-down'
  import IconPc from '~icons/lucide/monitor'

  let { entry } = $props()
  const card = $derived(entry.permission)
  const pending = $derived(card.status === 'pending')
  const question = $derived(card.tool === 'ask_user_question')
  const options = $derived(Array.isArray(card.arguments?.options) ? card.arguments.options : [])
  const info = $derived(describe({ kind: 'tool', name: card.tool, args: card.arguments, status: 'ask' }, currentWorkspace()?.path))
  const headline = $derived(
    { terminal: 'Wants to run a command', 'file-diff': 'Wants to change files', 'file-plus': 'Wants to change files', globe: 'Wants to access the web', 'file-text': 'Wants to read files', folder: 'Wants to read files', search: 'Wants to read files' }[info.icon] ?? 'Wants to use a tool',
  )
  const command = $derived(card.arguments?.command ?? card.arguments?.code ?? '')
  const detail = $derived(card.description && card.description !== `${card.tool} needs your approval to proceed` ? card.description : '')
  let expanded = $state(false)
  let sent = $state(null) // the answer on its way to the server
  let typed = $state('')

  const OUTCOME = { approved: 'Allowed', denied: 'Denied', expired: 'Expired — the bot moved on' }
  function respond(decision, text) {
    sent = decision + (text ?? '')
    answer(entry, decision, text)
  }
  const rows = [
    { decision: 'approve', label: 'Allow once', strong: true },
    { decision: 'approve_always', label: 'Always allow' },
    { decision: 'deny', label: 'Deny', bad: true },
  ]
</script>

<div class="card">
  <div class="head">
    <strong>{question ? 'Has a question' : headline}</strong>
    {#if pending}<span class="dot"></span>{/if}
  </div>
  {#if question}
    <div class="prose ask">{@html markdown(card.description)}</div>
  {:else}
    <code class="what">{info.verb} {info.target}</code>
    <span class="where"><IconPc /> Runs on this computer{currentWorkspace() ? ` · ${currentWorkspace().name}` : ''}</span>
    {#if command || detail}
      <button type="button" class="more" aria-expanded={expanded} onclick={() => (expanded = !expanded)}>Details <IconChevron style="transform: rotate({expanded ? 180 : 0}deg)" /></button>
      {#if expanded}
        <div class="body">
          {#if detail}<p>{detail}</p>{/if}
          {#if command}<pre>{command}</pre>{/if}
        </div>
      {/if}
    {/if}
  {/if}

  {#if pending && question}
    <div class="list">
      {#each options as option (option)}
        <button type="button" class="row press" disabled={sent} onclick={() => respond('approve', option)}>{option}</button>
      {/each}
      <form class="row type" onsubmit={(e) => (e.preventDefault(), typed.trim() && respond('approve', typed.trim()))}>
        <input bind:value={typed} placeholder={options.length ? 'Or type your own answer' : 'Type your answer'} disabled={sent} />
        <button type="button" class="skip" disabled={sent} onclick={() => respond('deny')}>Skip</button>
      </form>
    </div>
  {:else if pending}
    <div class="list">
      {#each rows as row (row.decision)}
        <button type="button" class="row press" class:strong={row.strong} class:bad={row.bad} disabled={sent && sent !== row.decision} onclick={() => respond(row.decision)}>
          {row.label}{#if sent === row.decision}<span class="wheel"></span>{/if}
        </button>
      {/each}
    </div>
  {:else}
    <span class="outcome">{OUTCOME[card.status] ?? 'Answered'}</span>
  {/if}
</div>

<style>
  .card { display: flex; flex-direction: column; gap: 8px; max-width: min(460px, 100%); padding: 14px 16px 16px; border-radius: 22px; background: var(--m-agent); }
  .head { display: flex; align-items: center; gap: 8px; font-size: 15px; }
  .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--m-warning); }
  .what { overflow: hidden; color: var(--m-secondary); font-size: var(--text-sm); display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow-wrap: anywhere; }
  .where { display: flex; align-items: center; gap: 6px; color: var(--m-tertiary); font-size: var(--text-xs); }
  .where :global(svg) { width: 13px; height: 13px; }
  .more { display: inline-flex; align-items: center; gap: 4px; align-self: flex-start; padding: 0; border: 0; background: none; color: var(--m-secondary); font-size: var(--text-xs); font-weight: 500; }
  .more :global(svg) { width: 12px; height: 12px; transition: transform 0.2s var(--m-spring); }
  .body { display: grid; gap: 6px; }
  .body p { margin: 0; color: var(--m-secondary); font-size: var(--text-xs); }
  .body pre { max-height: 240px; margin: 0; padding: 8px; overflow: auto; border-radius: 8px; background: var(--m-code); font-size: 12px; white-space: pre-wrap; overflow-wrap: anywhere; }
  .ask { font-size: var(--text-md); }
  .list { display: flex; flex-direction: column; margin-top: 4px; overflow: hidden; border-radius: 14px; background: var(--m-bg); }
  .list > * + * { border-top: 1px solid var(--m-border); }
  .row { display: flex; align-items: center; justify-content: space-between; gap: 8px; min-height: 44px; padding: 0 14px; border: 0; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .row:hover:not(:disabled) { background: var(--bg-hover); }
  .row:disabled { opacity: 0.4; }
  .row:disabled:has(.wheel) { opacity: 1; }
  .row.strong { font-weight: 600; }
  .row.bad { color: var(--m-danger); }
  .type input { flex: 1; min-width: 0; height: 100%; padding: 0; border: 0; outline: 0; background: none; color: var(--m-text); font-size: var(--text-md); }
  .skip { padding: 0; border: 0; background: none; color: var(--m-secondary); font-size: var(--text-sm); }
  .outcome { color: var(--m-secondary); font-size: var(--text-xs); font-weight: 500; }
  .wheel { width: 14px; height: 14px; border: 2px solid var(--m-border); border-top-color: var(--m-text); border-radius: 50%; animation: turn 0.8s linear infinite; }
  @keyframes turn { to { transform: rotate(360deg); } }
  @media (prefers-reduced-motion: reduce) { .wheel { animation: none; } }
</style>
