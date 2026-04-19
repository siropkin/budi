# AI Coding Agent Local Files — Inventory

- **Date**: 2026-04-13
- **Purpose**: Catalogue what local session/log files each AI coding agent produces, to inform the proxy-vs-file-watching design decision
- **Last updated**: 2026-04-13

> **Decision note after 2026-04-17 (ADR-0089).** This inventory was gathered for the proxy-vs-file-watching decision and is preserved as historical research. The open design question is closed: transcript tailing is the sole live path in 8.2+ per [ADR-0089](../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md).

---

## Summary Table

| Agent | Path | Format | Tokens | Cost | On by default | Stability |
|-------|------|--------|--------|------|---------------|-----------|
| Claude Code | `~/.claude/projects/<cwd>/<session>.jsonl` | JSONL | input, output, cache_read, cache_write | No (removed ~v1.0.9) | Yes | Fragile |
| Codex CLI | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | JSONL | input, output, cached, reasoning | No | Yes | Fragile |
| Gemini CLI | `~/.gemini/tmp/<hash>/chats/session-*.jsonl` | JSONL | input, output, cached, thoughts, tool | No | Yes | Fragile |
| Cursor | `state.vscdb` (SQLite) + JSONL transcripts | SQLite + JSONL | Per-message in bubbleId; aggregate in composerData | Yes (costInCents) | Yes | Fragile |
| Copilot CLI | `~/.copilot/session-state/<id>/events.jsonl` | JSONL | Aggregate per-model in session.shutdown only | No | Partial | Fragile |
| Copilot VS Code | state.vscdb + logs | SQLite + text | None | None | N/A | No usable data |
| Copilot JetBrains | Nitrite DB + JSONL partitions | Nitrite + JSONL | None | None | N/A | No usable data |
| Cline | `globalStorage/saoudrizwan.claude-dev/tasks/<id>/ui_messages.json` | JSON array | tokensIn, tokensOut, cacheReads, cacheWrites | Yes (cost) | Yes | Semi-stable |
| Roo Code | Same as Cline + `~/.roo/usage-tracking.json` | JSON | inputTokens, outputTokens, cachedTokens | Yes (cost) | Yes | Schema-validated |
| Aider | User-specified via `--analytics-log` | JSONL | prompt_tokens, completion_tokens | Yes (cost, total_cost) | No (opt-in) | Semi-documented |
| Windsurf | `~/.windsurf/transcripts/<id>.jsonl` + `state.vscdb` | JSONL + SQLite | Limited in transcripts | Credits (server-side) | Transcripts yes | Fragile |

---

## Detailed Findings

### Claude Code

**Files:**

| Path | Description |
|------|-------------|
| `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl` | Primary session transcripts (one per session, per project) |
| `~/.claude/projects/<encoded-cwd>/<session-uuid>/subagents/agent-*.jsonl` | Subagent transcripts |
| `~/.claude/history.jsonl` | Global prompt log |
| `~/.claude/sessions/<pid>.json` | Active session registry |
| `~/.claude/stats-cache.json` | Aggregate daily activity |

The `<encoded-cwd>` replaces `/` with `-` (e.g., `-Users-ivan-seredkin--projects-budi`).

**Data in session JSONL:**

Each line has a `type` field. Key types: `user`, `assistant`, `system`.

Assistant messages contain:
- `message.model` — e.g., `"claude-opus-4-6"`
- `message.usage.input_tokens` — non-cached input tokens
- `message.usage.output_tokens` — output tokens
- `message.usage.cache_creation_input_tokens` — tokens written to cache
- `message.usage.cache_read_input_tokens` — tokens read from cache
- `message.usage.cache_creation.ephemeral_5m_input_tokens` — 5-min cache bucket
- `message.usage.cache_creation.ephemeral_1h_input_tokens` — 1-hour cache bucket
- `message.usage.service_tier` — e.g., `"standard"`
- `message.usage.speed` — e.g., `"standard"` (added ~v2.1.81+)
- `message.usage.server_tool_use` — web search/fetch counts
- `message.usage.iterations[]` — per-iteration token breakdowns (added ~v2.1.81+)
- `message.stop_reason` — `"end_turn"` / `"tool_use"`
- `message.id` — Anthropic message ID
- `requestId` — Anthropic request ID
- `timestamp` — ISO 8601
- `sessionId` — UUID
- `version` — Claude Code version
- `cwd` — working directory
- `gitBranch` — current git branch
- `slug` — human-readable session name
- `entrypoint` — how session was started (e.g., `"cli"`)

**Not in session JSONL**: `costUSD` was removed ~v1.0.9 (mid-2025). Cost must be calculated from token counts + model pricing.

**Known issues:**
- Duplicate streaming entries: same `requestId` appears 2-6 times per assistant message. Must deduplicate.
- Output tokens undercount in older versions: intermediate streaming chunks may record `output_tokens: 1`.

**Stability**: Undocumented, no official schema. Fields added/removed between versions. Multiple community tools (ccusage, ccost, claude-code-transcripts) reverse-engineer it. The Claude Code SDK exposes `listSessions()` and `getSessionMessages()` as the closest to an official API.

---

### Codex CLI

**Files:**

| Path | Description |
|------|-------------|
| `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | Session recordings (one per session) |
| `~/.codex/sessions/archived/` | Archived sessions |
| `~/.codex/session_index.jsonl` | Cross-session index |
| `~/.codex/history.jsonl` | Chat history |

Override base dir: `CODEX_HOME` env var.

**Data in rollout JSONL:**

Each line is tagged with `type` and `payload`:
- `session_meta` — id (thread UUID), cwd, cli_version, model_provider, agent_nickname, timestamp
- `turn_context` — model (e.g., "gpt-5"), cwd, approval_policy, turn_id, trace_id, effort
- `event_msg` (subtype `token_count`) — `info.total_token_usage` and `info.last_token_usage` containing: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`, `total_tokens`
- `event_msg` (other subtypes) — user_message, agent_message, reasoning, turn_started, turn_complete, context_compact, etc.
- `response_item` — messages, tool calls, tool outputs
- `compacted` — compressed historical data after context compaction

Every line has a top-level `timestamp` (ISO 8601 with millisecond precision).

**Not in files**: Cost/pricing data. Must be computed externally.

**Stability**: Undocumented and fragile. No formal schema version marker. Defined in Rust structs in `codex-rs/protocol/src/protocol.rs`. The protocol file changes frequently (multiple commits per week as of April 2026). No releases or tags — ships via npm on a rolling basis. The `ccusage` project handles fallback behavior for missing fields.

---

### Gemini CLI

**Files:**

| Path | Description |
|------|-------------|
| `~/.gemini/tmp/<project_hash>/chats/session-*.jsonl` | Session files (migrating from .json to .jsonl) |
| `~/.gemini/tmp/<project_hash>/chats/<parent_id>/` | Subagent sessions (nested) |
| `~/.gemini/settings.json` | User configuration |

The `<project_hash>` is a SHA-256 hex digest of the project root path.

**Data in session JSONL:**

Session metadata record (first line):
- `sessionId`, `projectHash`, `startTime`, `lastUpdated`, `kind` ("main" | "subagent")

Message records (`type: "user"` | `"gemini"`):
- `id` — UUID
- `timestamp` — ISO 8601
- `model` — model identifier (gemini messages only)
- `content` — message parts
- `toolCalls` — tool name, arguments, results, status
- `tokens` — TokensSummary (gemini messages only)

TokensSummary fields: `input`, `output`, `cached`, `thoughts`, `tool`, `total`.

**Not in files**: Cost/pricing data. Must be computed externally.

**Stability**: Undocumented and fragile. Google explicitly closed a request to document the schema as "not planned" ([Issue #10160](https://github.com/google-gemini/gemini-cli/issues/10160)). Format is actively migrating from monolithic JSON to JSONL ([Issue #15292](https://github.com/google-gemini/gemini-cli/issues/15292)). Default retention: 30 days (auto-deleted).

---

### Cursor

**Files:**

| Path | Description |
|------|-------------|
| `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (macOS) | SQLite KV store (4+ GB) |
| `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl` | Agent conversation transcripts |
| `~/.cursor/projects/<slug>/worker.log` | Workspace path mapping |

**Data in state.vscdb:**

Two tables: `ItemTable` and `cursorDiskKV`.

`cursorDiskKV` key patterns:
- `composerData:<uuid>` — full session metadata with `usageData: {"<model>": {"costInCents": N, "amount": N}}`, `contextTokensUsed`, `contextTokenLimit`, `createdAt`, `lastUpdatedAt`, `name`, `status`
- `bubbleId:<composerId>:<bubbleId>` — per-message data with `tokenCount: {"inputTokens": N, "outputTokens": N}`, message type, text, requestId

`ItemTable` key patterns:
- `cursorAuth/accessToken` — JWT for Usage API
- `composer.composerHeaders` — session list with timing, workspace, lines added/removed
- `aiCodeTracking.dailyStats.v1.5.<date>` — daily line counts (no tokens/cost)

**Agent transcripts (JSONL)**: Contain conversation text but almost no token/cost data. The `usage` field is present but almost always empty.

**Remote Usage API** (undocumented, documented in our `docs/research/cursor-usage-api.md`):
- `POST https://cursor.com/api/dashboard/get-filtered-usage-events` — per-request: timestamp, model, inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens, totalCents, chargedCents
- Auth: JWT from state.vscdb sent as `WorkosCursorSessionToken` cookie

**Stability**: Undocumented, fragile. Schema version (`_v`) increments. File can grow to 25+ GB. No local HTTP API.

---

### Copilot CLI

**Files:**

| Path | Description |
|------|-------------|
| `~/.copilot/session-state/<session-id>/events.jsonl` | Session event stream |
| `~/.copilot/session-state/<session-id>/workspace.yaml` | Session metadata (repo, branch, cwd) |
| `~/.copilot/session-store.db` | SQLite cross-session index |
| `~/.copilot/logs/process-*.log` | Process logs |

Override base dir: `COPILOT_HOME` env var.

**Data in events.jsonl:**

Per-request token events (`assistant.usage`) are **ephemeral** — tracked in-memory but not written to JSONL.

The `session.shutdown` event IS written at session end with aggregate metrics:
```json
{
  "type": "session.shutdown",
  "data": {
    "totalPremiumRequests": N,
    "totalApiDurationMs": N,
    "modelMetrics": {
      "<model>": {
        "requests": {"count": N, "cost": N},
        "usage": {
          "inputTokens": N, "outputTokens": N,
          "cacheReadTokens": N, "cacheWriteTokens": N
        }
      }
    }
  }
}
```

**Stability**: Undocumented. Token persistence added ~v0.0.422 and still evolving. Known bugs (unescaped newlines in tool output).

### Copilot VS Code / JetBrains

**No usable token or cost data persisted locally.** VS Code extension stores metadata only (SKU, tracking ID, version). JetBrains uses Nitrite DB (Java NoSQL, MVStore format) with model names but no token counts.

---

### Cline (VS Code Extension)

**Files:**

| Path | Description |
|------|-------------|
| `globalStorage/saoudrizwan.claude-dev/tasks/<task-id>/ui_messages.json` | UI events with cost/token data |
| `globalStorage/saoudrizwan.claude-dev/tasks/<task-id>/api_conversation_history.json` | Raw LLM request/response pairs |

macOS: `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/`

**Data in ui_messages.json:**

The `api_req_started` entries contain embedded JSON in the `text` field:
```json
{
  "type": "say",
  "say": "api_req_started",
  "ts": 1708257600000,
  "text": "{\"cost\":0.12,\"tokensIn\":100,\"tokensOut\":50,\"cacheReads\":20,\"cacheWrites\":5,\"apiProtocol\":\"anthropic\"}"
}
```

Fields: `cost` (USD), `tokensIn`, `tokensOut`, `cacheReads`, `cacheWrites`, `apiProtocol`.

**Stability**: Undocumented but stable in practice. Splitrail, BurnRate, and tokscale all parse it successfully.

---

### Roo Code (VS Code Extension, Cline Fork)

**Files:**

Same as Cline (different extension ID: `rooveterinaryinc.roo-cline`) plus:

| Path | Description |
|------|-------------|
| `~/.roo/usage-tracking.json` | Global usage summary (survives task deletions, restarts) |

**Data in usage-tracking.json:**

Per usage item (Zod-validated `PersistentUsageItem`):
- `taskWorkspace` — workspace path
- `cost` — dollar amount
- `inputTokens`, `outputTokens`, `cachedTokens`
- `mode` — Roo mode (Code, Debug, Ask, etc.)

Uses atomic writes with file locking.

**Stability**: Undocumented but schema-validated. Cleanest summary source of any agent.

---

### Aider (CLI)

**Files:**

| Path | Description |
|------|-------------|
| `<project>/.aider.chat.history.md` | Conversation log (markdown, no structured data) |
| User-specified via `--analytics-log` or `AIDER_ANALYTICS_LOG` | Structured analytics JSONL |

**Analytics JSONL is opt-in only.** Not enabled by default.

When enabled, `message_send` events contain:
- `main_model`, `weak_model`, `editor_model`
- `prompt_tokens`, `completion_tokens`, `total_tokens`
- `cost` (per-request), `total_cost` (cumulative)
- `edit_format` — diff, whole, ask

**Stability**: Semi-documented at `aider.chat/docs/more/analytics.html`. Sample file in repo. Best documented of all agents, but requires user opt-in.

---

### Windsurf (Codeium)

**Files:**

| Path | Description |
|------|-------------|
| `~/.windsurf/transcripts/<trajectory_id>.jsonl` | Cascade transcript files (max 100, auto-pruned) |
| `~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb` | SQLite KV store (same pattern as Cursor/VS Code) |

**Data in transcripts:**

Each line has `type` (user_input, planner_response, code_action, etc.) and `status`. Recent changelog mentions token count fields (`input_tokens`, `output_tokens`, `cache_read_input_tokens`) but these are not reliably present.

Token/cost data is primarily managed server-side via the Windsurf credit system. Local transcripts are conversation-focused with limited cost metadata.

**Stability**: Fragile. Windsurf explicitly warns that "the exact structure of each step may change in future versions."

---

## Analysis

### Agents with usable local token data (no extra setup)

1. **Claude Code** — rich JSONL, tokens always present, no cost
2. **Codex CLI** — rich JSONL, tokens always present, no cost
3. **Gemini CLI** — JSONL, tokens present, no cost, 30-day retention
4. **Cline** — JSON with tokens AND cost
5. **Roo Code** — JSON with tokens AND cost, cleanest format
6. **Cursor** — SQLite with tokens and cost (composerData), but undocumented 4GB+ DB

### Agents with limited or no local data

7. **Copilot CLI** — aggregate only (session.shutdown), no per-request
8. **Windsurf** — limited tokens in transcripts, cost is server-side credits
9. **Aider** — excellent data but requires opt-in flag
10. **Copilot VS Code/JetBrains** — no usable token/cost data

### Common patterns

- **No agent documents their file format.** Every format is reverse-engineered by the community.
- **Cost is rarely stored locally.** Only Cline, Roo Code, and Cursor persist cost. All others require external pricing tables.
- **JSONL is the dominant format** for CLI agents. IDE agents use SQLite or JSON arrays.
- **All formats are fragile.** Fields are added, removed, or renamed between versions without notice.
