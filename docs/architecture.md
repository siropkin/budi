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
   - Git-aware re-ranking (dirty files, branch, cwd proximity)
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

- `~/.local/share/budi/repos/<repo-id>/index/state.json`: files, chunk metadata, embeddings, branch/head snapshot
- `~/.local/share/budi/repos/<repo-id>/index/tantivy/`: lexical index files
- in-memory daemon cache:
  - HNSW vector graph
  - chunk id map
  - Tantivy reader

## Incremental updates

- Async `PostToolUse` hook sends changed file path to daemon `/update`.
- Re-index computes changed hashes and re-embeds only changed files.
- HNSW graph is rebuilt in-memory from current chunk set.
