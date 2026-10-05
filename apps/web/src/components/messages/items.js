// What a chat or thread shows, from its entries: time separators after a gap of an hour, and who
// starts a run of messages.

const GAP = 3_600_000

export function buildItems(entries) {
  const out = []
  let last = null
  let before = null
  for (const entry of entries) {
    const text = entry.text?.trim()
    if (entry.kind === 'agent' && text === '(pass)') continue
    if (last === null || entry.created_at - last > GAP) {
      out.push({ id: `sep-${entry.id}`, separator: entry.created_at })
      before = null
    }
    const who = entry.kind === 'user' ? 'user' : entry.kind === 'agent' ? `agent:${entry.author}` : entry.kind
    out.push({ id: entry.nonce && entry.kind === 'user' ? `user-${entry.nonce}` : entry.id, entry, start: who !== before })
    before = entry.kind === 'user' || entry.kind === 'agent' ? who : null
    last = entry.created_at
  }
  return out
}
