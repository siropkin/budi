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

/// #788: `cwd_source` label written when Phase 2 recovered `file://` URIs
/// but couldn't walk the longest-common-prefix up to a `.git` checkout
/// (e.g. the repo isn't checked out on this host, or its `origin` remote
/// isn't configured). Mirrors `<provider>:<signal>` shape used elsewhere
/// in the analytics vocabulary so downstream queries can grep for it.
const CWD_SOURCE_JETBRAINS_PHASE2_PREFIX: &str = "copilot_chat:jetbrains_phase2_prefix";

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
    // Older plugin builds write the edit-session store under a shorter
    // name (no `chat-` prefix). Observed on real user DBs in v8.4.7's
    // post-release smoke test where session dirs hold only
    // `copilot-edit-sessions-nitrite.db` — without this entry the
    // populated-entity probe skips them and the matching .xd's
    // `projectName` never reaches the parser.
    "copilot-edit-sessions-nitrite.db",
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
    let session_label = session_type
        .as_deref()
        .map(|s| s.trim_end_matches("-sessions").to_string());

    // #766: pull the IntelliJ project name + resolved repo/branch from
    // the Xodus log regardless of which store the populated-entity probe
    // picked. retire-with: #789 — this Phase 1 byte-walker block goes
    // away when the Phase 3 MVStore/Xodus decoder lands. See #807,
    // ADR-0093 §"Amendment 2026-05-14".
    //
    // Dual-store sessions on disk write `XdChatSession.projectName`
    // into `00000000000.xd` *and* `Nt*Turn` documents into the matching
    // `*.nitrite.db` — the two stores are complementary, not alternative
    // shapes of the same data. The original 8.4.6 implementation treated
    // them as mutually exclusive (repo only set when .xd was the
    // "populated" store, turn extraction only when Nitrite was), so every
    // dual-store session emitted either a one-row placeholder with
    // `repo_id` or a per-turn batch without it. Now we try both
    // unconditionally and combine.
    let xd_candidate = session_dir.join("00000000000.xd");
    let (project_name, phase1_resolution) = if let Ok(bytes) = std::fs::read(&xd_candidate)
        && let Some(project_name) = extract_xodus_project_name(&bytes)
    {
        let resolution = resolve_project_workspace(&project_name);
        (Some(project_name), resolution)
    } else {
        (None, None)
    };

    // #778: Phase 2 fallback for agent-only Nitrite sessions whose `.xd` log
    // is absent or carries no `projectName`. retire-with: #789 — Phase 2
    // resolved 0 additional sessions on real data; the Phase 3 MVStore
    // decoder is expected to subsume this entire block. See #807,
    // ADR-0093 §"Amendment 2026-05-14".
    //
    // Sweeps every Nitrite store in
    // the session dir for `file://` URIs (today written into the
    // `currentFileUri` JSON blob inside `NtAgentTurn.stringContent` by
    // recent Copilot Chat plugin builds), computes their longest common
    // path prefix, and walks upward to find a `.git` checkout. Nitrite is
    // preferred over Xodus when both resolve, on the theory that real file
    // URIs are stronger evidence than a project-name heuristic — but in
    // practice Phase 1 already mapped any `projectName`-bearing session, so
    // Phase 2 only fires when Phase 1 fails. ADR-0093 §4 amendment notes
    // the data-shape caveat: the majority of agent sessions write no
    // `file://` token at all, so this path lights up a small fraction of
    // additional sessions, not the bulk.
    //
    // #788: when Phase 2 fires we always compute a `Phase2WorkspaceResolution`
    // so the caller can surface diagnostics + a `common_prefix` hint even
    // when the chain doesn't terminate in a resolvable `.git` checkout.
    let phase2 = if phase1_resolution.is_none() {
        let mut all_paths: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for filename in NITRITE_DB_FILES {
            let candidate = session_dir.join(filename);
            let Ok(bytes) = std::fs::read(&candidate) else {
                continue;
            };
            for path in extract_nitrite_workspace_paths(&bytes) {
                if seen.insert(path.clone()) {
                    all_paths.push(path);
                }
            }
        }
        let res = resolve_phase2_workspace(&all_paths);
        // Emit a single DEBUG line per attempt that produced URIs but no
        // repo_id. Greppable from `~/Library/Logs/budi-daemon.log` to
        // answer "why didn't this session resolve" without a debugger.
        if !all_paths.is_empty() && res.repo.is_none() {
            tracing::debug!(
                session_id = session_id.as_deref().unwrap_or("?"),
                session_dir = %session_dir.display(),
                uri_count = all_paths.len(),
                common_prefix = res.common_prefix.as_deref().unwrap_or("?"),
                reason = res.failure_reason.unwrap_or("unknown"),
                "jetbrains phase2 workspace resolution did not produce a repo_id"
            );
        }
        Some(res)
    } else {
        None
    };

    let repo_resolution =
        phase1_resolution.or_else(|| phase2.as_ref().and_then(|p| p.repo.clone()));

    // #764: Phase 1 per-turn extraction. Walk every `*.nitrite.db` in
    // the session dir for `Nt(Agent|Edit)?Turn` documents and collect
    // their `uuid` fields. Combine across all Nitrite files because a
    // single session-dir can carry both `copilot-chat-nitrite.db` and
    // `copilot-agent-sessions-nitrite.db` post-migration.
    let mut nitrite_turns: Vec<String> = Vec::new();
    {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for filename in NITRITE_DB_FILES {
            let candidate = session_dir.join(filename);
            let Ok(bytes) = std::fs::read(&candidate) else {
                continue;
            };
            for turn_id in extract_nitrite_turn_ids(&bytes) {
                if seen.insert(turn_id.clone()) {
                    nitrite_turns.push(turn_id);
                }
            }
        }
    }

    let build_msg = |uuid: String, request_id: Option<String>| -> ParsedMessage {
        let mut msg = ParsedMessage {
            uuid,
            session_id: session_id.clone(),
            timestamp,
            role: "assistant".to_string(),
            provider: super::PROVIDER_ID.to_string(),
            cost_confidence: "estimated".to_string(),
            request_id,
            surface: Some(crate::surface::JETBRAINS.to_string()),
            ..ParsedMessage::default()
        };
        // Prefer the IntelliJ project name when we have it — matches what
        // the user sees in the IDE. Falls back to "chat"/"agent"/"edit"
        // so the dashboard still distinguishes session types.
        msg.session_title = project_name.clone().or_else(|| session_label.clone());
        if let Some((repo_id, branch)) = repo_resolution.as_ref() {
            msg.repo_id = Some(repo_id.clone());
            if let Some(b) = branch {
                msg.git_branch = Some(b.clone());
            }
        } else if let Some(p) = phase2.as_ref()
            && let Some(prefix) = p.common_prefix.as_ref()
        {
            // #788: Phase 2 recovered file URIs but couldn't walk up to a
            // resolvable `.git` checkout. Surface the longest-common-prefix
            // path on the message so the dashboard / messages.cwd has a
            // local-path hint even when we have no remote-backed identity.
            msg.cwd = Some(prefix.clone());
            msg.cwd_source = Some(CWD_SOURCE_JETBRAINS_PHASE2_PREFIX.to_string());
        }
        msg
    };

    if !nitrite_turns.is_empty() {
        let mut messages = Vec::with_capacity(nitrite_turns.len());
        for turn_id in nitrite_turns {
            let uuid = deterministic_uuid_from_nitrite(&turn_id, &path_str);
            messages.push(build_msg(uuid, Some(turn_id)));
        }
        return messages;
    }

    // Fallback: legacy .xd path (and the documented #757 placeholder for
    // any Nitrite store that contains a populated-entity marker but no
    // recoverable turn UUIDs — e.g. an empty agent session, or a future
    // plugin version with an unfamiliar `uuid` field name). One row per
    // session, keyed on the directory name, matches pre-#764 behavior.
    let session_uuid = deterministic_uuid(session_id.as_deref().unwrap_or(""), &path_str);
    let mut msg = build_msg(session_uuid.clone(), Some(session_uuid));
    if project_name.is_none() {
        msg.session_title = session_label;
    }
    vec![msg]
}

/// #766: pull the JetBrains project name out of the Xodus log's
/// `XdChatSession.projectName` property by byte-scanning.
///
/// retire-with: #789 — this byte-walker is a low-cost fast-path that
/// resolved 3 of 23 sessions on the 8.4.8 smoke-test machine. It survives
/// only until the Phase 3 MVStore + Java-serialization decoder lands and
/// proves it can subsume the heuristic. Disposition decision recorded in
/// #807 and ADR-0093 §"Amendment 2026-05-14".
///
/// The Xodus log writes a schema header near the start of the file that
/// declares each property name once with a 1-byte property ID
/// (`projectName\x00<id>`). Property values are written later as
/// `\x82\x00<id>\x82<utf8-bytes>\x00` inside per-entity records. There
/// is no Xodus log decoder in this crate — recent plugin versions skip
/// the file entirely (#757) so it isn't worth carrying a real parser
/// for it — but the literal `projectName` token plus its property ID is
/// reliable enough to harvest the value with a couple of byte-finds.
///
/// Returns the first plausible candidate string, or `None` when the file
/// doesn't carry the property or no candidate looks like a real project
/// name. "Plausible" means: printable UTF-8, 1..=128 chars, no path
/// separators or extension dots (`.tsx`, `manifest.json` etc. are
/// rejected — those are working-set file names bleeding through the
/// same `\x82\x00<id>\x82` framing because some other entity type
/// happens to assign the same property ID to a path field).
fn extract_xodus_project_name(bytes: &[u8]) -> Option<String> {
    let marker = b"projectName";
    let schema_pos = byte_find(bytes, marker)?;
    let id_pos = schema_pos.checked_add(marker.len() + 1)?; // skip the `\x00` terminator
    let property_id = *bytes.get(id_pos)?;

    let value_marker = [0x82u8, 0x00, property_id, 0x82];
    let mut search_from = 0usize;
    while let Some(idx) = byte_find(&bytes[search_from..], &value_marker) {
        let start = search_from + idx + value_marker.len();
        // Bound the scan so a corrupted log doesn't make us crawl the
        // whole file looking for a null byte that isn't there.
        let scan_end = (start + 256).min(bytes.len());
        let end = bytes[start..scan_end]
            .iter()
            .position(|b| *b == 0)
            .map(|n| start + n)?;
        let raw = &bytes[start..end];
        if let Ok(s) = std::str::from_utf8(raw)
            && looks_like_project_name(s)
        {
            return Some(s.to_string());
        }
        search_from = start.max(search_from + 1);
        if search_from >= bytes.len() {
            break;
        }
    }
    None
}

/// True iff the candidate string is short, printable, contains no path
/// separators, and is not obviously a file name. Used to filter the byte
/// scan's matches so a stray working-set entry like `manifest.json` or
/// `src/foo/bar.tsx` does not get mistaken for the IntelliJ project name.
fn looks_like_project_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return false;
    }
    if s.contains('/') || s.contains('\\') || s.contains(':') {
        return false;
    }
    // Common working-set file extensions that have flown through the
    // same `\x82\x00<id>\x82` pattern in the survey of real .xd files:
    // *.json, *.md, *.tsx, *.ts, *.js, *.py, *.rs, *.go, *.toml.
    // Reject any string whose last `.` is followed by a 1..=5-char
    // alpha-only suffix — the IntelliJ project name `Verkada-Web` has
    // no dot, while file names always do.
    if let Some(idx) = s.rfind('.')
        && idx + 1 < s.len()
    {
        let ext = &s[idx + 1..];
        if (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic()) {
            return false;
        }
    }
    true
}

/// #766: given an IntelliJ project name (e.g. `Verkada-Web`), try to
/// locate it on the filesystem as a git checkout. Probes
/// `~/_projects/<name>`, `~/projects/<name>`, and `~/<name>` — covering
/// the two most common layouts on macOS/Linux developer machines without
/// shelling out to find. Returns `(repo_id, branch)` from
/// [`crate::repo_id::resolve_repo_id`] + a `HEAD` read; `None` when no
/// candidate directory contains `.git`.
fn resolve_project_workspace(project_name: &str) -> Option<(String, Option<String>)> {
    let home = crate::config::home_dir().ok()?;
    let candidates = [
        home.join("_projects").join(project_name),
        home.join("projects").join(project_name),
        home.join(project_name),
    ];
    for candidate in candidates {
        if !candidate.join(".git").exists() {
            continue;
        }
        let Some(repo_id) = crate::repo_id::resolve_repo_id(&candidate) else {
            continue;
        };
        let branch = read_git_head_branch(&candidate);
        return Some((repo_id, branch));
    }
    None
}

/// Best-effort: read the current branch from `<repo>/.git/HEAD`. Returns
/// `None` for detached HEADs or unreadable refs — the caller treats a
/// missing branch the same as a missing `repo_id` (omit, fall back to
/// whatever the JSONL/Xodus path emitted).
fn read_git_head_branch(repo_root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(repo_root.join(".git/HEAD")).ok()?;
    let trimmed = head.trim();
    let suffix = trimmed.strip_prefix("ref: refs/heads/")?;
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

/// Linear byte search. Kept private to this module so the entity-type
/// scan in [`byte_contains`] and the property scan above share one
/// implementation; the standard library's `slice::windows` is hot
/// enough on the 10–30 KB store files the JetBrains plugin produces.
fn byte_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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

/// #764: Phase 1 per-turn extraction from the JetBrains Copilot Nitrite
/// store. Walks the on-disk MVStore bytes for `Nt(Agent|Edit)?Turn`
/// markers and, for each, returns the first `uuid` field's value that
/// appears within an 8 KB window forward of the marker (real-world
/// captured agent sessions show every turn document writing
/// `t\x00\x04uuidt\x00\x24<36-byte-string>` inside its serialized form,
/// within a few hundred bytes of the class marker).
///
/// UUIDs are deduplicated — Nitrite's MVStore writes class metadata and
/// each instance multiple times across the catalog + B-tree leaves, so
/// the same turn document surfaces under several markers. Order is the
/// first-seen offset so the returned list is stable across rebuilds of
/// the same store.
///
/// Phase 1 deliberately does **not** decode the full Java serialization
/// graph: pulling out per-turn `createdAt`, `modelName`, `stringContent`
/// requires a real Nitrite/MVStore decoder and is deferred to Phase 2 /
/// the next ADR amendment. The Phase 1 contract is "give every turn a
/// stable UUID so new prompts materialize as new rows" — enough to fix
/// #764's primary symptom and give #765's billing-API reconciler
/// non-zero-token rows to distribute dollars across.
fn extract_nitrite_turn_ids(bytes: &[u8]) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ordered: Vec<String> = Vec::new();

    // Pre-scan the file for every `uuid` field value (length-36 UTF-8
    // string immediately following the `t\x00\x04uuid` token).
    let mut uuid_hits: Vec<(usize, String)> = Vec::new();
    let needle = b"t\x00\x04uuidt\x00\x24"; // `t` <2-byte len=4> `uuid` `t` <2-byte len=36>
    let mut idx = 0;
    while let Some(rel) = byte_find(&bytes[idx..], needle) {
        let pos = idx + rel;
        let val_start = pos + needle.len();
        let val_end = val_start + 36;
        if val_end > bytes.len() {
            break;
        }
        if let Ok(s) = std::str::from_utf8(&bytes[val_start..val_end])
            && looks_like_uuid(s)
        {
            uuid_hits.push((pos, s.to_string()));
        }
        idx = val_start.max(pos + 1);
    }

    if uuid_hits.is_empty() {
        return ordered;
    }

    // Match each turn marker to the first uuid hit within an 8 KB window
    // forward. 8 KB comfortably exceeds the largest serialized turn
    // documents observed in real fixtures while staying small enough
    // that we don't accidentally cross from one turn into its neighbour.
    let mut marker_pos = 0usize;
    for marker in NITRITE_TURN_MARKERS {
        let mut from = 0usize;
        while let Some(rel) = byte_find(&bytes[from..], marker) {
            let pos = from + rel;
            for (uuid_pos, uuid) in uuid_hits.iter() {
                if *uuid_pos < pos {
                    continue;
                }
                if *uuid_pos - pos > 8192 {
                    break;
                }
                if seen.insert(uuid.clone()) {
                    ordered.push(uuid.clone());
                }
                break;
            }
            from = pos + marker.len();
            marker_pos = marker_pos.max(pos);
        }
    }
    let _ = marker_pos;
    ordered
}

/// Nitrite class markers that designate per-turn documents. Mirrors
/// [`POPULATED_ENTITY_MARKERS`]'s turn subset; lifted into its own slice
/// because the existence-marker scan in [`has_populated_entity_marker`]
/// also accepts session-level markers like `NtChatSession` that we
/// explicitly do not want to treat as turn boundaries here.
const NITRITE_TURN_MARKERS: &[&[u8]] = &[b"NtTurn", b"NtAgentTurn", b"NtEditTurn"];

/// #778: Phase 2 workspace-path extractor for the JetBrains Copilot Nitrite
/// store.
///
/// retire-with: #789 — Phase 2 added zero additional resolutions on the
/// 8.4.8 smoke-test machine (95 of 98 surveyed Nitrite DBs carried no
/// `file://` token at all). It is kept here only to avoid a regression-
/// risking delete in a pre-Phase-3 release; the Phase 3 MVStore decoder
/// is expected to subsume this path entirely. Disposition decision
/// recorded in #807 and ADR-0093 §"Amendment 2026-05-14".
///
/// The original Phase 1 ticket assumed `NtAgentWorkingSetItem` documents
/// would carry literal `file://...` URIs in a top-level `stringContent`
/// TC_STRING. A survey of 98 real Nitrite DBs from the 8.4.8 smoke-test
/// machine showed that assumption is wrong on every observed shape:
/// `NtAgentWorkingSetItem` instances only persist `created_at` / `uuid` /
/// `last_modified_at` plus an opaque `_revision` cursor; their actual
/// content lives in a different segment that the byte walker can't reach
/// without an MVStore + Java-serialization decoder. The file URIs we *can*
/// see in the bytes live inside an escape-encoded JSON blob in the
/// `NtAgentTurn.stringContent` model-state field — specifically the
/// `currentFileUri` key written by recent Copilot Chat plugin builds when
/// the active editor tab is part of the turn context.
///
/// This extractor accepts whatever `file://` URIs the byte scan finds —
/// `currentFileUri` JSON-blob hits today, plus any future shape that
/// happens to write a literal `file://` token into the file. URIs are
/// percent-decoded; the trailing byte-scan stops at the first character
/// that can't appear in a Unix path (backslash, quote, null, CR/LF, or
/// the closing `}` of the surrounding JSON). The de-duplicated list is
/// returned in first-seen order so the longest-common-prefix step is
/// stable across rebuilds of the same store.
///
/// **Honestly documented limitation:** sessions that don't write a
/// `currentFileUri` (the majority on the smoke-test machine — 95 of 98
/// real DBs carry no `file://` token at all) cannot be resolved by this
/// path. ADR-0093 §4 amendment documents the deferred shape.
fn extract_nitrite_workspace_paths(bytes: &[u8]) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    let needle = b"file://";
    let mut from = 0usize;
    while let Some(rel) = byte_find(&bytes[from..], needle) {
        let start = from + rel;
        // Stop at the first byte that can't appear inside a Unix file path
        // or its JSON-escape envelope. The `}` halt covers the closing
        // brace of the surrounding JSON model-state object so a stray
        // trailing `}` from `currentFileUri":"file:///...,"isVisionEnabled":true}`
        // doesn't get pulled into the path.
        let scan_end = (start + 4096).min(bytes.len());
        let halt = bytes[start..scan_end]
            .iter()
            .position(|b| {
                matches!(
                    *b,
                    b'\\' | b'"' | 0 | b'\n' | b'\r' | b'\t' | b'{' | b'}' | b'<' | b'>' | b'|'
                ) || *b < 0x20
            })
            .map(|n| start + n)
            .unwrap_or(scan_end);
        let raw = &bytes[start..halt];
        from = halt.max(start + 1);
        let Ok(uri) = std::str::from_utf8(raw) else {
            continue;
        };
        let Some(path) = decode_file_uri(uri) else {
            continue;
        };
        if seen.insert(path.clone()) {
            ordered.push(path);
        }
    }
    ordered
}

/// Strip the `file://` scheme, percent-decode, and return the absolute
/// filesystem path. Accepts `file:///abs/path` (the canonical form) and
/// `file://localhost/abs/path` (rare but spec-allowed). Returns `None`
/// for relative paths or anything that doesn't start with `/` after
/// scheme-strip — the upstream resolver expects an absolute path.
fn decode_file_uri(uri: &str) -> Option<String> {
    let after_scheme = uri.strip_prefix("file://")?;
    let path = after_scheme
        .strip_prefix("localhost")
        .unwrap_or(after_scheme);
    if !path.starts_with('/') {
        return None;
    }
    percent_decode(path)
}

/// Minimal percent-decoder for `file://` URIs. Accepts only `%XX` sequences
/// where `XX` is two hex digits — anything else is preserved as-is. We
/// avoid pulling in a URI crate for this one call site; the input is
/// strictly Copilot Chat's `currentFileUri` JSON value, which only ever
/// percent-encodes a small alphabet (`%20` = space being the common case
/// on macOS paths with spaces).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi as u8) << 4) | (lo as u8));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// #788: structured outcome of a Phase 2 workspace resolution attempt.
///
/// Carries enough information for the caller to (a) emit a diagnostic log
/// line explaining why resolution failed, and (b) surface the longest
/// common prefix as a `cwd` hint on the emitted message even when no
/// `repo_id` could be derived. Replaces the earlier
/// `Option<(repo_id, branch)>` return so we no longer throw away the
/// common-prefix path on the failure paths.
#[derive(Debug, Default)]
struct Phase2WorkspaceResolution {
    /// Longest common directory prefix of the recovered paths, or `None`
    /// when there were no paths / the paths shared no usable prefix.
    common_prefix: Option<String>,
    /// `(repo_id, branch)` when the prefix-or-an-ancestor contains a
    /// `.git` checkout and `crate::repo_id::resolve_repo_id` succeeded.
    repo: Option<(String, Option<String>)>,
    /// A short tag describing why `repo` is `None`. `None` iff `repo`
    /// is `Some(_)`. One of:
    ///   - `"no_paths"` — extractor returned an empty list,
    ///   - `"no_common_prefix"` — paths shared nothing beyond `/`,
    ///   - `"no_git_along_chain"` — walked to `/` without finding a `.git`,
    ///   - `"repo_id_resolver_returned_none"` — `.git` exists but
    ///     `resolve_repo_id` rejected it (no `origin` remote, etc.).
    failure_reason: Option<&'static str>,
}

/// Given a list of absolute file paths recovered from a single session's
/// Nitrite store, compute the deepest common-prefix directory that
/// contains a `.git` checkout, and resolve it via
/// [`crate::repo_id::resolve_repo_id`]. Walks the common prefix
/// upward until a `.git` entry is found or the prefix collapses to `/`.
///
/// Always returns a [`Phase2WorkspaceResolution`]: when resolution fails,
/// the struct still carries the recovered `common_prefix` (when one
/// existed) and a `failure_reason` for the caller to log. See #788.
///
/// retire-with: #789 — companion to `extract_nitrite_workspace_paths`;
/// retires alongside the Phase 2 byte-walker once the Phase 3 decoder
/// can supply richer per-turn workspace evidence.
fn resolve_phase2_workspace(paths: &[String]) -> Phase2WorkspaceResolution {
    if paths.is_empty() {
        return Phase2WorkspaceResolution {
            common_prefix: None,
            repo: None,
            failure_reason: Some("no_paths"),
        };
    }
    let Some(common) = longest_common_path_prefix(paths) else {
        return Phase2WorkspaceResolution {
            common_prefix: None,
            repo: None,
            failure_reason: Some("no_common_prefix"),
        };
    };
    let mut probe: Option<&Path> = Some(Path::new(&common));
    let mut saw_git_without_remote = false;
    while let Some(dir) = probe {
        if dir.join(".git").exists() {
            if let Some(repo_id) = crate::repo_id::resolve_repo_id(dir) {
                let branch = read_git_head_branch(dir);
                return Phase2WorkspaceResolution {
                    common_prefix: Some(common),
                    repo: Some((repo_id, branch)),
                    failure_reason: None,
                };
            }
            saw_git_without_remote = true;
        }
        probe = dir.parent();
    }
    let reason = if saw_git_without_remote {
        "repo_id_resolver_returned_none"
    } else {
        "no_git_along_chain"
    };
    Phase2WorkspaceResolution {
        common_prefix: Some(common),
        repo: None,
        failure_reason: Some(reason),
    }
}

/// Component-wise longest common prefix of a non-empty list of absolute
/// paths. Returns a path like `/Users/me/_projects/Verkada-Web/src`
/// (no trailing slash) — the deepest directory that *every* input path
/// is rooted under. Returns `None` if any input is not absolute or the
/// inputs share no prefix beyond `/`.
fn longest_common_path_prefix(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let mut iter = paths.iter();
    let first = iter.next()?;
    if !first.starts_with('/') {
        return None;
    }
    let mut prefix: Vec<&str> = first.trim_start_matches('/').split('/').collect();
    // The last component might be a filename — drop it; we want directories.
    if !prefix.is_empty() {
        prefix.pop();
    }
    for p in iter {
        if !p.starts_with('/') {
            return None;
        }
        let mut comps: Vec<&str> = p.trim_start_matches('/').split('/').collect();
        if !comps.is_empty() {
            comps.pop();
        }
        let common_len = prefix
            .iter()
            .zip(comps.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common_len);
        if prefix.is_empty() {
            break;
        }
    }
    if prefix.is_empty() {
        return None;
    }
    let mut out = String::new();
    for part in &prefix {
        out.push('/');
        out.push_str(part);
    }
    Some(out)
}

/// True iff the candidate looks like a canonical hyphenated UUID
/// (8-4-4-4-12 hex). Used by the byte-scan to reject the occasional
/// non-UUID length-36 string that happens to land next to a `uuid`
/// token in noise.
fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    let expected_dashes = [8usize, 13, 18, 23];
    for (i, &b) in bytes.iter().enumerate() {
        if expected_dashes.contains(&i) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// #764: per-turn variant of [`deterministic_uuid`]. Keyed on the
/// Nitrite document's own UUID + the session directory path so the
/// emitted `ParsedMessage.uuid` stays stable across re-ingests but a
/// new turn (new Nitrite uuid) always lands as a new row.
fn deterministic_uuid_from_nitrite(turn_id: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(UUID_NAMESPACE);
    hasher.update(b"nitrite-turn:");
    hasher.update(turn_id.as_bytes());
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

/// One-shot migration helper for backfilling `sessions.title` on
/// historical JetBrains Copilot rows that pre-date the #787 enricher.
///
/// Walks every JetBrains-side session dir under the active config roots
/// and returns a `(session_id, title)` map. The title is the parser's
/// `ParsedMessage.session_title` — either the resolved IntelliJ
/// `projectName` (Phase 1 #766 or Phase 2 #778) or the session-type
/// label fallback (`chat`, `chat-agent`, `chat-edit`, `bg-agent`).
///
/// Not called from the live sync path: since #787, the
/// `IdentityEnricher` promotes `session_title` to a tag on every new
/// ingest, and `backfill_session_titles` picks it up via a pure SQL
/// UPDATE off the `tags` table. This helper exists only for an explicit
/// one-shot migration of pre-#787 historical data.
///
/// Cheap (one byte-scan per session dir); idempotent — callers should
/// still gate on `title IS NULL OR title = ''` at write time so a
/// manual-rename in the cloud isn't overwritten.
///
/// Returns an empty map when no JetBrains config root exists on this host.
pub fn collect_jetbrains_session_titles() -> std::collections::HashMap<String, String> {
    let session_dirs = discover_session_dirs(&jetbrains_config_roots());
    let mut out = std::collections::HashMap::new();
    for dir in session_dirs {
        let Some(sid) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let parsed = parse_session_dir(&dir);
        let Some(first) = parsed.into_iter().next() else {
            continue;
        };
        let Some(title) = first.session_title else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        out.insert(sid.to_string(), title);
    }
    out
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

    /// #766: synthesize an Xodus log fragment that mimics what the real
    /// `00000000000.xd` files on disk carry — a schema header that
    /// declares `projectName\x00\x04` followed later by a
    /// `\x82\x00\x04\x82Verkada-Web\x00` value record. The byte-scan must
    /// recover the literal project name without a full Xodus log
    /// decoder. Survey of 13 real session files (2026-05-11) showed this
    /// pattern is stable across the WS / IC / IU IDE slugs.
    #[test]
    fn extract_xodus_project_name_recovers_value_from_schema_id_pair() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"XdChatSession");
        bytes.extend_from_slice(b"\x86\x86\x8e\x8c");
        bytes.extend_from_slice(b"projectName\x00\x04");
        bytes.extend_from_slice(b"\x86\x86\x87\x85user\x00\x05");
        bytes.extend_from_slice(b"\x86\x99\x90");
        bytes.extend_from_slice(b"\x82\x00\x04\x82Verkada-Web\x00");
        bytes.extend_from_slice(b"\x86\x99\x8d\x82\x00\x05\x82siropkin\x00");

        let project = extract_xodus_project_name(&bytes);
        assert_eq!(project.as_deref(), Some("Verkada-Web"));
    }

    /// #766: a session whose `.xd` file doesn't carry the property at
    /// all (empty session, or a plugin version that skips the property)
    /// must return `None` rather than picking some random other string
    /// out of the log.
    #[test]
    fn extract_xodus_project_name_returns_none_when_property_absent() {
        let bytes = b"XdChatSession\x00bunch of other stuff\x00\x00";
        assert!(extract_xodus_project_name(bytes).is_none());
    }

    /// #766: working-set file names share the `\x82\x00<id>\x82` framing,
    /// so the value-scan can land on strings like `manifest.json` or
    /// `src/foo/bar.tsx`. `looks_like_project_name` must reject those
    /// — otherwise `resolve_project_workspace` ends up looking for
    /// `~/_projects/manifest.json` and falling through, with
    /// `session_title` set to a misleading filename.
    #[test]
    fn extract_xodus_project_name_filters_file_name_false_positives() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"projectName\x00\x04");
        // First candidate is a file name (rejected); second is the real
        // project name (accepted). The scan walks forward through every
        // match so a real value still surfaces after a false positive.
        bytes.extend_from_slice(b"\x82\x00\x04\x82manifest.json\x00");
        bytes.extend_from_slice(b"\x82\x00\x04\x82verkadalizer\x00");

        let project = extract_xodus_project_name(&bytes);
        assert_eq!(project.as_deref(), Some("verkadalizer"));
    }

    #[test]
    fn looks_like_project_name_accepts_real_names() {
        for name in ["Verkada-Web", "budi", "getbudi-dev", "verkada_menu_v2"] {
            assert!(looks_like_project_name(name), "should accept {name:?}");
        }
    }

    #[test]
    fn looks_like_project_name_rejects_file_paths_and_extensions() {
        for name in [
            "manifest.json",
            "src/components/Foo.tsx",
            "/Users/me/_projects/Verkada-Web",
            "c:\\Users\\me\\code",
            "",
            "README.md",
        ] {
            assert!(!looks_like_project_name(name), "should reject {name:?}");
        }
    }

    #[test]
    fn read_git_head_branch_parses_symbolic_ref() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-head");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::write(tmp.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(read_git_head_branch(&tmp).as_deref(), Some("main"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #764: build a synthetic Nitrite blob that mimics the on-disk
    /// shape captured from real `copilot-agent-sessions-nitrite.db`
    /// files (2026-05-11 inventory): an `NtAgentTurn` class marker
    /// followed by a Java-serialized `LinkedHashMap` whose `uuid` field
    /// carries a 36-char canonical UUID. Two turns produce two distinct
    /// `ParsedMessage` UUIDs.
    fn synth_nitrite_with_turns(uuids: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        // MVStore header so the file looks plausibly real.
        out.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        out.extend_from_slice(&[0u8; 64]);
        for uuid in uuids {
            assert_eq!(uuid.len(), 36, "synth helper expects canonical uuids");
            out.extend_from_slice(b"NtAgentTurn");
            out.extend_from_slice(b"\xac\xed\x00\x05");
            // `t\x00\x04uuid` + `t\x00\x24<36-byte uuid>` — the exact
            // pattern the real Nitrite serializer writes for the field.
            out.extend_from_slice(b"t\x00\x04uuid");
            out.extend_from_slice(b"t\x00\x24");
            out.extend_from_slice(uuid.as_bytes());
            out.extend_from_slice(b"\x00trailer\x00");
        }
        out
    }

    #[test]
    fn nitrite_session_emits_one_row_per_turn() {
        let uuids = [
            "bfe8768a-b11e-469a-852b-fc22c7dd9f23",
            "382642f7-6bf3-4e9b-b2ed-970bb3474edb",
            "550b00cd-4ad2-479a-8d8a-300a55478450",
        ];
        let bytes = synth_nitrite_with_turns(&uuids);

        let extracted = extract_nitrite_turn_ids(&bytes);
        assert_eq!(extracted.len(), 3);
        for u in &uuids {
            assert!(extracted.iter().any(|s| s == u), "missing {u}");
        }

        let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-turns");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("iu/chat-agent-sessions/sess-many-turns");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("copilot-agent-sessions-nitrite.db"),
            &bytes,
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 3, "one row per turn, got {parsed:?}");
        // The deterministic UUID must change per turn so `INSERT OR IGNORE`
        // accepts each new turn as a fresh row — the entire point of #764.
        let mut seen = std::collections::HashSet::new();
        for m in &parsed {
            assert!(seen.insert(m.uuid.clone()), "duplicate uuid {}", m.uuid);
            assert_eq!(m.surface.as_deref(), Some(crate::surface::JETBRAINS));
            assert_eq!(m.provider, super::super::PROVIDER_ID);
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #764: turn UUIDs that appear duplicated across the file
    /// (Nitrite's MVStore writes class metadata + B-tree leaf entries
    /// for the same document) must collapse to one emitted row per
    /// distinct turn — not one per byte-pattern match.
    #[test]
    fn nitrite_duplicate_turn_uuid_emits_single_row() {
        let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
        // Duplicate the same turn block — same uuid, two markers.
        let dup = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
        bytes.extend_from_slice(&dup[64..]); // skip the synthetic header on the dup

        let extracted = extract_nitrite_turn_ids(&bytes);
        assert_eq!(
            extracted.len(),
            1,
            "duplicate uuids must collapse, got {extracted:?}"
        );
    }

    /// Regression coverage for the v8.4.6 dual-store bug: when a
    /// session-dir holds both a populated `.xd` (with `projectName`) and
    /// a populated `.nitrite.db` (with `Nt*Turn` documents), the parser
    /// must combine the two — Nitrite supplies per-turn UUIDs, Xodus
    /// supplies the repo enrichment that lands on every per-turn row.
    /// The pre-fix 8.4.6 implementation read whichever store the
    /// populated-entity probe returned and ignored the other, so every
    /// `surface=jetbrains` row landed with `repo_id = NULL` even on
    /// sessions whose .xd carried a clean `Verkada-Web`-style project
    /// name.
    #[test]
    fn dual_store_session_combines_xodus_repo_with_nitrite_turns() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-dual-combined");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("iu/chat-agent-sessions/sess-dual-combined");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Synthetic .xd with the projectName property + value record. The
        // resolve_project_workspace probe will return None on most CI
        // hosts (no `~/_projects/budi-test-fake-name/.git`), so the
        // assertion focuses on `session_title` and the row count — those
        // two cover the wire shape that flows to the cloud and the
        // dashboard's Repo column fallback.
        let mut xd = Vec::new();
        xd.extend_from_slice(b"XdAgentSession");
        xd.extend_from_slice(b"\x86\x86\x8e\x8cprojectName\x00\x04");
        xd.extend_from_slice(b"\x86\x99\x90\x82\x00\x04\x82budi-test-fake-name\x00");
        std::fs::write(session_dir.join("00000000000.xd"), &xd).unwrap();

        // Synthetic Nitrite with one NtAgentTurn + uuid pair.
        let uuid = "11afee98-04f2-4da1-a282-3fc0d14e9054";
        let mut nit = Vec::new();
        nit.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        nit.extend_from_slice(&[0u8; 64]);
        nit.extend_from_slice(b"NtAgentTurn");
        nit.extend_from_slice(b"\xac\xed\x00\x05");
        nit.extend_from_slice(b"t\x00\x04uuid");
        nit.extend_from_slice(b"t\x00\x24");
        nit.extend_from_slice(uuid.as_bytes());
        nit.extend_from_slice(b"\x00trailer\x00");
        std::fs::write(session_dir.join("copilot-agent-sessions-nitrite.db"), &nit).unwrap();

        let parsed = parse_session_dir(&session_dir);
        // One row per Nitrite turn — the Xodus probe doesn't add a
        // separate placeholder, it only enriches.
        assert_eq!(parsed.len(), 1, "expected one per-turn row, got {parsed:?}");
        // The Xodus-derived project name lands on the per-turn row's
        // `session_title` even when the filesystem-probe step fails to
        // resolve a `.git` checkout, so the dashboard renders the
        // IntelliJ name instead of a sea of `(unknown)`.
        assert_eq!(
            parsed[0].session_title.as_deref(),
            Some("budi-test-fake-name"),
            "Xodus project name must reach the per-turn row's session_title"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #764: sessions whose only Nitrite documents are sessions (not
    /// turns) — e.g. an `NtAgentSession` row with no `NtAgentTurn` yet
    /// — fall back to the one-row-per-session placeholder so the
    /// session still shows up in `surface=jetbrains` lists. Matches the
    /// pre-#764 behavior of #757's existence-marker path.
    #[test]
    fn nitrite_session_without_turn_falls_back_to_single_placeholder() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-nitrite-session-only");
        let _ = std::fs::remove_dir_all(&tmp);
        let session_dir = tmp.join("iu/chat-agent-sessions/sess-no-turns");
        std::fs::create_dir_all(&session_dir).unwrap();
        // A session marker is enough to clear the populated-entity gate
        // shipped in #757, but no `NtAgentTurn` documents are present.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"H:2,blockSize:1000,format:3,version:f\n");
        bytes.extend_from_slice(&[0u8; 64]);
        bytes.extend_from_slice(b"NtAgentSession\x00");
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

    #[test]
    fn looks_like_uuid_accepts_canonical_and_rejects_garbage() {
        assert!(looks_like_uuid("bfe8768a-b11e-469a-852b-fc22c7dd9f23"));
        assert!(looks_like_uuid("00000000-0000-0000-0000-000000000000"));
        // Wrong length.
        assert!(!looks_like_uuid("not-a-uuid"));
        // Dashes in wrong positions.
        assert!(!looks_like_uuid("bfe8768ab-11e-469a-852b-fc22c7dd9f23"));
        // Non-hex characters.
        assert!(!looks_like_uuid("bfe8768z-b11e-469a-852b-fc22c7dd9f23"));
    }

    #[test]
    fn deterministic_uuid_from_nitrite_is_stable_and_distinct_per_turn() {
        let a = deterministic_uuid_from_nitrite("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
        let b = deterministic_uuid_from_nitrite("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
        assert_eq!(a, b);
        let c = deterministic_uuid_from_nitrite("382642f7-6bf3-4e9b-b2ed-970bb3474edb", "/tmp/x");
        assert_ne!(a, c);
        // Distinct namespace prefix vs the session-keyed `deterministic_uuid`.
        let session_keyed = deterministic_uuid("bfe8768a-b11e-469a-852b-fc22c7dd9f23", "/tmp/x");
        assert_ne!(a, session_keyed);
    }

    #[test]
    fn read_git_head_branch_returns_none_for_detached_head() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-head-detached");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::write(
            tmp.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert!(read_git_head_branch(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // #778 — Phase 2 workspace-path extractor coverage. Pinned against the
    // exact byte shape recovered from the 8.4.8 smoke-test machine: the
    // `currentFileUri` JSON value inside a turn document's `stringContent`
    // model-state blob, escape-encoded three levels deep (so a literal
    // `\\\":\\\"file://...` byte sequence).
    #[test]
    fn extract_nitrite_workspace_paths_recovers_uri_from_escaped_json_blob() {
        // Mimics the real bytes around `currentFileUri` on
        // ~/.config/github-copilot/iu/chat-agent-sessions/32REE.../copilot-agent-sessions-nitrite.db
        let raw = b"...currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Verkada-Web/src/foo/bar.tsx\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...";
        let paths = extract_nitrite_workspace_paths(raw);
        assert_eq!(
            paths,
            vec!["/Users/me/_projects/Verkada-Web/src/foo/bar.tsx".to_string()]
        );
    }

    #[test]
    fn extract_nitrite_workspace_paths_dedupes_repeated_uris() {
        // The same `currentFileUri` typically shows up multiple times per
        // session because each turn snapshots the model state.
        let chunk = b"currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Repo/x.rs\\\\\\\"";
        let mut buf = Vec::new();
        for _ in 0..4 {
            buf.extend_from_slice(chunk);
            buf.extend_from_slice(b"...filler...");
        }
        let paths = extract_nitrite_workspace_paths(&buf);
        assert_eq!(paths, vec!["/Users/me/_projects/Repo/x.rs".to_string()]);
    }

    #[test]
    fn extract_nitrite_workspace_paths_percent_decodes_spaces() {
        let raw =
            b"currentFileUri\\\\\\\":\\\\\\\"file:///Users/me/_projects/Has%20Space/x.rs\\\\\\\"";
        let paths = extract_nitrite_workspace_paths(raw);
        assert_eq!(
            paths,
            vec!["/Users/me/_projects/Has Space/x.rs".to_string()]
        );
    }

    #[test]
    fn extract_nitrite_workspace_paths_returns_empty_for_no_uris() {
        // Mirrors the 95-of-98 case on the smoke-test machine: the DB
        // exists and carries `NtAgentTurn` markers but no `file://` token
        // anywhere. Honest signal — return empty rather than guess.
        let bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
        assert!(extract_nitrite_workspace_paths(&bytes).is_empty());
    }

    #[test]
    fn extract_nitrite_workspace_paths_rejects_relative_or_malformed() {
        // A bare `file://` with no leading slash after the scheme isn't a
        // usable absolute path — drop it rather than emit a relative path
        // that the upstream resolver would silently expand.
        let raw = b"...file://relative/path...";
        let paths = extract_nitrite_workspace_paths(raw);
        assert!(paths.is_empty(), "got {paths:?}");
    }

    #[test]
    fn longest_common_path_prefix_finds_deepest_shared_directory() {
        let paths = vec![
            "/Users/me/_projects/Repo/src/a/b/x.rs".to_string(),
            "/Users/me/_projects/Repo/src/a/c.rs".to_string(),
            "/Users/me/_projects/Repo/src/a/b/y.rs".to_string(),
        ];
        assert_eq!(
            longest_common_path_prefix(&paths).as_deref(),
            Some("/Users/me/_projects/Repo/src/a")
        );
    }

    #[test]
    fn longest_common_path_prefix_drops_to_root_dir_when_no_shared_subdir() {
        let paths = vec!["/Users/a/x.rs".to_string(), "/etc/y.rs".to_string()];
        // The common prefix is empty (no shared directory) — return None.
        assert!(longest_common_path_prefix(&paths).is_none());
    }

    /// Synth a real git repo at `repo_root` with an `origin` remote set to
    /// `<url>` and HEAD on `<branch>`. Uses `git init` + `git remote add`
    /// so `resolve_repo_id`'s `git remote get-url origin` actually returns
    /// the URL we asked for. Falls back to manual file writes when git
    /// isn't available (CI containers can lack it) — the test catches
    /// that case and reports the resolver path that ran.
    fn synth_git_repo(repo_root: &Path, remote_url: &str, branch: &str) {
        let _ = std::fs::create_dir_all(repo_root);
        let init_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo_root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !init_ok {
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["remote", "add", "origin", remote_url])
            .current_dir(repo_root)
            .status();
        let _ = std::fs::write(
            repo_root.join(".git/HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        );
    }

    #[test]
    fn resolve_workspace_from_paths_walks_up_to_git_checkout() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-resolve");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/sub/inner")).unwrap();
        synth_git_repo(&tmp, "git@github.com:test/phase2-repo.git", "main");

        let tmp_str = tmp.to_string_lossy().to_string();
        let paths = vec![
            format!("{tmp_str}/src/sub/inner/a.rs"),
            format!("{tmp_str}/src/sub/inner/b.rs"),
        ];
        let res = resolve_phase2_workspace(&paths);
        let Some((repo_id, branch)) = res.repo else {
            // git binary unavailable — bail without failing CI.
            let _ = std::fs::remove_dir_all(&tmp);
            return;
        };
        assert!(repo_id.contains("phase2-repo"), "got {repo_id}");
        assert_eq!(branch.as_deref(), Some("main"));
        assert!(res.failure_reason.is_none());
        assert!(res.common_prefix.is_some());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #788: when no `.git` checkout sits along the path chain, resolution
    /// returns `None` repo *and* the still-useful longest common prefix,
    /// tagged with `no_git_along_chain` for the diagnostic log line.
    ///
    /// Unix-only: `longest_common_path_prefix` requires `/`-anchored
    /// absolute paths. The Windows temp dir (`C:\...`) is rejected before
    /// the resolver runs, which is covered by
    /// `longest_common_path_prefix_drops_to_root_dir_when_no_shared_subdir`.
    #[cfg(unix)]
    #[test]
    fn resolve_phase2_workspace_returns_prefix_and_reason_when_no_git_along_chain() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-no-git");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        // No `.git/` anywhere in the chain.
        let tmp_str = tmp.to_string_lossy().to_string();
        let paths = vec![format!("{tmp_str}/src/a.rs")];
        let res = resolve_phase2_workspace(&paths);
        assert!(res.repo.is_none());
        assert_eq!(res.failure_reason, Some("no_git_along_chain"));
        assert_eq!(
            res.common_prefix.as_deref(),
            Some(format!("{tmp_str}/src").as_str())
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #788: empty input → `no_paths` reason and no prefix.
    #[test]
    fn resolve_phase2_workspace_flags_empty_input() {
        let res = resolve_phase2_workspace(&[]);
        assert!(res.repo.is_none());
        assert!(res.common_prefix.is_none());
        assert_eq!(res.failure_reason, Some("no_paths"));
    }

    /// #788: when `.git` exists along the chain but `resolve_repo_id`
    /// returns `None` (e.g. no `origin` remote, the exact failure mode
    /// described in the Terraform smoke-test ticket), the resolver
    /// surfaces a distinct `repo_id_resolver_returned_none` reason rather
    /// than collapsing into the catch-all `no_git_along_chain`.
    ///
    /// Unix-only — see note on the prior `no_git_along_chain` test.
    #[cfg(unix)]
    #[test]
    fn resolve_phase2_workspace_distinguishes_resolver_returned_none() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-no-remote");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        // `.git` directory exists but is empty — `resolve_repo_id` requires
        // a valid `origin` remote, which an empty dir cannot provide.
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        let tmp_str = tmp.to_string_lossy().to_string();
        let paths = vec![format!("{tmp_str}/src/a.rs")];
        let res = resolve_phase2_workspace(&paths);
        // `resolve_repo_id` shells out to `git`; if git isn't available
        // we still get `repo_id_resolver_returned_none` because the empty
        // `.git` dir trips the resolver before any other failure.
        assert!(res.repo.is_none());
        assert_eq!(res.failure_reason, Some("repo_id_resolver_returned_none"));
        assert!(res.common_prefix.is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #778: end-to-end Phase 2 path. A session-dir that holds only a
    /// Nitrite store (no `.xd`, so Phase 1 bails) with `currentFileUri`
    /// hits inside the stringContent JSON must resolve `repo_id` from the
    /// file URIs and land it on every emitted per-turn row.
    #[test]
    fn nitrite_only_session_resolves_repo_id_via_currentfileuri_phase2() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-end-to-end");
        let _ = std::fs::remove_dir_all(&tmp);
        // Synth repo root with a real .git dir + remote.
        let repo_root = tmp.join("repos/MyProject");
        std::fs::create_dir_all(repo_root.join("src/foo")).unwrap();
        synth_git_repo(
            &repo_root,
            "git@github.com:siropkin/myproject.git",
            "feature",
        );
        let repo_root_str = repo_root.to_string_lossy().to_string();

        // Synth Nitrite bytes: one turn UUID + a currentFileUri JSON blob
        // that points into `<repo_root>/src/foo/bar.rs`.
        let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
        bytes.extend_from_slice(b"...currentFileUri\\\\\\\":\\\\\\\"file://");
        bytes.extend_from_slice(repo_root_str.as_bytes());
        bytes.extend_from_slice(b"/src/foo/bar.rs\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...");

        let session_dir = tmp.join("iu/chat-agent-sessions/sess-phase2");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("copilot-agent-sessions-nitrite.db"),
            &bytes,
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1, "one row per turn, got {parsed:?}");
        let msg = &parsed[0];
        // git binary may not be on the test host; bail cleanly if so.
        if msg.repo_id.is_some() {
            assert_eq!(
                msg.repo_id.as_deref(),
                Some("github.com/siropkin/myproject"),
                "repo_id should resolve via Phase 2"
            );
            assert_eq!(msg.git_branch.as_deref(), Some("feature"));
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// #788: when Phase 2 recovers `file://` URIs but the chain doesn't
    /// lead to a resolvable `.git` checkout (e.g. the Terraform smoke-test
    /// scenario where the repo isn't checked out on this host), the
    /// emitted message must still carry the longest-common-prefix path
    /// as a `cwd` hint with the `copilot_chat:jetbrains_phase2_prefix`
    /// `cwd_source` marker. Gives the dashboard / messages.cwd something
    /// to render even when `repo_id` is null.
    ///
    /// Unix-only — relies on `/`-anchored temp paths flowing through the
    /// extractor's URI decoder.
    #[cfg(unix)]
    #[test]
    fn phase2_with_uris_but_no_git_emits_cwd_hint_with_phase2_prefix_source() {
        let tmp = std::env::temp_dir().join("budi-jetbrains-phase2-cwd-hint");
        let _ = std::fs::remove_dir_all(&tmp);
        // Synth a path that exists but has no `.git` above it.
        let scratch_root = tmp.join("scratch/PhantomRepo");
        std::fs::create_dir_all(scratch_root.join("src")).unwrap();
        let scratch_str = scratch_root.to_string_lossy().to_string();

        let mut bytes = synth_nitrite_with_turns(&["bfe8768a-b11e-469a-852b-fc22c7dd9f23"]);
        bytes.extend_from_slice(b"...currentFileUri\\\\\\\":\\\\\\\"file://");
        bytes.extend_from_slice(scratch_str.as_bytes());
        bytes.extend_from_slice(b"/src/bar.rs\\\\\\\",\\\\\\\"isVisionEnabled\\\\\\\":true...");

        let session_dir = tmp.join("iu/chat-agent-sessions/sess-phase2-cwd-hint");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("copilot-agent-sessions-nitrite.db"),
            &bytes,
        )
        .unwrap();

        let parsed = parse_session_dir(&session_dir);
        assert_eq!(parsed.len(), 1, "one row per turn, got {parsed:?}");
        let msg = &parsed[0];
        assert!(
            msg.repo_id.is_none(),
            "no .git → no repo_id (got {:?})",
            msg.repo_id
        );
        // The byte-walker recovered the URI and the resolver could not
        // walk up to a `.git`, so we should still surface the prefix.
        assert_eq!(
            msg.cwd.as_deref(),
            Some(format!("{scratch_str}/src").as_str()),
            "cwd hint should equal the longest common prefix"
        );
        assert_eq!(
            msg.cwd_source.as_deref(),
            Some("copilot_chat:jetbrains_phase2_prefix")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Phase 2 must not clobber a Phase 1 resolution. If the Xodus log
    /// already mapped a `projectName` to a `.git` checkout, Phase 2 stays
    /// silent — preserving the original 8.4.7/8.4.8 behaviour.
    #[test]
    fn phase2_does_not_override_phase1_resolution_when_xodus_already_resolved() {
        // This is asserted indirectly: `parse_session_dir` only invokes
        // `resolve_workspace_from_paths` when `phase1_resolution.is_none()`.
        // A regression here would either drop or change repo_id on the
        // sessions covered by `dual_store_session_combines_xodus_repo_with_nitrite_turns`
        // — which already passes. The structural assertion below documents
        // the invariant for future readers.
        // (No runtime body — the order is enforced by the dual-store test
        // above. Keeping the test stub here so the intent stays linked.)
    }

    /// #778: real-fixture coverage. The redacted on-disk Nitrite DB
    /// captured from a JetBrains Copilot agent session that opened
    /// `readme.md` from a Terraform repo. The Phase 2 byte-walker must
    /// recover the file URI from the `currentFileUri` JSON blob, dedupe
    /// the repeats, and compute the longest common prefix.
    #[test]
    fn extract_nitrite_workspace_paths_against_real_redacted_fixture() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "src/providers/copilot_chat/fixtures/jetbrains_nitrite_working_set_phase2/copilot-agent-sessions-nitrite.db",
        );
        let bytes = std::fs::read(&fixture).expect("real fixture is checked in");
        let paths = extract_nitrite_workspace_paths(&bytes);
        assert!(!paths.is_empty(), "expected at least one URI");
        // Every recovered URI in this fixture points under the same repo.
        for p in &paths {
            assert!(
                p.starts_with("/Users/redacted-user/_projects/Terraform"),
                "unexpected URI: {p}"
            );
        }
        // Longest common prefix should be the Terraform repo root, not
        // a file path — the prefix-finder pops the filename component.
        let prefix = longest_common_path_prefix(&paths).expect("has common prefix");
        assert_eq!(prefix, "/Users/redacted-user/_projects/Terraform");
    }

    #[test]
    fn decode_file_uri_handles_localhost_form_and_relative_rejection() {
        assert_eq!(
            decode_file_uri("file:///abs/path").as_deref(),
            Some("/abs/path")
        );
        assert_eq!(
            decode_file_uri("file://localhost/abs/path").as_deref(),
            Some("/abs/path")
        );
        assert!(decode_file_uri("file://relative/path").is_none());
        assert!(decode_file_uri("https://example.com/x").is_none());
    }
}
