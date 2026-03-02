# Benchmark Methodology

`budi-bench` provides a reproducible local benchmark for query latency and injected-context size.

## Run

```bash
budi-bench --prompt "Fix the auth bug in login flow" --iterations 30
```

## Report fields

- `latency_ms_p50`: median daemon query latency
- `latency_ms_p95`: high percentile retrieval latency
- `avg_context_chars`: average injected context size in characters
- `avg_context_tokens_estimate`: rough tokens estimate (`chars / 4`)

## Extended KPI tracking

For end-to-end evaluations against native Claude Code behavior:

1. Record Time-To-First-Token in Claude Code with and without hook.
2. Compare token usage via `/cost` before and after.
3. Measure context hit-rate for files changed within the last 15 minutes.

## Debug logging during A/B runs

The A/B runner temporarily enables:

- `debug_io = true`
- `debug_io_full_text = false`
- `debug_io_max_chars = 1500`

After the run, it restores your previous local config values automatically (`~/.local/share/budi/repos/<repo-id>/config.toml`).

## Smart skip in benchmarks

By default, smart skip remains enabled during benchmark runs:

- `smart_skip_enabled = true`
- `skip_non_code_prompts = true`
- `min_confidence_to_inject = 0.45`

For retrieval stress tests where you always want injected context, add `@forcebudi` at the beginning of the benchmark prompt text.
