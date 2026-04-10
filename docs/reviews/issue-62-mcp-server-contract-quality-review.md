# Issue #62 Review: MCP Server Contract and Tool Quality

## Findings (highest severity first)

### P1 (fixed): analytics `period` input was stringly-typed and silently defaulted on invalid values
- Area: `crates/budi-cli/src/mcp.rs` (`PeriodRequest`, `BranchRequest`, `TagRequest`, `period_to_dates`)
- Risk: MCP clients could pass unsupported values (for example `quarter`) and receive month-scoped data without an explicit error, which weakens tool-contract correctness and can mislead agents.
- Fix in this PR: replaced free-form string period fields with a typed `Period` enum (`today|week|month|all`) and removed silent fallback behavior.

### P1 (fixed): daemon HTTP failures did not provide actionable agent-facing guidance
- Area: `crates/budi-cli/src/mcp.rs` (`daemon_get`, `sync_data`)
- Risk: non-2xx responses were surfaced as generic failures, making it hard for agents to distinguish “sync already running” vs “daemon not ready” vs endpoint mismatch.
- Fix in this PR: centralized daemon HTTP error formatting with targeted guidance for `409 Conflict`, `503 Service Unavailable`, `404 Not Found`, and generic server errors. `sync_data` now uses the same mapping for consistent feedback.

### P2 (fixed): `session_health` returned raw JSON only, inconsistent with other MCP tools
- Area: `crates/budi-cli/src/mcp.rs` (`session_health`)
- Risk: agents had to parse a large JSON blob while other tools returned concise textual summaries; this made multi-tool planning and response quality less consistent.
- Fix in this PR: `session_health` now returns an agent-readable summary (state, messages, cost, tip, vitals, and action hints) while preserving the same underlying endpoint.

## Tests added in this PR

Added unit tests in `crates/budi-cli/src/mcp.rs` for:
- period defaulting and unknown-period rejection
- daemon error-detail extraction and conflict messaging
- session-health text formatting (summary + vitals + actions)

## Documentation drift corrected

- `README.md`: MCP contract notes updated (strict period enum, `session_health` summary format).
- `SOUL.md`: MCP implementation notes updated with contract and error-behavior expectations.
- `CONTRIBUTING.md`: MCP testing section updated with a strict-contract check and daemon-failure expectation.

## Follow-up candidates (not in this PR)

1. Add an end-to-end MCP integration test harness that boots `budi-daemon` and asserts tool outputs over stdio JSON-RPC.
2. Consider adding an optional structured JSON payload field in `session_health` output for clients that prefer machine-readable detail alongside human-readable text.
3. Evaluate whether `sync_data` should surface partial-progress metadata (if daemon exposes it) when returning conflict/busy states.
