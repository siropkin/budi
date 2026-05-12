# ADR-0093: Copilot Chat — JetBrains Host Storage Shape

- **Date**: 2026-05-11
- **Status**: Accepted
- **Issue**: [#716](https://github.com/siropkin/budi/issues/716)
- **Milestone**: 8.4.3
- **Related**: [ADR-0092](./0092-copilot-chat-data-contract.md) (Copilot Chat data contract — this ADR amends §2.1 by adding a JetBrains host class), [ADR-0088](./0088-8x-local-developer-first-product-contract.md) §7 (host-scoped vs. provider-scoped surfaces)

## Context

ADR-0092 §2.1 pins the local-tail path roots for Copilot Chat at five VS Code-family directory names under each OS's application-support root (`Code`, `Code - Insiders`, `Code - Exploration`, `VSCodium`, `Cursor`). That contract covered every Copilot Chat host that mattered at the time the ADR was accepted — VS Code stable, Insiders, the Exploration channel, VSCodium, Cursor, and every remote/dev-container shape of each.

The 8.4.x train added JetBrains as a first-class host. The `budi-jetbrains` plugin shipped to the JetBrains Marketplace on 2026-05-08 (listing: <https://plugins.jetbrains.com/plugin/31662-budi>) and renders the statusline against whatever the daemon attributes to `surface=jetbrains`. The classifier at `crates/budi-core/src/surface.rs::infer_copilot_chat_surface` was already wired in #701 to return `jetbrains` for JetBrains-shaped paths, but the corresponding `watch_roots()` discovery in `crates/budi-core/src/providers/copilot_chat.rs` continued to iterate only VS Code-family roots. The JetBrains classifier therefore never fired against a real path in production — the statusline rendered zeros for every JetBrains-only user.

Before a JetBrains-side parser ticket can land, the storage shape needs the same evidence-based treatment ADR-0092 §2.3 gave to the VS Code side. Without a captured fixture and an explicit data contract, the parser would be guessing at file framing, entity names, and value-encoding strategies from documentation alone — and the JetBrains shape diverges materially enough from VS Code's plain-JSON tail that the existing contract cannot be stretched to cover it.

## Decision

The Copilot Chat provider's JetBrains-side discovery and parser are scoped to the storage shape described below. This ADR is the spec a forthcoming JetBrains parser ticket implements against; any change in the GitHub Copilot for JetBrains plugin's storage layout is handled by amending this ADR in the same PR as the parser update.

### 1. Provider identity unchanged

Provider id stays `copilot_chat`. Per ADR-0088 §7, the `surface` dimension carries the host distinction: VS Code-family rows surface as `surface=vscode` or `surface=cursor`; JetBrains rows surface as `surface=jetbrains`. This ADR does not introduce a new provider — JetBrains is a host of the same Copilot Chat provider, no different in identity from how Cursor is a host of the same Copilot Chat provider for VS Code-family files.

### 2. Path roots (per OS)

The provider's `watch_roots()` is extended with a JetBrains-side root in addition to the five VS Code-family roots already pinned in ADR-0092 §2.1.

- **macOS / Linux**: `~/.config/github-copilot/`
- **Windows**: `%LOCALAPPDATA%\github-copilot\` (needs an on-Windows capture pass to confirm; the on-disk layout is otherwise expected to be identical)

This root is **not** under `Application Support/JetBrains/<Product><Year>/` — the assumption in the original [#716](https://github.com/siropkin/budi/issues/716) ticket text turned out to be wrong. The GitHub Copilot plugin uses an XDG-style root keyed off the GitHub identity, identical to the path the GitHub CLI publishes under, plus a per-IDE-flavor sub-slug.

### 3. Layout under the JetBrains root

```
~/.config/github-copilot/
├── apps.json                       # OAuth tokens. Treat as secret; never read by budi.
├── versions.json                   # {"copilot-intellij":"<plugin-version>"}
├── copilot-intellij.db             # SQLite single-table state (first-boot flag etc.). Not interesting.
├── intellij/                       # Shared cross-IDE settings (instruction markdown, mcp.json)
└── <ide-slug>/                     # ic, iu, ws, and others per JetBrains product family
    ├── chat-sessions/<session-id>/
    ├── chat-agent-sessions/<session-id>/
    ├── chat-edit-sessions/<session-id>/
    └── bg-agent-sessions/<session-id>/
```

Known IDE slugs observed so far: `ic` (IntelliJ IDEA Community), `iu` (IntelliJ IDEA Ultimate), `ws` (WebStorm), `intellij` (shared cross-IDE settings — not a session-bearing slug). PyCharm, GoLand, RustRover, PhpStorm etc. will introduce additional slugs that the discovery code must enumerate by directory-listing rather than a hardcoded allowlist — the slug set is open by design, the same way ADR-0092 §2.1's VS Code-family allowlist would be wrong to pin closed.

`<session-id>` is a 27-character base58-shaped opaque identifier (e.g. `36WZJbBx05NpO28apIrHaBmmyCJ`). The same `<session-id>` may appear under multiple IDE slugs concurrently when the same chat conversation is opened from different JetBrains products — they are independent stores, not symlinked, and must be tailed independently.

### 4. Per-session storage layer (Xodus + Nitrite)

Each `<session-id>/` directory contains a **binary** dual-store layout:

```
<session-id>/
├── 00000000000.xd                  # Xodus log file (binary, JetBrains' embedded entity store)
├── xd.lck                          # ASCII Xodus lockfile; first line embeds host name & PID
├── copilot-chat-nitrite.db         # Nitrite NoSQL document store (MVStore-backed). May be absent on legacy sessions.
└── blobs/
    └── version                     # 4-byte version stamp (observed: 00 00 00 01)
```

This is fundamentally different from the VS Code-side contract (ADR-0092 §2.3), which is plain newline-delimited JSON the parser can stream-read with `serde_json::from_str`. The JetBrains side requires either a Java/Kotlin bridge, a reimplemented Xodus log reader, or — pragmatically — a parse-on-top of `strings(1)`-extracted byte patterns at the cost of robustness. The choice between these is parser-ticket scope, not ADR scope.

**Xodus log** (`00000000000.xd`): written by JetBrains' embedded transactional key/value store, accessed via [kotlinx-dnq](https://github.com/JetBrains/xodus-dnq) ORM. Entity types observed across the per-directory schemas:

| Directory | Xodus entity types |
|---|---|
| `chat-sessions/` | `XdChatSession`, `XdClient`, `XdSelectedModel`, `XdMigration` |
| `chat-agent-sessions/` | `XdAgentSession`, `XdMigration` |
| `chat-edit-sessions/` | TBD — same Xodus scaffold; capture in follow-up |
| `bg-agent-sessions/` | TBD — observed only under `iu/` so far |

`XdChatSession` properties observed: `activeAt`, `createdAt`, `editorName`, `editorPluginVersion`, `editorVersion`, `modifiedAt`, `nameSource`, `projectName`. `XdSelectedModel` properties observed: `modelName`, `scope`. `XdClient` carries a per-install UUID treat-as-PII.

**Nitrite store** (`copilot-chat-nitrite.db`, `copilot-agent-sessions-nitrite.db`, `copilot-edit-sessions-nitrite.db`): [Nitrite](https://github.com/nitrite/nitrite-java) NoSQL documents on top of H2 MVStore 2.2.224. Header line (ASCII at offset 0): `H:2,block:8,blockSize:1000,...`. Document values are wrapped in `NitriteDocument` (`LinkedHashMap`-backed) and persisted via standard Java serialization (`ac ed 00 05` framing). Collections observed:

| File | Nitrite collection (FQCN) |
|---|---|
| `copilot-chat-nitrite.db` | `com.github.copilot.chat.session.persistence.nitrite.entity.NtSelectedModel` |
| `copilot-agent-sessions-nitrite.db` | `NtAgentSession`, `NtAgentTurn`, `NtAgentWorkingSetItem` |
| `copilot-edit-sessions-nitrite.db` | TBD |

`NtSelectedModel` fields observed: `scope`, `modelName`, `_revision`, `_modified`, `$nitrite_id`. `NtAgentTurn` is the most likely candidate for per-message attribution but its inner schema needs a non-empty fixture capture before this ADR can pin it; the parser ticket is the right place for that follow-up.

### 5. No local token telemetry

**Critically, no `promptTokens` / `outputTokens` / per-message token counts were observed in either the Xodus or Nitrite schema-string inventory** on any captured session — empty or populated. The JetBrains plugin appears to persist session metadata and selected-model state locally, but **not** the per-turn token telemetry the VS Code-side surface emits to `usage.*` and `result.metadata.*` keys (ADR-0092 §2.3).

This has direct consequences for the data contract:

- **Local-tail attribution for JetBrains is best-effort metadata-only.** A JetBrains parser can attribute "a session existed at this time, with this model selected, under this project" but cannot dollarize the per-turn cost from local data alone.
- **GitHub Billing API reconciliation is the primary token source for JetBrains sessions**, not the supplementary truth-up role it plays for VS Code-family sessions. The reconciliation path in `crates/budi-core/src/sync/copilot_chat_billing.rs` already handles individually-licensed users; org-managed-license JetBrains users will see zero billing-API token rows in the same way they already do on VS Code, since the upstream API itself is empty for those users (ADR-0092 §3).
- **The statusline rolling 1d/7d/30d aggregate will not reflect JetBrains usage in real time** the way it does for VS Code-family local tails. JetBrains contributions land only after the next billing-API reconciliation pass. This is an acceptable trade for 8.4.x — the alternative (parse the binary stores) is parser-ticket scope and may be revisited in 8.5+ if the local stores turn out to carry token data in fields not yet inspected.

If a future inspection of `NtAgentTurn` reveals token fields, this section is amended in lockstep with the parser change that consumes them — the surface contract and the code must never disagree.

### 6. Surface attribution and the existing classifier

The classifier at `crates/budi-core/src/surface.rs::infer_copilot_chat_surface` already returns `surface::JETBRAINS` for paths under `~/.config/github-copilot/` (the unit test at `crates/budi-core/src/providers/copilot_chat.rs::surface_jetbrains_path_classifier_returns_jetbrains_placeholder` pins this). This ADR does not change classifier behavior; it only describes the storage shape the discovery code will walk before the classifier ever sees a path.

The "placeholder" framing in the existing classifier comments is retained until a JetBrains-side parser actually emits rows. Until then, the placeholder language honestly describes the state of the system: the classifier knows what `surface=jetbrains` means; the discovery code has the path; the parser is the missing piece.

## Consequences

- A new fixture (`crates/budi-core/src/providers/copilot_chat/fixtures/jetbrains_copilot_1_5_53_243_empty_session/` plus `.shape.md` and `.expected.json`) anchors the next parser ticket against ground truth instead of synthetic shapes.
- The JetBrains-side `watch_roots()` extension can land as a small follow-up against ADR-0092 §2.1; the path root is now pinned here.
- The statusline-only "Partial" status on the JetBrains row of the README's "Supported agents" table is the correct level of honesty until the parser lands. When local-store parsing or billing-API-only reconciliation produces non-zero JetBrains rows in production, that row promotes to "Supported" in the same PR as the parser merges.
- ADR-0092's §2.1 path-root contract continues to be the VS Code-family side's authoritative spec. This ADR sits alongside it as the JetBrains-side companion rather than amending §2.1 in place, because the binary dual-store shape diverges far enough from the plain-JSON contract that section-level amendment would obscure rather than clarify.

## Open questions for the parser ticket

1. Does `NtAgentTurn` carry token counts in its serialized document body, or are local-only files strictly metadata?
2. What's the `xd.lck` concurrency contract — does Copilot for JetBrains release the Xodus lock when idle, allowing read-only opens while the IDE is running, or must the daemon defer reads until IDE shutdown?
3. Confirm Windows path (`%APPDATA%` vs `%LOCALAPPDATA%`) once a Windows capture is feasible.
4. Confirm the IDE-slug discovery pattern (directory-listing under `~/.config/github-copilot/` excluding `intellij/` and known top-level files) is forward-compatible with new JetBrains products as the plugin ships to them.

## Amendment 2026-05-11 — #757: dual-store probe accepts either `.xd` or `.nitrite.db` as the existence marker

Post-acceptance smoke testing in v8.4.4 surfaced a behavioral fact §4 did not anticipate: recent versions of the GitHub Copilot for JetBrains plugin **skip the Xodus log entirely** on new sessions and persist conversation state to the Nitrite store only. A real-world `chat-sessions/<session-id>/` captured on 2026-05-11 carried `copilot-chat-nitrite.db` (mtime matched the most recent prompt) and no `00000000000.xd` at all. The original parser shape — which bailed when `.xd` was missing — therefore emitted zero rows for every post-migration JetBrains session, leaving the `surface=jetbrains` rollup at $0.00 even when reconciliation rows existed upstream.

### Decision

The parser's existence-check accepts **either** of the two stores. Probe order:

1. `00000000000.xd` — legacy shape; if present and populated, use it (this preserves the pre-#757 behavior for old sessions verbatim).
2. `copilot-chat-nitrite.db`, `copilot-agent-sessions-nitrite.db`, `copilot-chat-edit-sessions-nitrite.db` — current shape; first hit wins.

Either path supplies the same one-row-per-populated-session signal: a single `assistant`-role `ParsedMessage` with `surface=jetbrains`, zero token counts (cost reconciles via the GitHub Billing API per §5 above), and a deterministic UUID derived from the session-id + path.

### Populated-entity markers on the Nitrite side

Nitrite writes its collection class names verbatim into the MVStore catalog. The byte-scan looks for these suffixes (the FQCN prefix is the same for every entry — only the class-name tail is matched so the scan stays robust to future Java-package renames):

| Marker | Meaning |
|---|---|
| `NtChatSession` | Chat session record (chat-sessions/) |
| `NtAgentSession` | Agent session record (chat-agent-sessions/) |
| `NtEditSession` | Edit session record (chat-edit-sessions/) |
| `NtTurn` | Per-turn record under a chat session |
| `NtAgentTurn` | Per-turn record under an agent session |
| `NtEditTurn` | Per-turn record under an edit session |

`NtSelectedModel` is **not** in this set: it is the per-session model preference Nitrite writes the moment the user opens a chat pane, before any prompt has been sent. Treating it as a populated marker would synthesize fake assistant turns for every empty chat tab.

### What this amendment does not promise

The parser still does not extract token counts from Nitrite — §5's conclusion stands. The byte-scan is a "this session is non-empty" signal, not a full MVStore + Java-serialization decoder. Full per-turn extraction (parsing the BSON-like document bodies into `role` / `content` / `tokens` / `model`) remains parser-ticket scope and pairs naturally with the open question on `NtAgentTurn` above. The amendment closes the regression where Nitrite-only sessions emitted no rows at all; deeper extraction is a future ADR amendment.
