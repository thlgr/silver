<!-- The subagent editor: the catalogue, and the Markdown behind the custom entries. A
     definition is a file, so the editor works on the file and shows what was parsed out of it. -->
<script>
  import { app, loadAgents, agentMarkdown, saveAgent, deleteAgent, tryAgent, notify } from '../lib/state.svelte.js'
  import IconPlus from '~icons/lucide/plus'
  import IconTrash from '~icons/lucide/trash-2'
  import IconSend from '~icons/lucide/send'
  import ConfirmButton from './ConfirmButton.svelte'
  import IconLock from '~icons/lucide/lock'

  const TEMPLATE = `---
name: reviewer
description: Reviews a change on its own terms
tools: [read_file, search_files]
---

You review a change. Say what is wrong with it, concretely, with paths.
`

  let editing = $state(null) // { name, source, markdown }
  let busy = $state(false)
  let error = $state('')

  const custom = $derived(app.agents.filter((a) => a.source !== 'built-in'))
  const scope = $derived(app.scope ? 'project' : 'global')

  $effect(() => {
    app.panel, app.scope // reload when the tab or the workspace changes
    editing = null
    error = ''
    loadAgents()
  })

  async function open(agent) {
    if (!agent.editable) return
    busy = true
    error = ''
    try {
      editing = { name: agent.name, markdown: await agentMarkdown(agent.name) }
    } catch (e) {
      error = e.message
    } finally {
      busy = false
    }
  }

  function create() {
    editing = { name: '', markdown: TEMPLATE }
    error = ''
  }

  async function save() {
    if (!editing) return
    busy = true
    error = ''
    // The name the file will get is the one the frontmatter declares; an empty editor field
    // means "take it from the frontmatter".
    const declared = /^name:\s*(\S+)/m.exec(editing.markdown)?.[1]
    const name = editing.name || declared
    try {
      await saveAgent(name, editing.markdown, scope)
      notify(`Saved the ${name} agent`)
      editing = null
    } catch (e) {
      error = e.message
    } finally {
      busy = false
    }
  }

  async function remove(agent) {
    await deleteAgent(agent.name, agent.source === 'project' ? 'project' : 'global')
    if (editing?.name === agent.name) editing = null
  }

  const tools = (agent) => (agent.tools ? agent.tools.join(', ') : 'all tools')
  const denied = (agent) => (agent.disallowed_tools?.length ? ` except ${agent.disallowed_tools.join(', ')}` : '')
</script>

{#if editing}
  <div class="editor">
    <div class="row head">
      <div class="grow">
        <strong>{editing.name ? `Editing ${editing.name}` : 'New agent'}</strong>
        <div class="muted small">
          Saved as {scope === 'project' ? 'this workspace' : 'your home directory'}, and used the next
          time a run starts.
        </div>
      </div>
      <button class="btn" onclick={() => (editing = null)}>Cancel</button>
      <button class="btn primary" disabled={busy} onclick={save}>Save</button>
    </div>
    {#if error}<p class="error">{error}</p>{/if}
    <textarea class="field code" rows="18" bind:value={editing.markdown} spellcheck="false"></textarea>
    <p class="muted small">
      Frontmatter: <span class="mono">name</span>, <span class="mono">description</span>, and
      optionally <span class="mono">tools</span>, <span class="mono">disallowed_tools</span>,
      <span class="mono">model</span>, <span class="mono">max_turns</span>,
      <span class="mono">isolation: worktree</span>. Everything after the second
      <span class="mono">---</span> is the subagent's system prompt.
    </p>
  </div>
{:else}
  <div class="row head">
    <p class="grow muted">
      silver can hand a task to a subagent: it works on its own, with its own context and tools,
      and reports back. These are the ones it knows.
    </p>
    <button class="btn primary" onclick={create}><IconPlus /> New agent</button>
  </div>
  {#if error}<p class="error">{error}</p>{/if}
  {#each app.agents as agent (agent.name)}
    <div class="row">
      <div class="grow">
        <div>
          <span class="mono">{agent.name}</span>
          {#if !agent.editable}<IconLock class="lock" />{/if}
          <span class="tag">{agent.source}</span>
        </div>
        <div class="muted small">{agent.description}</div>
        <div class="muted small">
          tools: {tools(agent)}{denied(agent)}{agent.model ? ` · model ${agent.model}` : ''}{agent.max_turns ? ` · ${agent.max_turns} turns` : ''}
        </div>
      </div>
      <button class="btn" title="Ask for this agent in the composer" onclick={() => tryAgent(agent)}><IconSend /></button>
      {#if agent.editable}
        <button class="btn" onclick={() => open(agent)}>Edit</button>
        <ConfirmButton class="btn danger" title={`Delete ${agent.name}; its file is removed`} ask="Delete?" onconfirm={() => remove(agent)}><IconTrash /></ConfirmButton>
      {/if}
    </div>
  {/each}
  {#if !app.agents.length}
    <p class="muted">Loading</p>
  {/if}
{/if}

<style>
  .row { display: flex; align-items: center; gap: var(--space-3); padding: var(--space-3) 0; border-bottom: 1px solid var(--line); }
  .row.head { align-items: flex-start; padding-top: 0; }
  .grow { flex: 1; min-width: 0; overflow-wrap: anywhere; }
  .small { font-size: var(--text-xs); }
  .editor { display: grid; gap: var(--space-3); }
  .code { font: var(--text-xs) var(--mono); line-height: 1.6; resize: vertical; }
  :global(.lock) { width: 12px; height: 12px; color: var(--ink-3); vertical-align: -2px; }
  .tag { margin-left: var(--space-2); color: var(--ink-3); font-size: var(--text-xs); }
  .error { margin: 0; color: var(--danger); font-size: var(--text-sm); }
  p { margin: 0; }
</style>
