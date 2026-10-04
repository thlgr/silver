// A bot's face: an even grid of dots shaded as if its silhouette were a ball, with two hollow
// eyes (missing dots). While the bot works the eyes glance side to side and blink now and then;
// when it needs the user a ripple runs across it. Ported from Codync's CharacterAvatar.

export const COLORS = [
  { id: 'black', label: 'Black', hex: '#2b2b2b' },
  { id: 'brown', label: 'Brown', hex: '#936439' },
  { id: 'red', label: 'Red', hex: '#ff263c' },
  { id: 'orange', label: 'Orange', hex: '#ff6700' },
  { id: 'yellow', label: 'Yellow', hex: '#ff9800' },
  { id: 'green', label: 'Green', hex: '#00c972' },
  { id: 'cyan', label: 'Cyan', hex: '#00bca6' },
  { id: 'blue', label: 'Blue', hex: '#1084fe' },
  { id: 'violet', label: 'Violet', hex: '#9159fe' },
  { id: 'magenta', label: 'Magenta', hex: '#ff309b' },
  { id: 'gray', label: 'Gray', hex: '#777777' },
]

export const SHAPES = ['blob', 'pebble', 'squircle', 'tablet', 'wedge', 'hex', 'cloud', 'teardrop']

export const hexOf = (id) => (COLORS.find((c) => c.id === id) ?? COLORS[7]).hex

export function rgbOf(id) {
  const hex = hexOf(id)
  return [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16))
}

// Small faces use fewer, larger dots so the gaps and the eyes survive.
function grid(size) {
  const cells = size < 18 ? 7 : size < 28 ? 9 : 13
  return {
    cells,
    eyeColumns: cells === 7 ? [2, 4] : cells === 9 ? [3, 6] : [4, 8],
    eyeRows: cells === 13 ? [4, 5, 6] : cells === 9 ? [3, 4] : [2, 3],
  }
}

/** The silhouette as shapes a point can be tested against: Path2D parts, plus a stroke width
 *  for those that are outlined as well as filled. */
function silhouette(kind, w, h) {
  const part = (build) => {
    const path = new Path2D()
    build(path)
    return path
  }
  switch (kind) {
    case 'pebble':
      return { parts: [part((p) => p.ellipse(w / 2, h / 2, w / 2, h * 0.4, 0, 0, 2 * Math.PI))] }
    case 'squircle':
      return { parts: [part((p) => p.roundRect(w * 0.04, h * 0.04, w * 0.92, h * 0.92, w * 0.3))] }
    case 'tablet':
      return { parts: [part((p) => p.roundRect(w * 0.14, 0, w * 0.72, h, w * 0.22))] }
    case 'wedge':
      return {
        parts: [
          part((p) => {
            p.moveTo(w * 0.5, h * 0.04)
            p.quadraticCurveTo(w * 0.9, h * 0.4, w * 0.98, h * 0.86)
            p.quadraticCurveTo(w * 0.5, h * 1.04, w * 0.02, h * 0.86)
            p.quadraticCurveTo(w * 0.1, h * 0.4, w * 0.5, h * 0.04)
          }),
        ],
      }
    case 'hex':
      return {
        stroke: w * 0.08,
        parts: [
          part((p) => {
            for (let i = 0; i < 6; i++) {
              const a = (i * Math.PI) / 3 - Math.PI / 2
              const x = w / 2 + Math.cos(a) * w * 0.49
              const y = h / 2 + Math.sin(a) * h * 0.49
              i ? p.lineTo(x, y) : p.moveTo(x, y)
            }
            p.closePath()
          }),
        ],
      }
    case 'cloud':
      return {
        parts: [
          part((p) => p.ellipse(w * 0.275, h * 0.575, w * 0.275, h * 0.275, 0, 0, 2 * Math.PI)),
          part((p) => p.ellipse(w * 0.725, h * 0.575, w * 0.275, h * 0.275, 0, 0, 2 * Math.PI)),
          part((p) => p.ellipse(w * 0.5, h * 0.4, w * 0.32, h * 0.32, 0, 0, 2 * Math.PI)),
          part((p) => p.roundRect(w * 0.1, h * 0.5, w * 0.8, h * 0.35, w * 0.15)),
        ],
      }
    case 'teardrop':
      return {
        parts: [
          part((p) => {
            p.moveTo(w * 0.5, 0)
            p.bezierCurveTo(w * 0.62, h * 0.2, w * 0.94, h * 0.38, w * 0.94, h * 0.62)
            p.arc(w * 0.5, h * 0.62, w * 0.44, 0, Math.PI, false)
            p.bezierCurveTo(w * 0.06, h * 0.38, w * 0.38, h * 0.2, w * 0.5, 0)
          }),
        ],
      }
    default:
      return {
        parts: [
          part((p) => {
            for (let i = 0; i <= 64; i++) {
              const a = (i / 64) * 2 * Math.PI
              const r = 0.46 + 0.035 * Math.sin(a * 3 + 0.6)
              const x = w / 2 + Math.cos(a) * w * r
              const y = h / 2 + Math.sin(a) * h * r
              i ? p.lineTo(x, y) : p.moveTo(x, y)
            }
            p.closePath()
          }),
        ],
      }
  }
}

let probe = null
const cache = new Map()

/** The grid dots that fall inside a shape, for a face this many pixels across. */
function dotsFor(kind, size) {
  const key = `${kind}|${size}`
  if (cache.has(key)) return cache.get(key)
  probe ??= document.createElement('canvas').getContext('2d')
  const { cells } = grid(size)
  const step = size / cells
  const { parts, stroke } = silhouette(kind, size, size)
  probe.lineWidth = stroke ?? 1
  probe.lineJoin = 'round'
  const dots = []
  for (let row = 0; row < cells; row++) {
    for (let col = 0; col < cells; col++) {
      const x = (col + 0.5) * step
      const y = (row + 0.5) * step
      if (parts.some((p) => probe.isPointInPath(p, x, y) || (stroke && probe.isPointInStroke(p, x, y)))) {
        dots.push({ row, col, x, y })
      }
    }
  }
  cache.set(key, dots)
  return dots
}

/** Draw one face. `ink` is the page's text colour, `tint` the bot's colour, `t` seconds. */
export function paintAvatar(ctx, { shape, size, mood, ink, tint, t }) {
  const { cells, eyeColumns, eyeRows } = grid(size)
  const step = size / cells
  const half = size / 2
  const working = mood === 'working'
  // Glance in whole-cell steps, and blink now and then, only while the bot is busy.
  const glance = working ? Math.round(Math.sin((t * 2 * Math.PI) / 3.2) * 1.4) : 0
  const blinking = mood !== 'idle' && (t / 4.7) % 1 < 0.035
  const rows = blinking ? eyeRows.slice(-1) : eyeRows
  const cols = eyeColumns.map((c) => c + glance)
  const yaw = working ? t * 1.4 : -0.7
  const lx = Math.sin(yaw) * 0.8
  const ly = 0.55
  const lz = Math.cos(yaw) * 0.5 + 0.6
  const ll = Math.hypot(lx, ly, lz)
  ctx.clearRect(0, 0, size, size)
  for (const dot of dotsFor(shape, size)) {
    if (cols.includes(dot.col) && rows.includes(dot.row)) continue
    const u = (dot.x - half) / half
    const v = (half - dot.y) / half
    const z = Math.sqrt(Math.max(0.2, 1 - u * u - v * v))
    const nl = Math.hypot(u, v, z)
    let shade = 0.3 + 0.7 * Math.max(0, (u * lx + v * ly + z * lz) / (nl * ll))
    if (mood === 'needsInput') {
      const ripple = 0.5 + 0.5 * Math.sin(Math.hypot(u, v) * 9 - t * 5)
      shade *= 0.6 + 0.4 * ripple
    }
    const radius = step * 0.42 * (0.55 + 0.45 * shade)
    const strength = cells < 13 ? 0.4 + 0.4 * shade : 0.2 + 0.4 * Math.min(1, shade / 0.7)
    ctx.fillStyle = `rgb(${ink[0]} ${ink[1]} ${ink[2]} / ${strength})`
    ctx.beginPath()
    ctx.arc(dot.x, dot.y, radius, 0, 2 * Math.PI)
    ctx.fill()
    if (shade > 0.6) {
      ctx.fillStyle = `rgb(${tint[0]} ${tint[1]} ${tint[2]} / ${(shade - 0.6) / 0.4})`
      ctx.fill()
    }
  }
}
