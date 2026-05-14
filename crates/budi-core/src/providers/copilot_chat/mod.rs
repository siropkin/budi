//! Copilot Chat provider — tails GitHub Copilot Chat session files written
//! by the `github.copilot-chat` VS Code extension across Code, Insiders,
//! Exploration, VSCodium, Cursor, and remote-server installs.
//!
//! Contract: [ADR-0092](../../../../docs/adr/0092-copilot-chat-data-contract.md).
//! Any breaking change to the undocumented upstream must land as a paired
//! edit to ADR-0092 §2.3 and this module so the two never disagree.
//!
//! Local-tail half of the Copilot Chat surface (R1.4, #651). The
//! Billing API reconciliation half lives in
//! `crates/budi-core/src/sync/copilot_chat_billing.rs` (R1.5, #652) and
//! is wired into `Provider::sync_direct` below as a best-effort dollar
//! truth-up that runs alongside the file-based local-tail ingest.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, Provider};

pub mod jetbrains;

/// Canonical provider id. ADR-0093 §1: JetBrains is a host of the same
/// Copilot Chat provider as VS Code — the `surface` dimension carries the
/// host distinction, not the provider id. Threaded into the JetBrains-side
/// `ParsedMessage::provider` so both halves of the ingest path land under
/// the same provider key.
pub const PROVIDER_ID: &str = "copilot_chat";

/// Monotonically-incrementing version that surfaces in `budi doctor` (R1.6,
/// #653) when the parser shape changes. Mirrors the budi-cursor
/// `MIN_API_VERSION` pattern (ADR-0092 §2.6). Bump in lockstep with §2.3
/// of ADR-0092 whenever a fifth (or later) token-key shape lands.
///
/// v2 (8.4.0): the parser now descends into the `{ "kind": N, "v": [...] }`
/// JSONL envelope and the `{ "requests": [...] }` JSON-document envelope
/// that real VS Code Copilot Chat session files actually use. The four
/// token-key shapes from v1 still apply — they just match against records
/// inside the envelope rather than the envelope itself.
///
/// v3 (8.4.0): added a fifth shape — top-level `completionTokens` only —
/// to capture VS Code Copilot Chat builds that persist output-token
/// counts but no input-token counterpart anywhere on disk. These records
/// emit with `input_tokens = 0` so the row at least exists; the Billing
/// API reconciliation in §3 of ADR-0092 truths up the dollar number to
/// the real bill on the next tick for individually-licensed users.
///
/// v4 (8.4.1, R1.1): the JSONL parser is now a per-session **mutation-log
/// reducer** rather than a per-line independent extractor. VS Code 1.109+
/// (and `github.copilot-chat` ≥0.47.0) persist chat sessions as a JSON
/// Pointer mutation log: a `kind:0` snapshot followed by `kind:1` set-at-
/// pointer and `kind:2` array-splice patches. The token counts arrive on
/// later kind:1 patches like `{"kind":1,"k":["requests",8,"completionTokens"],"v":39}`
/// — buried inside `k`, never at the top of the line. The v3 parser saw
/// these as flat records with no token keys and emitted zero rows from
/// active sessions. v4 replays the mutation log onto a per-session state
/// and runs the four-then-five token-key shapes against the **materialized
/// request**, not the raw line. The four full-pair shapes from §2.3 are
/// unchanged; the output-only fallback is unchanged. Requests are
/// emit-keyed by `requestId` so a future patch on the same request never
/// produces a duplicate row. Paired with an ADR-0092 §2.3 amendment that
/// documents the reducer as the authoritative envelope shape.
///
/// v5 (8.5.1, #791): no parser logic change — this bump pins the
/// reducer against two newly-validated upstream behaviors that v4 already
/// handled correctly but never had a fixture for:
///
/// 1. `inputState.attachments` mutations on VS Code 1.119+ — large
///    DOM-like UI introspection blobs (per-attachment shape
///    `["ancestors","attributes","computedStyles","dimensions",
///    "fullName","icon","id","innerText","kind","modelDescription",
///    "name","value"]`) persisted any time the user drags a non-source
///    surface (Settings UI, Outline, file tree) into the chat input.
///    These dominate the byte stream — a single attachments mutation
///    is routinely tens to hundreds of kilobytes — but carry no
///    request data. They correctly land under `state.inputState` and
///    never trigger `state.requests` re-scan, so the reducer keeps
///    flowing without spurious unknown-shape warnings or row emits.
///    The new `vscode_chat_0_47_0_v5.jsonl` fixture pins this contract.
/// 2. `<workspaceStorage>/<hash>/GitHub.copilot-chat/debug-logs/<uuid>/
///    main.jsonl` was being mis-classified as a session file by
///    `collect_session_files`. It is OpenTelemetry span output (shape
///    `["attrs","dur","name","sid","spanId","status","ts","type","v"]`),
///    not chat data — every line warned `copilot_chat_unknown_record_shape`
///    while contributing exactly zero rows. Discovery now skips the
///    whole `debug-logs/` subtree so the daemon log stays quiet and
///    `budi doctor`'s `tailer rows / Copilot Chat` heuristic stops
///    flagging the noise as a parser regression.
pub const MIN_API_VERSION: u32 = 5;

/// VS Code-family directory names. Casing is preserved for the macOS
/// "Application Support" path, where the disk layout is case-sensitive on
/// stock APFS volumes; on Linux/Windows the filesystems we target are
/// case-insensitive enough for the literal name to match.
const VSCODE_VARIANTS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "Code - Exploration",
    "VSCodium",
    "Cursor",
];

/// Publisher-id directory candidates, lowercased for case-insensitive
/// matching against on-disk dir entries. Per ADR-0092 §2.2 the publisher
/// id has flipped between `GitHub` and `github` at least once, and
/// case-insensitive filesystems (macOS APFS, Windows NTFS) collapse the
/// two casings to a single dir, so we match against the actual entry
/// casing rather than joining a guessed-casing path.
const PUBLISHER_DIRS_LOWER: &[&str] = &["github.copilot-chat", "github.copilot"];

fn entry_matches_publisher(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PUBLISHER_DIRS_LOWER.iter().any(|p| lower == *p)
}

/// The Copilot Chat provider.
pub struct CopilotChatProvider;

impl Provider for CopilotChatProvider {
    fn name(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Copilot Chat"
    }

    fn is_available(&self) -> bool {
        any_user_root_has_copilot_marker(&user_root_candidates()) || jetbrains::is_available()
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let mut files = Vec::new();
        for user_root in user_root_candidates() {
            collect_session_files(&user_root, &mut files);
        }
        files.sort();
        files.dedup();
        // Newest-first for progressive sync.
        files.sort_by(|a, b| {
            let mtime = |p: &PathBuf| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            };
            mtime(b).cmp(&mtime(a))
        });
        Ok(files
            .into_iter()
            .map(|path| DiscoveredFile { path })
            .collect())
    }

    fn parse_file(
        &self,
        path: &Path,
        content: &str,
        offset: usize,
    ) -> Result<(Vec<ParsedMessage>, usize)> {
        Ok(parse_copilot_chat(path, content, offset))
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for user_root in user_root_candidates() {
            let ws = user_root.join("workspaceStorage");
            if ws.is_dir() {
                roots.push(ws);
            }
            let gs = user_root.join("globalStorage");
            if gs.is_dir() {
                roots.push(gs);
            }
        }
        roots.extend(jetbrains::watch_roots());
        roots.sort();
        roots.dedup();
        roots
    }

    fn sync_direct(
        &self,
        conn: &mut Connection,
        pipeline: &mut crate::pipeline::Pipeline,
        _max_age_days: Option<u64>,
    ) -> Option<Result<(usize, usize, Vec<String>)>> {
        // JetBrains-side ingest (ADR-0093): the binary Xodus+Nitrite stores
        // are not streamable through the `parse_file` text path, so the
        // JetBrains rows land via a direct discover-and-ingest sweep here.
        // Run before the billing-API reconciliation so the metadata-only
        // rows exist before the reconciliation tries to attach costs to
        // them on a (date, model) bucket basis.
        let _jb_ingested = jetbrains::sync_jetbrains_sessions(conn, pipeline);

        // R1.5 / ADR-0092 §3: best-effort GitHub Billing API
        // reconciliation. Local-tail is the primary signal; this just
        // truths-up `cost_cents` on existing rows on a (date, model)
        // bucket basis, so we deliberately return `None` and let the
        // dispatcher proceed to the file-based discovery path. The
        // billing pull is a side effect that complements ingest, never
        // a replacement for it.
        let config = crate::config::load_copilot_chat_config();
        if config.effective_billing_pat().is_some()
            && let Err(e) = crate::sync::copilot_chat_billing::run_reconciliation(conn, &config)
        {
            tracing::warn!("copilot_chat billing reconciliation failed: {e:#}");
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Path discovery (ADR-0092 §2.1, §2.2)
// ---------------------------------------------------------------------------

fn user_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let home = match crate::config::home_dir() {
        Ok(h) => h,
        Err(_) => return roots,
    };
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);

    for variant in VSCODE_VARIANTS {
        roots.push(
            home.join("Library/Application Support")
                .join(variant)
                .join("User"),
        );
        roots.push(home.join(".config").join(variant).join("User"));
        roots.push(home.join("AppData/Roaming").join(variant).join("User"));
        if let Some(ref ad) = appdata {
            roots.push(ad.join(variant).join("User"));
        }
    }

    // Remote / dev-container installs (ADR-0092 §2.1).
    roots.push(home.join(".vscode-server/data/User"));
    roots.push(home.join(".vscode-server-insiders/data/User"));
    roots.push(home.join(".vscode-remote/data/User"));
    roots.push(PathBuf::from("/tmp/.vscode-server/data/User"));
    roots.push(PathBuf::from("/workspace/.vscode-server/data/User"));

    roots.sort();
    roots.dedup();
    roots
}

fn any_user_root_has_copilot_marker(roots: &[PathBuf]) -> bool {
    for user_root in roots {
        if !user_root.is_dir() {
            continue;
        }
        let ws = user_root.join("workspaceStorage");
        if let Ok(entries) = std::fs::read_dir(&ws) {
            for entry in entries.flatten() {
                let hash_dir = entry.path();
                if !hash_dir.is_dir() {
                    continue;
                }
                if hash_dir.join("chatSessions").is_dir() {
                    return true;
                }
                if dir_has_publisher_child(&hash_dir) {
                    return true;
                }
            }
        }
        let gs = user_root.join("globalStorage");
        if gs.join("emptyWindowChatSessions").is_dir() {
            return true;
        }
        if dir_has_publisher_child(&gs) {
            return true;
        }
    }
    false
}

fn dir_has_publisher_child(parent: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && entry_matches_publisher(name)
        {
            return true;
        }
    }
    false
}

fn publisher_subdirs(parent: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && entry_matches_publisher(name)
        {
            out.push(path);
        }
    }
    out
}

fn collect_session_files(user_root: &Path, out: &mut Vec<PathBuf>) {
    if !user_root.is_dir() {
        return;
    }
    let start = out.len();

    // workspaceStorage/<hash>/...
    let ws = user_root.join("workspaceStorage");
    if let Ok(entries) = std::fs::read_dir(&ws) {
        for entry in entries.flatten() {
            let hash_dir = entry.path();
            if !hash_dir.is_dir() {
                continue;
            }
            push_session_files_recursive(&hash_dir.join("chatSessions"), out, 0);
            for pub_dir in publisher_subdirs(&hash_dir) {
                push_session_files_recursive(&pub_dir.join("chatSessions"), out, 0);
                // `debug-logs/<requestId>/main.jsonl` is OpenTelemetry span
                // output (shape: `["attrs","dur","name","sid","spanId",
                // "status","ts","type","v"]`), not chat content — every
                // line is a span event with no `promptTokens`/`outputTokens`/
                // `completionTokens` anywhere, so the parser flags each as
                // `copilot_chat_unknown_record_shape`. Skip the whole
                // directory so the daemon log stays quiet (#791,
                // ADR-0092 §2.2).
            }
        }
    }

    // globalStorage/...
    let gs = user_root.join("globalStorage");
    push_session_files_recursive(&gs.join("emptyWindowChatSessions"), out, 0);
    // ADR-0092 §2.2: globalStorage/{GitHub,github}.copilot{,-chat}/** is
    // intentionally recursive — the sub-directory layout has shifted multiple
    // times across releases. Iterate the actual on-disk dir entries so
    // case-insensitive filesystems collapse to a single match instead of
    // double-counting both casings. The recursion bottom-out is anchored at
    // a known session-storage directory name (chatSessions / chat-sessions /
    // sessions) so embedding caches and CLI state blobs sitting one level
    // under the publisher dir don't get pulled in (#684).
    for pub_dir in publisher_subdirs(&gs) {
        push_global_storage_session_files(&pub_dir, out, 0, false);
    }

    // Dedup the slice we just appended. On case-insensitive filesystems
    // (macOS APFS, Windows NTFS) the dual-publisher casing in PUBLISHER_DIRS
    // resolves to the same directory and would otherwise yield duplicate
    // entries. Sort+dedup is fine here — the per-provider sweep callers
    // re-sort by mtime later.
    if out.len() > start {
        let mut tail: Vec<PathBuf> = out.drain(start..).collect();
        tail.sort();
        tail.dedup();
        out.extend(tail);
    }
}

fn push_session_files_recursive(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    // Cap recursion to keep a misconfigured symlink from running away.
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            push_session_files_recursive(&path, out, depth + 1);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && (ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push(path);
        }
    }
}

/// Recurse through `globalStorage/{GitHub,github}.copilot{,-chat}/**` per
/// ADR-0092 §2.2, but only collect `*.json` / `*.jsonl` files that live under
/// a directory named `chatSessions`, `chat-sessions`, or `sessions`. The
/// recursion still tolerates layout shuffles below the publisher-id directory
/// (the canonical reason §2.2 is recursive at all), but the bottom-out is
/// anchored at a known session-storage directory name so the embedding caches
/// (`commandEmbeddings.json`, `settingEmbeddings.json`) and the Copilot CLI
/// state blob (`copilot.cli.oldGlobalSessions.json`) sitting as siblings of
/// the session directory never match (#684).
fn push_global_storage_session_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    depth: u32,
    inside_session_dir: bool,
) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let next_inside = inside_session_dir
                || path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_session_storage_dir_name);
            push_global_storage_session_files(&path, out, depth + 1, next_inside);
        } else if inside_session_dir
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
            && (ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push(path);
        }
    }
}

fn is_session_storage_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("chatSessions")
        || name.eq_ignore_ascii_case("chat-sessions")
        || name.eq_ignore_ascii_case("sessions")
}

// ---------------------------------------------------------------------------
// Parser dispatch (ADR-0092 §2.3 — §2.6)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct TokenSet {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

fn parse_copilot_chat(path: &Path, content: &str, offset: usize) -> (Vec<ParsedMessage>, usize) {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());

    if extension.as_deref() == Some("jsonl") {
        return parse_jsonl(path, content, offset);
    }

    // Treat anything else (`*.json` and unknown) as a JSON document.
    parse_json_document(path, content)
}

// ---------------------------------------------------------------------------
// Workspace enrichment (ADR-0092 §2.2 — workspaceStorage/<hash>/workspace.json)
// ---------------------------------------------------------------------------
//
// Per #681: every `copilot_chat` row landed with `cwd = NULL` because the
// parser hard-skipped workspace enrichment. VS Code writes a sibling
// `<workspaceStorage>/<hash>/workspace.json` next to every `chatSessions/`
// directory; the `folder` (single-root) or `configuration` (multi-root)
// field is the authoritative cwd. Once cwd lands, the GitEnricher
// resolves `repo_id` and the in-parser HEAD read below resolves
// `git_branch` — both flow off cwd, no provider-specific surface needed.

/// Resolve the cwd for a Copilot Chat session file by walking up to the
/// nearest `workspace.json`. Returns `None` for `emptyWindowChatSessions/*`
/// (legitimately no folder context — the user opened VS Code without a
/// workspace) and when no `workspace.json` is reachable from the session
/// path.
///
/// Walks up at most through the `workspaceStorage` boundary so a stray
/// `workspace.json` higher up the tree (e.g. user-level config) doesn't
/// pollute every session's cwd.
fn workspace_cwd_for_session_path(session_path: &Path) -> Option<String> {
    // emptyWindowChatSessions: VS Code opened with no folder. Skip cleanly.
    for ancestor in session_path.ancestors() {
        if ancestor
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("emptyWindowChatSessions"))
        {
            return None;
        }
    }

    let mut current = session_path.parent();
    while let Some(dir) = current {
        let candidate = dir.join("workspace.json");
        if candidate.is_file() {
            return read_workspace_json(&candidate);
        }
        // Stop at the workspaceStorage boundary — workspace.json lives at
        // <workspaceStorage>/<hash>/, never above. Walking further would
        // pick up a higher-level workspace.json that does not belong to
        // this session.
        if dir
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("workspaceStorage"))
        {
            return None;
        }
        current = dir.parent();
    }
    None
}

/// Read a `<workspaceStorage>/<hash>/workspace.json` file and resolve it
/// to a local-side cwd string. Returns `None` on missing file, malformed
/// JSON, or unrecognised shape — the caller treats `None` as "no
/// enrichment" and emits the row without a cwd.
fn read_workspace_json(path: &Path) -> Option<String> {
    let content = crate::fs_util::read_capped(path, crate::fs_util::PROBE_FILE_CAP)
        .ok()
        .flatten()?;
    let doc: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Single-root: `{"folder": "file:///path"}` (or `vscode-remote://...`,
    // `vscode-vfs://...`). The `folder` URI is authoritative.
    if let Some(folder) = doc.get("folder").and_then(|v| v.as_str()) {
        return Some(uri_to_local_path(folder));
    }

    // Multi-root: `{"configuration": "file:///abs/path/to/x.code-workspace"}`.
    // Read the workspace file and pick the first folder per #681 (or
    // fall back when the `inputState.workingSet` reference can't be
    // resolved — which is the case for ~all sessions today).
    if let Some(config) = doc.get("configuration").and_then(|v| v.as_str()) {
        let config_path = PathBuf::from(uri_to_local_path(config));
        if let Some(cwd) = first_folder_from_workspace_file(&config_path) {
            return Some(cwd);
        }
    }

    None
}

/// Read a `.code-workspace` file and return the first folder's resolved
/// path. Folder paths in `.code-workspace` are typically relative to the
/// workspace file's parent directory; absolute paths pass through.
fn first_folder_from_workspace_file(config_path: &Path) -> Option<String> {
    let content = crate::fs_util::read_capped(config_path, crate::fs_util::PROBE_FILE_CAP)
        .ok()
        .flatten()?;
    let ws: serde_json::Value = serde_json::from_str(&content).ok()?;
    let folders = ws.get("folders").and_then(|v| v.as_array())?;
    let first = folders.first()?;
    let folder_path = first.get("path").and_then(|v| v.as_str())?;
    let p = Path::new(folder_path);
    if p.is_absolute() {
        return Some(folder_path.to_string());
    }
    if let Some(parent) = config_path.parent() {
        return Some(parent.join(p).to_string_lossy().into_owned());
    }
    Some(folder_path.to_string())
}

/// Convert a `file://` / `vscode-remote://` / `vscode-vfs://` URI to a
/// local-side path string. Per #681:
/// - `file:///abs/path` → `/abs/path`
/// - `vscode-remote://ssh-remote+host/abs/path` → `/abs/path`
/// - `vscode-vfs://github/owner/repo/path` → `/owner/repo/path`
///   (host segment is dropped — won't resolve to a local repo, but keeps
///   the row tagged for cloud-dashboard grouping).
///
/// Percent-encoded sequences (e.g. `%20` for spaces in
/// `Application%20Support`) are decoded before stripping the scheme.
fn uri_to_local_path(uri: &str) -> String {
    let decoded = percent_decode(uri);
    if let Some(rest) = decoded.strip_prefix("file://") {
        return rest.to_string();
    }
    if let Some((_scheme, after)) = decoded.split_once("://") {
        if let Some(slash_idx) = after.find('/') {
            return after[slash_idx..].to_string();
        }
        return format!("/{after}");
    }
    decoded
}

/// Minimal RFC-3986 percent-decoder. Skips invalid escapes verbatim
/// rather than failing — workspace.json values are written by VS Code so
/// invalid escapes are not expected, and a parse failure here would lose
/// cwd enrichment for the whole session.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Read the current branch name from `<repo>/.git/HEAD`. Returns `None`
/// when the cwd is not inside a git repo (per `repo_root_for`), the
/// repo is in a detached-HEAD state, or `.git` is a worktree pointer
/// file rather than a directory (handled best-effort — Copilot Chat
/// sessions in worktrees fall back to `git_branch = NULL`, the same
/// shape as a non-repo cwd).
fn git_branch_for_cwd(cwd: &str) -> Option<String> {
    let root = crate::repo_id::repo_root_for(Path::new(cwd))?;
    let head = root.join(".git").join("HEAD");
    let contents = crate::fs_util::read_capped(&head, crate::fs_util::PROBE_FILE_CAP)
        .ok()
        .flatten()?;
    contents
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(|b| b.to_string())
}

/// Per-session enrichment derived once per `parse_file` call so the
/// builder helpers stay under clippy's `too_many_arguments` limit (a
/// single parameter holds both fields instead of plumbing two parallel
/// `Option<&str>`s through every call site).
#[derive(Default, Clone)]
struct SessionEnrichment {
    cwd: Option<String>,
    git_branch: Option<String>,
    /// Provenance label for `cwd` when it came from a fallback signal
    /// rather than the authoritative `workspace.json`. `Some` only for
    /// the emptyWindow editor-context hint path (#688). Workspace-anchored
    /// cwds and absent cwds both leave this `None`.
    cwd_source: Option<&'static str>,
}

impl SessionEnrichment {
    fn for_path(session_path: &Path) -> Self {
        let cwd = workspace_cwd_for_session_path(session_path);
        let git_branch = cwd.as_deref().and_then(git_branch_for_cwd);
        Self {
            cwd,
            git_branch,
            cwd_source: None,
        }
    }
}

/// `result.metadata.renderedUserMessage` `cwd_source` label for the
/// emptyWindow editor-context hint path (#688). Mirrors the constant
/// shape `<provider>:<signal>` used elsewhere in the analytics
/// vocabulary.
const CWD_SOURCE_EDITOR_CONTEXT_HINT: &str = "copilot_chat:editor_context_hint";

/// True iff the session lives under `globalStorage/emptyWindowChatSessions/`.
/// `workspace_cwd_for_session_path` already short-circuits this case to
/// `None`, but the editor-context hint fallback only applies *here* —
/// workspace-anchored sessions with a transiently-missing `workspace.json`
/// must not silently drift onto the hint path.
fn is_empty_window_session_path(session_path: &Path) -> bool {
    session_path.ancestors().any(|a| {
        a.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("emptyWindowChatSessions"))
    })
}

/// Scan the materialized session state's `requests[*].result.metadata
/// .renderedUserMessage[*].text` for the first `<editorContext>` block
/// and return its `parent` directory as a cwd hint. The block's
/// canonical shape (#688):
///
/// ```text
/// <editorContext>
/// The user's current file is /Users/.../foo.md. The current selection is from line 9 to line 9.
/// </editorContext>
/// ```
///
/// Only absolute paths are accepted — relative paths cannot be resolved
/// without a workspace root and we have none in the emptyWindow case.
/// Returns the `parent()` of the file, not the file itself: the cwd
/// dimension is "what folder were they in", not "what file did they
/// have open".
fn editor_context_cwd_hint_from_state(state: &serde_json::Value) -> Option<String> {
    let requests = state.get("requests").and_then(|v| v.as_array())?;
    for request in requests {
        let arr = request
            .pointer("/result/metadata/renderedUserMessage")
            .and_then(|v| v.as_array());
        let Some(arr) = arr else {
            continue;
        };
        for item in arr {
            let Some(text) = item.get("text").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(parent) = parent_dir_from_editor_context_text(text) {
                return Some(parent);
            }
        }
    }
    None
}

/// Extract the parent directory of `<editorContext>`'s "current file"
/// path from a single `renderedUserMessage` text. Returns `None` if no
/// `<editorContext>` block is present, no path was extractable, or the
/// path was not absolute.
fn parent_dir_from_editor_context_text(text: &str) -> Option<String> {
    const OPEN: &str = "<editorContext>";
    const CLOSE: &str = "</editorContext>";
    const PREFIX: &str = "The user's current file is ";

    let open_at = text.find(OPEN)?;
    let after_open = &text[open_at + OPEN.len()..];
    let block_end = after_open.find(CLOSE).unwrap_or(after_open.len());
    let block = &after_open[..block_end];

    let p_start = block.find(PREFIX)?;
    let after_prefix = &block[p_start + PREFIX.len()..];

    // The path runs to the next sentence terminator (`. ` followed by a
    // capitalised word, in practice "The current selection is...") or to
    // the end of the block. Splitting on `". "` is more robust than a
    // bare `.` because absolute file paths legitimately contain `.`s
    // (extensions, dotted user-dirs like `ivan.seredkin`).
    let raw_path = match after_prefix.find(". ") {
        Some(end) => &after_prefix[..end],
        None => after_prefix.trim_end_matches('.').trim_end(),
    };
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        return None;
    }

    // The editor-context path is whatever shape VS Code wrote on the user's
    // machine — POSIX-style on macOS/Linux, drive-letter or UNC on Windows.
    // `Path::is_absolute()` is platform-conditional (returns false for
    // `/Users/...` when the test runs on Windows CI), so we recognise the
    // three shapes explicitly and parse the parent off the literal string
    // rather than via `Path::parent()`.
    if !looks_absolute(raw_path) {
        return None;
    }
    parent_dir_string(raw_path)
}

/// Cross-platform "looks absolute" check that mirrors what VS Code may
/// write into `<editorContext>` regardless of which OS this code runs
/// on:
///
/// - POSIX: leading `/`.
/// - Windows UNC: leading `\\`.
/// - Windows drive: `<letter>:` followed by `\` or `/`.
fn looks_absolute(s: &str) -> bool {
    if s.starts_with('/') || s.starts_with("\\\\") {
        return true;
    }
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Parent-directory of an absolute path string, splitting on the last
/// `/` or `\` so the result is identical regardless of the host
/// platform. Returns `"/"` for a root-level file like `"/foo"`.
fn parent_dir_string(path: &str) -> Option<String> {
    let last = path.rfind(['/', '\\'])?;
    if last == 0 {
        return Some("/".to_string());
    }
    Some(path[..last].to_string())
}

fn parse_jsonl(path: &Path, content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    if start_offset > content.len() {
        return (Vec::new(), content.len());
    }

    // The mutation-log reducer (ADR-0092 §2.3 v4) replays kind:0/1/2 from
    // byte 0 to materialize per-session state. The framework only hands us
    // the appended chunk since the previous offset, so we read the full
    // file ourselves when it is on disk; unit tests (no file at `path`)
    // and transient I/O failures fall back to the appended chunk, which
    // is the legacy shape the synthetic fixtures use.
    let on_disk = std::fs::read_to_string(path).ok();
    let (parse_content, has_full_file): (&str, bool) = match on_disk.as_deref() {
        Some(s) if !s.is_empty() => (s, true),
        _ => (&content[start_offset..], false),
    };

    // Returned offset is in `content` coordinates (relative to the chunk
    // the framework gave us). Advance past the last complete line so a
    // mid-write record is replayed on the next tick.
    let new_offset = last_complete_line_end_in_content(content, start_offset);

    let session_id_from_path = session_id_for_path(path);
    let mut state = serde_json::json!({});
    if let Some(ref sid) = session_id_from_path
        && let Some(obj) = state.as_object_mut()
    {
        obj.insert(
            "sessionId".to_string(),
            serde_json::Value::String(sid.clone()),
        );
    }
    // Resolve workspace cwd once per session-file parse and reuse for
    // every emitted row — `parse_jsonl` corresponds 1:1 to a session, so
    // re-reading `workspace.json` per emit would be wasteful (per #681
    // "cwd should be cached per session").
    let mut enrichment = SessionEnrichment::for_path(path);
    // #688: emptyWindow sessions have no `workspace.json`. Defer the
    // editor-context hint scan until after the mutation log has been
    // replayed below — the hint reads off `result.metadata
    // .renderedUserMessage[]`, which only materialises once kind:0/1/2
    // patches are applied to `state`. We back-fill emitted rows after
    // the loop rather than racing emit order against patch order.
    let is_empty_window = is_empty_window_session_path(path);
    let mut session_default_model: Option<String> = None;
    let mut messages = Vec::new();
    // Tracks emit keys (typically `requestId`) so a request that
    // transitions to "complete" on one line and is then re-touched by a
    // later patch in the same parse only emits once. Cross-call dedup
    // relies on the deterministic UUID (keyed by the same emit key)
    // colliding at the database layer.
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Flat-line back-compat: monotonic line index used by the legacy
    // composite UUID derivation (line_index * 1_000_000 + record_index).
    // Only relevant when the line has no `kind` envelope.
    let mut flat_line_index: usize = if has_full_file {
        0
    } else {
        byte_offset_to_line_index(content, start_offset)
    };
    let mut flat_record_index: usize = 0;

    for line in parse_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flat_line_index += 1;
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Malformed or partially-written line — leave for the next tick.
            flat_line_index += 1;
            continue;
        };

        if value.get("kind").and_then(|v| v.as_u64()).is_some() {
            apply_mutation(&mut state, &value);

            if let Some(m) = extract_session_default_model(&state) {
                session_default_model = Some(m);
            }
            if let Some(v_payload) = value.get("v")
                && let Some(m) = extract_session_default_model(v_payload)
            {
                session_default_model = Some(m);
            }

            // Re-scan requests after each mutation. A request is
            // emit-eligible the moment `extract_tokens` returns Some — that
            // is, a kind:1 patch has just landed enough token counts on
            // the materialized request to satisfy one of the §2.3 shapes.
            if let Some(requests) = state.get("requests").and_then(|v| v.as_array()) {
                for (idx, request) in requests.iter().enumerate() {
                    if let Some(m) = request
                        .get("modelId")
                        .and_then(|v| v.as_str())
                        .map(|s| strip_copilot_prefix(s).to_string())
                    {
                        session_default_model = Some(m);
                    }
                    let Some(tokens) = extract_tokens(request) else {
                        continue;
                    };
                    let emit_key = request_emit_key(request, idx);
                    if !emitted.insert(emit_key.clone()) {
                        continue;
                    }
                    let rows = build_messages_for_request(
                        path,
                        request,
                        tokens,
                        state.get("sessionId").and_then(|v| v.as_str()),
                        session_default_model.as_deref(),
                        &emit_key,
                        &enrichment,
                    );
                    messages.extend(rows);
                }
            }
        } else {
            // Flat-line back-compat path (synthetic fixtures, pre-v4 shapes).
            flat_line_index += 1;
            if let Some(model) = extract_session_default_model(&value) {
                session_default_model = Some(model);
            }
            for record in flatten_records(&value) {
                if let Some(model) = extract_session_default_model(record) {
                    session_default_model = Some(model);
                }
                flat_record_index += 1;
                let composite_index = flat_line_index
                    .wrapping_mul(1_000_000)
                    .wrapping_add(flat_record_index);
                let rows = build_message(
                    path,
                    record,
                    state.get("sessionId").and_then(|v| v.as_str()),
                    session_default_model.as_deref(),
                    composite_index,
                    &enrichment,
                );
                if rows.is_empty() {
                    if !shape_matches_any(record) {
                        log_unknown_shape_once(path, record);
                    }
                } else {
                    messages.extend(rows);
                }
            }
        }
    }

    // #688: emptyWindow editor-context cwd hint back-fill.
    //
    // Workspace-anchored sessions are already fully enriched via
    // `workspace.json`; this fallback only applies to
    // `globalStorage/emptyWindowChatSessions/*` where there is no
    // workspace. We scan the *materialised* `state.requests[*].result
    // .metadata.renderedUserMessage[*].text` for the first
    // `<editorContext>` block and surface its file's parent directory as
    // a hint cwd, tagging every row with `cwd_source =
    // copilot_chat:editor_context_hint` so analytics can distinguish hint
    // cwds from authoritative ones.
    if is_empty_window
        && enrichment.cwd.is_none()
        && let Some(hint_cwd) = editor_context_cwd_hint_from_state(&state)
    {
        let hint_branch = git_branch_for_cwd(&hint_cwd);
        enrichment.cwd = Some(hint_cwd.clone());
        enrichment.git_branch = hint_branch.clone();
        enrichment.cwd_source = Some(CWD_SOURCE_EDITOR_CONTEXT_HINT);
        for msg in &mut messages {
            if msg.cwd.is_none() {
                msg.cwd = Some(hint_cwd.clone());
            }
            if msg.git_branch.is_none() {
                msg.git_branch.clone_from(&hint_branch);
            }
            if msg.cwd_source.is_none() {
                msg.cwd_source = Some(CWD_SOURCE_EDITOR_CONTEXT_HINT.to_string());
            }
        }
    }

    (messages, new_offset)
}

/// Position of the byte immediately after the last `\n` in `content`,
/// clamped to be at least `start_offset`. If no newline exists past
/// `start_offset` the offset is left unchanged so the partial line is
/// re-read on the next tick — mirrors the original truncation contract
/// (`parse_jsonl_truncates_partial_final_line`).
fn last_complete_line_end_in_content(content: &str, start_offset: usize) -> usize {
    let from = start_offset.min(content.len());
    let after = &content.as_bytes()[from..];
    match after.iter().rposition(|&b| b == b'\n') {
        Some(i) => from + i + 1,
        None => start_offset,
    }
}

/// Stable emit key for a materialized request — `requestId` when present,
/// else a synthetic key derived from the in-array index. The reducer
/// re-scans on every mutation, so the key must be stable across mutations
/// to the same request (otherwise a kind:1 patch on `result.metadata`
/// after the tokens already landed would re-emit).
fn request_emit_key(request: &serde_json::Value, idx: usize) -> String {
    if let Some(rid) = request.get("requestId").and_then(|v| v.as_str())
        && !rid.is_empty()
    {
        return format!("rid:{rid}");
    }
    if let Some(rid) = request
        .pointer("/result/metadata/requestId")
        .and_then(|v| v.as_str())
        && !rid.is_empty()
    {
        return format!("rid:{rid}");
    }
    if let Some(mid) = request
        .pointer("/result/metadata/modelMessageId")
        .and_then(|v| v.as_str())
        && !mid.is_empty()
    {
        return format!("mmid:{mid}");
    }
    format!("idx:{idx}")
}

/// Apply a single mutation-log line (`kind:0`, `kind:1`, or `kind:2`) to
/// the per-session reducer state. Lines without a recognised `kind` are
/// no-ops here — the caller routes those through the flat-record path.
///
/// Per ADR-0092 §2.3 v4:
/// - kind:0 — `v` is a session snapshot. Top-level keys are merged into
///   state (a later kind:1 patch can override individual fields).
/// - kind:1 — `v` is a value to set at JSON-Pointer-shaped path `k`.
///   Auto-creates intermediate arrays/objects when the path traverses
///   indices that haven't been allocated yet (`k=["requests",8,
///   "completionTokens"]` with state.requests.len() < 9).
/// - kind:2 — `v` is an array of items to append at `k`. Defaults to
///   `["requests"]` when `k` is missing or empty (the hand-trimmed
///   real-world fixtures used in the v3 unit tests, and a common shape
///   used by some Copilot Chat builds before they landed an explicit `k`).
fn apply_mutation(state: &mut serde_json::Value, line: &serde_json::Value) {
    let kind = match line.get("kind").and_then(|v| v.as_u64()) {
        Some(k) => k,
        None => return,
    };

    match kind {
        0 => {
            let Some(v) = line.get("v") else { return };
            // The snapshot is normally `{requests: [...], sessionId: "...",
            // ...}`. If it's not an object (older fixtures pass a string or
            // array on kind:0), leave state alone.
            if let Some(v_obj) = v.as_object()
                && let Some(state_obj) = state.as_object_mut()
            {
                for (k, val) in v_obj {
                    state_obj.insert(k.clone(), val.clone());
                }
            }
        }
        1 => {
            let Some(k_arr) = line.get("k").and_then(|k| k.as_array()) else {
                return;
            };
            let Some(v_val) = line.get("v") else { return };
            set_at_path(state, k_arr, v_val.clone());
        }
        2 => {
            let Some(items) = line.get("v").and_then(|x| x.as_array()) else {
                return;
            };
            let k_arr = line.get("k").and_then(|k| k.as_array());
            let path: Vec<serde_json::Value> = match k_arr {
                Some(arr) if !arr.is_empty() => arr.clone(),
                _ => vec![serde_json::Value::String("requests".to_string())],
            };
            append_at_path(state, &path, items);
        }
        _ => {}
    }
}

/// Set `value` at the JSON-Pointer-shaped `path` inside `state`, creating
/// any missing intermediate arrays/objects on the way. Numeric segments
/// address arrays (auto-grown with placeholders); string segments address
/// objects.
fn set_at_path(
    state: &mut serde_json::Value,
    path: &[serde_json::Value],
    value: serde_json::Value,
) {
    if path.is_empty() {
        *state = value;
        return;
    }
    let head = &path[0];
    let rest = &path[1..];

    if let Some(idx) = head.as_u64().map(|n| n as usize) {
        if !state.is_array() {
            *state = serde_json::Value::Array(Vec::new());
        }
        let arr = state.as_array_mut().expect("just ensured array");
        while arr.len() <= idx {
            arr.push(placeholder_for_path(rest));
        }
        set_at_path(&mut arr[idx], rest, value);
    } else if let Some(key) = head.as_str() {
        if !state.is_object() {
            *state = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = state.as_object_mut().expect("just ensured object");
        if !obj.contains_key(key) {
            obj.insert(key.to_string(), placeholder_for_path(rest));
        }
        set_at_path(obj.get_mut(key).expect("just inserted"), rest, value);
    }
}

/// Append `items` to the array at `path` inside `state`, creating any
/// missing intermediate containers. If `path` resolves to a non-array,
/// the call is a no-op (the upstream extension occasionally writes
/// `kind:2` patches that race with a `kind:1` overwrite of the same
/// path; preserving the new shape over a re-coerced array is safer).
fn append_at_path(
    state: &mut serde_json::Value,
    path: &[serde_json::Value],
    items: &[serde_json::Value],
) {
    if path.is_empty() {
        if let Some(arr) = state.as_array_mut() {
            for item in items {
                arr.push(item.clone());
            }
        }
        return;
    }
    let head = &path[0];
    let rest = &path[1..];

    if let Some(idx) = head.as_u64().map(|n| n as usize) {
        if !state.is_array() {
            *state = serde_json::Value::Array(Vec::new());
        }
        let arr = state.as_array_mut().expect("just ensured array");
        while arr.len() <= idx {
            arr.push(if rest.is_empty() {
                serde_json::Value::Array(Vec::new())
            } else {
                placeholder_for_path(rest)
            });
        }
        if rest.is_empty() {
            if let Some(target) = arr[idx].as_array_mut() {
                for item in items {
                    target.push(item.clone());
                }
            }
        } else {
            append_at_path(&mut arr[idx], rest, items);
        }
    } else if let Some(key) = head.as_str() {
        if !state.is_object() {
            *state = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = state.as_object_mut().expect("just ensured object");
        if !obj.contains_key(key) {
            obj.insert(
                key.to_string(),
                if rest.is_empty() {
                    serde_json::Value::Array(Vec::new())
                } else {
                    placeholder_for_path(rest)
                },
            );
        }
        if rest.is_empty() {
            if let Some(target) = obj.get_mut(key).and_then(|v| v.as_array_mut()) {
                for item in items {
                    target.push(item.clone());
                }
            }
        } else {
            append_at_path(obj.get_mut(key).expect("just inserted"), rest, items);
        }
    }
}

fn placeholder_for_path(rest: &[serde_json::Value]) -> serde_json::Value {
    match rest.first() {
        Some(seg) if seg.as_u64().is_some() => serde_json::Value::Array(Vec::new()),
        Some(_) => serde_json::Value::Object(serde_json::Map::new()),
        None => serde_json::Value::Null,
    }
}

/// Build the per-turn rows for a materialized request — up to two
/// `ParsedMessage`s: an optional user row (when `request.message` carries
/// any prompt text) and the assistant row (when token counts are
/// present). The deterministic UUIDs are keyed off `emit_key` (typically
/// the request's `requestId`), with a `:user` suffix on the user-row
/// key so a future re-emit on the same request never collides with the
/// assistant row.
///
/// Mirrors the `claude_code` per-role emit shape: same `session_id` on
/// both rows; the assistant row's `parent_uuid` references the user
/// row's `uuid` so the prompt-classifier's existing pairing logic — and
/// any downstream aggregation that walks the user→assistant edge —
/// works without changes.
fn build_messages_for_request(
    path: &Path,
    request: &serde_json::Value,
    tokens: TokenSet,
    session_id: Option<&str>,
    session_default_model: Option<&str>,
    emit_key: &str,
    enrichment: &SessionEnrichment,
) -> Vec<ParsedMessage> {
    let model = extract_model_id(request).or_else(|| session_default_model.map(|s| s.to_string()));
    let timestamp = extract_timestamp(request);
    let tool_data = extract_tool_data(request);

    let path_key = path.display().to_string();
    let sid = session_id.unwrap_or(path_key.as_str());
    let assistant_uuid = deterministic_uuid_for_key(sid, &path_key, emit_key);

    let mut rows = Vec::with_capacity(2);

    let user_row = extract_user_message_text(request).map(|text| {
        let user_emit_key = format!("{emit_key}:user");
        let user_uuid = deterministic_uuid_for_key(sid, &path_key, &user_emit_key);
        let classification = crate::hooks::classify_prompt_detailed(&text);
        let (prompt_category, prompt_category_source, prompt_category_confidence) =
            match classification {
                Some(c) => (
                    Some(c.category),
                    Some(c.source.to_string()),
                    Some(c.confidence.to_string()),
                ),
                None => (None, None, None),
            };
        ParsedMessage {
            uuid: user_uuid,
            session_id: session_id.map(String::from),
            timestamp: extract_user_timestamp(request),
            cwd: enrichment.cwd.clone(),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: enrichment.git_branch.clone(),
            repo_id: None,
            provider: "copilot_chat".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "n/a".to_string(),
            pricing_source: None,
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
            prompt_category,
            prompt_category_source,
            prompt_category_confidence,
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
            tool_files: Vec::new(),
            tool_outcomes: Vec::new(),
            cwd_source: enrichment.cwd_source.map(str::to_string),
            surface: Some(crate::surface::infer_copilot_chat_surface(path).to_string()),
        }
    });

    let parent_uuid = user_row.as_ref().map(|u| u.uuid.clone());
    if let Some(u) = user_row {
        rows.push(u);
    }

    rows.push(ParsedMessage {
        uuid: assistant_uuid,
        session_id: session_id.map(String::from),
        timestamp,
        cwd: enrichment.cwd.clone(),
        role: "assistant".to_string(),
        model,
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_creation_tokens: tokens.cache_write,
        cache_read_tokens: tokens.cache_read,
        git_branch: enrichment.git_branch.clone(),
        repo_id: None,
        provider: "copilot_chat".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        pricing_source: None,
        request_id: None,
        speed: None,
        cache_creation_1h_tokens: 0,
        web_search_requests: 0,
        prompt_category: None,
        prompt_category_source: None,
        prompt_category_confidence: None,
        tool_names: tool_data.names,
        tool_use_ids: tool_data.ids,
        tool_files: tool_data.files,
        tool_outcomes: Vec::new(),
        cwd_source: enrichment.cwd_source.map(str::to_string),
        surface: Some(crate::surface::infer_copilot_chat_surface(path).to_string()),
    });

    rows
}

fn deterministic_uuid_for_key(session_id: &str, path: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"copilot_chat:");
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(path.as_bytes());
    hasher.update(b"|key:");
    hasher.update(key.as_bytes());
    let hash = hasher.finalize();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
        ])
    )
}

fn parse_json_document(path: &Path, content: &str) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let new_offset = content.len();

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return (messages, new_offset);
    }

    let Ok(doc) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return (messages, new_offset);
    };

    let session_id = doc
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| session_id_for_path(path));

    let mut session_default_model = extract_session_default_model(&doc);
    let enrichment = SessionEnrichment::for_path(path);

    let records: Vec<&serde_json::Value> = flatten_records(&doc);

    for (index, record) in records.iter().enumerate() {
        if let Some(model) = extract_session_default_model(record) {
            session_default_model = Some(model);
        }
        let rows = build_message(
            path,
            record,
            session_id.as_deref(),
            session_default_model.as_deref(),
            index,
            &enrichment,
        );
        if rows.is_empty() {
            if !shape_matches_any(record) {
                log_unknown_shape_once(path, record);
            }
        } else {
            messages.extend(rows);
        }
    }

    (messages, new_offset)
}

/// Return the candidate records to try for token extraction.
///
/// Per ADR-0092 §2.3: the on-disk shapes wrap their per-message records
/// inside an envelope key. Three are known:
///
/// * `{ "kind": N, "v": [ ... ] }` — JSONL line written by recent VS Code
///   builds. Each item in `v` is a request/response record carrying tokens
///   under one of the four shapes from §2.3 (typically
///   `result.metadata.{promptTokens,outputTokens}`).
/// * `{ "requests": [ ... ] }` — `.json` document written by the same
///   extension as a session snapshot.
/// * `{ "messages": [ ... ] }` — older `.json` document shape, retained
///   for back-compat with the synthetic fixtures used by §2.3 v1.
///
/// If none of the envelope keys are present (or `v` is an object rather
/// than an array, as on the `kind: 0` manifest line that carries
/// session-level metadata), fall back to treating the value itself as a
/// flat record. This preserves the v1 flat-line shape that the unit
/// fixtures and the budi-cursor integration tests rely on.
fn flatten_records(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    for key in ["v", "requests", "messages"] {
        if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
            return arr.iter().collect();
        }
    }
    vec![value]
}

/// Build per-turn rows for a flat-line / JSON-document record. Returns
/// the same shape as [`build_messages_for_request`] (up to two rows: an
/// optional user row, then the assistant row), keyed off `index` for
/// flat-line back-compat. An empty `Vec` means the record carried no
/// extractable tokens — the caller checks `shape_matches_any` to decide
/// whether to log an unknown-shape warning.
fn build_message(
    path: &Path,
    record: &serde_json::Value,
    session_id: Option<&str>,
    session_default_model: Option<&str>,
    index: usize,
    enrichment: &SessionEnrichment,
) -> Vec<ParsedMessage> {
    let Some(tokens) = extract_tokens(record) else {
        return Vec::new();
    };

    let model = extract_model_id(record).or_else(|| session_default_model.map(|s| s.to_string()));

    let timestamp = extract_timestamp(record);
    let tool_data = extract_tool_data(record);

    let path_key = path.display().to_string();
    let sid = session_id.unwrap_or(path_key.as_str());
    let assistant_uuid = deterministic_uuid(sid, &path_key, index);

    let mut rows = Vec::with_capacity(2);

    let user_row = extract_user_message_text(record).map(|text| {
        // Use a derived key so the user row's deterministic UUID never
        // collides with the assistant row's UUID at the same index.
        let user_uuid = deterministic_uuid_for_key(sid, &path_key, &format!("idx:{index}:user"));
        let classification = crate::hooks::classify_prompt_detailed(&text);
        let (prompt_category, prompt_category_source, prompt_category_confidence) =
            match classification {
                Some(c) => (
                    Some(c.category),
                    Some(c.source.to_string()),
                    Some(c.confidence.to_string()),
                ),
                None => (None, None, None),
            };
        ParsedMessage {
            uuid: user_uuid,
            session_id: session_id.map(String::from),
            timestamp: extract_user_timestamp(record),
            cwd: enrichment.cwd.clone(),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: enrichment.git_branch.clone(),
            repo_id: None,
            provider: "copilot_chat".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "n/a".to_string(),
            pricing_source: None,
            request_id: None,
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
            prompt_category,
            prompt_category_source,
            prompt_category_confidence,
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
            tool_files: Vec::new(),
            tool_outcomes: Vec::new(),
            cwd_source: enrichment.cwd_source.map(str::to_string),
            surface: Some(crate::surface::infer_copilot_chat_surface(path).to_string()),
        }
    });

    let parent_uuid = user_row.as_ref().map(|u| u.uuid.clone());
    if let Some(u) = user_row {
        rows.push(u);
    }

    rows.push(ParsedMessage {
        uuid: assistant_uuid,
        session_id: session_id.map(String::from),
        timestamp,
        cwd: enrichment.cwd.clone(),
        role: "assistant".to_string(),
        model,
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_creation_tokens: tokens.cache_write,
        cache_read_tokens: tokens.cache_read,
        git_branch: enrichment.git_branch.clone(),
        repo_id: None,
        provider: "copilot_chat".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        pricing_source: None,
        request_id: None,
        speed: None,
        cache_creation_1h_tokens: 0,
        web_search_requests: 0,
        prompt_category: None,
        prompt_category_source: None,
        prompt_category_confidence: None,
        tool_names: tool_data.names,
        tool_use_ids: tool_data.ids,
        tool_files: tool_data.files,
        tool_outcomes: Vec::new(),
        cwd_source: enrichment.cwd_source.map(str::to_string),
        surface: Some(crate::surface::infer_copilot_chat_surface(path).to_string()),
    });

    rows
}

/// Return tokens for the first shape (in §2.3 order) where both input and
/// output token counts are non-zero. ADR-0092 §2.3 — partial matches do not
/// count, EXCEPT for the output-only fallback (§2.3.v3) which is tried
/// last and emits a row with `input = 0`.
fn extract_tokens(record: &serde_json::Value) -> Option<TokenSet> {
    if let Some(t) = extract_tokens_vscode_delta(record) {
        return Some(t);
    }
    if let Some(t) = extract_tokens_copilot_cli(record) {
        return Some(t);
    }
    if let Some(t) = extract_tokens_legacy(record) {
        return Some(t);
    }
    if let Some(t) = extract_tokens_feb_2026(record) {
        return Some(t);
    }
    // Output-only fallback — must be tried after the four full-pair shapes
    // so a record that legitimately carries both keys never lands here.
    if let Some(t) = extract_tokens_completion_only(record) {
        return Some(t);
    }
    None
}

/// VS Code delta shape — top-level `promptTokens` / `outputTokens`,
/// optional `cacheReadTokens` / `cacheWriteTokens`.
fn extract_tokens_vscode_delta(record: &serde_json::Value) -> Option<TokenSet> {
    let input = record.get("promptTokens")?.as_u64()?;
    let output = record.get("outputTokens")?.as_u64()?;
    if input == 0 || output == 0 {
        return None;
    }
    Some(TokenSet {
        input,
        output,
        cache_read: record
            .get("cacheReadTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: record
            .get("cacheWriteTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Copilot CLI-style shape — `modelMetrics.inputTokens` /
/// `modelMetrics.outputTokens`.
fn extract_tokens_copilot_cli(record: &serde_json::Value) -> Option<TokenSet> {
    let input = record.pointer("/modelMetrics/inputTokens")?.as_u64()?;
    let output = record.pointer("/modelMetrics/outputTokens")?.as_u64()?;
    if input == 0 || output == 0 {
        return None;
    }
    Some(TokenSet {
        input,
        output,
        cache_read: record
            .pointer("/modelMetrics/cacheReadTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: record
            .pointer("/modelMetrics/cacheWriteTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Legacy `usage.*` shape — `usage.promptTokens` / `usage.completionTokens`.
fn extract_tokens_legacy(record: &serde_json::Value) -> Option<TokenSet> {
    let usage = record.get("usage")?;
    let input = usage.get("promptTokens")?.as_u64()?;
    let output = usage.get("completionTokens")?.as_u64()?;
    if input == 0 || output == 0 {
        return None;
    }
    Some(TokenSet {
        input,
        output,
        cache_read: usage
            .get("cacheReadInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: usage
            .get("cacheCreationInputTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Feb-2026+ shape — `result.metadata.promptTokens` /
/// `result.metadata.outputTokens`.
fn extract_tokens_feb_2026(record: &serde_json::Value) -> Option<TokenSet> {
    let meta = record.pointer("/result/metadata")?;
    let input = meta.get("promptTokens")?.as_u64()?;
    let output = meta.get("outputTokens")?.as_u64()?;
    if input == 0 || output == 0 {
        return None;
    }
    Some(TokenSet {
        input,
        output,
        cache_read: meta
            .get("cacheReadTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write: meta
            .get("cacheWriteTokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Output-only fallback shape (v3, 8.4.x amendment to ADR-0092 §2.3) —
/// top-level `completionTokens` with no input-token counterpart. Captures
/// VS Code Copilot Chat builds that persist response-token counts but not
/// prompt-token counts anywhere on disk.
///
/// The emitted [`TokenSet`] has `input = 0` and a non-zero `output`. This
/// is the only shape where the both-non-zero invariant from the four
/// full-pair shapes is intentionally relaxed; it is also the only shape
/// allowed to produce a row with zero input tokens. Downstream cost
/// pricing handles `input = 0` correctly (cost is output-only at the
/// manifest layer); the Billing API reconciliation worker truths the
/// dollar number up to the real bill on the next tick for users with a
/// configured PAT (see §3 of ADR-0092).
fn extract_tokens_completion_only(record: &serde_json::Value) -> Option<TokenSet> {
    let output = record.get("completionTokens")?.as_u64()?;
    if output == 0 {
        return None;
    }
    Some(TokenSet {
        input: 0,
        output,
        cache_read: 0,
        cache_write: 0,
    })
}

/// Returns true when *any* of the five token-key shapes can be located on
/// this record, even if the values are zero. Used to distinguish "valid
/// shape, just an empty record" (no warn) from "shape we don't recognize"
/// (warn-once via [`log_unknown_shape_once`]).
fn shape_matches_any(record: &serde_json::Value) -> bool {
    record.get("promptTokens").is_some()
        || record.get("outputTokens").is_some()
        || record.get("completionTokens").is_some()
        || record.pointer("/modelMetrics/inputTokens").is_some()
        || record.pointer("/modelMetrics/outputTokens").is_some()
        || record.pointer("/usage/promptTokens").is_some()
        || record.pointer("/usage/completionTokens").is_some()
        || record.pointer("/result/metadata/promptTokens").is_some()
        || record.pointer("/result/metadata/outputTokens").is_some()
}

/// Resolve the model id Budi attributes a Copilot Chat row to.
///
/// Three-step fallback (ADR-0092 §2.4, amended by #685):
///
/// 1. `result.metadata.resolvedModel` — when shape-clean and known to
///    the pricing manifest (or its alias overlay), this is the actual
///    model GitHub routed to. Catches non-Anthropic `auto` routes
///    (e.g. `grok-code-fast-1`) and dated Anthropic forms
///    (`claude-haiku-4-5-20251001`) without us guessing. Skips
///    GPU-fleet codes (`capi-noe-ptuc-h200-oswe-vscode-prime`) because
///    they're never in the manifest.
/// 2. `modelId` (top-level or under `result.metadata`) — the
///    user-facing label, with the optional `copilot/` prefix stripped.
///    Returned as-is unless it is the literal `"auto"` router
///    placeholder.
/// 3. `auto` + `agent.id` static-table fallback (ADR-0092 §2.4.1,
///    R1.4 / #671) — optimistic guess based on the agent surface the
///    user invoked. Demoted from primary to last-resort by #685.
///
/// The §2.4.1 table stays intact as the safety net for sessions where
/// `resolvedModel` is missing or fleet-shaped; only its precedence
/// changes.
fn extract_model_id(record: &serde_json::Value) -> Option<String> {
    // (1) Prefer the actual server-side resolution when we can prove
    // it prices cleanly. Manifest membership (direct or via alias) is
    // the gate — fleet codes aren't in the manifest, so they fall
    // through to step (2)/(3) without a wrong guess.
    if let Some(resolved) = record
        .pointer("/result/metadata/resolvedModel")
        .and_then(|v| v.as_str())
    {
        let candidate = strip_copilot_prefix(resolved);
        if is_clean_model_shape(candidate) && crate::pricing::is_known(candidate) {
            return Some(candidate.to_string());
        }
    }

    // (2) User-facing modelId. Strip the optional `copilot/` prefix
    // and return as-is unless it's the `"auto"` router placeholder.
    let raw = record
        .get("modelId")
        .and_then(|v| v.as_str())
        .or_else(|| {
            record
                .pointer("/result/metadata/modelId")
                .and_then(|v| v.as_str())
        })
        .map(|s| strip_copilot_prefix(s).to_string())?;

    if raw != "auto" {
        return Some(raw);
    }

    // (3) Router-placeholder fallback. The user picked "auto" in the
    // model selector and GitHub Copilot Chat picked the actual model
    // server-side, but step (1) couldn't pin it down (resolvedModel
    // missing, fleet-shaped, or unknown to the manifest). ADR-0092
    // §2.4.1 resolves "auto" via `agent.id`; on miss, leave the value
    // as "auto" so the row still emits (priced through to
    // `no_pricing`, trued up by §3 reconciliation on the next tick
    // for individually-licensed users).
    if let Some(agent_id) = record.pointer("/agent/id").and_then(|v| v.as_str())
        && let Some(resolved) = resolve_auto_model_id(agent_id)
    {
        return Some(resolved.to_string());
    }
    Some(raw)
}

/// Cheap shape filter for a candidate model id pulled from
/// `result.metadata.resolvedModel`: lowercase ASCII letters, digits,
/// and hyphens, leading character must be a letter. Rejects values
/// with dots, slashes, or uppercase characters before the manifest
/// probe. Note this does NOT exclude GPU-fleet codes — those share
/// the same shape — so the manifest membership check in step (1) is
/// the real safety guard.
fn is_clean_model_shape(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn strip_copilot_prefix(model: &str) -> &str {
    model.strip_prefix("copilot/").unwrap_or(model)
}

/// Map a Copilot Chat `agent.id` to the model `auto` most likely resolves
/// to for that agent at the time of the 8.4.1 patch. Returns `None` when
/// the agent id is missing or unrecognised so the caller can preserve
/// the literal `"auto"` model id.
///
/// Per ADR-0092 §2.4.1: GitHub does not contractually pin which model
/// `auto` resolves to — the table reflects the **current most-common
/// default** for each `agent.id` and is the optimistic-resolution arm of
/// the three options laid out in #671. Wrong guesses only affect
/// org-managed-license users (the §3 Billing API reconciliation trues up
/// dollars for individually-licensed users on the next tick), so the cost
/// of a stale entry is bounded.
///
/// The mapping table lives inline here for 8.4.1; ADR-0092 §2.4.1 calls
/// out migration to a `model_aliases` block on the LiteLLM manifest cache
/// (Option C in #671) as the longer-term home — defer to 9.0.0 unless the
/// inline table proves unreliable in practice.
///
/// Adding a new agent id is a one-line edit + ADR-0092 §2.4.1 amendment in
/// the same PR (per the §2.6-style "contract and code never disagree"
/// rule).
fn resolve_auto_model_id(agent_id: &str) -> Option<&'static str> {
    match agent_id {
        // Edit-mode / agent-mode chat. Copilot has routed to Claude Sonnet
        // for code-edit-heavy turns since the GPT-5 / Sonnet 4.5 dual-default
        // rollout in early 2026.
        "github.copilot.editsAgent" | "github.copilot.codingAgent" => Some("claude-sonnet-4-5"),
        // `@workspace`, `@terminal`, and the plain chat panel. `gpt-4.1` is
        // Copilot's prevailing default for non-edit chat completions.
        "github.copilot.workspaceAgent"
        | "github.copilot.terminalAgent"
        | "github.copilot.default"
        | "github.copilot.chat-default"
        | "github.copilot" => Some("gpt-4.1"),
        _ => None,
    }
}

/// Try to pluck a per-session default model from a record or document. This
/// is best-effort — Copilot Chat embeds the active model on the session
/// manifest (`messages[].modelId` or top-level `defaultModelId` /
/// `currentModel`), and we record the most recent value seen so a later
/// record without a model id can still be priced.
fn extract_session_default_model(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value
        .get("defaultModelId")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("currentModel").and_then(|v| v.as_str()))
        .or_else(|| value.get("modelId").and_then(|v| v.as_str()))
        .or_else(|| {
            value
                .pointer("/result/metadata/modelId")
                .and_then(|v| v.as_str())
        })
    {
        return Some(strip_copilot_prefix(s).to_string());
    }
    None
}

/// Extract the user-prompt text for a request — the text the human typed
/// that produced the assistant turn. Looks at `request.message.text` and
/// `request.message.parts[*].text` (text-typed parts only, joined in
/// order). Returns `None` if neither shape carries any text.
///
/// Per ADR-0092 §2.3 the canonical user-prompt text lives on
/// `request.message`, never on `result.metadata.renderedUserMessage`
/// (which is a *re-rendered* copy with the editor-context envelope
/// already wrapped around it). The renderer is decorative; the source
/// is `message`.
fn extract_user_message_text(request: &serde_json::Value) -> Option<String> {
    let message = request.get("message")?;
    if let Some(s) = message.get("text").and_then(|v| v.as_str())
        && !s.is_empty()
    {
        return Some(s.to_string());
    }
    if let Some(parts) = message.get("parts").and_then(|v| v.as_array()) {
        let joined: String = parts
            .iter()
            .filter_map(|p| {
                // A text-typed part carries `{kind: "text", text: "..."}` in
                // some builds and a bare `{text: "..."}` in others; we
                // accept either as long as a `text` string exists. Non-text
                // parts (e.g. file references, ephemeral cache markers)
                // expose other keys and have no `text`, so they're skipped.
                p.get("text").and_then(|v| v.as_str())
            })
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    None
}

/// Bundle of tool-attribution data extracted from a single
/// materialized request. Returned by [`extract_tool_data`] and piped
/// straight into the assistant row's `tool_*` slots.
///
/// `names` and `ids` align positionally — `names[i]` and `ids[i]`
/// describe the same `toolCallRounds[r].toolCalls[c]` entry, in
/// flattened-walk order. `files` is the union of file paths
/// projected from per-tool argument shapes (no positional tie-back to
/// names/ids — downstream consumers want the "files this turn touched"
/// set, not the per-call tuple).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ToolData {
    names: Vec<String>,
    ids: Vec<String>,
    files: Vec<String>,
}

/// Walk `result.metadata.toolCallRounds[].toolCalls[]` and surface tool
/// names, tool-use ids, and file-path arguments for the file-attribution
/// pipeline (#687).
///
/// Per ADR-0092 §2.3 v4, each `toolCallRounds[r]` carries a `toolCalls[]`
/// array whose entries look like
/// `{ "name": "replace_string_in_file", "arguments": { "filePath": "…" }, "id": "<call-id>" }`.
/// We flatten across all rounds, preserve order, and project file paths
/// per a small per-tool table:
///
/// * `replace_string_in_file` / `multi_replace_string_in_file` /
///   `create_file` / `read_file` → `arguments.filePath`
/// * `apply_patch` → walk `arguments.patches[].filePath` first, then
///   fall back to `arguments.filePath` for single-file patches.
/// * Unknown tools → no file extracted (don't fail the parse).
///
/// Adding a new tool name is a one-line edit + ADR-0092 §2.3 amendment in
/// the same PR (per the §2.6-style "contract and code never disagree"
/// rule).
///
/// Names/ids are emitted positionally per call (one entry per toolCall,
/// even if duplicated across rounds) so a future "tool fan-out per turn"
/// metric stays correct. Empty `name`/`id` fields are skipped — pre-stub
/// records (kind:2 inflight) reach this fn before the model has named
/// the call, and emitting an empty-string entry would corrupt the
/// downstream `tag_value` check that filters out blanks.
fn extract_tool_data(request: &serde_json::Value) -> ToolData {
    let Some(rounds) = request
        .pointer("/result/metadata/toolCallRounds")
        .and_then(|v| v.as_array())
    else {
        return ToolData::default();
    };

    let mut names = Vec::new();
    let mut ids = Vec::new();
    let mut files = Vec::new();

    for round in rounds {
        let Some(calls) = round.get("toolCalls").and_then(|v| v.as_array()) else {
            continue;
        };
        for call in calls {
            let name = call
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            if let Some(n) = name {
                names.push(n.to_string());
            }
            if let Some(i) = id {
                ids.push(i.to_string());
            }
            if let Some(n) = name {
                let args = call.get("arguments").unwrap_or(&serde_json::Value::Null);
                project_tool_files(n, args, &mut files);
            }
        }
    }

    ToolData { names, ids, files }
}

/// Per-tool projection of `arguments` to candidate file paths. URI-shaped
/// values (`file://`, `vscode-vfs://`) are converted to local-path form
/// using the same [`uri_to_local_path`] helper #681 wired in for
/// workspace.json — keeps the on-wire format consistent with cwd
/// enrichment so the downstream `FileEnricher` privacy filter sees a
/// single canonical shape.
fn project_tool_files(tool_name: &str, args: &serde_json::Value, out: &mut Vec<String>) {
    match tool_name {
        "replace_string_in_file" | "multi_replace_string_in_file" | "create_file" | "read_file" => {
            push_file_path(args.get("filePath"), out);
        }
        "apply_patch" => {
            // Multi-file patches expose a `patches[]` array; single-file
            // patches use the same top-level `filePath` shape as the
            // edit tools. Try both — apply_patch shapes have shifted at
            // least once across github.copilot-chat releases.
            if let Some(patches) = args.get("patches").and_then(|v| v.as_array()) {
                for patch in patches {
                    push_file_path(patch.get("filePath"), out);
                }
            } else {
                push_file_path(args.get("filePath"), out);
            }
        }
        _ => {
            // Unknown tool — don't fail the parse, just emit no path.
            // ADR-0092 §2.3 amendments add new arms here in lockstep
            // with parser shape bumps.
        }
    }
}

fn push_file_path(v: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(s) = v.and_then(|v| v.as_str()) else {
        return;
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return;
    }
    // Strip URI schemes the same way #681 normalises workspace.json `folder`.
    // Plain absolute or relative paths pass through unchanged so the
    // downstream `file_attribution` normaliser sees its existing shapes.
    let normalized = if trimmed.contains("://") {
        uri_to_local_path(trimmed)
    } else {
        trimmed.to_string()
    };
    if !normalized.is_empty() {
        out.push(normalized);
    }
}

/// Best-effort timestamp for a user row — prefer the message-side timestamp
/// (when the prompt was submitted) over the request-level one (when the
/// assistant response started). Falls back to the request timestamp.
fn extract_user_timestamp(request: &serde_json::Value) -> DateTime<Utc> {
    if let Some(v) = request.pointer("/message/timestamp")
        && let Some(ms) = v.as_i64()
        && let Some(ts) = DateTime::from_timestamp_millis(ms)
    {
        return ts;
    }
    extract_timestamp(request)
}

fn extract_timestamp(record: &serde_json::Value) -> DateTime<Utc> {
    let candidates = [
        "/timestamp",
        "/createdAt",
        "/result/metadata/timestamp",
        "/requestStartTime",
    ];
    for ptr in candidates {
        if let Some(v) = record.pointer(ptr) {
            if let Some(ts) = v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()) {
                return ts;
            }
            if let Some(ms) = v.as_i64()
                && let Some(ts) = DateTime::from_timestamp_millis(ms)
            {
                return ts;
            }
        }
    }
    DateTime::from_timestamp(0, 0).expect("epoch is valid")
}

fn session_id_for_path(path: &Path) -> Option<String> {
    // chatSessions/<session-id>.{json,jsonl} — pull the file stem.
    if let Some(parent) = path.parent()
        && let Some(parent_name) = parent.file_name().and_then(|n| n.to_str())
        && parent_name.eq_ignore_ascii_case("chatSessions")
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
    {
        return Some(stem.to_string());
    }
    None
}

fn deterministic_uuid(session_id: &str, path: &str, index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"copilot_chat:");
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(path.as_bytes());
    hasher.update(b"|");
    hasher.update(index.to_le_bytes());
    let hash = hasher.finalize();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]),
        u16::from_be_bytes([hash[4], hash[5]]),
        u16::from_be_bytes([hash[6], hash[7]]),
        u16::from_be_bytes([hash[8], hash[9]]),
        u64::from_be_bytes([
            0, 0, hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
        ])
    )
}

fn byte_offset_to_line_index(content: &str, offset: usize) -> usize {
    let bound = offset.min(content.len());
    content[..bound].bytes().filter(|&b| b == b'\n').count()
}

/// `(file_path, sorted_top_level_keys)` signature used to deduplicate
/// unknown-shape warnings.
type UnknownShapeSignature = (String, Vec<String>);

fn log_unknown_shape_once(path: &Path, record: &serde_json::Value) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static SEEN: OnceLock<Mutex<HashSet<UnknownShapeSignature>>> = OnceLock::new();

    let mut keys: Vec<String> = match record.as_object() {
        Some(map) => map.keys().cloned().collect(),
        None => return,
    };
    keys.sort();

    let path_key = path.display().to_string();
    let signature = (path_key.clone(), keys.clone());

    let lock = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = lock
        .lock()
        .expect("copilot_chat unknown-shape mutex poisoned");
    if !seen.insert(signature) {
        return;
    }
    drop(seen);

    tracing::warn!(
        target: "budi_core::providers::copilot_chat",
        path = %path_key,
        keys = ?keys,
        "copilot_chat_unknown_record_shape"
    );
}

// ---------------------------------------------------------------------------
// JetBrains host (ADR-0093) — implementation lives in `jetbrains` submodule.
// See `crates/budi-core/src/providers/copilot_chat/jetbrains.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
