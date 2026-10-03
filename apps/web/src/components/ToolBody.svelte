<!-- The open body of a tool row: picks a renderer from `kind` (describe().body). `input` shows
     the call's arguments instead of its result, for the approval card. -->
<script>
  import { result, preview, diff, withLineNumbers } from '../lib/tools.js'
  import { authImage } from '../lib/api.js'
  import { currentWorkspace } from '../lib/state.svelte.js'
  import IconPending from '~icons/lucide/square'
  import IconActive from '~icons/lucide/square-dot'
  import IconDone from '~icons/lucide/square-check'

  let { step, kind, input = false } = $props()

  const args = $derived(step.args ?? {})
  const command = $derived(args.command ?? args.code)
  const parsed = $derived(result(step.output))
  const text = $derived(parsed.text ?? '')
  const shown = $derived(preview(text))
  let expanded = $state(false)

  const startLine = $derived.by(() => {
    const match = /at line (\d+)/.exec(text)
    return match ? Number(match[1]) : null
  })
  const diffRows = $derived.by(() => {
    const rows = diff(args.old_string, args.new_string)
    return startLine ? withLineNumbers(rows, startLine) : rows
  })
  const diffShown = $derived(expanded ? diffRows : diffRows.slice(0, 12))

  const contentLines = $derived((typeof args.content === 'string' ? args.content : '').split('\n'))
  const contentShown = $derived(input || expanded ? contentLines : contentLines.slice(0, 8))

  const todos = $derived(parsed.envelope?.todos ?? args.todos ?? [])
  const TODO_MARK = { completed: IconDone, in_progress: IconActive, cancelled: IconPending }

  // The picture itself: the daemon serves it from the workspace, confined to the root, so the
  // browser never has the bytes the model saw.
  const image = $derived.by(() => {
    const id = currentWorkspace()?.id
    const path = typeof args.path === 'string' ? args.path : ''
    return id && path ? `/v1/workspaces/${id}/files?path=${encodeURIComponent(path)}` : null
  })
</script>

{#if kind === 'output' && input && command == null}
  <pre class="well mono">{JSON.stringify(args, null, 2)}</pre>
{:else if kind === 'output'}
  <!-- One terminal block: the full command (the row header cuts it) with its output below. -->
  <div class="well term mono">
    {#if command != null}<pre class="cmd"><span class="prompt">$ </span>{command}</pre>{/if}
    {#if text}
      <pre class="out">{#if !expanded && shown.hidden}{shown.head.join('\n')}
<button class="more" onclick={() => (expanded = true)}>{'…'} {shown.hidden} more line{shown.hidden === 1 ? '' : 's'}</button>
{shown.tail.join('\n')}{:else}{text}{/if}</pre>
    {:else if step.status === 'completed'}
      <pre class="out none">no output</pre>
    {/if}
  </div>
{:else if kind === 'diff'}
  <div class="diff mono">
    {#each diffShown as row, i (i)}
      <span class="gutter">{row.line ?? ''}</span><span class="sign {row.sign === '+' ? 'add' : row.sign === '-' ? 'del' : ''}">{row.sign}</span><span class="text {row.sign === '+' ? 'add' : row.sign === '-' ? 'del' : ''}">{row.text}</span>
    {/each}
    {#if !expanded && diffRows.length > diffShown.length}
      <button class="more" onclick={() => (expanded = true)}>{'…'} {diffRows.length - diffShown.length} more line{diffRows.length - diffShown.length === 1 ? '' : 's'}</button>
    {/if}
  </div>
{:else if kind === 'content'}
  <div class="diff mono">
    {#each contentShown as line, i (i)}<span class="gutter">{i + 1}</span><span class="sign"></span><span class="text">{line}</span>{/each}
    {#if !input && contentLines.length > contentShown.length}
      <button class="more" onclick={() => (expanded = true)}>{'…'} {contentLines.length - contentShown.length} more line{contentLines.length - contentShown.length === 1 ? '' : 's'}</button>
    {/if}
  </div>
{:else if kind === 'image'}
  {#if image}
    <img class="shot" use:authImage={image} alt={args.question ?? args.path ?? 'loaded image'} />
  {/if}
  <pre class="well mono">{text}</pre>
{:else if kind === 'todos'}
  <ul class="todos">
    {#each todos as todo (todo.id ?? todo.text)}
      {@const Mark = TODO_MARK[todo.status] ?? IconPending}
      <li class={todo.status}>
        <Mark class="mark" />
        <span class="text">{todo.text ?? todo.content}</span>
        {#if todo.status === 'cancelled'}<span class="tag">cancelled</span>{/if}
      </li>
    {/each}
  </ul>
{:else if kind === 'memory'}
  {#if args.action === 'remove'}
    <pre class="well mono strike">{args.old_text}</pre>
  {:else}
    <pre class="well mono">{args.content ?? text}</pre>
  {/if}
{:else}
  <pre class="well mono">{JSON.stringify(args, null, 2)}</pre>
  {#if !input && text}<pre class="well mono">{text}</pre>{/if}
{/if}

<style>
  .well {
    margin: 0;
    max-height: 400px;
    overflow: auto;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--bg-sunken);
    font-size: var(--text-xs);
    line-height: 20px;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .well + .well { margin-top: 2px; }
  .term { display: grid; gap: var(--space-1); white-space: normal; }
  .term pre { margin: 0; font: inherit; white-space: pre-wrap; }
  /* Wrapped command lines hang under the command, not under the prompt. */
  .cmd { padding-left: 2ch; text-indent: -2ch; color: var(--ink); }
  .prompt { color: var(--ink-3); user-select: none; }
  .out.none { color: var(--ink-3); }
  .more {
    display: block;
    padding: 0;
    border: 0;
    background: none;
    color: var(--ink-3);
    font-size: var(--text-xs);
    text-align: left;
  }
  .more:hover { color: var(--ink-2); }

  .diff {
    display: grid;
    grid-template-columns: max-content max-content 1fr;
    max-height: 400px;
    overflow: auto;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--bg-sunken);
    font-size: var(--text-xs);
    line-height: 20px;
  }
  .diff > .more { grid-column: 1 / -1; }
  .gutter { padding-right: var(--space-3); color: var(--ink-3); text-align: right; }
  .sign { width: 1ch; }
  .sign.add, .text.add { color: var(--success); background: var(--diff-add); }
  .sign.del, .text.del { color: var(--danger); background: var(--diff-del); }
  .text { white-space: pre-wrap; word-break: break-word; }

  .todos { display: grid; gap: 2px; margin: 0; padding: 0; list-style: none; font-size: var(--text-sm); }
  .todos li { display: flex; align-items: center; gap: var(--space-2); color: var(--ink-2); }
  .todos :global(.mark) { flex: none; width: 14px; height: 14px; color: var(--ink-3); }
  .todos .in_progress { color: var(--ink); font-weight: 500; }
  .todos .in_progress :global(.mark) { color: var(--accent); }
  .todos .completed, .todos .cancelled { color: var(--ink-3); text-decoration: line-through; }
  .todos .tag { color: var(--ink-3); text-decoration: none; }

  .strike { text-decoration: line-through; color: var(--ink-3); }

  .shot {
    display: block;
    max-width: 100%;
    max-height: 420px;
    margin-bottom: var(--space-2);
    border: 1px solid var(--line);
    border-radius: var(--radius-sm);
    background: var(--bg-sunken);
  }
</style>
