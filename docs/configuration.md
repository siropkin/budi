# budi Configuration Reference

Config lives at `~/.local/share/budi/repos/<repo-id>/config.toml`. Every field has a default — an empty file is valid. Unknown fields are silently ignored (safe to add comments or remove fields you don't need).

Find the path for the current repo:

```bash
budi repo status  # prints the data directory path
```

---

## Daemon

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `daemon_host` | string | `"127.0.0.1"` | Host the daemon binds to. Change to `"0.0.0.0"` only in trusted environments. |
| `daemon_port` | integer | `7878` | Port the daemon listens on. Change if 7878 conflicts with another service. |

---

## Retrieval

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `retrieval_limit` | integer | `8` | Max snippets returned per query. Per-intent limits (5–8) apply automatically when this equals the default; explicit values always win. |
| `context_char_budget` | integer | `12000` | Hard cap on total injected context characters per prompt. Includes call graph context. |
| `min_inject_score` | float | `0.05` | Minimum score threshold to inject *any* context. Raise (e.g. `0.10`) for more conservative injection. |
| `topk_lexical` | integer | `20` | Candidate pool size from the BM25 lexical channel before fusion. |
| `topk_vector` | integer | `20` | Candidate pool size from the HNSW vector channel before fusion. |
| `skip_non_code_prompts` | bool | `true` | Skip injection when the prompt looks like a non-code question ("what time is it", "summarize this text"). |
| `smart_skip_enabled` | bool | `true` | Apply intent-aware heuristics to suppress low-confidence injections on non-code intents. |

---

## Indexing

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `index_extensions` | string[] | `["rs","ts","tsx","js","jsx","py","go","java","kt","swift","cpp","cc","cxx","c","h","hpp","cs","rb","php","scala","sql","sh","graphql","proto"]` | File extensions to index (without leading dot). |
| `index_basenames` | string[] | `["Dockerfile","Makefile","Rakefile","Gemfile","Procfile"]` | Exact filenames (no extension) to index regardless of extension. |
| `max_file_bytes` | integer | `1500000` | Files larger than this are skipped. Default is ~1.5 MB. |
| `max_index_files` | integer | `20000` | Hard cap on total indexed files per repo. |
| `max_index_chunks` | integer | `250000` | Hard cap on total indexed chunks per repo. |
| `chunk_lines` | integer | `80` | Target chunk size in lines (sliding window). Larger chunks = more context per snippet, fewer total chunks. |
| `chunk_overlap` | integer | `20` | Overlap in lines between adjacent chunks. Larger overlap catches more cross-boundary definitions. |

---

## Embeddings

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `embedding_batch_size` | integer | `96` | Chunks embedded per batch call. Reduce if the process runs out of memory on large repos. |
| `embedding_retry_attempts` | integer | `3` | Retries on a failed embedding batch before the chunk is skipped. |
| `embedding_retry_backoff_ms` | integer | `75` | Base backoff in milliseconds between retries (exponential). |

---

## Debug / Telemetry

All debug fields are **off by default** and have no runtime cost when disabled.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `debug_io` | bool | `false` | Log every hook event (query, prefetch, session-start) to `logs/hook-io.jsonl`. Useful for inspecting what budi injects and why. |
| `debug_io_full_text` | bool | `false` | Include full injected context text in each log entry. Requires `debug_io = true`. |
| `debug_io_max_chars` | integer | `1200` | Max characters of context to log per entry when `debug_io_full_text = true`. |

### Reading the debug log

```bash
# Print last 5 query events
tail -n 50 ~/.local/share/budi/repos/<repo-id>/logs/hook-io.jsonl | \
  jq 'select(.event == "query")' | head -100

# Watch live
tail -f ~/.local/share/budi/repos/<repo-id>/logs/hook-io.jsonl | jq .
```

### Example config for debugging

```toml
debug_io = true
debug_io_full_text = true
debug_io_max_chars = 4000
```

---

## Example production config

```toml
# Raise injection bar to reduce noise on large repos
min_inject_score = 0.10

# Larger context window for complex codebases
context_char_budget = 16000

# Index more files
max_index_files = 30000
```
