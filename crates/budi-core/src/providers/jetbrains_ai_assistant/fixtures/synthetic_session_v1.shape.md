# JetBrains AI Assistant storage shape (synthetic v1)

Companion to fixture `synthetic_session_v1.jsonl`. JetBrains AI Assistant
(plugin id `com.intellij.ml.llm`) is the JetBrains-published, Anthropic-backed
assistant — distinct from the GitHub Copilot for JetBrains plugin that ADR-0093
covers. Billing flows through the user's JetBrains AI subscription, not
through GitHub.

This fixture is **synthetic**: no real-world chat session was captured for
the v0.2 daemon landing. The Anthropic wire shape mirrored below is the
public Anthropic Messages API response format that the JetBrains backend
proxies through to the IDE plugin (per JetBrains' AI Assistant docs — see
[Open questions](#open-questions-for-a-future-real-capture) for items the
follow-up ticket should pin against ground truth).

## Path roots (per OS)

JetBrains AI Assistant persists chat sessions under the per-IDE configuration
root that JetBrains itself owns — **not** under the GitHub-Copilot-flavored
XDG path ADR-0093 pins for Copilot for JetBrains. The chat-session
directory uses the assistant-specific subdir name `aiAssistant/chats/`,
established by the bundled plugin's `PersistentStateComponent` and matching
JetBrains' documented "AI Assistant chat history" location.

| OS | Root |
|---|---|
| macOS | `~/Library/Application Support/JetBrains/<Product><Year>/aiAssistant/chats/` |
| Linux | `~/.config/JetBrains/<Product><Year>/aiAssistant/chats/` |
| Windows | `%APPDATA%\JetBrains\<Product><Year>\aiAssistant\chats\` |

`<Product><Year>` is the IDE configuration directory slug used by every
JetBrains product (`IntelliJIdea2025.3`, `WebStorm2026.1`, `PyCharm2025.3`,
…). The parser enumerates every JetBrains product directory under the
platform-specific root rather than hardcoding a closed allowlist — the
product set is open by design, the same way ADR-0093 §3 keeps the
GitHub-Copilot IDE-slug set open.

`<Product><Year>/aiAssistant/chats/` is the directory the daemon's tailer
attaches a recursive watcher to. Individual chat sessions live one level
deeper under per-session subdirectories.

## Per-session layout

```
<Product><Year>/aiAssistant/chats/
├── <session-id>.jsonl              # Anthropic-style JSON Lines transcript (one event per line)
└── <session-id>.meta.json          # Optional metadata sidecar (project name, model, created/modified)
```

`<session-id>` is an opaque 36-character UUID (e.g.
`8c2e5d63-6f4a-4e21-8d11-2b9a3a4e5b6c`). New sessions write the transcript
file on first turn and append on every subsequent turn; the file is **not**
rotated.

## Wire format (Anthropic-style JSONL)

Each line is one self-contained JSON object describing one event in the
turn lifecycle. The fields the daemon reads are pinned below; any extra
fields the JetBrains backend adds (telemetry, classification hints,
provider-internal flags) are ignored.

### `message_start` event

Emitted once at the beginning of an assistant turn. Carries the session
id, the timestamp, the model id, and the **initial** `usage` block —
which already contains the input-token count Anthropic returns at the
top of a streamed response.

```json
{
  "type": "message_start",
  "id": "msg_01ABCdef",
  "session_id": "8c2e5d63-6f4a-4e21-8d11-2b9a3a4e5b6c",
  "timestamp": "2026-05-11T18:00:00Z",
  "model": "claude-sonnet-4-20250514",
  "role": "assistant",
  "usage": {
    "input_tokens": 1240,
    "cache_creation_input_tokens": 0,
    "cache_read_input_tokens": 0,
    "output_tokens": 1
  }
}
```

### `message_delta` event(s)

Streamed deltas; the daemon ignores them.

### `message_stop` event

Emitted once at the end of an assistant turn. Carries the **final**
`usage` block — the input-token count is unchanged from `message_start`,
but `output_tokens` now reflects the full assistant completion size.

```json
{
  "type": "message_stop",
  "id": "msg_01ABCdef",
  "session_id": "8c2e5d63-6f4a-4e21-8d11-2b9a3a4e5b6c",
  "timestamp": "2026-05-11T18:00:14Z",
  "model": "claude-sonnet-4-20250514",
  "role": "assistant",
  "usage": {
    "input_tokens": 1240,
    "cache_creation_input_tokens": 0,
    "cache_read_input_tokens": 0,
    "output_tokens": 612
  }
}
```

The parser keys off `message_stop` events to emit one
`ParsedMessage` per assistant turn — that is the event with the
finalized token counts. `message_start` is read only to populate the
running model id for incremental tailing (so a `message_stop` line that
omits the model still carries the correct attribution).

### `user_turn` event

Optional; emitted once per user turn. Not currently parsed for
analytics rows; the analytics-relevant token cost is on the assistant
turn that follows.

## Privacy

The committed fixture deliberately contains **no** PII:

- No prompt text or completion text — only token counts.
- No project name, cwd, git branch, user id, or machine id.
- Session ids are synthesized random UUIDs, not captured from a real session.

A future real-world capture must redact the same fields before landing as
a replacement fixture (the JetBrains plugin tends to embed project paths
in the optional `.meta.json` sidecar, and the JetBrains AI server proxies
the user's organisation id in the `id` field on enterprise accounts —
those need to be stripped).

## Open questions for a future real capture

1. **Exact path**. The `aiAssistant/chats/` subdir is the published
   convention; confirm against a live IDE install once one is available.
   If the bundled plugin moves to a JetBrains-owned XDG-style root in a
   future version, this shape doc and the parser's `watch_roots()` change
   together.
2. **Sidecar metadata**. Whether `<session-id>.meta.json` carries the
   project path / cwd in a form the daemon can normalize (the cwd would
   feed the GitEnricher and FileEnricher) is unconfirmed. If not, the
   parser emits `cwd = None` and relies on session-level propagation.
3. **Streaming-time framing**. JetBrains AI Assistant streams via SSE;
   the daemon needs to verify that the plugin actually appends complete
   JSONL lines (not partial chunks) before flushing. The Copilot Chat
   provider's `mutation-log reducer` (R1.1) gives a precedent for
   handling streaming writes safely.
4. **Anthropic model id form**. Confirm whether the JetBrains plugin
   forwards the wire-level model id (`claude-sonnet-4-20250514`) or
   substitutes a JetBrains-side alias (`anthropic/claude-sonnet`). The
   manifest-backed `pricing::lookup` resolves both, but the fixture's
   `expected.json` should be regenerated against the real form.
