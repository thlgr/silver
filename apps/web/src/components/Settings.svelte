<script>
  import { api, hasToken, setToken } from '../lib/api.js'
  import { app, setApprovalMode, setAdvisor, setRunTimeout, connect, logout, beginLogin, pollLogin, loadProviders, notify, currentPreset, setPreset, savePreset, deletePreset, loadPresets } from '../lib/state.svelte.js'
  import IconX from '~icons/lucide/x'
  import IconGeneral from '~icons/lucide/settings'
  import IconKey from '~icons/lucide/key-round'
  import IconTools from '~icons/lucide/wrench'
  import IconSun from '~icons/lucide/sun'
  import IconMoon from '~icons/lucide/moon'
  import IconMonitor from '~icons/lucide/monitor'
  import IconCheck from '~icons/lucide/check'
  import ConfirmButton from './ConfirmButton.svelte'

  const TABS = [['general', 'General', IconGeneral], ['providers', 'Providers', IconKey], ['presets', 'Presets', IconTools]]
  // Transports that reach an HTTP endpoint the user may point elsewhere (llama.cpp, a proxy).
  const HTTP = ['openai_compatible', 'ollama', 'anthropic']
  const THEMES = [['light', 'Light', IconSun], ['dark', 'Dark', IconMoon], ['system', 'System', IconMonitor]]
  // Keyless in silver, but not free of setup: the credentials live outside it.
  const OUTSIDE = { bedrock: 'Uses AWS credentials', vertex: 'Uses Google Cloud credentials', acp: 'External agent' }

  let expanded = $state(null)
  let key = $state('')
  let url = $state('')
  let filter = $state('')
  let login = $state(null) // { provider, code, url, status }
  let catalog = $state(null)
  let nameField = $state(null)
  let editing = $state(null) // the preset the editor shows; picking it here does not switch the chat
  let timeout = $state(null) // run timeout in minutes, as typed
  // Seed the field from the server's value once it is known.
  $effect(() => {
    if (app.server && timeout === null) timeout = Math.max(1, Math.round(app.server.run_timeout_seconds / 60))
  })

  $effect(() => {
    if (app.settingsTab === 'presets' && !catalog) {
      Promise.all([api('/v1/tools'), api('/v1/skills')]).then(([t, s]) => (catalog = { tools: t.tools, skills: s.skills }))
    }
  })

  const inUse = $derived(currentPreset())
  const shown = $derived(app.presets.find((p) => p.id === editing) ?? inUse)
  const builtin = $derived(!!shown?.builtin)
  const skillNames = $derived((catalog?.skills ?? []).map((s) => s.name))
  const skillMode = $derived(shown?.skills && 'only' in shown.skills ? 'only' : 'except')
  const checkedSkills = $derived.by(() => {
    const filter = shown?.skills ?? { except: [] }
    if ('only' in filter) return new Set(filter.only ?? [])
    const except = new Set(filter.except ?? [])
    return new Set(skillNames.filter((n) => !except.has(n)))
  })
  // Listed in the preset but not registered now (an MCP server may be offline); shown so they can be removed.
  const missingTools = $derived(catalog ? (shown?.tools ?? []).filter((t) => !catalog.tools.some((c) => c.name === t)) : [])
  const hasSkillTools = $derived((shown?.tools ?? []).some((t) => ['skills_list', 'skill_view', 'skill_manage'].includes(t)))

  const providers = $derived(
    app.providers
      .filter((p) => `${p.label} ${p.id}`.toLowerCase().includes(filter.trim().toLowerCase()))
      .sort((a, b) => b.active - a.active || b.authenticated - a.authenticated || a.label.localeCompare(b.label)),
  )
  const standing = (p) =>
    p.active ? 'Active' : HTTP.includes(p.kind) && !p.base_url ? 'Needs an endpoint' : !p.requires_key ? (OUTSIDE[p.kind] ?? 'No key needed') : p.authenticated ? (p.key_source === 'env' ? `Key from $${p.api_key_env}` : 'Signed in') : ''

  const reveal = (node) => node.scrollIntoView({ block: 'nearest' })

  // Forgiving: "localhost:8080/v1/" reaches the same server as "http://localhost:8080/v1".
  function endpoint(raw) {
    const trimmed = raw.trim().replace(/\/+$/, '')
    return !trimmed || trimmed.includes('://') ? trimmed : `http://${trimmed}`
  }

  function toggle(p) {
    expanded = expanded === p.id ? null : p.id
    key = ''
    url = p.base_url ?? ''
  }

  async function save(p) {
    const base_url = HTTP.includes(p.kind) && endpoint(url) !== p.base_url ? endpoint(url) : undefined
    const saved = await connect(p.id, { api_key: key.trim() || undefined, base_url })
    if (saved) {
      key = ''
      url = saved.base_url
    }
  }

  async function oauth(p) {
    try {
      const start = await beginLogin(p.id)
      const url = start.verification_uri ?? start.verification_url ?? start.authorize_url ?? start.auth_url ?? start.url
      login = { provider: p.id, code: start.user_code, url, status: 'Waiting for you to finish in the browser' }
      if (!start.user_code && url) window.open(url, '_blank', 'noopener')
      let interval = (start.interval ?? 5) * 1000
      while (login?.provider === p.id) {
        await new Promise((r) => setTimeout(r, interval))
        const res = await pollLogin(p.id)
        const status = (typeof res === 'string' ? res : res.status ?? res.state ?? '').toLowerCase()
        if (['authorized', 'authorised', 'ok', 'success'].includes(status)) {
          login = null
          notify(`Signed in to ${p.label}`)
          return loadProviders()
        }
        if (['denied', 'error', 'expired', 'timeout'].includes(status)) throw new Error(res.description ?? res.message ?? `Sign-in ${status}`)
        if (res.interval) interval = res.interval * 1000
      }
    } catch (e) {
      login = null
      notify(e.message, true)
    }
  }

  async function toggleTool(name, on) {
    if (!shown || shown.builtin) return
    const tools = on ? [...shown.tools, name] : shown.tools.filter((t) => t !== name)
    await savePreset({ ...shown, tools })
  }

  function filterFor(mode, checked) {
    return mode === 'only'
      ? { only: [...checked] }
      : { except: skillNames.filter((n) => !checked.has(n)) }
  }

  async function toggleSkill(name, on) {
    if (!shown || shown.builtin) return
    const checked = new Set(checkedSkills)
    if (on) checked.add(name)
    else checked.delete(name)
    await savePreset({ ...shown, skills: filterFor(skillMode, checked) })
  }

  async function switchSkillMode(mode) {
    if (!shown || shown.builtin || mode === skillMode) return
    await savePreset({ ...shown, skills: filterFor(mode, checkedSkills) })
  }

  async function rename(name) {
    if (!shown || shown.builtin) return
    await savePreset({ ...shown, name })
  }

  async function newPreset() {
    if (!shown) return
    const names = new Set(app.presets.map((p) => p.name.toLowerCase()))
    const base = `${shown.name} copy`
    let candidate = base
    for (let i = 2; names.has(candidate.toLowerCase()); i++) candidate = `${base} ${i}`
    // shown is a $state proxy, so snapshot it into plain data (structuredClone throws on proxies).
    const saved = await savePreset({ name: candidate, tools: $state.snapshot(shown.tools), skills: $state.snapshot(shown.skills ?? { except: [] }) })
    if (!saved) return
    editing = saved.id
    requestAnimationFrame(() => {
      nameField?.focus()
      nameField?.select()
    })
  }
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && (app.settingsTab = null)} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={(e) => e.target === e.currentTarget && (app.settingsTab = null)}>
  <div class="dialog" role="dialog" aria-modal="true" aria-label="Settings">
    <nav>
      <h2>Settings</h2>
      {#each TABS as [id, label, Icon] (id)}
        <button class:active={app.settingsTab === id} onclick={() => (app.settingsTab = id)}><Icon /> {label}</button>
      {/each}
    </nav>

    <section>
      <header>
        <h3>{TABS.find(([id]) => id === app.settingsTab)?.[1]}</h3>
        <button class="icon-btn" title="Close" aria-label="Close" onclick={() => (app.settingsTab = null)}><IconX /></button>
      </header>

      {#if app.settingsTab === 'general'}
        <div class="setting">
          <div><div>Appearance</div></div>
          <div class="themes">
            {#each THEMES as [id, label, Icon] (id)}
              <button class:active={app.settings.theme === id} onclick={() => (app.settings.theme = id)}><Icon /> {label}</button>
            {/each}
          </div>
        </div>
        <label class="setting">
          <div><div>Approvals</div><p>When the agent must ask before writing files or running commands. Applies to every session.</p></div>
          <select class="field" value={app.approvals?.mode} disabled={app.approvals?.frozen} onchange={(e) => setApprovalMode(e.target.value)}>
            <option value="manual">Ask before changes</option>
            <option value="smart">Ask for risky changes</option>
            <option value="off">Never ask</option>
          </select>
        </label>
        {#if app.server}
          <label class="setting">
            <div><div>Run timeout</div><p>How long a hosted run may work before it stops and asks you to continue. Applies to new runs; local models have no limit. Minutes.</p></div>
            <input class="field narrow" type="number" min="1" bind:value={timeout} onchange={() => timeout && setRunTimeout(Math.round(timeout) * 60)} />
          </label>
        {/if}
        {#if app.advisor}
          <label class="setting">
            <div>
              <div>Jev hints</div>
              <p>Jev, a fast classifier on OpenRouter, checks each step of a run and gives the model a hint when it helps, such as reading a project's build docs first. Its answers show as Jev steps in the chat. Sends the task and excerpts of command output to OpenRouter.</p>
              {#if !app.advisor.has_key}<p class="error">Needs an OpenRouter key: add one under Providers.</p>{/if}
            </div>
            <input type="checkbox" checked={app.advisor.enabled} onchange={(e) => setAdvisor(e.target.checked)} />
          </label>
        {/if}
        <label class="setting">
          <div><div>Tool progress</div><p>What a running turn shows of its tool calls.</p></div>
          <select class="field" bind:value={app.settings.verbose}>
            <option value="all">Every step</option>
            <option value="new">Latest step</option>
            <option value="off">Nothing</option>
          </select>
        </label>
        <label class="setting">
          <div><div>Focus mode</div><p>Show only prompts and final replies.</p></div>
          <input type="checkbox" bind:checked={app.settings.focus} />
        </label>
        <label class="setting">
          <div><div>Messages</div><p>Switch silver to a messaging app: bots that keep their own chats, group chats, threads, and bots that ask each other for help.</p></div>
          <input type="checkbox" bind:checked={app.settings.messaging} />
        </label>
        <label class="setting">
          <div><div>Goal budget</div><p>Continuations a /goal may run before it stops.</p></div>
          <input class="field narrow" type="number" min="1" bind:value={app.settings.goalBudget} />
        </label>
        {#if hasToken()}
          <div class="setting">
            <div><div>Access token</div><p>Saved in this browser to sign in to the server.</p></div>
            <button class="btn" onclick={() => (setToken(null), location.reload())}>Sign out</button>
          </div>
        {/if}

      {:else if app.settingsTab === 'providers'}
        <p class="lead">Sign in to a hosted provider, or point silver at a local server such as llama.cpp. Keys are stored by silver, never in the browser.</p>
        <input class="field search" type="search" placeholder="Search providers" bind:value={filter} />
        {#each providers as p (p.id)}
          <div class="provider" class:open={expanded === p.id}>
            <button class="provider-head" onclick={() => toggle(p)}>
              <span>{p.label}</span> <span class="muted">{p.id}</span>
              <span class="state">{standing(p)}</span>
            </button>
            {#if expanded === p.id}
              <div class="provider-body">
                <form class="fields" onsubmit={(e) => (e.preventDefault(), save(p))}>
                  {#if HTTP.includes(p.kind)}
                    <label>Endpoint URL <input class="field" placeholder="http://localhost:8080/v1" bind:value={url} /></label>
                  {/if}
                  {#if p.requires_key || HTTP.includes(p.kind)}
                    <label>
                      API key{p.requires_key ? '' : ' (optional)'}
                      <input class="field" type="password" autocomplete="off" placeholder={p.authenticated && p.key_source !== 'none' ? 'Saved; type to replace' : ''} bind:value={key} />
                    </label>
                  {/if}
                  <div class="inline">
                    <button class="btn primary" disabled={(p.requires_key && !p.authenticated && !key.trim()) || (HTTP.includes(p.kind) && !url.trim())}>
                      {p.active ? 'Save' : 'Save and use'}
                    </button>
                    {#if p.key_source !== 'env' && (p.requires_key ? p.authenticated : p.configured)}<button class="btn danger" type="button" onclick={() => logout(p.id).then(() => (expanded = null))}>{p.requires_key ? 'Sign out' : 'Reset'}</button>{/if}
                    {#if p.signup_url}<a class="muted small" href={p.signup_url} target="_blank" rel="noreferrer">{p.requires_key ? 'Get a key' : 'Website'}</a>{/if}
                  </div>
                </form>
                <!-- The composer, where notices usually show, is behind this dialog. -->
                {#if app.notice}<p class="small" class:error={app.notice.error} use:reveal>{app.notice.text}</p>{/if}
                {#if login?.provider === p.id}
                  <p class="small">
                    {#if login.code}Enter <strong class="mono">{login.code}</strong> at{:else}Continue at{/if}
                    <a href={login.url} target="_blank" rel="noreferrer">{login.url}</a>.
                    <span class="muted">{login.status}</span>
                  </p>
                {/if}
                {#if p.oauth}<div class="inline"><button class="btn" disabled={login?.provider === p.id} onclick={() => oauth(p)}>Sign in with browser</button></div>{/if}
              </div>
            {/if}
          </div>
        {/each}

      {:else if app.settingsTab === 'presets'}
        <p class="lead">A preset decides which tools and skills the agent can use. Pick one for each chat from the wrench menu under the message box. Changes apply from the next message.</p>
        <div class="pills">
          {#each app.presets as p (p.id)}
            <button class="btn pill" aria-pressed={shown?.id === p.id} class:active={shown?.id === p.id} onclick={() => (editing = p.id)}>{p.name}</button>
          {/each}
          <button class="btn pill" onclick={newPreset}>+ New preset</button>
        </div>
        {#if shown}
          <div class="inline use">
            {#if shown.id === inUse?.id}
              <IconCheck /> {app.session ? 'This chat uses' : 'New chats use'} {shown.name}.
            {:else}
              <button class="btn primary" onclick={() => setPreset(shown.id)}>Use {shown.name} {app.session ? 'in this chat' : 'for new chats'}</button>
            {/if}
          </div>
          {#if builtin}
            <p class="muted small">
              {#if shown.id === 'minimal'}Minimal uses the tools turned on in config.toml ([tools]).
              {:else if shown.id === 'pi'}Pi gives the agent one shell tool (bash) and nothing else.
              {/if}
              Built-in presets can't be changed. New preset starts an editable copy.
            </p>
          {:else}
            <div class="inline preset-head">
              <label>Name <input class="field" bind:this={nameField} value={shown.name} onchange={(e) => rename(e.target.value)} /></label>
              <ConfirmButton class="btn danger" title="Chats on this preset switch to Minimal" ask="Delete {shown.name}?" onconfirm={() => deletePreset(shown)}>Delete</ConfirmButton>
            </div>
            {#if app.notice}<p class="small" class:error={app.notice.error}>{app.notice.text}</p>{/if}
          {/if}
          <h3>Tools · {shown.tools.length} on</h3>
          {#each catalog?.tools ?? [] as t (t.name)}
            <label class="entry check">
              <input type="checkbox" checked={shown.tools.includes(t.name)} disabled={builtin} onchange={(e) => toggleTool(t.name, e.target.checked)} />
              <div><span class="mono">{t.name}</span> <span class="muted small">{t.toolset}{t.requires_workspace ? ', needs a workspace' : ''}</span></div>
              <p>{t.description}</p>
            </label>
          {/each}
          {#each missingTools as name (name)}
            <label class="entry check">
              <input type="checkbox" checked disabled={builtin} onchange={() => toggleTool(name, false)} />
              <div><span class="mono">{name}</span> <span class="muted small">not available right now</span></div>
              <p>Its MCP server may be offline. Uncheck to remove it from this preset.</p>
            </label>
          {/each}
          <h3>Skills</h3>
          {#if !hasSkillTools}
            <p class="muted">Skills need skill_view. Turn on skill_view above to let the agent load skills.</p>
          {:else}
            <label class="entry check">
              <input type="radio" name="skill-mode" checked={skillMode === 'except'} disabled={builtin} onchange={() => switchSkillMode('except')} />
              <div>All skills except unchecked <span class="muted small">· new skills are on</span></div>
            </label>
            <label class="entry check">
              <input type="radio" name="skill-mode" checked={skillMode === 'only'} disabled={builtin} onchange={() => switchSkillMode('only')} />
              <div>Only checked skills <span class="muted small">· new skills are off</span></div>
            </label>
            {#each catalog?.skills ?? [] as s (s.name)}
              <label class="entry check">
                <input type="checkbox" checked={checkedSkills.has(s.name)} disabled={builtin} onchange={(e) => toggleSkill(s.name, e.target.checked)} />
                <div class="mono">{s.name}</div>
                <p>{s.description}</p>
              </label>
            {:else}
              <p class="muted">No skills installed.</p>
            {/each}
          {/if}
        {/if}
      {/if}
    </section>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; z-index: 30; display: grid; place-items: center; padding: var(--space-6); background: var(--scrim); }
  .dialog { display: grid; grid-template-columns: 200px 1fr; width: min(820px, 100%); height: min(640px, 100%); border-radius: var(--radius-lg); background: var(--bg-raised); overflow: hidden; }
  nav { display: flex; flex-direction: column; gap: 2px; padding: var(--space-4) var(--space-3); }
  h2 { margin: 0 0 var(--space-4) var(--space-2); font-size: var(--text-lg); font-weight: 600; }
  nav button { display: flex; align-items: center; gap: var(--space-2); height: 36px; padding: 0 var(--space-3); border: 0; border-radius: var(--radius-md); background: none; font-size: var(--text-sm); text-align: left; }
  nav button:hover { background: var(--bg-hover); }
  nav button.active { background: var(--bg-active); }

  section { overflow-y: auto; padding: 0 var(--space-6) var(--space-8); }
  section header {
    position: sticky;
    top: 0;
    z-index: 1;
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin: 0 calc(-1 * var(--space-2)) var(--space-2) 0;
    padding: var(--space-4) 0 var(--space-2);
    background: var(--bg-raised);
  }
  h3 { margin: var(--space-8) 0 var(--space-4); font-size: var(--text-lg); font-weight: 600; }
  header h3 { margin: 0; }
  .lead { margin: 0 0 var(--space-4); color: var(--ink-2); font-size: var(--text-sm); }
  .small { font-size: var(--text-xs); }

  .setting { display: flex; align-items: center; justify-content: space-between; gap: var(--space-6); padding: var(--space-4) 0; border-bottom: 1px solid var(--line); font-size: var(--text-sm); }
  .setting p { margin: 2px 0 0; color: var(--ink-3); font-size: var(--text-xs); }
  .setting select { width: auto; }
  .narrow { width: 80px; }
  .themes { display: flex; gap: var(--space-2); }
  .themes button { display: grid; justify-items: center; gap: var(--space-1); width: 88px; padding: var(--space-3) 0; border: 1px solid var(--line-strong); border-radius: var(--radius-md); background: none; font-size: var(--text-xs); }
  .themes button :global(svg) { font-size: 16px; }
  .themes button.active { border-color: var(--ink); }

  .provider { border-bottom: 1px solid var(--line); }
  .provider-head { display: flex; align-items: baseline; gap: var(--space-2); width: 100%; padding: var(--space-3) 0; border: 0; background: none; font-size: var(--text-sm); text-align: left; }
  .provider-head .muted { font-size: var(--text-xs); }
  .state { margin-left: auto; color: var(--ink-2); font-size: var(--text-xs); }
  .provider-body { display: grid; gap: var(--space-3); padding: 0 0 var(--space-4); }
  .inline { display: flex; align-items: center; gap: var(--space-2); }
  .fields { display: grid; gap: var(--space-3); }
  .fields label { display: grid; gap: var(--space-1); color: var(--ink-2); font-size: var(--text-xs); }
  .search { margin-bottom: var(--space-2); }
  .inline:empty { display: none; }
  .provider-body a { color: var(--ink-2); }

  .entry { padding: var(--space-2) 0; border-bottom: 1px solid var(--line); font-size: var(--text-sm); }
  .entry p { margin: 2px 0 0; color: var(--ink-3); font-size: var(--text-xs); display: -webkit-box; -webkit-line-clamp: 2; line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
  .pills { display: flex; flex-wrap: wrap; gap: var(--space-2); margin-bottom: var(--space-4); }
  .pill.active, .pill.active:hover:not(:disabled) { border-color: var(--ink); background: var(--bg-active); font-weight: 600; }
  .use { min-height: 32px; margin-bottom: var(--space-4); color: var(--ink-2); font-size: var(--text-sm); }
  .check { display: grid; grid-template-columns: auto 1fr; gap: 2px var(--space-2); align-items: baseline; cursor: pointer; }
  .check input { margin-top: 4px; }
  .check div, .check p { grid-column: 2; }
  .preset-head { justify-content: space-between; margin-bottom: var(--space-2); }
  /* A phone gets the tabs as a row above the content, so the content keeps the full width. */
  @media (max-width: 720px) {
    .backdrop { padding: var(--space-3); }
    .dialog { grid-template-columns: 1fr; grid-template-rows: auto 1fr; height: 100%; }
    nav { flex-direction: row; flex-wrap: wrap; padding: var(--space-3) var(--space-3) 0; }
    h2 { display: none; }
    section { padding: 0 var(--space-4) var(--space-8); }
    .setting { flex-wrap: wrap; row-gap: var(--space-2); }
  }
  .preset-head label { display: grid; gap: var(--space-1); font-size: var(--text-xs); color: var(--ink-2); }
</style>
