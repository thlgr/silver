// The thinking orb: a sphere of dots that turns while a bot works and breathes while it listens.

/** How fast engine time runs for a state at a size. */
export function speed(state, size) {
  const small = size < 40
  switch (state) {
    case 'listening': return small ? 3.998 : 4.388
    default: return small ? 3.9 : 1.885
  }
}

const hash = (a, b) => {
  const h = Math.sin(a * 12.9898 + b * 78.233) * 43758.5453
  return h - Math.floor(h)
}

const len = ([x, y, z]) => Math.hypot(x, y, z)
const scale = ([x, y, z], k) => [x * k, y * k, z * k]
const add = (a, b) => [a[0] + b[0], a[1] + b[1], a[2] + b[2]]

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

/** The listening orb: a breathing, rippling lattice of dots. */
function lattice(size, t) {
  const small = size < 40
  const rs = (size / 300) ** 0.6
  const rings = small ? 5 : 9
  const density = small ? 13 : 23
  const multiplier = small ? 1.6 : 1
  const radius = (size / 2) * 0.874
  const project = projector(t * 0.18, 0.38, size, 1)
  const dots = []
  for (let ri = 0; ri <= rings; ri++) {
    const lat = -Math.PI / 2 + (ri / rings) * Math.PI
    const [cosLat, sinLat] = [Math.cos(lat), Math.sin(lat)]
    const wave = 0.62 * Math.sin(t * 2.1 - ri * 0.52) + 0.38 * Math.sin(t * 1.27 + ri * 0.83)
    const rr = radius * (0.88 + 0.105 * wave)
    const lonCount = Math.max(1, Math.round(Math.abs(cosLat) * density))
    for (let j = 0; j < lonCount; j++) {
      const lon = (j / lonCount) * 2 * Math.PI
      const p = project(scale([cosLat * Math.cos(lon), sinLat, cosLat * Math.sin(lon)], rr))
      const depth = (p[2] / radius + 1) / 2
      const crest = Math.max(0, wave)
      dots.push(dot(p, (0.6 + 1.7 * depth) * multiplier * (1 + 0.4 * crest) * rs, 0.66 - 0.56 * depth - 0.1 * crest))
    }
  }
  return { dots, lines: [] }
}

/** The dots and lines to draw for a state at engine time `t` (already scaled by `speed`). */
export function orbFrame(state, size, t) {
  const frame = state === 'listening' ? lattice : orbits
  const { dots, lines } = frame(size, t)
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
