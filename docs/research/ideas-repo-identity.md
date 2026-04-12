# Repo Identity Resolution Design

- **Date**: 2026-03-22
- **Status**: Implemented (repo_id column exists, hash-based)
- **See**: `crates/budi-core/src/repo_id.rs`

## Problem

Same git repo in different worktrees or clones counts as separate projects in stats/dashboard. User expects them merged.

## Design (Implemented)

Resolution logic at ingestion time:

1. **Git + remote origin** -> normalize URL to `github.com/user/repo` (strip `.git` suffix, protocol, auth tokens)
2. **Git + no remote** -> git root folder name
3. **No git** -> current folder name

Storage:
- `repo_id` column on `messages` table (hash of normalized path)
- `project_dir` / `cwd` kept for display (actual filesystem path)
- All queries group/aggregate by `repo_id`

## Current State (8.0)

- `repo_id` is a SHA256 hash of the repo root path (see `crates/budi-core/src/repo_id.rs`)
- Proxy attribution resolves repo_id from `X-Budi-Repo` header or git resolution from cwd
- Cloud sync uses `repo_id` hash in daily rollups (ADR-0083) — actual path never leaves machine
- Sessions have `repo_id` for session-level attribution

## Future Enhancement

- Display a human-readable project name derived from remote URL (e.g., `siropkin/budi`) instead of the hash
- Allow user-defined project names via config
- Cross-worktree dedup is handled by the hash approach
