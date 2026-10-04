<!-- The message box under a chat or a thread: send, or stop while the bot (or the group) is
     working there. In a group, typing @ suggests its bots. -->
<script>
  import { addFiles, botById, chat, isFileDrag, laneKey, membersOf, removeFile, send, stop, storeFiles, workingIn } from '../../lib/chat.svelte.js'
  import { attachmentNote, getDraft, notify, setDraft } from '../../lib/state.svelte.js'
  import Avatar from './Avatar.svelte'
  import IconSend from '~icons/lucide/arrow-up'
  import IconClip from '~icons/lucide/paperclip'
  import IconX from '~icons/lucide/x'
  import IconStop from '~icons/lucide/square'

  let { botId, thread = null, onsent } = $props()
  const bot = $derived(botById(botId))
  const group = $derived(bot?.kind === 'group')
  const key = $derived(`chat:${botId}${thread ? `/${thread}` : ''}`)
  const fileKey = $derived(laneKey(botId, thread))
  const working = $derived(bot ? workingIn(bot, thread) : false)
  let text = $state('')
  let box = $state()
  let picker = $state()
  const files = $derived(chat.files[fileKey] ?? [])
  let over = $state(false)

  // One draft per chat or thread, kept across switching.
  $effect(() => {
    text = getDraft(key)
  })
  // Grow with the text, up to eight lines.
  $effect(() => {
    text
    if (!box) return
    box.style.height = 'auto'
    box.style.height = `${Math.min(box.scrollHeight, 190)}px`
  })

  const placeholder = $derived(
    thread ? 'Reply…' : group ? `Message ${bot?.name} · @ to ask one bot` : working ? `Queue a message for ${bot?.name}` : `Ask ${bot?.name}`,
  )
  const empty = $derived(!text.trim() && !files.length)
  // A file needs a folder to be stored in, so a group, or a bot without one, takes none.
  const folder = $derived(bot?.kind === 'agent' ? bot.workspace_id : null)

  // The @partial being typed at the end, if any.
  const mention = $derived.by(() => {
    if (!group) return null
    const at = text.lastIndexOf('@')
    if (at < 0 || (at > 0 && !/\s/.test(text[at - 1]))) return null
    const query = text.slice(at + 1)
    return /\n/.test(query) || query.length > 24 ? null : query
  })
  const suggestions = $derived(
    mention === null ? [] : membersOf(bot).filter((member) => member.name.toLowerCase().includes(mention.toLowerCase())),
  )

  function pick(member) {
    text = `${text.slice(0, text.lastIndexOf('@'))}@${member.name.split(/\s+/)[0]} `
    setDraft(key, text)
    box.focus()
  }

  async function attach(list) {
    addFiles(fileKey, await storeFiles(folder, list))
  }

  const dropped = (event) => {
    over = false
    if (!isFileDrag(event)) return
    event.preventDefault()
    if (!folder) return noWorkspace(bot)
    attach([...event.dataTransfer.files])
  }
  const dragLeave = (event) => {
    // Moving onto a child of the box fires this too; only leaving the box clears the outline.
    if (!event.currentTarget.contains(event.relatedTarget)) over = false
  }
  const pasted = (event) => {
    const images = [...(event.clipboardData?.files ?? [])].filter((file) => file.type.startsWith('image/'))
    if (!images.length) return
    event.preventDefault()
    if (!folder) return noWorkspace(bot)
    attach(images)
  }

  function submit() {
    if (empty) return
    // The words, then a marker line per file: the bot reads the path with a tool.
    const lines = [text.trim() || 'Look at the attached file.', ...files.map(attachmentNote)]
    send(botId, lines.join('\n'), thread)
    text = ''
    setDraft(key, '')
    chat.files[fileKey] = []
    onsent?.()
  }

  function keydown(event) {
    if (event.key === 'Tab' && suggestions.length) {
      event.preventDefault()
      pick(suggestions[0])
    } else if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault()
      submit()
    }
  }

  const stopping = $derived(working && empty)
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="composer" ondragover={(e) => isFileDrag(e) && (e.preventDefault(), (over = true))} ondragleave={dragLeave} ondrop={dropped}>
  {#if suggestions.length}
    <div class="mentions">
      {#each suggestions as member (member.id)}
        <button type="button" class="who press" onclick={() => pick(member)}>
          <Avatar shape={member.avatar_shape} color={member.avatar_color} size={18} />{member.name}
        </button>
      {/each}
    </div>
  {/if}
  {#if files.length}
    <div class="files">
      {#each files as file, i (file.path)}
        <span class="file">
          {#if file.preview}<img src={file.preview} alt="" />{:else}<IconClip />{/if}
          <span>{file.name}</span>
          <button type="button" title="Remove" aria-label="Remove {file.name}" onclick={() => removeFile(fileKey, i)}><IconX /></button>
        </span>
      {/each}
    </div>
  {/if}
  <div class="box" class:over>
    {#if folder}
      <button type="button" class="clip press" title="Add files" aria-label="Add files" onclick={() => picker.click()}><IconClip /></button>
      <input bind:this={picker} type="file" multiple hidden onchange={(e) => (attach([...e.target.files]), (e.target.value = ''))} />
    {/if}
    <textarea bind:this={box} bind:value={text} rows="1" {placeholder} aria-label={placeholder} oninput={() => setDraft(key, text)} onkeydown={keydown} onpaste={pasted}></textarea>
    <button
      type="button"
      class="go press"
      class:ready={stopping || !empty}
      disabled={!stopping && empty}
      title={stopping ? 'Stop' : 'Send'}
      aria-label={stopping ? 'Stop' : 'Send'}
      onclick={() => (stopping ? stop(botId) : submit())}
    >
      {#if stopping}<IconStop />{:else}<IconSend />{/if}
    </button>
  </div>
</div>

<style>
  .composer { display: flex; flex-direction: column; gap: 6px; width: 100%; max-width: 852px; margin: 0 auto; padding: 6px 16px 16px; }
  .mentions { display: flex; gap: 6px; overflow-x: auto; animation: rise 0.2s var(--m-spring); }
  .who { display: inline-flex; flex: none; align-items: center; gap: 6px; padding: 6px 10px; border: 0; border-radius: 999px; background: var(--m-surface); color: var(--m-text); font-size: var(--text-sm); }
  .box { display: flex; align-items: flex-end; gap: 8px; padding: 4px 4px 4px 16px; border-radius: 24px; background: var(--m-surface); box-shadow: 0 2px 14px var(--m-shadow); }
  .box.over { box-shadow: inset 0 0 0 2px var(--m-text), 0 2px 14px var(--m-shadow); }
  .clip { display: grid; flex: none; place-items: center; align-self: flex-end; width: 32px; height: 32px; margin-left: -8px; padding: 0; border: 0; border-radius: 50%; background: none; color: var(--m-secondary); }
  .clip:hover { background: var(--bg-hover); color: var(--m-text); }
  .clip :global(svg) { width: 17px; height: 17px; }
  .files { display: flex; gap: 6px; overflow-x: auto; }
  .file { display: inline-flex; flex: none; align-items: center; gap: 6px; max-width: 220px; padding: 5px 6px 5px 10px; border-radius: 999px; background: var(--m-surface); font-size: var(--text-xs); }
  .file img { width: 20px; height: 20px; border-radius: 5px; object-fit: cover; }
  .file :global(svg) { width: 13px; height: 13px; color: var(--m-secondary); }
  .file span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .file button { display: grid; place-items: center; width: 18px; height: 18px; padding: 0; border: 0; border-radius: 50%; background: none; color: var(--m-secondary); }
  .file button:hover { background: var(--bg-active); }
  textarea { flex: 1; min-width: 0; max-height: 190px; padding: 8px 0; border: 0; outline: 0; resize: none; background: none; color: var(--m-text); font: inherit; font-size: 15px; line-height: 1.4; }
  textarea::placeholder { color: var(--m-tertiary); }
  .go { display: grid; flex: none; place-items: center; width: 32px; height: 32px; padding: 0; border: 0; border-radius: 50%; background: var(--m-dim); color: var(--m-tertiary); transition: background-color 0.2s var(--m-spring), color 0.2s var(--m-spring), transform 0.05s var(--m-ease); }
  .go.ready { background: var(--m-fill); color: var(--m-on-fill); }
  .go :global(svg) { width: 15px; height: 15px; stroke-width: 2.6; }
  @keyframes rise { from { opacity: 0; transform: translateY(6px); } }
  @media (prefers-reduced-motion: reduce) { .mentions { animation: none; } }
  @media (max-width: 720px) { textarea { font-size: 16px; } }
</style>
