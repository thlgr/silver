import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import Icons from 'unplugin-icons/vite'

// Dev server only: `npm run dev` proxies /v1 to a running silver, adding the bearer token
// server-side. The built UI is embedded in the silver binary and served same-origin.
const target = process.env.SILVER_URL ?? 'http://127.0.0.1:7777'
const token = process.env.SILVER_BEARER_TOKEN
const proxy = {
  '/v1': {
    target,
    headers: token ? { authorization: `Bearer ${token}` } : {},
  },
}

export default defineConfig({
  plugins: [svelte(), Icons({ compiler: 'svelte' })],
  server: { proxy },
  preview: { proxy },
})
