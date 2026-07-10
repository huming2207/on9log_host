# on9log web UI

Bun + Vite + React frontend for the `on9log --web` backend.

## Commands

```bash
bun install
bun run dev
bun run build
bun run lint
bun run format:check
```

During development, Vite proxies `/api/*` and `/ws/*` to
`http://127.0.0.1:9090`. To point at another backend:

```bash
VITE_ON9LOG_BACKEND_URL=http://127.0.0.1:9092 bun run dev
```

The production build uses same-origin API and websocket URLs. The
`on9log-cli` build script embeds the current `dist` directory into the Rust
binary, and the Axum backend serves those bundled files when `on9log --web` is
running.
