<!-- "New message": a To: field that searches your bots, or starts a new one. Picking a bot opens
     its chat, and sends what you typed below. -->
<script>
  import { app } from '../../lib/state.svelte.js'
  import { chat, roster, select, send } from '../../lib/chat.svelte.js'
  import BotAvatar from './BotAvatar.svelte'
  import IconPlus from '~icons/lucide/plus'
  import IconX from '~icons/lucide/x'
  import IconSend from '~icons/lucide/arrow-up'

  let { onclose } = $props()
  let query = $state('')
  let text = $state('')
  const matches = $derived(roster().filter((bot) => bot.kind === 'agent' && (!query.trim() || bot.name.toLowerCase().includes(query.trim().toLowerCase()))))
  const focus = (node) => node.focus()

  function open(bot) {
    select(bot.id)
    if (text.trim()) send(bot.id, text.trim())
  }

  function create() {
    onclose()
    chat.editor = { bot: null }
  }
</script>

<section class="new">
  <header>
    <label class="to"><span>To:</span><input placeholder="Search your bots" bind:value={query} use:focus onkeydown={(e) => e.key === 'Enter' && matches[0] && open(matches[0])} /></label>
    <button type="button" class="round press" title="Close" aria-label="Close" onclick={onclose}><IconX /></button>
  </header>
  <div class="list">
    <button type="button" class="pick press" onclick={create}><span class="plus"><IconPlus /></span><span>New bot</span></button>
    {#each matches as bot (bot.id)}
      <button type="button" class="pick press" onclick={() => open(bot)}>
        <BotAvatar {bot} size={34} badge={false} />
        <span class="name">{bot.name}<small>{[app.workspaces.find((w) => w.id === bot.workspace_id)?.name ?? 'No workspace', bot.description].filter(Boolean).join(' · ')}</small></span>
      </button>
    {/each}
  </div>
  <footer>
    <div class="box">
      <input placeholder="Message" bind:value={text} />
      <button type="button" class="go press" class:ready={text.trim()} disabled={!text.trim() || !matches[0]} title="Send to {matches[0]?.name ?? 'the first match'}" aria-label="Send" onclick={() => matches[0] && open(matches[0])}><IconSend /></button>
    </div>
  </footer>
</section>

<style>
  .new { display: flex; flex-direction: column; height: 100%; background: var(--m-bg); }
  header { display: flex; align-items: center; gap: 8px; padding: 12px 20px; border-bottom: 1px solid var(--m-border); }
  .to { display: flex; align-items: center; flex: 1; gap: 10px; font-size: 17px; }
  .to span { color: var(--m-secondary); }
  .to input { flex: 1; min-width: 0; border: 0; outline: 0; background: none; color: var(--m-text); font: inherit; }
  header .round { background: none; color: var(--m-secondary); }
  .list { flex: 1; min-height: 0; padding: 8px 12px; overflow-y: auto; }
  .pick { display: flex; align-items: center; gap: 12px; width: 100%; padding: 8px 10px; border: 0; border-radius: 12px; background: none; color: var(--m-text); font-size: var(--text-md); text-align: left; }
  .pick:hover { background: var(--bg-hover); }
  .plus { display: grid; place-items: center; width: 34px; height: 34px; border-radius: 50%; background: var(--m-agent); }
  .plus :global(svg) { width: 15px; height: 15px; }
  .name { display: flex; flex-direction: column; font-weight: 600; }
  .name small { color: var(--m-tertiary); font-size: var(--text-xs); font-weight: 400; }
  footer { padding: 6px 16px 16px; }
  .box { display: flex; align-items: center; gap: 8px; max-width: 852px; margin: 0 auto; padding: 4px 4px 4px 16px; border-radius: 24px; background: var(--m-surface); }
  .box input { flex: 1; min-width: 0; height: 36px; border: 0; outline: 0; background: none; color: var(--m-text); font: inherit; font-size: 15px; }
  .go { display: grid; place-items: center; width: 32px; height: 32px; padding: 0; border: 0; border-radius: 50%; background: var(--m-dim); color: var(--m-tertiary); }
  .go.ready { background: var(--m-fill); color: var(--m-on-fill); }
  .go :global(svg) { width: 15px; height: 15px; stroke-width: 2.6; }
</style>
