# Web UI (apps/web)

The browser client of [silver](../../README.md), a Svelte 5 app compiled into the binary and
served by the daemon on its own port. See [docs/web-ui.md](../../docs/web-ui.md) for what it
does and how it is structured.

## Development

    npm install
    npm run dev        # http://localhost:5173 with HMR, proxying /v1 to silver
    npm run build      # refresh apps/web/dist; a debug silver serves it without recompiling

Vite proxies `/v1` to `SILVER_URL` (default `http://127.0.0.1:7777`) and adds
`SILVER_BEARER_TOKEN` as the `Authorization` header when set. Run a debug silver in another
terminal (`cargo run -p silver`). A debug build serves `apps/web/dist` from disk, so
`npx vite build --watch` plus a refresh on :7777 also works.