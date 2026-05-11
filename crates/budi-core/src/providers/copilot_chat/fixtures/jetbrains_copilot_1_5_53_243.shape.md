# Copilot-for-JetBrains storage shape (plugin `1.5.53-243`)

Captured 2026-05-11 from `github-copilot-intellij` plugin version `1.9.0-251`
(reported via `~/.config/github-copilot/versions.json`) running under WebStorm
2025.2 / IntelliJ IDEA Ultimate 2025.2 / IntelliJ IDEA Community 2025.2 on
macOS. The same on-disk layout was observed across all three IDE flavors —
the per-IDE sub-roots differ only by their two-letter slug.

Companion to fixture `jetbrains_copilot_1_5_53_243_empty_session/`. See
ADR-0093 for the data-contract treatment.

## Root path (per host)

| OS | Root |
|---|---|
| macOS / Linux | `~/.config/github-copilot/` |
| Windows | `%LOCALAPPDATA%\github-copilot\` (per JetBrains "Toolbox-managed" install layout — needs an on-Windows capture pass to confirm) |

Notable: this is **not** under `Application Support/JetBrains/<Product><Year>/`
as the ticket placeholder originally assumed. The Copilot plugin uses the
GitHub-flavored XDG-style root that the GitHub CLI / VS Code Copilot
extensions also key off, plus an IDE-flavor sub-slug.

## Layout

```
~/.config/github-copilot/
├── apps.json                       # OAuth tokens. NEVER commit; treat as secret.
├── versions.json                   # {"copilot-intellij":"<plugin-version>"}
├── copilot-intellij.db             # SQLite, table `state`, used for first-boot flag etc.
├── intellij/                       # Shared cross-IDE settings (markdown instructions, mcp.json)
│   ├── global-agents-instructions.md
│   ├── global-copilot-instructions.md
│   ├── global-git-commit-instructions.md
│   └── mcp.json
├── ic/                             # IntelliJ IDEA Community
├── iu/                             # IntelliJ IDEA Ultimate
├── ws/                             # WebStorm
└── <other IDE slugs...>            # PyCharm, GoLand, RustRover etc. — slug = product-id short code
    ├── chat-sessions/<session-id>/
    ├── chat-agent-sessions/<session-id>/
    ├── chat-edit-sessions/<session-id>/
    └── bg-agent-sessions/<session-id>/
```

`<session-id>` is a 27-character base58-shaped string (e.g.
`36WZJbBx05NpO28apIrHaBmmyCJ`). Sessions with the **same** id may appear
under multiple IDE slugs concurrently when the same chat is opened from
different JetBrains products — they are independent stores, not symlinked.

## Per-session-directory contents

```
<session-id>/
├── 00000000000.xd                  # Xodus log file (binary)
├── xd.lck                          # Xodus lock; ASCII; first line embeds host name & PID
├── copilot-chat-nitrite.db         # Nitrite NoSQL DB (MVStore-backed). May be absent on older sessions.
└── blobs/
    └── version                     # 4-byte version stamp (observed: 00 00 00 01)
```

Lockfile (`xd.lck`) header format:

```
Private property of Exodus: <pid>@<hostname>

<kotlin stack trace from the locking caller>
```

The stack trace identifies the JVM call site that opened the store —
`com.github.copilot.chat.session.persistence.xodus.XdChatSessionPersistenceService`
for `chat-sessions/`, mirrored as `com.github.copilot.agent.session.persistence.*`
for `chat-agent-sessions/` and `chat-edit-sessions/`. These names are stable
hints for which entity layer owns each directory.

## Storage layer: Xodus + kotlinx-dnq

The `.xd` log file is the JetBrains [Xodus](https://github.com/JetBrains/xodus)
embedded transactional key/value log, read via the
[kotlinx-dnq](https://github.com/JetBrains/xodus-dnq) ORM. Entity types
observed in the embedded schema header (extracted via `strings(1)`):

| Directory | Xodus entity types |
|---|---|
| `chat-sessions/` | `XdChatSession`, `XdClient`, `XdSelectedModel`, `XdMigration` |
| `chat-agent-sessions/` | `XdAgentSession`, `XdMigration` |
| `chat-edit-sessions/` | (TBD — same Xodus scaffold; capture in follow-up) |
| `bg-agent-sessions/` | (TBD — observed only on Ultimate so far) |

Property names observed on `XdChatSession`:

```
activeAt, createdAt, editorName, editorPluginVersion, editorVersion,
modifiedAt, nameSource, projectName
```

Property names observed on `XdSelectedModel`:

```
modelName, scope
```

Property names observed on `XdAgentSession`:

```
activeAt, createdAt, modifiedAt, nameSource
```

`XdClient` carries the per-install client identity (a UUID — treat as PII,
redact in fixtures). Sample observed `XdSelectedModel.scope` values:
`chat-panel`, `edit-panel`, `agent-panel`. Sample `XdSelectedModel.modelName`
values: `GPT-4.1`, `GPT-5 mini`. The data plane stores the **display name**,
not the OpenAI/Anthropic model id — a parser will need a display-to-id
lookup table (or to read the model registry from the IDE).

Critically, **no `promptTokens` / `outputTokens` / per-message token counts
were observed in the Xodus schema**. Per-turn telemetry appears to live
either in the Nitrite store (see below) or to not be persisted locally at
all. Token-level reconciliation likely requires the GitHub Billing API path
already wired in `sync/copilot_chat_billing.rs`.

## Storage layer: Nitrite (MVStore-backed)

The `.db` files are [Nitrite](https://github.com/nitrite/nitrite-java) NoSQL
documents, persisted on top of H2 MVStore 2.2.224. Header line (ASCII at
offset 0):

```
H:2,block:8,blockSize:1000,chunk:1,clean:1,created:<hex>,format:3,version:<n>,fletcher:<hex>
```

Indices and collections observed:

| File | Nitrite collection / entity |
|---|---|
| `copilot-chat-nitrite.db` (chat-sessions) | `com.github.copilot.chat.session.persistence.nitrite.entity.NtSelectedModel` |
| `copilot-agent-sessions-nitrite.db` | `NtAgentSession`, `NtAgentTurn`, `NtAgentWorkingSetItem` |
| `copilot-edit-sessions-nitrite.db` | (TBD) |

Document values are stored via standard Java serialization
(`java.io.ObjectOutputStream` framing — magic bytes `ac ed 00 05` at chunk
roots) wrapped inside Nitrite's `NitriteDocument` (`LinkedHashMap`-backed).
Key fields observed inside `NtSelectedModel` documents:

```
scope          (String, e.g. "agent-panel")
modelName      (String, e.g. "GPT-5 mini")
_revision      (Integer)
_modified      (Long, ms epoch)
$nitrite_id    (NitriteId / String, e.g. "2014033936592187393")
```

`NtAgentTurn` is the most likely candidate for per-message attribution
(turn = one user/assistant round-trip) but its schema needs a fresh capture
against a session with non-empty agent activity. The fixture committed
alongside this doc is an **empty** session (schema scaffold only) — see the
"Why an empty fixture?" note below.

## Why an empty fixture?

The captured session (`36WZJbBx05NpO28apIrHaBmmyCJ` under `iu/chat-sessions/`)
holds only the Xodus migration bootstrap and Nitrite store-info record —
zero `XdChatSession` / `NtSelectedModel` rows. This was a deliberate trade:

- Empty sessions contain **no PII** in the Xodus log: no GitHub username, no
  project name, no client UUID, no host name in the data plane. Only the
  schema strings (Xodus internal tables and refactor flags) appear.
- Sessions with actual chat activity embed `siropkin`-style GitHub usernames,
  customer project names, and per-install UUIDs as length-prefixed byte
  strings inside the binary log. Byte-exact same-length redaction is
  feasible but brittle (Xodus log entries carry length prefixes that *can*
  be invalidated by careless edits) and the redacted result would teach a
  future parser nothing the empty fixture doesn't already teach about file
  framing.
- The actionable signal — the entity/property/index/collection name set —
  is captured here in `shape.md` from `strings(1)` extraction of multiple
  non-empty real sessions, not from the committed binary.

If a future ticket needs a non-empty fixture (e.g. to validate a kotlinx-dnq
loader against real document framing), capture one fresh under a throwaway
GitHub account with no project context.

## `xd.lck` redaction in the committed fixture

The committed `xd.lck` header has been byte-exact replaced:

```
Private property of Exodus: <pid>@<hostname>
```

becomes

```
Private property of Exodus: 0000@redacted.invalid------------
```

The dash padding preserves the original byte length so the rest of the
ASCII stack trace stays at its original offsets. (The lock file is not
read by Xodus once the store is opened, so the redaction does not affect
loadability.)

## Open questions for the parser ticket

1. Do `NtAgentTurn` documents carry token counts, or are local-only files
   strictly "session metadata" with telemetry deferred to the billing API?
2. What's the `xd.lck` semantics — does the JetBrains plugin always hold
   the lock while running, or release on idle? (Affects whether budi can
   open the store read-only while the IDE is alive — VS Code's plain-JSON
   files are read-without-locking; JetBrains may need
   `Environment.tryOpenReadOnly` semantics.)
3. Which JetBrains Toolbox-style "shared install" path is used on Linux
   (XDG_CONFIG_HOME vs `~/.config/github-copilot/`)?
4. Windows path — verify whether the plugin honors `%APPDATA%`,
   `%LOCALAPPDATA%`, or the per-IDE config root.
