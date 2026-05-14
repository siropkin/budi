# `claude_code` provider — 8.5.2 review pass

- **Module**: `crates/budi-core/src/providers/claude_code.rs` (140 LOC, no submodules)
- **Tracking**: #799
- **ADRs**: [0089](../../adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md)

## Shape

The smallest provider in the tree. The `Provider` trait impl is a thin
delegation: `parse_file` calls `jsonl::parse_transcript`, `discover_files`
walks `~/.claude/projects/**/*.jsonl`, `watch_roots` returns the same
projects directory.

This is the reference shape ADR-0089 anticipates for any agent whose
transcript format the central `jsonl` module already understands.

## Adherence to ADRs

- **ADR-0089 §1**: ✓ — `discover_files`, `parse_file`, `watch_roots` all
  implemented. No `sync_direct` override (correct — Claude Code has no
  Usage API equivalent).
- **ADR-0089 §6** (daemon outage safety): ✓ — pure file-tail path.

## Observations

### Minor

- `discover_jsonl_files` is `pub(crate)` but the only caller is this
  module's own `discover_files` (`rg -n 'discover_jsonl_files'` confirms a
  single external-to-this-fn call). Could be private (`fn` without the
  `pub(crate)` modifier). Net cleanup: one keyword.
- `collect_jsonl_recursive` caps recursion at `depth > 4` with no comment
  explaining why `4` (codex caps at `5`, copilot_chat at `8`). Either
  add a one-line rationale or factor a shared cap constant.
- The two unit tests build directories under `std::env::temp_dir()` with
  best-effort cleanup. Standard Rust convention prefers `tempfile::TempDir`
  so a panic mid-test still cleans up. This is a small papercut, not a
  bug — the same pattern is used across the other providers, so a
  separate "providers: tempdir hygiene" pass would address them all at
  once rather than one-off here.

### No issues found

- No tracing-noise concerns (the file emits no `tracing::*` calls — work
  delegated to `jsonl`).
- No magic numbers in the parser path itself (parsing lives in `jsonl`).
- No public API leakage — `ClaudeCodeProvider` is the only `pub` symbol.
- Module doc-comment is accurate and current.

## Concrete follow-up

- **Drop `pub(crate)` on `discover_jsonl_files`** — net one-line cleanup,
  belongs in #800 (code organization) or as a tiny standalone PR.
- **Justify or extract the depth cap** — same pattern recurs in codex
  and copilot_chat; track in #800.
- **Move tempdir-based tests to `tempfile::TempDir`** — provider-wide;
  belongs in #805 (test organization).

No 8.5.2-scoped fixes required. The provider is healthy and the right
shape for the reference pattern.
