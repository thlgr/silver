// Date and time labels for a chat and its roster.

const clock = (date) => date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })
const sameDay = (a, b) => a.toDateString() === b.toDateString()
const dayLabel = (date, now) => {
  const yesterday = new Date(now)
  yesterday.setDate(now.getDate() - 1)
  if (sameDay(date, now)) return 'Today'
  if (sameDay(date, yesterday)) return 'Yesterday'
  return date.toLocaleDateString([], { month: 'short', day: 'numeric' })
}

/** The time of a message: 3:45 PM. */
export const time = (ms) => clock(new Date(ms))

/** A roster stamp: 3:45 PM, Yesterday, Wednesday or Sep 16. */
export function day(ms, now = new Date()) {
  const date = new Date(ms)
  if (sameDay(date, now)) return clock(date)
  const label = dayLabel(date, now)
  if (label === 'Yesterday') return label
  return now - date < 6 * 86_400_000 ? date.toLocaleDateString([], { weekday: 'long' }) : label
}

/** A chat separator, shown after a gap: Today 3:27 PM, Yesterday 5:20 PM or Sep 16 9:02 AM. */
export const separator = (ms, now = new Date()) => `${dayLabel(new Date(ms), now)} ${clock(new Date(ms))}`

/** How long a turn has been going: 12s, 3m 04s. */
export function elapsed(ms) {
  const seconds = Math.max(0, Math.floor(ms / 1000))
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, '0')}s`
}
