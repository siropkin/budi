# `copilot_cli` provider — 8.5.2 review pass

- **Module**: `crates/budi-core/src/providers/copilot.rs` (558 LOC, single file)
- **Tracking**: #799
- **ADRs**: [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)
- **Note**: `provider_id = "copilot_cli"`, distinct from the
  VS Code-family `copilot_chat` provider (ADR-0092 §1).

## Shape

Tails the standalone GitHub Copilot CLI's `~/.copilot/session-state/<sid>/
events.jsonl`. Per-line walker matches on `assistant.turn_start` (model
hint) and `assistant.usage` (token rollup). Workspace metadata
(`cwd`, `git_branch`) is pulled out of a sibling `workspace.yaml` via a
hand-rolled mini-parser.

`COPILOT_HOME` env override is supported (well-placed for tests and
operator overrides).

## Adherence to ADRs

- **ADR-0089 §1**: ✓ — `watch_roots` returns `~/.copilot/session-state/`
  (recursive watcher picks up new `<sid>/events.jsonl` automatically).
- **ADR-0089 §4** (attribution from transcript): ✓ — cwd and git_branch
  flow from the on-disk YAML, not from headers.

## Observations

### Worth fixing

1. **`parse_workspace_yaml` always returns `Some(WorkspaceMetadata)`**
   (copilot.rs:164-184) — even when no fields matched. The `Option`
   wrapper is a redundant API; either return `WorkspaceMetadata` (always
   meaningful — default = `cwd: None, git_branch: None`), or actually
   return `None` when nothing was parsed. Small leak, easy fix.
2. **No fixture / `MIN_API_VERSION` / unknown-shape warn-once.** Same
   gap as the Codex review — the `assistant.usage` and
   `assistant.turn_start` shapes are an undocumented GitHub contract.
   `parse_usage_event` (copilot.rs:269) already defensively reads either
   `data.input_tokens` or `data.usage.input_tokens` — that fallback is a
   contract-drift accommodation that *should* be pinned in an ADR. Punt
   to 8.6.x: file follow-up.
3. **`COPILOT_HOME` env override not user-documented.** Only the module
   doc-comment mentions it. If we want users to discover it, surface in
   `SOUL.md` or `--help`. If we don't, the doc-comment is fine.

### Minor

- `deterministic_uuid` (copilot.rs:134) is identical to codex.rs:124 —
  cut and paste. See "Cross-cutting findings" in `claude_code.md` /
  codex review for the shared-helper proposal (#800).
- `parse_copilot_transcript` (copilot.rs:194-267) line-walker byte
  accounting matches codex/jetbrains_ai_assistant/cursor verbatim. Same
  shared-helper opportunity.
- `parse_usage_event` returns a `ParsedMessage` with the same 27-field
  struct literal as everywhere else (copilot.rs:308-342). Half the
  fields are uniform defaults that could land in a
  `ParsedMessage::for_provider("copilot_cli")` constructor.
- `cost_confidence: "estimated"` magic string (copilot.rs:327).
- `surface: TERMINAL` hardcoded (copilot.rs:341) — correct (this *is*
  a CLI), but the constant lives at `crate::surface::TERMINAL`; the
  `to_string()` round-trip is wasteful. Same pattern in codex. Worth a
  `pub const TERMINAL: &str` → `surface: TERMINAL.into()` once and for
  all in a separate sweep.

### No issues found

- Workspace-yaml mini-parser correctly trims both `"..."` and `'...'`
  quote forms. Comment justifies the no-serde_yaml choice.
- Tests cover both bare and `data.usage.*`-nested shapes, plus zero-token
  skip + incremental offset.
- ADR-0089 watch_roots correct (returns empty when dir missing, doesn't
  panic).

## Concrete follow-up

- **`parse_workspace_yaml` return-type cleanup** — single-line fix,
  could land in 8.5.2 with the test update.
- **Fixture + `MIN_API_VERSION` for Copilot CLI** — paired with the
  `data.usage.*` fallback ADR. Track in 8.6.x.
- **Shared helpers** (`deterministic_uuid`, line walker,
  `CostConfidence`) — bundle into #800.

No 8.5.2-scoped behavior change required.
