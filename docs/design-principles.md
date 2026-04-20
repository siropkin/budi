# Budi Design Principles

Core design philosophy that guides all feature decisions. These were established through experience — features that violated them were built, tested, and removed.

## 1. Lightweight, Fast, Stable, Accurate

Budi must be lightweight, simple, fast, easy to use, and focused on core features. Developers should love using it.

Before adding any feature, ask: **does this help answer "where did my tokens/money go?"**

- No admin tooling (users can manage files themselves)
- No telemetry/debugging infrastructure in shipped code
- No auto-detection magic (document manual integration instead)
- Count things simply (no heuristic skip logic)
- Every feature must earn its place in lines of code

## 2. No Heavy Subprocess Spawning

Don't build features that spawn many subprocesses per sync cycle.

**Origin**: Git enrichment (git log per session, author resolution, batch processing) was built and then removed because it turned a 2-minute sync into 10+ minutes. The ROI wasn't there.

**Rule**: Before adding any feature that spawns external processes (git, curl, etc.) per session or per message — can this be done with pure string parsing of data we already have? If subprocess spawning is needed, it should be opt-in or on-demand, not part of every sync cycle.

## 3. Privacy First

Prompts, code, and model responses never leave the local machine. This is structural, not configurable.

- Cloud sync uses only pre-aggregated daily rollups (ADR-0083)
- No "full upload" mode. No toggle. No exception.
- Never-upload fields are enforced by the sync worker reading only from rollup tables

## 4. JSONL Tailing as Sole Live Path (8.2+)

> **Changed by [ADR-0089](./adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (2026-04-17).** The "Proxy-First Architecture (8.0+)" principle that previously held this slot is superseded. Budi 8.1.x still ships the proxy during the transition window; it is removed in Budi 8.2.0. The principle as it applies going forward is below.

Budi's live data source is a filesystem tailer over the transcript files agents already write locally. There is one live ingestion path, not two.

- Don't reintroduce the proxy, hooks, or OTEL for live data — the tailer is the live data mechanism
- Don't mutate shell profiles, Cursor `settings.json`, `~/.codex/config.toml`, or any user config during install; `budi init` creates the data dir, registers the daemon, and exits
- Daemon outage must not break the user's agent — the agent keeps working; Budi catches up on next tail
- Adding a new agent = one `Provider` impl (`discover_files` + `parse_file` + `watch_roots`) — no proxy matrix, no base-URL injection, no agent wrapper
- Latency budget: single-digit seconds end-to-end is acceptable for every downstream surface (stats, sessions, statusline, doctor, cloud sync)
- Privacy boundary is unchanged ([ADR-0083](./adr/0083-cloud-ingest-identity-and-privacy-contract.md)): the tailer reads the same files `budi db import` already reads; nothing leaves the machine

## 5. Local-First, Cloud-Optional

Everything works without cloud. Cloud adds team visibility, not core functionality.

- All analytics, budgets, and health vitals are computed locally
- Cloud receives only what's needed for team aggregation
- No cloud-to-daemon command channel (budgets are local)
- Cloud sync is explicit opt-in via `cloud.toml`, never automatic
