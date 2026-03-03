# budi Architecture

## Overview

`budi` uses Claude Code hooks to inject deterministic repository context at prompt submit time.

### Data flow

1. Claude Code fires `UserPromptSubmit`.
2. Hook command `budi hook user-prompt-submit` parses stdin payload.
3. CLI calls local daemon `/query`.
4. Daemon performs hybrid retrieval on local index:
   - lexical search via Tantivy (BM25)
   - vector ANN via HNSW (`hnsw_rs`)
   - symbol/path and resolver-backed call-graph channels
   - intent-policy re-ranking with pruned core rules (dirty files, scope/path, docs/symbol intent)
5. Daemon returns context pack.
6. Hook emits JSON with `hookSpecificOutput.additionalContext`.

## Components

- `budi-cli`: init, index, status, doctor, preview, and hook entrypoints.
- `budi-daemon`: local HTTP daemon serving query/index/status/update.
- `budi-core`: shared logic:
  - file discovery with `.gitignore` + local budi ignore file (repo-scoped in user home)
  - chunking
  - embedding engine (fastembed with deterministic fallback)
  - persistent state
  - hybrid retrieval and context packing

## Index state

- `~/.local/share/budi/repos/<repo-id>/index/index.sqlite`: transactional catalog for files + chunks + embeddings
- `~/.local/share/budi/repos/<repo-id>/index/tantivy/`: lexical index files
- `~/.local/share/budi/fastembed-cache/`: embedding model cache (kept outside repos)
- `~/.local/share/budi/embedding-cache.json`: global content-hash embedding reuse cache
- in-memory daemon cache:
  - HNSW vector graph
  - chunk id map
  - Tantivy reader

## Incremental updates

- Async `PostToolUse` hook sends changed file path hints to daemon `/update`.
- Daemon also starts a repo file watcher after first repo request and batches FS change events with debounce.
- A periodic reconcile signal triggers metadata-based re-scan to recover from missed watcher/hook events.
- Re-index computes changed hashes and re-embeds only changed files unless a reconcile pass is requested.
- HNSW graph is rebuilt in-memory from current chunk set.
