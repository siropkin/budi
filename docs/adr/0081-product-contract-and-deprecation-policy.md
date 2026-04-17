# ADR-0081: 8.0 Product Contract and Deprecation Policy

- **Date**: 2026-04-10
- **Status**: Implemented (amended — see banner)
- **Issue**: [#81](https://github.com/siropkin/budi/issues/81)
- **Milestone**: 8.0.0

> **Amended by [ADR-0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (2026-04-17).** The §Provider System note that "JSONL file sync will be removed from the continuous sync loop when proxy mode ships (R2)" is **rescinded**. In Budi 8.2+, JSONL tailing **is** the continuous sync loop. The `Provider` trait is extended with `watch_roots()` to serve it. The rest of this ADR's deprecation policy framework stands as written.

## Context

Budi 8.0 pivots from a file-discovery analytics tool to a **proxy-first, buyer-focused, cloud-enabled** AI coding cost platform. Before code churn begins, every major surface must be explicitly categorized so that downstream issues (R1–R5) have a locked scope to build against.

All pre-8.0 releases were beta. There is no stable user base relying on backward compatibility. Legacy ingestion paths (hooks, OTEL, JSONL file sync) add complexity without serving the proxy-first direction. They should be removed, not preserved as fallbacks. 8.0.0 is the first stable release — the surfaces that ship in it define the product contract going forward.

The current codebase ships:

| Surface | Location | Description |
|---------|----------|-------------|
| budi-core | `crates/budi-core/` | Business logic: providers, pipeline, SQLite analytics, cost math |
| budi-daemon | `crates/budi-daemon/` | axum HTTP server on 127.0.0.1:7878; owns SQLite, serves dashboard and APIs |
| budi-cli | `crates/budi-cli/` | Thin HTTP client: init, stats, sync, doctor, statusline, hook, mcp-serve, etc. |
| Local dashboard | `frontend/dashboard/` | React SPA served by the daemon at `/dashboard` |
| MCP server | `crates/budi-cli/src/mcp.rs` | `budi mcp-serve` over stdio; 15 tools calling daemon HTTP API |
| Cursor extension | `extensions/cursor-budi/` | VS Code extension: status bar, health panel, session deep-links |
| Starship integration | `crates/budi-cli/src/commands/integrations.rs` | `[custom.budi]` block in `~/.config/starship.toml` |
| Hook ingestion | `crates/budi-core/src/hooks.rs` | `budi hook` → daemon `POST /hooks/ingest` → durable queue |
| OTEL ingestion | `crates/budi-core/src/otel.rs` | `POST /v1/logs` → durable queue; `/v1/metrics` is a stub |
| JSONL file sync | `crates/budi-core/src/providers/` | Claude Code transcript discovery + Cursor API/transcript fallback |
| Config system | `crates/budi-core/src/config.rs` | Per-repo `config.toml`, statusline/tags/integrations in `~/.config/budi/` |

## Decision

### Surface Disposition Matrix

Each surface is categorized as one of:

- **Ship** — Actively developed in 8.0; breaking changes allowed per the round that touches it.
- **Freeze** — Kept working and shipped; no new features; bugs fixed only if blocking.
- **Remove** — Deleted from the codebase when the replacement lands in the same round or the next.
- **Move** — Extracted to a separate repository during 8.0 (R4.4).

There is no "deprecate" category. Pre-8.0 was beta — breaking changes are allowed now. What ships in 8.0.0 becomes the stable contract.

| Surface | Disposition | Notes |
|---------|-------------|-------|
| **budi-core** | **Ship** | Evolves across R1–R5: proxy metadata, budget engine, cloud sync support. Legacy provider/hook/OTEL modules pruned as proxy replaces them. |
| **budi-daemon** | **Ship** | Gains proxy mode (R2), cloud sync worker (R4), budget engine (R5). Hook and OTEL endpoints removed when proxy ships. |
| **budi-cli** | **Ship** | Rich CLI becomes primary local UX (R3); `budi hook` and `budi mcp-serve` commands removed during pruning/proxy rounds. |
| **Local dashboard** | **Move** | Moved to the budi-cloud repo in R4.4 and evolved into the cloud dashboard. Removed from the main budi repo once the Rich CLI (R3.3) ships as the primary local UX. The dashboard codebase lives on in budi-cloud, not deleted. |
| **MCP server** | **Remove** | Removed during the pruning round (R1). The `budi mcp-serve` command and `mcp.rs` are deleted. The proxy and Rich CLI replace any useful functionality. |
| **Cursor extension** | **Ship → Move** | Gets minimal bootstrap + status flow in R3.2. Extracted to separate repo in R4.4. |
| **Starship integration** | **Remove** | Removed during the pruning round (R1). The Rich CLI statusline replaces it. |
| **Hook ingestion** | **Remove** | Removed when proxy mode ships (R2). No fallback path. The `budi hook` command, `POST /hooks/ingest` endpoint, hook enricher, durable queue hook path, and `hooks.rs` are all deleted. |
| **OTEL ingestion** | **Remove** | Removed when proxy mode ships (R2). The `POST /v1/logs` endpoint, OTEL parser, merge logic, and `otel.rs` are all deleted. The `/v1/metrics` stub is dropped immediately. |
| **JSONL file sync** | **Remove** | Removed from the continuous sync loop when proxy mode ships (R2). Provider code may be retained behind a one-time `budi import` command for historical backfill (see [Provider System](#provider-system)). |
| **Config system** | **Ship** | Evolves to support proxy settings, cloud credentials, budget thresholds. Legacy integration config (hooks, OTEL, Starship) cleaned up during pruning. |

### Dashboard Migration Policy

The local dashboard is moved to budi-cloud, not deleted:

1. R3.3 ships the Rich CLI as the primary local UX (stats, health, session detail).
2. R4.4 extracts the dashboard source (`frontend/dashboard/`) into the budi-cloud repo, where it evolves into the cloud dashboard alpha (R4.3).
3. After extraction, `frontend/dashboard/`, its build pipeline (`scripts/build-dashboard.sh`), and the daemon's static asset serving are removed from the main budi repo.

### Issue #30: Codex Support

**Disposition: Re-scope, not out of scope.**

Issue #30 originally requested a first-class Codex provider with discovery, parsing, and sync logic — the same per-agent pattern used for Claude Code and Cursor today. That ingestion pattern is being removed in 8.0, but the issue is broader than ingestion: it's about how budi onboards and supports Codex users end-to-end.

The proxy solves data ingestion generically — no dedicated provider needed. But onboarding (how a Codex user installs budi, configures the proxy, and gets value on first run) is a real concern that the proxy alone doesn't address.

**What happens to #30:**

- It stays open in the backlog, not in the 8.0.0 milestone.
- It should be re-scoped from "add a Codex provider" to "support Codex users," covering:
  - **Onboarding**: `budi init` or equivalent setup that configures the proxy for Codex traffic.
  - **Classification**: Codex-specific metadata handling (model names, pricing, display name) at the proxy layer.
  - **Documentation**: Getting-started guide for Codex users.
- The re-scoped issue depends on the proxy (R2) and onboarding (R3) rounds landing first. It becomes actionable post-8.0 or late in the 8.0 cycle if capacity allows.

### Provider System

The existing provider trait (`Provider` in `crates/budi-core/src/provider.rs`) and its two implementations (Claude Code, Cursor) are **removed from the ongoing sync pipeline** when the proxy ships in R2. However, they may be retained as a **one-time historical import** mechanism.

- **R0–R1**: Providers still exist as the primary ingestion path (the proxy hasn't shipped yet).
- **R2**: Proxy ships and becomes the primary ingestion path. Providers are removed from the continuous sync loop.
- **Historical import**: Providers may be kept (or re-scoped) behind a `budi import` command for one-time backfill of pre-proxy data (Claude Code transcripts on disk, Cursor usage history). This lets new users load their existing data when they first set up budi. The decision on whether to keep or rebuild this is made during R2 implementation based on actual complexity.
- **New agents**: Supported via proxy traffic classification only. No new `Provider` implementations for ongoing sync.

### Downstream Impact on R2.4

Issue #92 ([R2.4] "Keep hooks, OTEL, and historical importers as transition fallback paths") is **superseded** by this ADR. That issue should be re-scoped or closed:

- There are no fallback paths to maintain.
- If R2.4 has useful sub-tasks (e.g., ensuring clean removal doesn't break the DB schema), those should be captured as scoped sub-issues under R2.

## Consequences

### Expected

- Clear scope lock for R1–R5 implementation.
- Simpler codebase — one ingestion path (proxy), one local UX (Rich CLI), one web UX (cloud dashboard).
- Less code to maintain, test, and reason about.
- Faster iteration — no energy spent on backward compatibility with paths that are being replaced.

### Trade-offs

- Users on current (pre-8.0 beta) versions lose hook/OTEL/JSONL ingestion with no migration path. Acceptable since all pre-8.0 releases were beta.
- The dashboard leaves the main repo (R3) before the cloud dashboard ships (R4). The Rich CLI covers the local gap; the dashboard codebase continues in budi-cloud.
- Providers may be retained for historical import (`budi import`), so pre-proxy data is not lost. The scope of this is decided during R2.

### What This ADR Does NOT Decide

- Proxy protocol details → ADR-0082.
- Cloud ingest, identity, and privacy → ADR-0083.
- Specific config keys or CLI flags for proxy/budget → decided in their respective round issues.
