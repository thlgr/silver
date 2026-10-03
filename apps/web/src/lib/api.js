// Thin fetch wrapper over the silver HTTP API (same origin; proxied under /v1 by `vite dev`).

const TOKEN_KEY = 'silver.token'
let bearer = null
try { bearer = localStorage.getItem(TOKEN_KEY) } catch { bearer = null }

export const hasToken = () => !!bearer

export function setToken(value) {
  bearer = value || null
  try { bearer ? localStorage.setItem(TOKEN_KEY, bearer) : localStorage.removeItem(TOKEN_KEY) } catch { return }
}

const authorization = () => (bearer ? { authorization: `Bearer ${bearer}` } : {})

// 'up' | 'down' (the server does not answer) | 'locked' (it wants a bearer token)
let link = 'up'
let onLink = () => {}

function report(next) {
  if (next === link) return
  link = next
  onLink(next)
}

async function probe() {
  try {
    const res = await fetch('/v1/daemon/status', { headers: authorization() })
    report(res.status === 401 ? 'locked' : 'up')
  } catch {
    report('down')
  }
}

/** Call `fn` when the server goes away, comes back or asks for a token. One small request every
 *  15 s (3 s while it is away) notices a change nobody has triggered yet. */
export function watchLink(fn) {
  onLink = fn
  const tick = async () => {
    if (!document.hidden) await probe()
    setTimeout(tick, link === 'down' ? 3000 : 15000)
  }
  setTimeout(tick, 15000)
  document.addEventListener('visibilitychange', () => document.hidden || probe())
}

export async function api(path, { method = 'GET', body, query } = {}) {
  const params = query && new URLSearchParams(Object.entries(query).filter(([, v]) => v != null))
  const url = params?.size ? `${path}?${params}` : path
  let res
  try {
    res = await fetch(url, {
      method,
      headers: { ...authorization(), ...(body === undefined ? {} : { 'content-type': 'application/json' }) },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  } catch {
    report('down')
    throw new Error(`Can't reach silver at ${location.origin}. Is it running?`)
  }
  report(res.status === 401 ? 'locked' : 'up')
  const text = await res.text()
  const data = text ? JSON.parse(text) : null
  if (!res.ok) throw new Error(data?.error?.message ?? data?.message ?? `HTTP ${res.status}`)
  return data
}

/** Svelte action for an <img> served by `/v1`: an <img> cannot send the bearer token, so with one
 *  saved the picture is fetched with it and shown as a data: URL, which the CSP allows. */
export function authImage(img, url) {
  async function show(next) {
    if (!bearer) {
      img.src = next
      return
    }
    try {
      const res = await fetch(next, { headers: authorization() })
      if (!res.ok) return
      const reader = new FileReader()
      reader.onload = () => (img.src = reader.result)
      reader.readAsDataURL(await res.blob())
    } catch {
      return
    }
  }
  show(url)
  return { update: show }
}

/** Follow a run's SSE stream with fetch, because EventSource cannot send the bearer token. A
 *  dropped connection resumes after the last event id it saw. */
export function subscribe(runId, onEvent) {
  const stop = new AbortController()
  let lastId = null

  function handle(frame) {
    const data = []
    for (const line of frame.split(/\r?\n/)) {
      if (line.startsWith('id:')) lastId = line.slice(3).trim()
      else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''))
    }
    if (!data.length) return
    try { onEvent(JSON.parse(data.join('\n'))) } catch (e) { console.error(e) }
  }

  ;(async () => {
    while (!stop.signal.aborted) {
      try {
        const res = await fetch(`/v1/runs/${runId}/events`, {
          headers: { ...authorization(), ...(lastId && { 'last-event-id': lastId }) },
          signal: stop.signal,
        })
        report(res.status === 401 ? 'locked' : 'up')
        if (!res.ok) return
        const reader = res.body.pipeThrough(new TextDecoderStream()).getReader()
        let buffer = ''
        for (;;) {
          const { done, value } = await reader.read()
          if (done) break
          const frames = (buffer + value).split(/\r?\n\r?\n/)
          buffer = frames.pop()
          frames.forEach(handle)
        }
      } catch {
        if (stop.signal.aborted) return
        report('down')
      }
      await new Promise((resolve) => setTimeout(resolve, 1000))
    }
  })()
  return { close: () => stop.abort() }
}
