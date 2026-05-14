# 8.5.2 provider review pass

Tracking: [#799](https://github.com/siropkin/budi/issues/799) (part of [#798](https://github.com/siropkin/budi/issues/798))

Per-provider review notes for the 8.5.2 polish release:

- [claude_code](./claude_code.md) — 140 LOC; reference shape.
- [codex](./codex.md) — 528 LOC; contract-drift guardrails missing.
- [copilot](./copilot.md) — 558 LOC; `copilot_cli` provider id; redundant `Option` on yaml parser.
- [copilot_chat](./copilot_chat.md) — 4,335 LOC (incl. JetBrains submodule); best ADR discipline.
- [cursor](./cursor.md) — 3,128 LOC; three ingestion paths; best operator-diagnostics surface.
- [jetbrains_ai_assistant](./jetbrains_ai_assistant.md) — 524 LOC; no workspace attribution.

## Top 3 takeaways across all providers

1. **Five hand-rolled `deterministic_uuid` formatters.**
   Every provider has its own SHA-256-to-UUID-shape helper with subtly
   different namespacing and only cursor sets the RFC 4122 v4/variant
   bits. Cost: ~80 LOC duplication, inconsistent UUID shape across the
   tree. Fix: a shared `crate::util::uuid::deterministic_from(domain,
   &[bytes])` helper that always emits RFC 4122 v4. Tracking: #800.
2. **Contract-drift discipline is uneven.** `copilot_chat` is the
   model — pinned to ADR-0092, `MIN_API_VERSION` bumps in lockstep,
   `log_unknown_shape_once` with a `(path, shape_signature)` dedup,
   real-world fixtures committed per version. The other four providers
   (`codex`, `copilot_cli`, `cursor`'s bubble shape, `jetbrains_ai_assistant`)
   each tail an undocumented upstream **with no ADR pin, no version
   constant, and no unknown-shape warn**. A silent upstream rename will
   emit zero rows and no signal until users complain. Fix: the
   copilot_chat pattern is reusable; apply it to each remaining
   provider, ideally one ADR per surface (ADR-0090 is precedent for
   Cursor; codex / copilot_cli / jetbrains_ai_assistant want siblings).
   Track as 8.6.x prep, not 8.5.2 scope.
3. **`ParsedMessage` struct-literal duplication is the biggest
   cleanup target.** `ParsedMessage { … 27 fields … }` literals appear
   13 times across the providers/, with most fields holding the same
   uniform defaults (`tool_*: Vec::new()`, `parent_uuid: None`,
   `pricing_source: None`, `cost_confidence: "estimated"`). Combined
   with magic-string `cost_confidence` values (`"estimated"`, `"exact"`,
   `"n/a"`, `""`), this is ~600 LOC of duplication. Fix:
   `ParsedMessage::for_provider(id)` constructor that pre-fills defaults
   + a `CostConfidence` enum on the field. Tracking: #800.

## Cross-cutting follow-up bundle (target: #800 "split mega-modules")

- Shared `deterministic_uuid` (#800 / new ticket).
- Shared `for_each_complete_line` line-walker (#800).
- `CostConfidence` enum on `ParsedMessage` (#800).
- `ParsedMessage::for_provider(id)` constructor (#800).
- Shared `percent_decode` helper (copilot_chat + jetbrains.rs both
  carry one).
- Single `read_git_head_branch` in `crate::repo_id` (cursor.rs and
  copilot_chat/jetbrains.rs both have one).

## 8.5.2-scoped fixes

Nothing in this review **requires** a behavior change inside the 8.5.2
window. Each provider is correct on today's upstream shapes. The
follow-ups above are deliberately punted to 8.6.x or to the other
8.5.2 tickets (#800 code-organization, #804 coverage, #805 test
organization). One small candidate — `copilot.rs::parse_workspace_yaml`
returning `Some(default)` when nothing parses — is a near-trivial fix
that could ride into 8.5.2 with the rest of #799's followups; left as
optional.

## Scope explicitly out

- Refactors > 200 LOC per provider — each becomes its own ticket.
- Behavior changes affecting the data contract — each needs an ADR
  amendment.
- The `retire-with: #789` byte-walkers in `copilot_chat/jetbrains.rs` —
  disposition already decided in ADR-0093 §"Amendment 2026-05-14"
  (#807), kept in tree for 8.5.x.
