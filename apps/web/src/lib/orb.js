// The thinking orb: a sphere of dots that turns while a bot works, scans while it searches,
// breathes while it listens and links up while it connects. Ported from Codync's ThinkingOrb,
// itself a port of thinking-orbs 0.3.1 (MIT, Jakub Antalik); the maths is the same so the two look alike.

/** How fast engine time runs for a state at a size. */
export function speed(state, size) {
  const small = size < 40
  switch (state) {
    case 'searching': return small ? 2.665 : 2.015
    case 'listening': return small ? 3.998 : 4.388
    case 'connecting': return small ? 6.63 : 3.315
    default: return small ? 3.9 : 1.885
  }
}

const hash = (a, b) => {
  const h = Math.sin(a * 12.9898 + b * 78.233) * 43758.5453
  return h - Math.floor(h)
}

function noise(x, y) {
  const xi = Math.floor(x)
  const yi = Math.floor(y)
  const dx = x - xi
  const dy = y - yi
  const fx = dx * dx * (3 - 2 * dx)
  const fy = dy * dy * (3 - 2 * dy)
  const a = hash(xi, yi)
  const b = hash(xi + 1, yi)
  const c = hash(xi, yi + 1)
  const d = hash(xi + 1, yi + 1)
  return a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy
}

const len = ([x, y, z]) => Math.hypot(x, y, z)
const scale = ([x, y, z], k) => [x * k, y * k, z * k]
const add = (a, b) => [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
const sub = (a, b) => [a[0] - b[0], a[1] - b[1], a[2] - b[2]]

function projector(yaw, tilt, size, factor) {
  const [sy, cy, st, ct] = [Math.sin(yaw), Math.cos(yaw), Math.sin(tilt), Math.cos(tilt)]
  return ([px, py, pz]) => {
    const x = px * cy + pz * sy
    const z = -px * sy + pz * cy
    return [size / 2 + x * factor, size / 2 - (py * ct - z * st) * factor, py * st + z * ct]
  }
}

const dot = (p, radius, white, alpha = 1) => ({ x: p[0], y: p[1], z: p[2], radius, white, alpha })

function orbits(size, t) {
  const small = size < 40
  const radius = (size / 2) * 0.82
  const project = projector(t * 0.12, 0.3, size, 1)
  const rs = (size / 300) ** 0.6
  const multiplier = small ? 2.4 : 1
  const orbitCount = small ? 3 : 12
  const ghostCount = small ? 10 : 40
  const dots = []
  for (let orb = 0; orb < orbitCount; orb++) {
    const h1 = hash(orb, 1.7)
    const h2 = hash(orb, 5.2)
    const h3 = hash(orb, 8.9)
    const ro = radius * (0.45 + 0.52 * h1)
    const theta = h1 * 2 * Math.PI
    const phi = Math.acos(2 * h2 - 1)
    const normal = [Math.sin(phi) * Math.cos(theta), Math.cos(phi), Math.sin(phi) * Math.sin(theta)]
    let u = [-normal[1], normal[0], 0]
    u = scale(u, 1 / Math.max(1e-6, len(u)))
    const v = [-normal[2] * u[1], normal[2] * u[0], normal[0] * u[1] - normal[1] * u[0]]
    const around = (angle) => scale(add(scale(u, Math.cos(angle)), scale(v, Math.sin(angle))), ro)
    const pace = (0.25 + 0.55 * h3) * (h3 > 0.5 ? 1 : -1)
    for (let k = 0; k < ghostCount; k++) {
      const p = project(around((k / ghostCount) * 2 * Math.PI))
      const depth = (p[2] / ro + 1) / 2
      dots.push(dot(p, 0.9 * multiplier * rs, 0.72, 0.5 * (0.4 + 0.6 * depth)))
    }
    for (let m = 0; m < 3; m++) {
      const p = project(around(t * pace + (m / 3) * 2 * Math.PI + h2 * 6))
      const depth = (p[2] / ro + 1) / 2
      dots.push(dot(p, (1.2 + 1.6 * depth) * multiplier * rs, 0.3 - 0.22 * depth))
    }
  }
  return { dots, lines: [] }
}

function lattice(size, t, listening) {
  const small = size < 40
  const rs = (size / 300) ** 0.6
  const rings = listening ? (small ? 5 : 9) : small ? 6 : 11
  const density = listening ? (small ? 13 : 23) : small ? 14 : 29
  const multiplier = listening ? (small ? 1.6 : 1) : small ? 1.75 : 1.15
  const radius = (size / 2) * (listening ? 0.874 : 0.82)
  const tilt = listening ? 0.38 : 0.4 + 0.06 * Math.sin(t * 0.35)
  const project = projector(t * (listening ? 0.18 : 0.5), tilt, size, listening ? 1 : radius)
  const scan = t * (0.5 + 1.2 * (small ? 4.335 : 4.08))
  const dots = []
  for (let ri = 0; ri <= rings; ri++) {
    const lat = -Math.PI / 2 + (ri / rings) * Math.PI
    const [cosLat, sinLat] = [Math.cos(lat), Math.sin(lat)]
    const wave = 0.62 * Math.sin(t * 2.1 - ri * 0.52) + 0.38 * Math.sin(t * 1.27 + ri * 0.83)
    const rr = listening ? radius * (0.88 + 0.105 * wave) : 1
    const lonCount = Math.max(1, Math.round(Math.abs(cosLat) * density))
    for (let j = 0; j < lonCount; j++) {
      const lon = (j / lonCount) * 2 * Math.PI
      const p = project(scale([cosLat * Math.cos(lon), sinLat, cosLat * Math.sin(lon)], rr))
      const depth = (p[2] / (listening ? radius : 1) + 1) / 2
      if (listening) {
        const crest = Math.max(0, wave)
        dots.push(dot(p, (0.6 + 1.7 * depth) * multiplier * (1 + 0.4 * crest) * rs, 0.66 - 0.56 * depth - 0.1 * crest))
      } else {
        const angle = lon + t * 0.5 - scan
        const delta = Math.atan2(Math.sin(angle), Math.cos(angle))
        const boost = Math.exp(-(delta * delta) / 0.18) * Math.max(0, p[2])
        dots.push(dot(p, ((0.6 + 1.7 * depth) * multiplier + boost) * rs, 0.62 - 0.54 * depth, 0.45 + 0.55 * Math.min(1, boost)))
      }
    }
  }
  return { dots, lines: [] }
}

function web(size, t) {
  const small = size < 40
  const rs = (size / 300) ** 0.6
  const count = small ? 8 : 41
  const signals = small ? 1 : 7
  const multiplier = small ? 1.52 : 0.95
  const threshold = 0.72
  const project = projector(t * 0.12, 0.32, size, (size / 2) * 0.8)
  const golden = Math.PI * (3 - Math.sqrt(5))
  const nodes = Array.from({ length: count }, (_, i) => {
    const y = 1 - (2 * (i + 0.5)) / count
    const r = Math.sqrt(1 - y * y)
    const a = i * golden
    const p = [
      r * Math.cos(a) + 0.3 * (noise(i * 0.31 + 9, t * 0.24) - 0.5) * 2,
      y + 0.3 * (noise(i * 0.53 + 27, t * 0.21) - 0.5) * 2,
      r * Math.sin(a) + 0.3 * (noise(i * 0.77 + 55, t * 0.27) - 0.5) * 2,
    ]
    return scale(p, 1 / len(p))
  })
  const dots = []
  const lines = []
  for (let i = 0; i < count; i++) {
    for (let j = i + 1; j < count; j++) {
      const distance = len(sub(nodes[i], nodes[j]))
      if (distance >= threshold) continue
      const a = project(nodes[i])
      const b = project(nodes[j])
      const depth = ((a[2] + b[2]) / 2 + 1) / 2
      lines.push({
        x1: a[0], y1: a[1], x2: b[0], y2: b[1], white: 0.42,
        alpha: (1 - distance / threshold) * (0.3 + 0.55 * depth), width: Math.max(0.6, 0.8 * rs),
      })
    }
    const p = project(nodes[i])
    const depth = (p[2] + 1) / 2
    const pulse = 1 + 0.25 * Math.sin(t * 1.4 + i * 2.7)
    dots.push(dot(p, (1.4 + 1.8 * depth) * multiplier * pulse * rs, 0.55 - 0.45 * depth))
  }
  for (let s = 0; s < signals; s++) {
    const segment = Math.floor(t * 0.55 + s * 7.31)
    const a = Math.floor(hash(segment, s * 3.1 + 1.7) * count)
    const b = Math.floor(hash(segment, s * 5.7 + 4.2) * count)
    if (a === b) continue
    const tick = t * 0.55 + s * 7.31
    const node = add(nodes[a], scale(sub(nodes[b], nodes[a]), tick - Math.floor(tick)))
    const p = project(scale(node, 1 / Math.max(1e-6, len(node))))
    const depth = (p[2] + 1) / 2
    dots.push(dot(p, (1.4 * 1.5 + 1.8 * depth) * multiplier * rs, 0.05, 0.5 + 0.5 * depth))
  }
  return { dots, lines }
}

/** The dots and lines to draw for a state at engine time `t` (already scaled by `speed`). */
export function orbFrame(state, size, t) {
  const frame = { orbits, searching: (s, time) => lattice(s, time, false), listening: (s, time) => lattice(s, time, true), connecting: web }[state]
  const { dots, lines } = (frame ?? orbits)(size, t)
  return {
    dots: dots
      .filter((d) => d.alpha >= 0.02)
      .map((d) => ({ ...d, radius: Math.max(0.3, d.radius) }))
      .sort((a, b) => a.z - b.z),
    lines: lines.filter((l) => l.alpha >= 0.02),
  }
}

/** How strongly something that is `white` (0 dark … 1 light) at `alpha` is inked. */
export const ink = (white, alpha) => (1 - Math.min(1, Math.max(0, white))) * alpha
