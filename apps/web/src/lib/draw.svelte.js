// What the avatars and the orb share: one animation loop for every canvas that moves, and the
// page's text colour, which they draw with so a theme change repaints them.

const live = new Set()
let frame = 0

function loop(now) {
  for (const item of live) {
    if (item.every && now - item.last < item.every) continue
    item.last = now
    item.paint(now / 1000)
  }
  frame = live.size ? requestAnimationFrame(loop) : 0
}

/** Call `paint(seconds)` every frame (or at most every `every` ms) until the returned stop runs. */
export function animate(paint, every = 0) {
  const item = { paint, every, last: 0 }
  live.add(item)
  frame ||= requestAnimationFrame(loop)
  return () => live.delete(item)
}

export const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)')

/** Bumped whenever the colour scheme changes, so a canvas that read it can draw again. */
export const scheme = $state({ tick: 0 })
const bump = () => scheme.tick++
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', bump)
new MutationObserver(bump).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] })

/** `rgb(r g b)` of a computed colour, as three numbers. */
export function rgb(element) {
  const match = getComputedStyle(element).color.match(/[\d.]+/g) ?? ['128', '128', '128']
  return match.slice(0, 3).map(Number)
}

/** A canvas sized for the screen's pixel density; returns its 2D context in CSS pixels. */
export function sharpen(canvas, width, height) {
  const scale = devicePixelRatio || 1
  canvas.width = Math.round(width * scale)
  canvas.height = Math.round(height * scale)
  const ctx = canvas.getContext('2d')
  ctx.setTransform(scale, 0, 0, scale, 0, 0)
  return ctx
}

/** Pause a canvas that is scrolled out of view. `onChange(visible)` fires on each change. */
export function whenVisible(element, onChange) {
  const observer = new IntersectionObserver(([entry]) => onChange(entry.isIntersecting))
  observer.observe(element)
  return () => observer.disconnect()
}
