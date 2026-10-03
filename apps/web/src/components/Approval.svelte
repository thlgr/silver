<!-- The pending tool approval. Keys mirror the TUI: y allow, a session, A always, n deny. A plan
     to approve and a question to answer get their own cards. -->
<script>
  import { app, decide, approveInNewSession, currentWorkspace } from '../lib/state.svelte.js'
  import { describe } from '../lib/tools.js'
  import { toolIcon } from '../lib/tool-icons.js'
  import { markdown, copyCode } from '../lib/markdown.js'
  import ToolBody from './ToolBody.svelte'

  const a = $derived(app.approval)
  const step = $derived({ kind: 'tool', name: a.name, args: a.arguments_preview, status: 'ask' })
  const info = $derived(describe(step, currentWorkspace()?.path))
  const Icon = $derived(toolIcon(info.icon))
  const KEYS = { y: 'approve', a: 'approve_session', A: 'approve_always', n: 'deny' }
  const kind = $derived({ exit_plan_mode: 'plan', ask_user_question: 'question' }[a.name] ?? 'tool')
  const options = $derived(Array.isArray(a.arguments_preview?.options) ? a.arguments_preview.options : [])
  const ACTION = { bash: 'Runs a shell command', run_command: 'Runs a shell command', execute_code: 'Runs code', patch: 'Edits a file', write_file: 'Edits a file' }
  const action = $derived(ACTION[a.name] ?? `${info.verb} ${info.target}`.trim())
  const where = $derived(action.startsWith('Runs') && currentWorkspace() ? ` in ${currentWorkspace().name}` : '')
  // The server sends the risky-command detail, or a generic sentence the plain action repeats.
  const reason = $derived(a.description && a.description !== `${a.name} needs your approval to proceed` ? a.description : '')
  // Parallel subagents can ask for several approvals at once; one is answered at a time.
  const queued = $derived(app.approvalQueue.length)
  let other = $state('')

  // The composer keeps its keys unless it is empty, so typing a follow-up never approves.
  // A question takes digits for its options; y/n would clash with typing an answer.
  function onkeydown(e) {
    const typing = e.target.matches('textarea, input') && e.target.value
    if (typing || e.ctrlKey || e.metaKey || e.altKey) return
    if (kind === 'question') {
      if (e.target.matches('textarea, input') || !options[e.key - 1]) return
      e.preventDefault()
      return decide('approve', options[e.key - 1])
    }
    if (kind === 'plan' && e.key === 's') {
      e.preventDefault()
      return approveInNewSession()
    }
    const decision = kind === 'plan' ? { y: 'approve', n: 'deny' }[e.key] : KEYS[e.key]
    if (!decision) return
    e.preventDefault()
    decide(decision)
  }

  function answer(e) {
    e.preventDefault()
    if (other.trim()) decide('approve', other.trim())
  }
</script>

<svelte:window {onkeydown} />

<div class="approval" role="alertdialog" aria-label={kind === 'plan' ? 'Plan approval' : kind === 'question' ? 'Question' : 'Approval required'}>
  {#if kind === 'plan'}
    <p class="head"><strong>Approve this plan?</strong></p>
    <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
    <div class="plan prose" onclick={copyCode}>{@html markdown(a.description)}</div>
    <div class="actions">
      <button class="btn" onclick={() => decide('deny')}>Keep planning <kbd>n</kbd></button>
      <span class="spacer"></span>
      <button class="btn" onclick={approveInNewSession}>Approve in new session <kbd>s</kbd></button>
      <!-- svelte-ignore a11y_autofocus -->
      <button class="btn primary" autofocus onclick={() => decide('approve')}>Approve and start <kbd>y</kbd></button>
    </div>
  {:else if kind === 'question'}
    <div class="prose question">{@html markdown(a.description)}</div>
    {#if options.length}
      <div class="options">
        {#each options as option, i (i)}
          <button class="btn option" onclick={() => decide('approve', option)}><kbd>{i + 1}</kbd> {option}</button>
        {/each}
      </div>
    {/if}
    <form class="actions" onsubmit={answer}>
      <!-- svelte-ignore a11y_autofocus -->
      <input class="field" bind:value={other} placeholder={options.length ? 'Or type your own answer' : 'Type your answer'} autofocus={!options.length} />
      <button class="btn primary" disabled={!other.trim()}>Answer</button>
      <button class="btn" type="button" onclick={() => decide('deny')}>Skip</button>
    </form>
  {:else}
    <p class="head">
      <Icon class="tool-icon" />
      <strong>{info.verb} <span class="mono">{info.target}</span>?</strong>
      {#if queued > 1}<span class="queued">{queued} approvals waiting</span>{/if}
    </p>
    {#if a.agent}<p class="muted">Asked by the agent working on “{a.agent}”</p>{/if}
    <p class="muted">{action}{where}{#if reason} · {reason}{/if}</p>
    <ToolBody {step} kind={info.body} input />
    <div class="actions">
      <button class="btn" onclick={() => decide('deny')}>Deny <kbd>n</kbd></button>
      <span class="spacer"></span>
      <button class="btn" onclick={() => decide('approve_always')}>Always allow this command <kbd>A</kbd></button>
      <button class="btn" onclick={() => decide('approve_session')}>Allow for session <kbd>a</kbd></button>
      <!-- svelte-ignore a11y_autofocus -->
      <button class="btn primary" autofocus onclick={() => decide('approve')}>Allow once <kbd>y</kbd></button>
    </div>
  {/if}
</div>

<style>
  /* One column the card's width, so a long option or question wraps instead of widening it. */
  .approval {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-3);
    padding: var(--space-4);
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-lg);
    background: var(--bg-raised);
  }
  .head { display: flex; align-items: center; gap: var(--space-2); margin: 0; }
  .head :global(.tool-icon) { flex: none; width: 16px; height: 16px; color: var(--ink-3); }
  .muted { margin: 0; color: var(--ink-3); font-size: var(--text-sm); }
  .queued { margin-left: auto; color: var(--ink-3); font-size: var(--text-xs); }
  .plan, .question { max-height: 50vh; overflow: auto; }
  .question { font-weight: 500; }
  .options { display: grid; gap: var(--space-2); }
  .option {
    justify-content: flex-start;
    align-items: baseline;
    height: auto;
    min-height: 32px;
    padding: var(--space-2) var(--space-3);
    text-align: left;
    white-space: normal;
  }
  .actions { display: flex; flex-wrap: wrap; gap: var(--space-2); }
  .actions .field { flex: 1; width: auto; min-width: 12em; }
  .spacer { flex: 1; }
  kbd { color: inherit; opacity: 0.55; font: var(--text-xs) var(--mono); }
</style>
