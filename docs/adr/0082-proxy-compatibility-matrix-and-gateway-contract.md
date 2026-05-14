# ADR-0082: Proxy Compatibility Matrix and Local Gateway Contract

- **Date**: 2026-04-10
- **Status**: Superseded by [ADR-0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (2026-04-17)
- **Issue**: [#82](https://github.com/siropkin/budi/issues/82)
- **Milestone**: 8.0.0
- **Depends on**: [ADR-0081](./0081-product-contract-and-deprecation-policy.md)

> **Superseded by [ADR-0089](./0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md) (2026-04-17).** Budi 8.2 replaces proxy-first ingestion with JSONL tailing as the sole live path. The compatibility matrix, gateway contract, streaming behavior, and `X-Budi-*` attribution header protocol described below are retired in 8.2 R2.1. Agent compatibility in 8.2+ is a function of `Provider::watch_roots()` + `Provider::parse_file`, not of proxy base-URL configuration. The record below is preserved for historical context only.

## Context

Budi 8.0 replaces hook/OTEL/JSONL ingestion with a **local proxy** that sits between the developer's AI coding agent and the provider API. Before building the proxy (R2), every agent's actual ability to route traffic through a local endpoint must be validated, and the proxy's protocol contract must be locked so that R2 implementation issues have a stable target.

The agents evaluated are the five named in issue #82: **Claude Code**, **Cursor**, **Codex CLI**, **Gemini CLI**, and **Copilot CLI**.

### Current State

The budi daemon (`crates/budi-daemon/`) runs an axum HTTP server on `127.0.0.1:7878`. It serves the management API (analytics, sync, admin) and the local dashboard. There is no proxy functionality today. The daemon has no SSE/streaming routes.

## Decision

### 1. Agent Compatibility Matrix

| Agent | Redirect mechanism | Env / config key | Protocol family | Streaming format | Confidence | Notes |
|-------|-------------------|-------------------|-----------------|------------------|------------|-------|
| **Claude Code** | Env var | `ANTHROPIC_BASE_URL` | Anthropic Messages (`POST /v1/messages`) | SSE (`text/event-stream`) | **High** | Also set `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` to prevent startup traffic bypassing the proxy. VS Code extension does not respect the env var — CLI only. |
| **Cursor** | Settings UI | `Override OpenAI Base URL` in Cursor Settings → Models | OpenAI Chat Completions (`POST /v1/chat/completions`) | SSE (`text/event-stream`) | **Medium** | Some built-in Cursor features may still use Cursor-managed routes regardless of override. User must change a GUI setting, not just an env var. |
| **Codex CLI** | Env var or config | `OPENAI_BASE_URL` env (deprecated) or `openai_base_url` in `config.toml` | OpenAI Chat Completions (`POST /v1/chat/completions`) | SSE (`text/event-stream`) | **High** | Config key is preferred since March 2026; env var still works with a deprecation warning. |
| **Gemini CLI** | Env var (SDK-native) | `GOOGLE_GEMINI_BASE_URL` | Gemini (`POST /v1beta/models/{model}:streamGenerateContent`) | SSE (requires `?alt=sse` query param) | **Medium** | URL handling evolved — SDK now natively reads the env var. Different API shape from OpenAI/Anthropic. |
| **Copilot CLI** | Env vars (BYOK) | `COPILOT_PROVIDER_BASE_URL` + `COPILOT_PROVIDER_TYPE` + `COPILOT_MODEL` | OpenAI Chat Completions (when `COPILOT_PROVIDER_TYPE=openai`) | SSE (`text/event-stream`) | **Medium** | Does **not** support standard `OPENAI_BASE_URL`. Requires BYOK env vars. `HTTPS_PROXY` does not reliably intercept model traffic. |
| **Codex Desktop** | Settings file | `openai_base_url` in `~/.codex/config.toml` | OpenAI Chat Completions | SSE | **Medium** | macOS GUI app. Shares config with Codex CLI but cannot be launched via env var wrapper. Sandboxed — may need `network_access = true` in sandbox config. |

#### Confidence Definitions

- **High**: Env var or config key documented by the vendor; tested in community proxy setups; no known regressions.
- **Medium**: Works but has caveats — requires GUI settings, proprietary env vars, undocumented API format, or partial bypass of the redirect in some code paths.

#### Tier 1 vs Tier 2 Agents

Based on confidence and market share, the proxy implementation should prioritize:

- **Tier 1 (R2 must-have)**: Claude Code, Codex CLI. Both use simple env vars, both have high confidence, and both use well-documented API formats (Anthropic Messages, OpenAI Chat Completions).
- **Tier 2 (R2 should-have)**: Cursor, Copilot CLI, Codex Desktop. All route through OpenAI-compatible endpoints, but require GUI settings, proprietary env vars, or manual config file edits. Support is achievable but onboarding friction is higher.
- **Tier 3 (R2 stretch / post-R2)**: Gemini CLI. Different API format (not OpenAI-compatible), partially documented streaming protocol, and the env var situation was in flux. Supporting Gemini requires a separate protocol handler.

### 2. Supported Protocol Surface (v1)

The proxy must understand two API protocol families for v1:

#### OpenAI Chat Completions (covers Cursor, Codex, Copilot)

| Aspect | Contract |
|--------|----------|
| Endpoint | `POST /v1/chat/completions` |
| Content-Type (request) | `application/json` |
| Auth header | `Authorization: Bearer <key>` — passed through to upstream |
| Streaming trigger | `"stream": true` in request body |
| Streaming response | `Content-Type: text/event-stream`; `data: {json}\n\n` lines; terminal `data: [DONE]\n\n` |
| Non-streaming response | `Content-Type: application/json`; single JSON body |
| Models endpoint | `GET /v1/models` — proxy may serve a synthetic response or pass through |
| Metadata to capture | `model` from request body; `usage.prompt_tokens`, `usage.completion_tokens` from response or final stream chunk |

#### Anthropic Messages (covers Claude Code)

| Aspect | Contract |
|--------|----------|
| Endpoint | `POST /v1/messages` |
| Content-Type (request) | `application/json` |
| Auth header | `x-api-key: <key>` + `anthropic-version: 2023-06-01` — passed through to upstream |
| Streaming trigger | `"stream": true` in request body |
| Streaming response | `Content-Type: text/event-stream`; named events (`message_start`, `content_block_delta`, `message_delta`, `message_stop`) |
| Non-streaming response | `Content-Type: application/json`; single JSON body |
| Metadata to capture | `model` from request body; `usage.input_tokens` from `message_start`; `usage.output_tokens` from `message_delta` |

#### Gemini (deferred to post-Tier-1)

| Aspect | Contract |
|--------|----------|
| Endpoint | `POST /v1beta/models/{model}:streamGenerateContent?alt=sse` |
| Auth header | `x-goog-api-key: <key>` or OAuth — passed through |
| Streaming response | SSE with `GenerateContentResponse` JSON chunks |
| Status | **Not implemented in v1.** Requires a third protocol handler. Added when Gemini CLI confidence reaches High or demand justifies it. |

### 3. Proxy Port Strategy

**Decision: dedicated proxy port, separate from the management daemon.**

| Concern | Decision |
|---------|----------|
| Daemon port | Stays on `127.0.0.1:7878` (management API, analytics, dashboard). No change. |
| Proxy default port | `127.0.0.1:9878`. Chosen to mirror the daemon port pattern (`x878`) while avoiding `8080` (common dev-server conflict) and `9090` (Prometheus default). |
| Port conflict handling | On startup, attempt to bind. If `EADDRINUSE`, log a clear error with the conflicting process and exit. Do **not** silently pick another port — that breaks agent env-var configuration. |
| Override | `proxy_port` in `config.toml`; `--proxy-port` CLI flag; `BUDI_PROXY_PORT` env var. Precedence: env > CLI flag > config > default. |
| Bind address | `127.0.0.1` only. The proxy handles local agent traffic; it must never bind to `0.0.0.0`. |
| TLS | **Not in v1.** All agents connect to `http://localhost:…`. TLS termination is unnecessary for loopback traffic and would add certificate management complexity. |

### 4. Upstream Routing

The proxy must determine which upstream provider to forward each request to. The routing is **path-based**:

| Request path pattern | Upstream provider | Default upstream base URL |
|---------------------|-------------------|--------------------------|
| `/v1/messages` | Anthropic | `https://api.anthropic.com` |
| `/v1/chat/completions` | OpenAI | `https://api.openai.com` |
| `/v1/models` | OpenAI | `https://api.openai.com` |
| `/v1beta/models/*/streamGenerateContent*` | Gemini | `https://generativelanguage.googleapis.com` (deferred) |

The agent's API key in the request header determines authentication with the upstream. The proxy does not inject, replace, or store API keys. It is a transparent pass-through for auth.

### 5. Streaming Behavior

| Requirement | Contract |
|-------------|----------|
| Pass-through latency | The proxy must forward each SSE chunk to the client as soon as it is received from upstream. No buffering of the full response. First-byte latency added by the proxy should be < 10ms on localhost. |
| Metadata capture | The proxy reads the stream to extract metadata (model, tokens, cost) but does **not** modify the stream content. Capture happens via a tee/tap on the byte stream, not by deserializing and re-serializing every chunk. |
| Backpressure | If the client stops reading (slow consumer), the proxy applies TCP backpressure to the upstream. No unbounded internal buffering. |
| Connection lifecycle | The proxy holds the upstream connection open for the duration of the client connection. If the client disconnects, the proxy drops the upstream connection. |
| Timeouts | Connect timeout to upstream: 30s. No read timeout on streaming responses (the stream ends when upstream closes or sends a terminal event). |

### 6. Failure Behavior

| Failure mode | Proxy behavior |
|--------------|---------------|
| Upstream returns HTTP error (4xx/5xx) | Pass the error response through to the client unmodified. Log the status code and model for analytics. (e.g. invalid API key returns `401 Unauthorized` from upstream). |
| Upstream is unreachable (DNS, connect timeout) | Return `502 Bad Gateway` with a JSON body: `{"error": {"type": "proxy_error", "message": "..."}}`. Log the failure. |
| Proxy itself crashes or is not running | The agent gets `ECONNREFUSED` on the proxy port. The agent's own error handling applies. The proxy does not implement automatic fallback to direct connections — that would silently bypass observability. |
| Malformed request from agent | Return `400 Bad Request`. Do not forward to upstream. |
| Request body too large | Enforce a 16 MiB limit (matching the current daemon OTEL limit). Return `413 Payload Too Large`. |

**No silent fallback.** If the proxy is configured but not running, the agent should fail visibly. Silent fallback to direct API calls would defeat the purpose of the proxy (observability, cost tracking). The `budi doctor` command will check proxy health.

### 7. Tool and MCP Visibility

The proxy sits on the LLM API path only. This determines what agent activity it can and cannot observe:

| Data | Visible to proxy? | Explanation |
|------|-------------------|-------------|
| LLM tool-call requests (model decides to invoke a tool) | **Yes** | Tool definitions and invocation appear in the request/response body (`tools` array, `tool_use` blocks). |
| Tool results fed back to the model | **Yes** | Follow-up messages with `role: tool` (OpenAI) or `tool_result` blocks (Anthropic) pass through the proxy. |
| MCP server calls | **No** | MCP traffic is direct stdio or local HTTP between the agent and the MCP server. It never touches the LLM provider API. |
| Agent-internal actions (file reads, shell commands, code edits) | **No** | Local execution inside the agent process; not proxied. |

The proxy captures *what the model was asked to do with tools* and *what tool results were sent back to the model*, but not the actual MCP/tool execution traffic. For full MCP observability, a different capture mechanism (agent plugin, hooks, or agent-specific integration) would be needed — that is outside the 8.0 proxy scope and could be explored post-8.0.

### 8. Attribution Capture

Each proxied request produces a **proxy event** record with the following metadata:

| Field | Source | Required |
|-------|--------|----------|
| `timestamp` | Proxy clock | Yes |
| `provider` | Path-based routing decision | Yes |
| `model` | Request body `model` field | Yes |
| `input_tokens` | Response body or stream metadata | Best-effort |
| `output_tokens` | Response body or stream metadata | Best-effort |
| `duration_ms` | Time from first upstream byte to stream end | Yes |
| `status_code` | Upstream HTTP status | Yes |
| `repo` | Git repo root of budi config | Yes |
| `branch` | `git rev-parse --abbrev-ref HEAD` at request time | Best-effort |
| `ticket` | Extracted from branch name (see below) | Best-effort |
| `session_id` | Correlation ID if agent provides one, else generated | Best-effort |
| `is_streaming` | Whether `stream: true` was in the request | Yes |

### 9. Branch-to-Ticket Attribution and Fallback

The proxy extracts ticket identifiers from the current Git branch name to attribute cost to work items.

#### Extraction Rules

Branch names are matched against common patterns:

| Pattern | Example branch | Extracted ticket |
|---------|---------------|-----------------|
| `<PREFIX>-<NUMBER>` | `PROJ-1234-fix-bug` | `PROJ-1234` |
| `<prefix>/<id>-<slug>` | `feature/PROJ-1234-add-auth` | `PROJ-1234` |
| Numeric only | `fix/1234-typo` | `1234` |

The regex: `(?i)(?:^|/)([A-Z]{2,10}-\d+|\d+)(?:-|$)` — case-insensitive, matches the first ticket-like segment after the last `/` or at the start.

#### Fallback for Poor Branch Names

| Condition | Ticket value | Behavior |
|-----------|-------------|----------|
| No branch (detached HEAD) | `Unassigned` | Log a warning once per session. |
| Branch name has no ticket pattern | `Unassigned` | Attribute to `Unassigned`. The dashboard and Rich CLI can filter/group by this value. |
| Branch is `main`, `master`, `develop` | `Unassigned` | These are integration branches, not ticket branches. |
| Git is not available or fails | `Unassigned` | Degrade gracefully; do not fail the proxy request. |

The `Unassigned` value is a literal string stored in the `ticket` field. It is queryable and filterable. Users who do not use ticket-based branch naming still get full cost tracking — just without ticket attribution.

### 10. Config Surface

New fields in `config.toml` (added in the proxy round, not before):

```toml
[proxy]
enabled = true
port = 9878
# Override upstream URLs (optional, for enterprise API gateways)
# anthropic_upstream = "https://api.anthropic.com"
# openai_upstream = "https://api.openai.com"
```

New env vars:

| Var | Purpose |
|-----|---------|
| `BUDI_PROXY_PORT` | Override proxy port |
| `BUDI_PROXY_ENABLED` | `true`/`false` override |

### 11. What This ADR Does NOT Decide

- How proxy events are stored (SQLite schema changes — decided in R2.3).
- Budget engine integration with proxy — decided in R5.
- Cloud sync of proxy events — decided in R4.
- Rich CLI display of proxy data — decided in R3.
- Exact Rust crate structure for the proxy (could be part of `budi-daemon` or a new `budi-proxy` crate) — decided during R2.1 implementation.

## Consequences

### Expected

- R2 implementation issues have a locked protocol target and agent priority list.
- Tier 1 agents (Claude Code, Codex CLI) can be supported with simple env var configuration and two protocol handlers.
- Port strategy avoids common conflicts and is overridable.
- Attribution capture is designed to work even with poor branch hygiene (fallback to `Unassigned`).

### Trade-offs

- **Gemini CLI is deferred.** Users on Gemini CLI will not get proxy-based observability until a third protocol handler is built. They can still use budi for analytics on other agents.
- **Cursor requires config file patching.** Unlike CLI agents, Cursor uses GUI settings backed by a JSON config file. `budi init` patches this file directly when the user selects Cursor. `budi disable cursor` reverts the change.
- **No silent fallback.** If the proxy is down, agents fail. This is intentional — silent fallback undermines the observability guarantee — but it means the proxy must be reliable. Daemon autostart (launchd/systemd) is a hard prerequisite. `budi doctor` and the Cursor extension health check (R3.2) mitigate this.
- **No TLS.** Loopback traffic is unencrypted. This is fine for localhost but means the proxy cannot be used across machines. This matches the "local-first" design of budi 8.0.
- **Token capture is best-effort.** Some providers may change their response format or omit usage fields. The proxy must not fail if token data is missing.

### Downstream Impact

- **R2.1** (proxy mode): Implements the Anthropic and OpenAI protocol handlers per this contract, on port `9878`.
- **R2.2** (streaming): Implements the SSE pass-through per the streaming behavior section.
- **R2.3** (metadata persistence): Uses the attribution schema from section 7.
- **R2.4** (fallback paths): Per ADR-0081, hook/OTEL/JSONL fallbacks are removed, not maintained. R2.4 should be re-scoped to "clean removal of legacy ingestion" rather than "keep fallbacks."
- **R3.1** (CLI wrapper): `budi launch` remains available as an explicit alternative but is no longer the primary onboarding path. `budi init` injects env vars into the shell profile for selected CLI agents and patches config files for IDE agents (Cursor, Codex Desktop). `budi disable <agent>` reverses the changes. `BUDI_BYPASS=1` skips the proxy for a single session.
- **R3.2** (Cursor bootstrap): `budi init` patches Cursor's settings.json directly when the user selects Cursor. No manual GUI configuration needed.

---

*Last verified against code on 2026-05-14.*
