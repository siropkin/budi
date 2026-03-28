# Endpoint Review — 2026-03-28

## Critical

- [x] 1. Validate `sort_by` in `/analytics/sessions` (SQL injection risk)
- [x] 2. Add max limit cap to repos/tags/tools/mcp endpoints (capped at 200)

## High (consistency)

- [x] 3. Rename `/analytics/repos` → `/analytics/projects` (router, client, dashboard JS, MCP)
- [x] 4. Move tools/mcp handlers from `hooks.rs` to `analytics.rs`
- [x] 5. ~~Add `offset` + `total_count` wrapper~~ — Skipped: projects/tags/tools/mcp are top-N chart endpoints, not paginated lists. Only messages and sessions need pagination (already have it). Adding offset/total_count would be overengineering.
- [x] 6. Add `limit` to `/analytics/models` (default 50, max 200), `/analytics/branches` (default 50, max 200). Skipped `/analytics/providers` — only 2-3 rows ever (one per provider).

## Medium (cleanup)

- [x] 7. Typed response struct for session tags (`SessionTag` struct replaces `json!()`)
- [x] 8. Consolidate param structs: removed `BranchDetailParams` (→ `DateRangeParams`), removed `ListParams` (→ `ProjectsParams`). 9 → 7 structs.
- [x] 9. Typed structs for `health` (`HealthResponse`), `sync_status` (`SyncStatusResponse`), and all sync endpoints (`SyncResponse`). Kept `health_integrations`/`health_check_update`/`schema_version`/`migrate` as untyped JSON (complex/admin-only).
