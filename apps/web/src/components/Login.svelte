<!-- Shown when the server answers 401: it wants the token from server.bearer_token. -->
<script>
  import { hasToken, setToken } from '../lib/api.js'

  let token = $state('')
  const rejected = hasToken()

  function submit(e) {
    e.preventDefault()
    setToken(token.trim())
    location.reload()
  }
</script>

<main>
  <form onsubmit={submit}>
    <h1>silver</h1>
    <p class="muted">
      {rejected ? 'This server did not accept the saved access token.' : 'This server needs an access token.'}
      It is <code>server.bearer_token</code> in config.toml, or <code>SILVER_BEARER_TOKEN</code>.
    </p>
    <!-- svelte-ignore a11y_autofocus -->
    <input class="field" type="password" placeholder="Access token" aria-label="Access token" autocomplete="off" autofocus bind:value={token} />
    <button class="btn primary" disabled={!token.trim()}>Continue</button>
  </form>
</main>

<style>
  main { display: grid; place-items: center; height: 100vh; padding: var(--space-4); }
  form { display: grid; gap: var(--space-3); width: min(420px, 100%); }
  h1 { margin: 0; font-size: var(--text-xl); font-weight: 600; letter-spacing: -0.02em; }
  p { margin: 0; font-size: var(--text-sm); }
</style>
