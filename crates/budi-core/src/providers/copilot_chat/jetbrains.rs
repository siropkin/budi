//! GitHub Copilot for JetBrains — host-side discovery and parser.
//!
//! ADR-0093 pins the storage shape: `~/.config/github-copilot/<ide-slug>/`
//! holds per-IDE chat-session subtrees in JetBrains' binary Xodus+Nitrite
//! dual-store layout. Per §5 of that ADR, the local stores do **not**
//! carry per-turn token telemetry — token attribution flows through the
//! GitHub Billing API reconciliation in `crate::sync::copilot_chat_billing`.
//! This module's job is to surface "a session existed" rows so the
//! reconciliation has somewhere to attach costs, and so
//! `budi stats surfaces` lights up the JetBrains row instead of rendering
//! `$0.00` against an empty surface bucket.
//!
//! What we emit:
//!   - one assistant-role `ParsedMessage` per session directory whose
//!     `00000000000.xd` carries an `XdChatSession` or `XdAgentSession`
//!     entity-type marker (the binary log always names entity types as
//!     literal ASCII inside the schema header — extracted via byte-scan
//!     rather than a full Xodus log decoder),
//!   - `timestamp` from the `.xd` file's mtime (best signal we have without
//!     parsing the binary log frames),
//!   - `session_id` from the session directory name (27-char base58-shaped),
//!   - `surface = jetbrains`, zero tokens — costs land later via billing API.
//!
//! The empty fixture under `fixtures/jetbrains_copilot_1_5_53_243_empty_session/`
//! contains only `XdMigration` bootstrap entries (no `XdChatSession`), so the
//! parser correctly emits zero rows against it; the populated case is
//! exercised by integration tests that synthesize a session dir with the
//! entity-type marker present.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::jsonl::ParsedMessage;

/// Stable provenance prefix used inside the deterministic UUID so the
/// JetBrains-side rows never collide with the VS Code-side `copilot_chat`
/// UUIDs (which use a different prefix in `super::deterministic_uuid`).
const UUID_NAMESPACE: &[u8] = b"copilot_chat:jetbrains:";

/// Session directories live under `<ide-slug>/<session-type>/<session-id>/`.
/// `intellij/` is the shared cross-IDE settings dir (markdown instructions,
/// `mcp.json`) — it is not a session-bearing slug, see ADR-0093 §3.
const SESSION_TYPE_DIRS: &[&str] = &[
    "chat-sessions",
    "chat-agent-sessions",
    "chat-edit-sessions",
    "bg-agent-sessions",
];

/// Top-level files/dirs under `~/.config/github-copilot/` that are not
/// IDE-slug session roots. Walked by `discover_session_dirs` to skip noise
/// without hardcoding a closed allowlist of IDE slugs.
const NON_IDE_TOP_LEVEL: &[&str] = &[
    "intellij",
    "apps.json",
    "versions.json",
    "copilot-intellij.db",
];

/// Entity-type markers that indicate a session with actual chat activity.
///
/// The JetBrains-side storage shape has gone through two iterations and the
/// parser has to recognize either:
///
/// - **Xodus log (`00000000000.xd`)** — the legacy shape from ADR-0093 §4.
///   Empty sessions hold only `XdMigration` bootstrap rows; sessions with
///   chat activity carry an `XdChatSession` or `XdAgentSession` entity-type
///   record (length-prefixed ASCII inside the binary log header).
///
/// - **Nitrite store (`copilot-chat-nitrite.db`, `copilot-agent-sessions-
///   nitrite.db`, `copilot-chat-edit-sessions-nitrite.db`)** — the current
///   shape (#757). Nitrite is a Java-side embedded NoSQL DB that writes
///   class names into its catalog as `com.github.copilot.chat.session.
///   persistence.nitrite.entity.Nt<...>`. An empty session contains only
///   `NtSelectedModel` (the per-session model preference); sessions with
///   user turns also carry `NtChatSession`/`NtAgentSession` and the
///   per-turn `NtTurn`/`NtAgentTurn` records. The byte-level scan looks
///   for these literal class-name suffixes — Nitrite's MVStore-format
///   pages embed them verbatim, so a full Nitrite/MVStore decoder isn't
///   needed for the "session exists and is non-empty" signal.
const POPULATED_ENTITY_MARKERS: &[&[u8]] = &[
    // Xodus log markers (legacy shape).
    b"XdChatSession",
    b"XdAgentSession",
    // Nitrite catalog markers (#757). Match the Nt-prefixed entity class
    // names rather than the fully-qualified path so the test fixtures can
    // be tiny and the scan stays robust to future Java-package renames.
    b"NtChatSession",
    b"NtAgentSession",
    b"NtEditSession",
    b"NtTurn",
    b"NtAgentTurn",
    b"NtEditTurn",
];

/// Filenames the JetBrains Copilot plugin uses for its Nitrite stores.
/// One per session-type subdirectory; only one of these typically exists
/// in any given session directory, but #757 covers all three shapes so
/// the parser doesn't regress when a future plugin version splits another
/// session-type out.
const NITRITE_DB_FILES: &[&str] = &[
    "copilot-chat-nitrite.db",
    "copilot-agent-sessions-nitrite.db",
    "copilot-chat-edit-sessions-nitrite.db",
];

/// Platform-specific roots that contain the per-IDE-slug session subtrees.
pub(super) fn jetbrains_config_roots() -> Vec<PathBuf> {
    let Ok(home) = crate::config::home_dir() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        roots.push(home.join(".config/github-copilot"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("github-copilot"));
        }
        roots.push(home.join("AppData/Local/github-copilot"));
        // Some Toolbox-managed installs fall back to %APPDATA% — include
        // it as a secondary candidate so we don't miss those layouts.
        if let Ok(roaming) = std::env::var("APPDATA") {
            roots.push(PathBuf::from(roaming).join("github-copilot"));
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home;
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Returns true when any JetBrains-side Copilot session marker is on disk.
/// Used by `CopilotChatProvider::is_available`.
pub(super) fn is_available() -> bool {
    !discover_session_dirs(&jetbrains_config_roots()).is_empty()
}

/// Watch roots for the tailer: the per-session-type parent directories
/// (`<ide-slug>/chat-sessions/`, `<ide-slug>/chat-agent-sessions/`, …).
/// Binary writes inside these dirs do not trigger meaningful tail-side
/// parsing — JetBrains updates the Xodus log atomically — but registering
/// the watcher means new session directories appearing under one of these
/// roots will at least be picked up on the next `sync_direct` tick.
pub(super) fn watch_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for cfg in jetbrains_config_roots() {
        for ide_dir in ide_slug_dirs(&cfg) {
            for session_type in SESSION_TYPE_DIRS {
                let p = ide_dir.join(session_type);
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Enumerate `<ide-slug>/` directories under each config root. The slug
/// set is open by design (PyCharm, GoLand, RustRover, etc. each add their
/// own short code); we discover them by listing rather than allow-listing.
fn ide_slug_dirs(config_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(config_root) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if NON_IDE_TOP_LEVEL.iter().any(|skip| skip == &name.as_str()) {
            continue;
        }
        dirs.push(path);
    }
    dirs.sort();
    dirs
}

/// Discover every `<ide-slug>/<session-type>/<session-id>/` directory under
/// the provided config roots. Each entry is a session directory containing
/// the binary Xodus + Nitrite stores.
pub(super) fn discover_session_dirs(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sessions = Vec::new();
    for cfg in config_roots {
        for ide_dir in ide_slug_dirs(cfg) {
            for session_type in SESSION_TYPE_DIRS {
                let stype_dir = ide_dir.join(session_type);
                let Ok(entries) = std::fs::read_dir(&stype_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        sessions.push(path);
                    }
                }
            }
        }
    }
    sessions.sort();
    sessions
}

/// Read a session directory and emit at most one assistant-role
/// `ParsedMessage` representing "this session exists and carries chat
/// activity". Returns an empty vector for empty sessions (ADR-0093 §4)
/// and for directories that cannot be read.
///
/// #757 widened the storage probe to accept either of the two shapes the
/// JetBrains Copilot plugin has shipped: the legacy Xodus log
/// (`00000000000.xd`) and the current Nitrite store
/// (`copilot-chat-nitrite.db` / `copilot-agent-sessions-nitrite.db` /
/// `copilot-chat-edit-sessions-nitrite.db`). Recent plugin versions skip
/// the Xodus file entirely; pre-#757 the parser would bail on
/// `.xd not found` and the session would never emit a row even though
/// `nitrite.db` contained the conversation.
pub(super) fn parse_session_dir(session_dir: &Path) -> Vec<ParsedMessage> {
    // Look at both candidate stores. The first that exists *and* carries
    // a populated-entity marker wins — its mtime feeds the message
    // timestamp. We do not require the .xd file when nitrite.db is
    // present (#757) — the storage shapes are alternatives, not layers.
    let populated_path = populated_store_in(session_dir);
    let Some(store_path) = populated_path else {
        return Vec::new();
    };

    let timestamp = store_path
        .metadata()
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());

    let session_id = session_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let session_type = session_dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let path_str = session_dir.to_string_lossy().to_string();
    let id = deterministic_uuid(session_id.as_deref().unwrap_or(""), &path_str);

    let mut msg = ParsedMessage {
        uuid: id.clone(),
        session_id: session_id.clone(),
        timestamp,
        role: "assistant".to_string(),
        provider: super::PROVIDER_ID.to_string(),
        cost_confidence: "estimated".to_string(),
        request_id: Some(id),
        surface: Some(crate::surface::JETBRAINS.to_string()),
        ..ParsedMessage::default()
    };
    // Surface the session-type as a human-readable session title so
    // dashboards can distinguish chat vs. agent vs. edit sessions without
    // needing a separate column. Stripped to plain "chat"/"agent"/"edit"/"bg-agent"
    // to match the rest of the system's terminology.
    msg.session_title = session_type.map(|s| s.trim_end_matches("-sessions").to_string());
    vec![msg]
}

/// #757: locate the store file in `session_dir` that the parser should
/// treat as the timestamp source for this session. Returns the first
/// candidate that exists on disk *and* carries a populated-entity
/// marker.
///
/// Probe order: `00000000000.xd` first (legacy sessions still parse the
/// same way they used to), then each `NITRITE_DB_FILES` entry. A session
/// directory that contains both — observed on a real DB at the time of
/// #757 — is treated as Xodus-driven for back-compat. Sessions that
/// contain only the `.nitrite.db` (the common case post-migration) read
/// from the Nitrite store.
fn populated_store_in(session_dir: &Path) -> Option<std::path::PathBuf> {
    let xd_path = session_dir.join("00000000000.xd");
    if let Ok(bytes) = std::fs::read(&xd_path)
        && has_populated_entity_marker(&bytes)
    {
        return Some(xd_path);
    }
    for filename in NITRITE_DB_FILES {
        let candidate = session_dir.join(filename);
        let Ok(bytes) = std::fs::read(&candidate) else {
            continue;
        };
        if has_populated_entity_marker(&bytes) {
            return Some(candidate);
        }
    }
    None
}

/// Scan the store-file bytes for entity-type markers that indicate the
/// session carries chat activity. Empty sessions hold only bootstrap
/// rows (Xodus: `XdMigration`; Nitrite: `NtSelectedModel`), so the
/// absence of any populated-entity marker is the honest signal that
/// there is nothing for the parser to emit. See [`POPULATED_ENTITY_MARKERS`].
fn has_populated_entity_marker(bytes: &[u8]) -> bool {
    POPULATED_ENTITY_MARKERS
        .iter()
        .any(|needle| byte_contains(bytes, needle))
}

fn byte_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn deterministic_uuid(session_id: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(UUID_NAMESPACE);
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(path.as_bytes());
    let hash = hasher.finalize();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15],
        ])
    )
}

/// Discover JetBrains-side sessions, parse each, run the resulting messages
/// through the pipeline, and ingest them. Side-effect path called from
/// `CopilotChatProvider::sync_direct` so the JetBrains rows land in the
/// same DB as the VS Code-side ingest does.
///
/// Returns the count of newly ingested messages (best-effort — duplicates
/// from previous ticks are dropped by the `uuid` primary key). Errors are
/// logged and swallowed so a single JetBrains-side blip never breaks the
/// VS Code-side file ingest that runs after this in the dispatcher.
pub(super) fn sync_jetbrains_sessions(
    conn: &mut Connection,
    pipeline: &mut crate::pipeline::Pipeline,
) -> usize {
    let session_dirs = discover_session_dirs(&jetbrains_config_roots());
    if session_dirs.is_empty() {
        return 0;
    }

    let mut messages: Vec<ParsedMessage> = session_dirs
        .iter()
        .flat_map(|d| parse_session_dir(d))
        .collect();
    if messages.is_empty() {
        return 0;
    }

    let tags = pipeline.process(&mut messages);
    match crate::analytics::ingest_messages(conn, &messages, Some(&tags)) {
        Ok(count) => count,
        Err(e) => {
            tracing::warn!("copilot_chat jetbrains ingest failed: {e:#}");
            0
        }
    }
}

#[cfg(test)]
pub(super) fn empty_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/providers/copilot_chat/fixtures/jetbrains_copilot_1_5_53_243_empty_session")
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn parse_session_dir_for_tests(
    session_dir: &Path,
) -> anyhow::Result<Vec<ParsedMessage>> {
    Ok(parse_session_dir(session_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_fixture_parses_to_zero_messages() {
        let dir = empty_fixture_dir();
        let parsed = parse_session_dir(&dir);
        assert!(
            parsed.is_empty(),
            "empty fixture must not emit rows (only XdMigration markers — no XdChatSession): {parsed:?}"
        );
    }

    #[test]
    fn populated_session_marker_yields_one_row() {
        // Synthesize a session dir whose 00000000000.xd carries the literal
        // ASCII bytes for XdChatSession somewhere in its content. The byte
        // scan is shape-agnostic by design — see ADR-0093 §4.
        let tmp = std::env::temp_dir().join("budi-jetbrains-populated");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_id = "36WZJbBx05NpO28apIrHaBmmyCJ";
        let session_dir = tmp.join("ic/chat-sessions").join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x00\x01\x02\x03some xodus framing");
        bytes.extend_from_slice(b"XdChatSession");
        bytes.extend_from_slice(b"\x00more framing\x00");
        std::fs::write(session_dir.join("00000000000.xd"), &bytes).unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1);
        let m = &parsed[0];
        assert_eq!(m.role, "assistant");
        assert_eq!(m.provider, super::super::PROVIDER_ID);
        assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
        assert_eq!(m.session_id.as_deref(), Some(session_id));
        assert_eq!(m.input_tokens, 0);
        assert_eq!(m.output_tokens, 0);
        assert_eq!(m.session_title.as_deref(), Some("chat"));
        assert_eq!(m.cost_confidence, "estimated");
        assert!(m.cost_cents.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn agent_session_marker_titled_agent() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-agent");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("iu/chat-agent-sessions/sess-xyz");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("00000000000.xd"),
            b"prefix XdAgentSession suffix",
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_title.as_deref(), Some("chat-agent"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_xd_file_yields_zero_rows() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-missing");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("ic/chat-sessions/sess-empty");
        std::fs::create_dir_all(&session_dir).unwrap();
        // No 00000000000.xd written.
        assert!(parse_session_dir(&session_dir).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #757: post-migration JetBrains Copilot sessions skip the Xodus
    /// `.xd` log entirely and write only `copilot-chat-nitrite.db`. The
    /// parser used to bail (no `.xd` → return empty) and the JetBrains
    /// surface stayed at $0.00 forever. After the fix it reads the
    /// Nitrite store, recognizes the populated-entity marker (`NtTurn`
    /// or `NtChatSession`), and emits one assistant-role placeholder
    /// the same shape an Xodus-only session would have produced.
    #[test]
    fn nitrite_only_session_emits_one_row() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-only");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_id = "32REEyBFLmeFBR9TT7Luu0z1Rh8";
        let session_dir = tmp.join("ws/chat-sessions").join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        // Simulate Nitrite's MVStore header + a single Nitrite catalog
        // entry naming the populated-entity class. Real-world bytes
        // around the marker are MVStore page payload + Java
        // serialization; only the literal class-name suffix needs to
        // round-trip for the byte scan to fire.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(
            b"com.github.copilot.chat.session.persistence.nitrite.entity.NtChatSession",
        );
        bytes.extend_from_slice(&[0u8; 32]);
        bytes.extend_from_slice(
            b"com.github.copilot.chat.session.persistence.nitrite.entity.NtTurn",
        );
        std::fs::write(session_dir.join("copilot-chat-nitrite.db"), &bytes).unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1, "Nitrite session should emit one row");
        let m = &parsed[0];
        assert_eq!(m.role, "assistant");
        assert_eq!(m.provider, super::super::PROVIDER_ID);
        assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
        assert_eq!(m.session_id.as_deref(), Some(session_id));
        assert_eq!(m.session_title.as_deref(), Some("chat"));
        assert_eq!(m.input_tokens, 0);
        assert_eq!(m.output_tokens, 0);
        assert!(
            m.cost_cents.is_none(),
            "tokens come from billing API per ADR-0093 §5"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #757: a Nitrite store that carries *only* `NtSelectedModel` (the
    /// per-session model preference Nitrite writes the moment the user
    /// opens a chat pane, even before sending a message) must NOT emit
    /// a row — that mirrors the existing Xodus rule about
    /// `XdMigration`-only sessions. Without this, every freshly-opened
    /// chat tab would synthesize a fake assistant turn.
    #[test]
    fn nitrite_with_only_selected_model_emits_no_row() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-prefonly");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("ic/chat-sessions/sess-prefs-only");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(
            b"com.github.copilot.chat.session.persistence.nitrite.entity.NtSelectedModel",
        );
        std::fs::write(session_dir.join("copilot-chat-nitrite.db"), &bytes).unwrap();
        assert!(parse_session_dir(&session_dir).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #757: chat-agent sessions write `copilot-agent-sessions-nitrite.db`
    /// (different filename from `copilot-chat-nitrite.db`). The parser
    /// must look at both — otherwise post-migration agent sessions stay
    /// invisible the same way chat sessions did.
    #[test]
    fn nitrite_agent_session_emits_row_with_agent_title() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-agent");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("iu/chat-agent-sessions/sess-agent");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(
            b"com.github.copilot.chat.session.persistence.nitrite.entity.NtAgentTurn",
        );
        std::fs::write(
            session_dir.join("copilot-agent-sessions-nitrite.db"),
            &bytes,
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_title.as_deref(), Some("chat-agent"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #757: when both stores are present (real-world dual-store DBs
    /// during migration), the parser must still emit exactly one row —
    /// not two. The Xodus probe runs first; a populated `.xd` wins and
    /// supplies the timestamp.
    #[test]
    fn dual_store_session_emits_exactly_one_row() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-dual-store");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("ic/chat-sessions/sess-dual");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("00000000000.xd"),
            b"prefix XdChatSession suffix",
        )
        .unwrap();
        std::fs::write(
            session_dir.join("copilot-chat-nitrite.db"),
            b"H:2,blockSize:1000\nNtChatSession\nNtTurn",
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_session_dirs_finds_all_session_types_and_slugs() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-discover");
        let _ = std::fs::remove_dir_all(&tmp);
        for (slug, stype) in [
            ("ic", "chat-sessions"),
            ("iu", "chat-agent-sessions"),
            ("ws", "chat-edit-sessions"),
            ("iu", "bg-agent-sessions"),
        ] {
            std::fs::create_dir_all(tmp.join(slug).join(stype).join("sess-1")).unwrap();
        }
        // Noise that must be skipped per ADR-0093 §3.
        std::fs::create_dir_all(tmp.join("intellij")).unwrap();
        std::fs::write(tmp.join("apps.json"), b"{}").unwrap();
        std::fs::write(tmp.join("versions.json"), b"{}").unwrap();

        let dirs = discover_session_dirs(std::slice::from_ref(&tmp));
        assert_eq!(dirs.len(), 4, "expected four session dirs, got {dirs:?}");
        assert!(dirs.iter().all(|d| d.ends_with("sess-1")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_session_dirs_handles_missing_root() {
        let dirs = discover_session_dirs(&[PathBuf::from("/nonexistent/github-copilot-root")]);
        assert!(dirs.is_empty());
    }

    #[test]
    fn watch_roots_includes_session_type_dirs() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-watch");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("ic/chat-sessions")).unwrap();
        std::fs::create_dir_all(tmp.join("iu/chat-agent-sessions")).unwrap();
        std::fs::create_dir_all(tmp.join("intellij")).unwrap();

        let mut roots = Vec::new();
        for ide_dir in ide_slug_dirs(&tmp) {
            for session_type in SESSION_TYPE_DIRS {
                let p = ide_dir.join(session_type);
                if p.is_dir() {
                    roots.push(p);
                }
            }
        }
        roots.sort();
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|p| p.ends_with("ic/chat-sessions")));
        assert!(roots.iter().any(|p| p.ends_with("iu/chat-agent-sessions")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn deterministic_uuid_is_stable_and_namespaced() {
        let a = deterministic_uuid("sess-1", "/tmp/x");
        let b = deterministic_uuid("sess-1", "/tmp/x");
        assert_eq!(a, b);
        let c = deterministic_uuid("sess-2", "/tmp/x");
        assert_ne!(a, c);
        // Distinct namespace prefix means we never collide with the
        // VS Code-side `deterministic_uuid` in the parent module.
        let vscode_side = super::super::deterministic_uuid("sess-1", "/tmp/x", 0);
        assert_ne!(a, vscode_side);
    }

    #[test]
    fn byte_contains_basic() {
        assert!(byte_contains(b"hello world", b"world"));
        assert!(!byte_contains(b"hello", b"world"));
        assert!(!byte_contains(b"hi", b"hello"));
        assert!(!byte_contains(b"x", b""));
    }
}
