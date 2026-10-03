<!-- Layout: sidebar | conversation | optional workspace panel, plus the settings dialog. -->
<script>
  import { app, boot } from './lib/state.svelte.js'
  import { loadCommands } from './lib/commands.js'
  import Sidebar from './components/Sidebar.svelte'
  import Chat from './components/Chat.svelte'
  import Panel from './components/Panel.svelte'
  import Settings from './components/Settings.svelte'
  import Login from './components/Login.svelte'

  // A phone-width window overlays the sidebar and panel instead of squeezing the chat, and
  // closes the sidebar once a session is picked.
  const narrow = matchMedia('(max-width: 720px)')
  let sidebarOpen = $state(!narrow.matches)
  $effect(() => {
    app.session
    if (narrow.matches) sidebarOpen = false
  })
  boot()
  loadCommands()
</script>

{#if app.link === 'locked'}
  <Login />
{:else}
  <div class="app" class:sidebar={sidebarOpen} class:panel={app.panel}>
    {#if sidebarOpen}
      <Sidebar onCollapse={() => (sidebarOpen = false)} />
      <button class="scrim" aria-label="Close sidebar" onclick={() => (sidebarOpen = false)}></button>
    {/if}
    <Chat {sidebarOpen} onExpand={() => (sidebarOpen = true)} />
    {#if app.panel}<Panel />{/if}
    {#if app.panel}<button class="scrim" aria-label="Close panel" onclick={() => (app.panel = null)}></button>{/if}
  </div>
  {#if app.settingsTab}<Settings />{/if}
  {#if app.link === 'down'}<p class="offline" role="status">Connection lost. Retrying…</p>{/if}
{/if}

<style>
  .offline {
    position: fixed; top: var(--space-3); left: 50%; z-index: 30; margin: 0; transform: translateX(-50%);
    padding: var(--space-1) var(--space-3); border: 1px solid var(--line-strong); border-radius: var(--radius-lg);
    background: var(--bg-raised); color: var(--danger); font-size: var(--text-xs); pointer-events: none;
  }
  .scrim { display: none; }
  .app { display: grid; grid-template-columns: 1fr; grid-template-rows: minmax(0, 1fr); height: 100vh; }
  .app.sidebar { grid-template-columns: var(--sidebar-width) minmax(0, 1fr); }
  .app.panel { grid-template-columns: minmax(0, 1fr) var(--panel-width); }
  .app.sidebar.panel { grid-template-columns: var(--sidebar-width) minmax(0, 1fr) var(--panel-width); }
  @media (max-width: 720px) {
    .app.app { grid-template-columns: minmax(0, 1fr); }
    .app :global(.sidebar), .app :global(.panel) { position: fixed; top: 0; bottom: 0; z-index: 20; }
    .scrim { display: block; position: fixed; inset: 0; z-index: 19; padding: 0; border: 0; background: var(--scrim); }
    .app :global(.sidebar) { left: 0; width: min(var(--sidebar-width), 85vw); }
    .app :global(.panel) { right: 0; width: 100vw; }
  }
</style>
