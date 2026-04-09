# Issue #60 Review: Daemon API Surface and Background Orchestration

## Findings (highest severity first)

### P1 (fixed): privileged daemon routes were reachable from non-loopback clients when externally bound
- Area: `crates/budi-daemon/src/main.rs`, `crates/budi-daemon/src/routes/mod.rs`
- Risk: admin and sync mutation endpoints had no transport-level guard. If daemon host was configured as non-loopback (for example `0.0.0.0`), remote clients could invoke migration/repair/install or force sync jobs.
- Fix in this PR:
  - added loopback-only middleware (`require_loopback`) for privileged routes
  - applied guard to all `/admin/*` endpoints and sync mutation routes (`POST /sync`, `POST /sync/all`, `POST /sync/reset`)
  - switched server startup to `into_make_service_with_connect_info::<SocketAddr>()` so peer address checks are enforced in runtime, not just tests

### P2 (fixed): daemon route docs drifted from implemented contract
- Area: `README.md`, `SOUL.md`
- Risk: missing endpoint docs can cause frontend/client integration mismatches and incomplete operational runbooks.
- Fix in this PR:
  - documented missing analytics routes (`/analytics/filter-options`, `/analytics/messages/{message_uuid}/detail`, `/analytics/sessions/{id}`, `/analytics/sessions/{id}/curve`, `/analytics/sessions/{id}/hook-events`, `/analytics/sessions/{id}/otel-events`, `/analytics/session-audit`)
  - documented `/admin/integrations/install`
  - clarified loopback-only behavior for privileged routes
  - corrected OTEL route summary to include both `/v1/logs` and `/v1/metrics`

## Runtime behavior and contract checks added

- `protected_admin_route_requires_connect_info`
- `protected_admin_route_allows_loopback_client`
- `protected_admin_route_blocks_non_loopback_client`
- `sync_mutation_route_blocks_non_loopback_client`

All were added in `crates/budi-daemon/src/main.rs` and assert loopback policy at the router boundary.

## Remaining daemon-level coverage suggestions

1. Add integration tests that assert privileged route behavior through a real bound socket (not only `oneshot`) for both IPv4 and IPv6 loopback addresses.
2. Add a startup smoke test for daemon replacement behavior (`kill_existing_daemon`) to ensure no false-positive process termination on shared ports.
3. Add queue worker lifecycle coverage for graceful shutdown semantics so background jobs stop cleanly during daemon exit/restart.
4. Add API contract tests asserting error payload shape consistency across `hooks`, `otel`, `sync`, and `admin` routes.

## Follow-up candidates (not in this PR)

- Consider introducing explicit auth (token or signed local session) for privileged routes as defense-in-depth beyond loopback transport checks.
- Decide and document whether `/sync/status` should remain remotely readable when daemon host is non-loopback.
