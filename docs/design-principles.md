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

## 4. Proxy-First Architecture (8.0+)

The proxy is the sole live data source. Historical data (JSONL, Cursor API) is available via `budi import` for backfill only.

- Don't reintroduce hooks, OTEL, or continuous file watching for live data
- Proxy is transparent — no SDK, no per-agent integration code for live tracking
- Adding a new agent = documenting its base URL configuration

### Auto-proxy-install (8.0+)

The proxy is installed automatically for agents the user selects during `budi init`. Users should not need to remember special launch commands — tracking starts the moment they open a terminal or IDE.

- **CLI agents** (Claude Code, Codex, Copilot CLI, Gemini CLI): env vars injected into the user's shell profile (`~/.zshrc`, `~/.bashrc`) inside a `# >>> budi >>> ... # <<< budi <<<` guard block
- **IDE agents** (Cursor, Codex Desktop): config files patched directly (`Override OpenAI Base URL` in Cursor's settings.json, `openai_base_url` in `~/.codex/config.toml`)
- **Opt-out**: `budi disable <agent>` removes the env vars or reverts config changes
- **Bypass**: `BUDI_BYPASS=1 <agent>` skips the proxy for a single session
- **Prerequisite**: daemon autostart (launchd/systemd) is required — if env vars point to `localhost:9878` but the daemon isn't running, API calls fail

## 5. Local-First, Cloud-Optional

Everything works without cloud. Cloud adds team visibility, not core functionality.

- All analytics, budgets, and health vitals are computed locally
- Cloud receives only what's needed for team aggregation
- No cloud-to-daemon command channel (budgets are local)
- Cloud sync is explicit opt-in via `cloud.toml`, never automatic
