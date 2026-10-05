<!-- The thinking orb: a sphere of dots that turns while a bot works. Say what it means in the text
     beside it; this is decoration. -->
<script>
  import { ink, orbFrame, speed } from '../../lib/orb.js'
  import { animate, reducedMotion, rgb, scheme, sharpen, whenVisible } from '../../lib/draw.svelte.js'

  let { kind = 'working', size = 16, color = 'var(--m-secondary)', animated = true } = $props()
  let canvas = $state()
  let visible = $state(true)

  $effect(() => (canvas ? whenVisible(canvas, (shown) => (visible = shown)) : undefined))

  $effect(() => {
    if (!canvas) return
    scheme.tick
    color // repaint when the colour changes
    const ctx = sharpen(canvas, size, size)
    const [r, g, b] = rgb(canvas)
    const rate = speed(kind, size)
    const paint = (seconds) => {
      const dots = orbFrame(kind, size, 0.6 + seconds * rate)
      ctx.clearRect(0, 0, size, size)
      for (const d of dots) {
        ctx.fillStyle = `rgb(${r} ${g} ${b} / ${ink(d.white, d.alpha)})`
        ctx.beginPath()
        ctx.arc(d.x, d.y, d.radius, 0, 2 * Math.PI)
        ctx.fill()
      }
    }
    if (!animated || reducedMotion.matches || !visible) {
      paint(0)
      return
    }
    const start = performance.now() / 1000
    return animate((now) => paint(now - start), 33)
  })
</script>

<canvas bind:this={canvas} style:width="{size}px" style:height="{size}px" style:color aria-hidden="true"></canvas>

<style>
  canvas { display: block; flex: none; }
</style>
