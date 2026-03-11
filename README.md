# budi

[![CI](https://github.com/siropkin/budi/actions/workflows/ci.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/ci.yml)
[![Release](https://github.com/siropkin/budi/actions/workflows/release.yml/badge.svg)](https://github.com/siropkin/budi/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/siropkin/budi)](https://github.com/siropkin/budi/blob/main/LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/siropkin/budi?style=social)](https://github.com/siropkin/budi)

**Stop paying Claude to rediscover your codebase on every prompt.**

`budi` indexes your repo locally and pre-injects the right code snippets before Claude starts searching. Faster answers, fewer wasted tool calls, 3-32% lower cost.

<p align="center">
  <img src="assets/demo.gif" alt="budi demo — init, index, and preview on Flask" width="700">
</p>

## How it works

You ask Claude Code a question. Before Claude starts searching, `budi` detects your intent, searches a local index across five retrieval channels (lexical, semantic, symbol, path, call graph), and injects the best snippets into context. Claude starts reasoning with the right code already in view.

No extra tool calls. No round trips. Just the code Claude was about to look for, already there.

## Features

- **Fast** — retrieval runs in ~10ms; indexing takes seconds with warm cache
- **Automatic** — Claude Code hooks run silently in the background
- **Local** — your code stays on your machine; no cloud, no uploads
- **Intent-aware** — routes queries through symbol lookup, call tracing, architecture, config, and test discovery
- **Language-aware** — AST-powered chunking for JS/TS, Python, Rust, Go, Java, C/C++, C#, Ruby, Kotlin, Swift, Scala, PHP
- **Incremental** — file edits update the index in-place without rebuilding
- **Controllable** — skip once with `@nobudi`, force once with `@forcebudi`

## A/B results

Tested on 8 open-source repos (131 judged prompts) with an independent LLM judge:

| Metric | Result |
|--------|--------|
| Non-regression rate | **~91%** (same or better quality) |
| Cost savings | **3-32%** lower on most repos |
| Best result | FastAPI: 100% non-regression, 11 quality wins |

budi's goal: same answer quality at lower cost. Ties (same quality, less cost) are the primary success metric.

Full methodology, prompts, and per-prompt evidence: [docs/benchmark.md](docs/benchmark.md)

## Install

### Quick start (Claude Code plugin)

```text
/plugin marketplace add siropkin/budi
/plugin install budi-hooks@budi-plugins
```

Then in your repo:

```bash
budi init --index
```

That's it. Use Claude Code normally.

### Manual install

```bash
# From latest GitHub release (requires gh CLI):
git clone https://github.com/siropkin/budi.git && cd budi
./scripts/install.sh --from-release

# Or build from source (requires Rust toolchain):
git clone https://github.com/siropkin/budi.git && cd budi
./scripts/install.sh
```

## Useful commands

```bash
budi doctor              # check installation health
budi repo status         # see index state and stats
budi repo search "X"     # search the index directly
budi repo preview "..."  # preview what budi would inject
budi index --hard        # full re-index
```

## MCP server

`budi` also works in Cursor, Zed, Windsurf, and any editor supporting [MCP](https://modelcontextprotocol.io/). See [configuration docs](docs/configuration.md#mcp-server) for setup.

## Compared to alternatives

| | budi | context-mode | Augment Context Engine | GitNexus |
|---|---|---|---|---|
| **Strategy** | Pre-inject before search | Compress output after search | MCP search on demand | Knowledge graph + MCP |
| **Latency** | Zero (hooks, no round trip) | Zero (intercept) | +1 tool call | +1 tool call |
| **Retrieval** | 5-channel with intent routing | BM25 with fallback | Proprietary | BM25 + semantic |
| **A/B validated** | 8 repos, 131 prompts | No | No | No |
| **Privacy** | 100% local | 100% local | Cloud | 100% local |

budi is complementary with output-compression tools like context-mode.

## Docs

- [Benchmark methodology](docs/benchmark.md) and [per-prompt evidence](docs/benchmark-details.md)
- [Configuration](docs/configuration.md)
- [Architecture](docs/architecture.md)
- [Installer details](docs/installer.md)

## Privacy

Everything runs locally. No cloud index. No repo upload. No external retrieval service.

## License

[MIT](LICENSE)
