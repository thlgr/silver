// Small display formatters shared by several components.

export function ago(iso) {
  const minutes = Math.floor((Date.now() - new Date(iso)) / 60000)
  if (minutes < 1) return 'now'
  if (minutes < 60) return `${minutes}m`
  if (minutes < 1440) return `${Math.floor(minutes / 60)}h`
  if (minutes < 10080) return `${Math.floor(minutes / 1440)}d`
  return new Date(iso).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}

/** A session's name before the daemon titles it: the first prompt stands in. */
export const sessionLabel = (s) => s.title || s.preview || 'Untitled session'

export const tokens = (n) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}K` : `${n ?? 0}`)

export const seconds = (ms) => (ms < 60000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.floor(ms / 60000)}m ${Math.round((ms % 60000) / 1000)}s`)

export const cost = (usd) => (usd ? `$${usd < 0.01 ? usd.toFixed(4) : usd.toFixed(2)}` : '')

// The daemon budgets three bytes per token; the provider's own count of a request (a
// context usage with prompt_tokens) gives the ratio its tokenizer really has.
export const perByte = (c) => (c?.prompt_tokens ? c.prompt_tokens / c.total_bytes : 1 / 3)

export const approxTokens = (bytes, ratio) => tokens(Math.round(bytes * ratio))

/** Tokens in the context: the provider's count once the model answered, else the request's
 * bytes at the latest ratio. */
export const contextTokens = (c, ratio) => c.prompt_tokens ?? Math.round(c.total_bytes * ratio)

export const percent = (c) => (c?.budget_bytes ? Math.min(100, Math.round((c.total_bytes * 100) / c.budget_bytes)) : 0)

/** Generated tokens per second, at the latest bytes-per-token ratio. `ms` is only the time the
 *  provider spent streaming, so a tool call's silence and a stall never count. */
export const perSecond = (bytes, ms, ratio) => {
  if (ms <= 0) return ''
  const tps = (bytes * ratio) / (ms / 1000)
  return `~${tps < 10 ? tps.toFixed(1) : Math.round(tps)} tok/s`
}
