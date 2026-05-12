# JetBrains Copilot Nitrite — Phase 2 working-set fixture

Captured from the 8.4.8 smoke-test machine on 2026-05-12 against
`~/.config/github-copilot/iu/chat-agent-sessions/3752q4yKfWI3idOcTjex7rvPqxh/copilot-agent-sessions-nitrite.db`,
size-preserving redacted before commit (user path component rewritten
to `redacted-user`, UUIDs replaced with all-zero UUIDs, username
`siropkin` → `redacted`).

## What the parser must see

- One `NtAgentTurn` document carrying a `stringContent` model-state
  blob, inside which `currentFileUri` points at
  `file:///Users/redacted-user/_projects/Terraform/readme.md`.
- Multiple repeats of the same URI (the per-turn model-state snapshot
  duplicates across MVStore catalog + leaf pages).
- The Phase 2 byte-walker drops the duplicates, returns the URI once,
  derives `/Users/redacted-user/_projects/Terraform` as the
  longest-common-prefix directory after the filename pop, and walks
  upward looking for `.git`.

## What the parser cannot reach (deferred)

- `NtAgentWorkingSetItem` documents in this same file persist only
  `created_at` / `uuid` / `last_modified_at` plus an opaque `_revision`
  cursor. Their actual file-reference payload lives in a different
  MVStore segment that requires a real Nitrite + Java-serialization
  decoder.
- 95 of 98 real Nitrite DBs on the smoke-test machine carry no
  `file://` token anywhere — those sessions cannot be resolved by
  Phase 2 and fall through to the Phase 1 `projectName` heuristic
  (where present).

See ADR-0093 §4 for the full data-shape inventory.
