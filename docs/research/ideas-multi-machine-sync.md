# Multi-Machine Sync Design

- **Date**: 2026-03-22
- **Status**: Deferred (post-8.0, superseded by cloud sync for team use case)
- **Origin**: Reddit request (berlinguyinca) — uses Claude Code on 5 machines, wants combined analytics

## Problem

Same developer using budi on multiple machines (laptop, desktop, work, home) sees separate analytics per machine. They want a combined view.

## Options Considered

### Serverless

1. **cr-sqlite (CRDTs)** — SQLite extension with automatic conflict-free merging. No server needed, works over any transport. Most promising.
2. **Git-based sync** — Append-only event log pushed to private git repo. Each machine commits + pulls + replays.
3. **Syncthing/Resilio** — P2P file sync. Problem: SQLite concurrent writes = corruption unless using event log.
4. **Cloud storage** (iCloud/Dropbox) — Same SQLite concurrency issue.
5. **Export/Import** — Manual `budi export` / `budi import` JSON flow.

### Server-based

- `budi sync --remote` pushes E2E encrypted data to hosted server
- Server merges, provides web dashboard for combined view
- Freemium: free = local single machine, paid = multi-machine sync + team dashboards

## Original Decision (pre-8.0)

cr-sqlite for serverless sync. Transports: GitHub private repo or shared folder.

## 8.0 Update

The cloud sync worker (R4, #101) partially addresses this:
- Cloud dashboard shows aggregated data across devices within an org
- Identity model: Org -> User -> Device (a user can have multiple devices)
- But cloud only shows daily rollups, not per-message detail

For per-machine local analytics with full detail, cr-sqlite or export/import is still relevant as a post-8.0 feature.

## Related

- `github.com/berlinguyinca/ai-sync` — syncs Claude Code config files via git (complementary, not competing)
- ADR-0083 §3 — Device identity model (one user, multiple devices)
