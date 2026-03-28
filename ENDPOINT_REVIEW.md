# Endpoint Consistency Review — 2026-03-28

## Must Fix

- [x] 1. Rename `ProjectsParams` → `ListParams` (reused by models, branches, tools, mcp — naming is misleading)
- [x] 2. Standardize `limit` max: all endpoints now max 200, defaults 20 for chart endpoints, 50 for paginated
- [x] 3. Add typed response structs for `health_integrations`, `health_check_update`, `schema_version`, `migrate`
- [x] 4. ~~Wrap bare array responses~~ — Skipped: these are top-N chart endpoints, not paginated lists. Wrapping adds complexity for no real benefit. Only messages/sessions need pagination (already have it).
- [x] 5. ~~Add `offset` to list endpoints~~ — Skipped: same as #4. Chart endpoints are top-N, not paginated. Adding offset would be unused code.
- [x] 6. Move non-analytics endpoints to `/admin/` namespace: schema-version → `/admin/schema`, migrate → `/admin/migrate`, registered-providers → `/admin/providers`. Updated: router, dashboard JS, CLI client, MCP client.

## Won't Fix

- Hyphen convention in multi-word names — consistent pattern for multi-word
- `since/until` on all data endpoints — already consistent
- Error format `{ ok: false, error }` — already consistent
- Session sub-resources without pagination — bounded by session size
- `provider` filter only on summary/cost — not needed elsewhere currently
