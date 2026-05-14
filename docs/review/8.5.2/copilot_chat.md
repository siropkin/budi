# `copilot_chat` provider — 8.5.2 review pass

- **Modules**:
  - `crates/budi-core/src/providers/copilot_chat.rs` (4,335 LOC; prod 2,145 / tests 2,190)
  - `crates/budi-core/src/providers/copilot_chat/jetbrains.rs` (2,042 LOC; prod ~1,103 / tests ~939)
- **Tracking**: #799
- **ADRs**: [0092](../../adr/0092-copilot-chat-data-contract.md), [0093](../../adr/0093-copilot-chat-jetbrains-storage-shape.md), [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)

## Shape

By far the largest provider in the tree and also the one with the
highest engineering discipline. Three responsibilities sit under one
provider id:

1. **VS Code-family local tail** (copilot_chat.rs): five
   `workspaceStorage` / `globalStorage` subpath shapes per ADR-0092 §2.2,
   a JSON Pointer mutation-log reducer per §2.3 v4, four full-pair
   token shapes plus one output-only fallback per §2.3.
2. **GitHub Billing API reconciliation** (delegated to
   `crate::sync::copilot_chat_billing` from `sync_direct`).
3. **JetBrains host ingest** (copilot_chat/jetbrains.rs): byte-scans
   over Xodus log + Nitrite stores per ADR-0093.

## Adherence to ADRs

- **ADR-0092 §1 (provider id)**: ✓ — `pub const PROVIDER_ID = "copilot_chat"`.
- **ADR-0092 §2.1, §2.2 (path roots)**: ✓ — all five VS Code variants,
  remote/dev-container roots, dual-publisher casing (`GitHub` /
  `github`) all enumerated. `is_session_storage_dir_name` enforces the
  §2.2 directory-name allowlist (#684).
- **ADR-0092 §2.3 (token shapes)**: ✓ — four full-pair shapes
  (`extract_tokens_vscode_delta`, `_copilot_cli`, `_legacy`,
  `_feb_2026`) plus the v3 output-only fallback (`_completion_only`).
  Reducer logic (`apply_mutation`, `set_at_path`, `append_at_path`)
  matches the v4 spec.
- **ADR-0092 §2.4 (model id resolution)**: ✓ — three-step fallback in
  `extract_model_id` (resolvedModel → modelId → `agent.id` table).
  §2.4.1 `agent.id` table lives in `resolve_auto_model_id`
  (copilot_chat.rs:1816-1831); five entries, matches the ADR table.
- **ADR-0092 §2.6 (parser tolerance + `MIN_API_VERSION`)**: ✓ —
  `MIN_API_VERSION = 5` (copilot_chat.rs:93), bumped in lockstep with
  each amendment. `log_unknown_shape_once` (copilot_chat.rs:2108)
  warns once per `(file_path, shape_signature)` per daemon run.
- **ADR-0093 §2-4 (JetBrains discovery + dual-store probe)**: ✓ — both
  Xodus `.xd` and Nitrite `.db` accepted; #757 amendment honored
  (jetbrains.rs:592-609). Phase 1 (`extract_xodus_project_name`,
  jetbrains.rs:464) and Phase 2 (`extract_nitrite_workspace_paths`,
  jetbrains.rs:752) byte-walkers both carry the `retire-with: #789`
  annotation contract per ADR-0093 §"Amendment 2026-05-14".
- **ADR-0089 §1**: ✓ — file watcher only; `sync_direct` is a side
  effect (billing pull + JetBrains binary-store sweep), returns `None`
  so the dispatcher still runs the file path.

## Observations

### Where the discipline shines

The pattern other providers should copy:

- **Tight ADR/code lockstep**: every "amendment" section in ADR-0092
  has a matching `// vN (...)` block in the `MIN_API_VERSION` doc-comment
  (copilot_chat.rs:36-92). When a fifth shape lands, the contract and
  the code never disagree.
- **Warn-once with a structured signature** keeps daemon-log noise
  bounded without losing the signal an operator needs.
- **Fixture-pinned envelope shapes**: `vscode_chat_0_47_0.jsonl`,
  `vscode_chat_0_47_0.expected.json`, `vscode_chat_0_47_0_v5.jsonl`,
  `jetbrains_copilot_1_5_53_243_empty_session/`, etc., all sanitized but
  shape-preserving. The other providers have no equivalent.

### Worth fixing (defer follow-up tickets)

1. **`build_message` + `build_messages_for_request` duplicate ~150 LOC
   of struct-literal construction.** Both build one optional user row
   and one assistant row from a record, both produce two near-identical
   `ParsedMessage { … }` literals (copilot_chat.rs:1235-1283 vs
   1455-1503; 1290-1324 vs 1511-1545). The only differences:
   - Emit-key shape (`requestId`-based vs `idx`-based)
   - Tool-data presence
   - Timestamp-extractor choice
   One generic builder `build_messages(path, record, _, role_picker,
   uuid_seed, &enrichment)` would collapse both. Tracking in #800.
2. **`apply_mutation` / `set_at_path` / `append_at_path` is a from-scratch
   JSON Pointer reducer in 100 LOC** (copilot_chat.rs:1047-1193). Works
   correctly and is well-tested, but if any future provider needs the
   same pattern, it belongs as a `crate::json_pointer_reducer` helper.
   Not a 8.5.2 fix; tracking note for whenever a second use-case lands.
3. **Two private `percent_decode` implementations in the same module
   tree** (copilot_chat.rs:602 + jetbrains.rs:812). Both target the
   same RFC 3986 surface, both for `file://` URIs. One shared private
   helper in a `super` module is the obvious cleanup. Tracking in #800.
4. **The whole file is read from disk on every `parse_file` tick**
   (copilot_chat.rs:814 — `std::fs::read_to_string(path)`). The
   in-line comment justifies this against the v4 reducer requirement.
   The comment also flags the missing "per-session last-processed line
   index" cache; for long sessions this is a real CPU cost. Worth
   measuring (via `cargo flamegraph` against a few-MB session file)
   before deciding whether to land the cache. Tracking note.
5. **Tests live inline alongside ~2,200 LOC of production code.** Both
   submodules cross the threshold #805 sets (split-into-sibling-test-modules).

### Minor

- `deterministic_uuid` and `deterministic_uuid_for_key` are two near-
  identical SHA-256 → UUID-shape formatters (copilot_chat.rs:1329 vs
  2078). With a shared helper (#800) both collapse.
- `extract_timestamp` (copilot_chat.rs:2044) walks four JSON Pointer
  candidates. The four are spelled out in a local `candidates` array —
  could promote to a `const` so a fifth shape lands as a one-line edit.
- `editor_context_cwd_hint_from_state` (copilot_chat.rs:710) does
  ad-hoc string parsing of an editor-context English sentence. Comment
  explains why; honest. Worth a tracking note if Copilot Chat ever
  changes the wording.
- `cost_confidence: "estimated"` / `"n/a"` magic strings (4 sites in this
  file). Same enum proposal as the other providers.

### JetBrains-specific (jetbrains.rs)

1. **`resolve_project_workspace` probes hardcoded home-dir paths**
   (jetbrains.rs:533-549): `~/_projects/<name>`, `~/projects/<name>`,
   `~/<name>`. The `~/_projects/` prefix is the maintainer's personal
   convention (and yes — this repo lives in
   `/Users/ivan.seredkin/_projects/budi/`). On a fresh developer's
   machine these probes will miss every time. Worth either (a) widening
   to a configurable list, or (b) deleting the heuristic when Phase 3
   (#789) lands. Since the whole block is marked
   `retire-with: #789` per ADR-0093, the latter is the path.
2. **Three `#[cfg(test)]` blocks at lines 1103, 1109, 1117** —
   `empty_fixture_dir` helper, the inner test module declaration, and
   the real `mod tests`. Reads cleanly but the layering is unusual;
   worth a quick comment or a merge.
3. **`byte_find` / `byte_contains`** (jetbrains.rs:572, 622) reimplement
   `slice::windows(n).position(...)` / `.any(...)`. Std is fast enough
   for the 10-30 KB inputs documented in the comment. Cleanup, not a
   bug.
4. **`read_git_head_branch` is a parallel to
   `cursor::resolve_git_branch_from_head`** (cursor.rs:1880). Two
   different files, same function. Shared `crate::repo_id::head_branch()`
   would land both. Tracking in #800.

## Concrete follow-up

- **Test-organization split** — paired with #805, this is a single PR
  per submodule that moves the inline tests into
  `copilot_chat/tests.rs` and `copilot_chat/jetbrains/tests.rs`. Net
  ~3,100 LOC of test code out of two production files.
- **Build-message dedup** — needs a careful pass because the two
  builders differ in tool-data routing; not a 8.5.2 fix.
- **JetBrains hard-coded `~/_projects/` workspace probe** — retire
  alongside the `retire-with: #789` block when Phase 3 lands; **do not
  delete in 8.5.2** per ADR-0093 §"Amendment 2026-05-14" #807 disposition.
- **Profile `read_to_string` cost on long sessions** — measurement
  ticket, not a fix.

No 8.5.2-scoped behavior change required. The provider is the
healthiest in the tree on contract-drift discipline; the cleanups are
all forward-looking.
