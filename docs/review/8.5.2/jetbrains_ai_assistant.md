# `jetbrains_ai_assistant` provider — 8.5.2 review pass

- **Module**: `crates/budi-core/src/providers/jetbrains_ai_assistant.rs` (524 LOC; prod 345 / tests 179)
- **Tracking**: #799
- **ADRs**: [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)
- **Note**: distinct from `copilot_chat`'s JetBrains host (ADR-0093).
  This is JetBrains' own Anthropic-backed AI Assistant product
  (`com.intellij.ml.llm`), billed through JetBrains AI subscriptions,
  surface tag is shared (`jetbrains`).

## Shape

Tails Anthropic-shaped JSONL transcripts under
`<JetBrains config root>/<Product><Year>/aiAssistant/chats/*.jsonl`.
Per-line walker tracks `current_model` from `message_start` events and
emits one `ParsedMessage` per `message_stop` event.

## Adherence to ADRs

- **ADR-0089 §1**: ✓ — pure file-tail; `watch_roots` returns each
  chat dir discovered via filesystem listing (no closed allowlist).
- **ADR-0089 §4** (attribution from transcript): ⚠ — the
  Anthropic-style event stream doesn't carry `cwd` or `git_branch`
  itself, so every row emits with `cwd: None`, `git_branch: None`,
  `repo_id: None` (jetbrains_ai_assistant.rs:295-303). See "Worth
  fixing" #1 below.

## Observations

### Worth fixing (defer follow-up tickets)

1. **No workspace/repo attribution.** Every emitted row has `cwd`,
   `git_branch`, `repo_id` set to `None`. Unlike Cursor, there's no
   `attach_session_context_to_*` step that patches them after the fact.
   The result on the dashboard: JetBrains AI Assistant cost lands in
   the "(unknown)" repo bucket regardless of which project the user
   was working in. Two possible signals to fish out:
   - Sibling files in the chat dir (some IDE plugins write project
     hints next to the transcript).
   - The chat dir's parent dir path (`IntelliJIdea2025.3` vs
     `WebStorm2026.1`) carries IDE-flavor info but not project info.
   Worth a discovery ticket: capture a real-world session to see what
   else is on disk.
2. **Synthetic fixture only.** `fixtures/synthetic_session_v1.jsonl`
   is hand-crafted; the module doc-comment explicitly says "open
   questions for a real-world capture." Same gap as the codex /
   copilot_cli reviews: capture a real session and pin a fixture +
   `MIN_API_VERSION`. Should pair with a tracking issue under
   ADR-0093-style "JetBrains AI Assistant Data Contract".
3. **No `MIN_API_VERSION`, no unknown-shape warn-once.** If JetBrains
   changes the `usage.*` key names in a plugin update, the parser will
   silently drop rows. Same fix pattern as codex / copilot_cli.
4. **`uuid: id.clone()` and `request_id: Some(id)` are the same
   value** (jetbrains_ai_assistant.rs:292 + :312). Two fields hold
   one piece of data. If this is intentional (e.g. `request_id` is
   meant to be a separate field once the event stream exposes it),
   add a comment. Otherwise drop the duplication.

### Minor

- `deterministic_uuid` (jetbrains_ai_assistant.rs:328-343) is a
  fourth-or-fifth hand-rolled UUID formatter — see #800 sweep proposal.
  Keyed on `(session_id, timestamp_nanos)`, which means two
  `message_stop` events with the exact same nanosecond timestamp would
  collide. Vanishingly unlikely; worth a note.
- Per-line walker (jetbrains_ai_assistant.rs:187-227) duplicates the
  byte-accounting pattern from codex / copilot / cursor. Same shared
  helper proposal.
- `cost_confidence: "estimated"` magic string (jetbrains_ai_assistant.rs:310).
- `role: "assistant"` hardcoded on every row, same as codex /
  copilot_cli — token-rollup events aren't really "assistant turns"
  but the analytics taxonomy treats them as such.
- Two-test discovery coverage is good (presence + absence cases).
- `discover_chat_dirs` uses `std::env::temp_dir()` for tests with
  best-effort cleanup — same papercut as the other providers.

### No issues found

- Platform-specific roots (macOS / Linux / Windows / fallback) are
  correctly cfg-gated.
- Cross-platform `vec![]` returns when target isn't macOS/Linux/Windows
  rather than panicking — good defensive shape.
- `parse_message_stop` correctly inherits `current_model` from the
  paired `message_start`, with a fallback to the literal event value.
- Tests cover: synthetic happy-path (3 turns, with/without cache),
  zero-token skip, model inheritance, fallback-session-id, incremental
  offset (including partial-trailing-line), malformed-line tolerance.
  Good baseline.

## Concrete follow-up

- **Capture a real-world session** under
  `src/providers/jetbrains_ai_assistant/fixtures/` and add
  `MIN_API_VERSION` + unknown-shape warn-once. Track in 8.6.x.
- **Workspace attribution** — discovery ticket for what's on disk
  near the chat transcript. Until then, JetBrains AI rows land in
  the unknown-repo bucket.
- **Drop the `uuid == request_id` duplication** or comment why both
  exist.
- **Shared `deterministic_uuid` / line-walker / `CostConfidence`** —
  bundle into #800.

No 8.5.2-scoped behavior change required. The provider is small and
clean; the gap is in coverage (real fixture, workspace signal) rather
than correctness.
