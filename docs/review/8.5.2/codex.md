# `codex` provider — 8.5.2 review pass

- **Module**: `crates/budi-core/src/providers/codex.rs` (528 LOC, single file)
- **Tracking**: #799
- **ADRs**: [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)

## Shape

Tails OpenAI Codex Desktop + Codex CLI transcripts at
`~/.codex/sessions/` and `~/.codex/archived_sessions/`. Per-line walker
keys off `session_meta` (cwd / git_branch / session_id), `turn_context`
(model), and `event_msg.payload.type == "token_count"` (usage rollup).

Self-contained — no shared transcript-walker with the other providers.

## Adherence to ADRs

- **ADR-0089 §1**: ✓ — both root directories declared as watch roots;
  parse path is file-only.
- **ADR-0089 §4** (attribution from transcript): ✓ — cwd and git_branch
  pulled out of `session_meta` rather than headers.
- No provider-specific ADR. The `session_meta` / `turn_context` /
  `event_msg.token_count` envelope **is** an undocumented contract owned
  by OpenAI Codex; no pin against drift today. See "Contract-drift
  discipline" below.

## Observations

### Worth fixing (defer follow-up tickets)

1. **No fixture and no `MIN_API_VERSION`.** Unlike copilot_chat
   (ADR-0092 §2.6), Codex has no monotonic version constant that surfaces
   in `budi doctor` when the parser shape needs review. A silent change
   to Codex's `last_token_usage` shape would just produce zero rows. Two
   options: (a) commit a real-world capture under
   `src/providers/codex/fixtures/` plus a tiny `MIN_API_VERSION`, or
   (b) add a `codex_unknown_record_shape` warn-once analogous to
   `log_unknown_shape_once` in copilot_chat.rs:2108. Either is cheap.
   Punt to 8.6.x: file follow-up.
2. **Surface attribution is hardcoded to `TERMINAL`** (codex.rs:314).
   The module doc-comment says "covers both Codex Desktop and Codex
   CLI" — Codex Desktop is an Electron app, not a terminal session.
   The current attribution is wrong for the Desktop arm. Two fixes:
   tighten the doc-comment (CLI-only) or distinguish at parse time
   (e.g. by directory name — `~/.codex/sessions/` is CLI, the Desktop
   arm writes elsewhere). Worth a separate ticket because it changes a
   surface tag.
3. **`role: "assistant"` hardcoded on every emitted row** (codex.rs:286).
   Token-count events are *summary* rows, not assistant turns. The same
   pattern lands in copilot_cli and jetbrains_ai_assistant — it's
   consistent across the lightweight providers — but it conflates "this
   turn finished with these tokens" with "the assistant said something".
   Worth thinking about a `summary` role at the provider trait level
   in 8.6.x. Out of scope for 8.5.2.

### Minor

- `deterministic_uuid` (codex.rs:124-141) hand-rolls a UUID-shape string
  out of SHA-256 bytes. This duplicates verbatim into copilot.rs:134-149,
  with a longer-prefixed variant in copilot_chat.rs:1329 / .rs:2078,
  another shape in jetbrains.rs:986 / .rs:1006, and a more
  RFC-4122-compliant version in cursor.rs:952. **Five hand-rolled
  UUID-shape formatters in the providers/ tree.** Worth a shared
  `crate::uuid::deterministic_from(domain, &[bytes])` helper. Tracking
  in #800.
- `collect_jsonl_recursive` (codex.rs:106) caps depth at `> 5`; claude_code
  uses `4`, copilot_chat uses `8`. No rationale per cap. See claude_code
  review.
- The per-line walker pattern (codex.rs:170-218 — `pos`/`offset` byte
  accounting + `for line in remaining.lines()`) is duplicated almost
  verbatim into copilot.rs:209-264, jetbrains_ai_assistant.rs:187-227,
  and cursor.rs:2257-2269. Single shared `for_each_complete_line(content,
  start_offset, |line, offset| { ... })` would deduplicate ~40 LOC ×
  four sites. Tracking in #800.
- `cost_confidence: "estimated"` is a magic string (codex.rs:300). Same
  string ("estimated" / "exact" / "n/a" / `""` empty) is scattered across
  every provider. A `CostConfidence` enum with a `Display` impl belongs
  on `ParsedMessage` itself; tracking in #800.
- `ParsedMessage { … 27 fields … }` struct-literal (codex.rs:281-315) is
  another sweep candidate — see the copilot review.

### No issues found

- ADR-0089 watch_roots semantics correct (both `sessions/` and
  `archived_sessions/` returned when present, missing roots filtered).
- Test coverage is broad: token-count happy path, null-info skip,
  zero-tokens skip, incremental offset, watch_roots variants. Good baseline.
- No `tracing::warn!` spam.

## Concrete follow-up

- **Fixture + `MIN_API_VERSION` for Codex** — paired with the
  unknown-shape warn-once. Track via 8.5.2 successor (8.6.x).
- **Decide Desktop vs CLI surface** — small ticket, paired ADR
  amendment-free since this provider isn't pinned to an ADR yet.
- **De-dup `deterministic_uuid` / `for_each_complete_line` /
  `CostConfidence` magic strings** — bundle into #800.

No 8.5.2-scoped behavior change required. The provider works correctly
on today's Codex format; the gaps are forward-looking guardrails.
