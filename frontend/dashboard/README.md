# budi dashboard

React + TypeScript dashboard SPA mounted at `/dashboard` (client route basename) and built as static assets under `/static/dashboard/`.

## Local development

```bash
npm install
npm run dev
```

Vite runs on `http://127.0.0.1:5174` and proxies daemon API routes (`/analytics`, `/admin`, `/sync`, `/health`, `/hooks`, `/v1`) to `http://127.0.0.1:7878`.

### App surfaces

- Overview: cost, token, and attribution summaries
- Insights: cache/session/tool/MCP breakdowns
- Sessions: sortable session table, full-text search, CSV export, and drill-down
- Session detail: per-session health, tags, curve, and paginated messages
- Settings: daemon/integration status and maintenance actions

## Production build and daemon handoff

```bash
npm run build
```

The build writes directly to `crates/budi-daemon/static/dashboard-dist` (`vite.config.ts` `outDir`), which is served by `budi-daemon` at `/dashboard` with static assets rooted at `/static/dashboard/`.

From repo root you can run:

```bash
./scripts/build-dashboard.sh
```

That script installs dashboard dependencies and runs the same production build used for daemon-embedded assets.
