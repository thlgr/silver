<!-- A bot's face: halftone dots shaded like a ball, with hollow eyes that glance and blink while
     the bot works and a ripple when it needs you. -->
<script>
  import { paintAvatar, rgbOf } from '../../lib/avatar.js'
  import { animate, reducedMotion, rgb, scheme, sharpen, whenVisible } from '../../lib/draw.svelte.js'

  let { shape = 'blob', color = 'blue', size = 40, mood = 'idle' } = $props()
  let canvas = $state()
  let visible = $state(true)

  $effect(() => (canvas ? whenVisible(canvas, (shown) => (visible = shown)) : undefined))

  $effect(() => {
    if (!canvas) return
    scheme.tick // repaint when the theme changes
    const ctx = sharpen(canvas, size, size)
    const ink = rgb(canvas)
    const tint = rgbOf(color)
    const paint = (t) => paintAvatar(ctx, { shape, size, mood, ink, tint, t })
    if (mood === 'idle' || reducedMotion.matches || !visible) {
      paint(0)
      return
    }
    return animate(paint)
  })
</script>

<canvas bind:this={canvas} style:width="{size}px" style:height="{size}px" aria-hidden="true"></canvas>

<style>
  canvas { display: block; flex: none; color: var(--m-text); }
</style>
