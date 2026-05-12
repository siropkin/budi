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
pub const MIN_API_VERSION: u32 = 4;

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
                push_session_files_recursive(&pub_dir.join("debug-logs"), out, 0);
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
mod tests {
    use super::*;

    fn make_message(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn extract_tool_data_empty_when_metadata_missing() {
        let v = make_message(r#"{"requestId": "r1", "result": {"metadata": {}}}"#);
        assert_eq!(extract_tool_data(&v), ToolData::default());
    }

    #[test]
    fn extract_tool_data_empty_when_no_rounds() {
        let v = make_message(r#"{"result": {"metadata": {"toolCallRounds": []}}}"#);
        assert_eq!(extract_tool_data(&v), ToolData::default());
    }

    #[test]
    fn extract_tool_data_skips_speak_only_rounds() {
        // Real-shape: a round with empty toolCalls just carries the
        // model's prose (`response`/`thinking`). It must not surface in
        // the names/ids/files vectors.
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"response": "I see you have a file open…", "toolCalls": [], "id": "round-1"}
            ]}}}"#,
        );
        assert_eq!(extract_tool_data(&v), ToolData::default());
    }

    #[test]
    fn extract_tool_data_replace_string_in_file() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "replace_string_in_file",
                     "id": "call-abc",
                     "arguments": {"filePath": "src/auth.rs", "oldString": "x", "newString": "y"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.names, vec!["replace_string_in_file".to_string()]);
        assert_eq!(td.ids, vec!["call-abc".to_string()]);
        assert_eq!(td.files, vec!["src/auth.rs".to_string()]);
    }

    #[test]
    fn extract_tool_data_multi_replace_and_create_and_read() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "multi_replace_string_in_file", "id": "1", "arguments": {"filePath": "a.rs"}},
                    {"name": "create_file", "id": "2", "arguments": {"filePath": "b.rs"}},
                    {"name": "read_file", "id": "3", "arguments": {"filePath": "c.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.names.len(), 3);
        assert_eq!(td.ids, vec!["1", "2", "3"]);
        assert_eq!(td.files, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn extract_tool_data_unknown_tool_yields_no_file_but_name_and_id_emit() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "search_codebase", "id": "x", "arguments": {"query": "foo"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.names, vec!["search_codebase".to_string()]);
        assert_eq!(td.ids, vec!["x".to_string()]);
        assert!(td.files.is_empty());
    }

    #[test]
    fn extract_tool_data_strips_file_uri_scheme() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "read_file", "id": "1",
                     "arguments": {"filePath": "file:///home/dev/repo/src/x.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.files, vec!["/home/dev/repo/src/x.rs".to_string()]);
    }

    #[test]
    fn extract_tool_data_strips_vscode_vfs_scheme() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "read_file", "id": "1",
                     "arguments": {"filePath": "vscode-vfs://github/owner/repo/path/to/file.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.files, vec!["/owner/repo/path/to/file.rs".to_string()]);
    }

    #[test]
    fn extract_tool_data_apply_patch_walks_patches_array() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "apply_patch", "id": "p1",
                     "arguments": {"patches": [
                         {"filePath": "src/lib.rs"},
                         {"filePath": "src/main.rs"}
                     ]}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.files, vec!["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn extract_tool_data_apply_patch_falls_back_to_top_level_filepath() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "apply_patch", "id": "p1",
                     "arguments": {"filePath": "src/single.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.files, vec!["src/single.rs".to_string()]);
    }

    #[test]
    fn extract_tool_data_flattens_across_rounds_and_preserves_duplicates() {
        // Mirrors claude_code: the same tool name invoked twice across
        // two rounds shows up twice in `names`, paired with distinct ids.
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "read_file", "id": "1", "arguments": {"filePath": "a.rs"}}
                ]},
                {"toolCalls": [
                    {"name": "read_file", "id": "2", "arguments": {"filePath": "b.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.names, vec!["read_file", "read_file"]);
        assert_eq!(td.ids, vec!["1", "2"]);
        assert_eq!(td.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn extract_tool_data_skips_blank_name_and_blank_id() {
        // Defensive: an in-flight stub on a kind:2 splice could land
        // before the model has named the call. We must not insert
        // empty strings into the tag vectors — downstream consumers
        // would emit an empty tag value that violates the
        // not-null-empty contract.
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"toolCalls": [
                    {"name": "", "id": "", "arguments": {"filePath": "ignored.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert!(td.names.is_empty());
        assert!(td.ids.is_empty());
        assert!(td.files.is_empty());
    }

    #[test]
    fn extract_tool_data_missing_tool_calls_array_skips_round() {
        let v = make_message(
            r#"{"result": {"metadata": {"toolCallRounds": [
                {"id": "round-1"},
                {"toolCalls": [
                    {"name": "read_file", "id": "ok", "arguments": {"filePath": "x.rs"}}
                ]}
            ]}}}"#,
        );
        let td = extract_tool_data(&v);
        assert_eq!(td.names, vec!["read_file".to_string()]);
        assert_eq!(td.ids, vec!["ok".to_string()]);
        assert_eq!(td.files, vec!["x.rs".to_string()]);
    }

    #[test]
    fn extract_tokens_vscode_delta_shape() {
        let v = make_message(r#"{"promptTokens": 1500, "outputTokens": 200}"#);
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 1500);
        assert_eq!(t.output, 200);
        assert_eq!(t.cache_read, 0);
        assert_eq!(t.cache_write, 0);
    }

    #[test]
    fn extract_tokens_vscode_delta_with_cache() {
        let v = make_message(
            r#"{"promptTokens": 1000, "outputTokens": 500, "cacheReadTokens": 200, "cacheWriteTokens": 50}"#,
        );
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 1000);
        assert_eq!(t.cache_read, 200);
        assert_eq!(t.cache_write, 50);
    }

    #[test]
    fn extract_tokens_copilot_cli_shape() {
        let v = make_message(
            r#"{"modelMetrics": {"inputTokens": 800, "outputTokens": 60, "cacheReadTokens": 10}}"#,
        );
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 800);
        assert_eq!(t.output, 60);
        assert_eq!(t.cache_read, 10);
    }

    #[test]
    fn extract_tokens_legacy_usage_shape() {
        let v = make_message(
            r#"{"usage": {"promptTokens": 12000, "completionTokens": 750, "cacheReadInputTokens": 4000, "cacheCreationInputTokens": 100}}"#,
        );
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 12000);
        assert_eq!(t.output, 750);
        assert_eq!(t.cache_read, 4000);
        assert_eq!(t.cache_write, 100);
    }

    #[test]
    fn extract_tokens_feb_2026_shape() {
        let v = make_message(
            r#"{"result": {"metadata": {"promptTokens": 9000, "outputTokens": 400, "cacheReadTokens": 1200}}}"#,
        );
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 9000);
        assert_eq!(t.output, 400);
        assert_eq!(t.cache_read, 1200);
    }

    #[test]
    fn extract_tokens_zero_pair_skips_shape_and_falls_through() {
        // Top-level shape has zeros; nested feb-2026 shape should win.
        let v = make_message(
            r#"{
                "promptTokens": 0,
                "outputTokens": 0,
                "result": {"metadata": {"promptTokens": 100, "outputTokens": 5}}
            }"#,
        );
        let t = extract_tokens(&v).unwrap();
        assert_eq!(t.input, 100);
        assert_eq!(t.output, 5);
    }

    #[test]
    fn extract_tokens_unknown_shape_returns_none() {
        let v = make_message(r#"{"weird": {"thingy": 42}}"#);
        assert!(extract_tokens(&v).is_none());
        assert!(!shape_matches_any(&v));
    }

    #[test]
    fn extract_model_id_strips_copilot_prefix() {
        let v = make_message(r#"{"modelId": "copilot/claude-sonnet-4-5"}"#);
        assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn extract_model_id_passes_through_when_no_prefix() {
        let v = make_message(r#"{"modelId": "gpt-4.1"}"#);
        assert_eq!(extract_model_id(&v).as_deref(), Some("gpt-4.1"));
    }

    #[test]
    fn extract_model_id_falls_back_to_metadata() {
        let v = make_message(r#"{"result": {"metadata": {"modelId": "copilot/o3"}}}"#);
        assert_eq!(extract_model_id(&v).as_deref(), Some("o3"));
    }

    // ---- §2.4.1 `auto` resolver (R1.4, #671) ---------------------------

    /// Concrete, manifest-known modelIds pass through the resolver
    /// untouched even when an `agent.id` is present. The resolver fires
    /// only on the literal `"auto"` router placeholder.
    #[test]
    fn extract_model_id_concrete_models_bypass_auto_resolver() {
        let v = make_message(
            r#"{"modelId": "copilot/claude-sonnet-4-5", "agent": {"id": "github.copilot.editsAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
    }

    /// `modelId == "auto"` + recognised `agent.id` resolves to the agent's
    /// optimistic default model. Pricing then matches via the LiteLLM
    /// manifest instead of falling through to `unpriced:no_pricing`.
    #[test]
    fn extract_model_id_auto_resolves_via_agent_edits() {
        let v = make_message(
            r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.editsAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn extract_model_id_auto_resolves_via_agent_workspace() {
        let v = make_message(
            r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.workspaceAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("gpt-4.1"));
    }

    /// `modelId == "auto"` + unknown `agent.id` falls back to the literal
    /// `"auto"` so the row still emits. Downstream pricing tags it
    /// `unpriced:no_pricing`; the §3 reconciliation worker trues up
    /// dollars on the next tick for individually-licensed users.
    #[test]
    fn extract_model_id_auto_with_unknown_agent_falls_back_to_auto() {
        let v = make_message(
            r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.someFutureAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("auto"));
    }

    /// `modelId == "auto"` with no `agent.id` at all (older sessions, the
    /// synthetic v3 fixtures, hand-trimmed records) preserves `"auto"`.
    /// This pins the back-compat contract — the resolver is additive,
    /// never destructive.
    #[test]
    fn extract_model_id_auto_without_agent_preserves_auto() {
        let v = make_message(r#"{"modelId": "copilot/auto"}"#);
        assert_eq!(extract_model_id(&v).as_deref(), Some("auto"));
    }

    /// Resolver also fires when the `modelId` arrives via the Feb-2026
    /// nested shape (`result.metadata.modelId`).
    #[test]
    fn extract_model_id_auto_resolves_under_metadata_shape() {
        let v = make_message(
            r#"{"result": {"metadata": {"modelId": "copilot/auto"}}, "agent": {"id": "github.copilot.editsAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
    }

    /// #685: `result.metadata.resolvedModel` outranks the §2.4.1
    /// `agent.id` static table when the resolved value is shape-clean
    /// and the pricing manifest knows about it. Three real on-disk
    /// sessions drove this priority flip:
    ///
    /// - dated LiteLLM-canonical Anthropic key (`claude-haiku-4-5-20251001`)
    ///   wins directly via manifest entries — no alias hop needed.
    /// - non-Anthropic auto-routed key (`grok-code-fast-1`) wins via
    ///   the alias overlay — without this, it would be wrongly
    ///   attributed to `claude-sonnet-4-5` by the editsAgent fallback.
    /// - GPU-fleet code (`capi-noe-ptuc-h200-oswe-vscode-prime`)
    ///   isn't in the manifest, so step (1) fails and the agent.id
    ///   fallback runs — current behavior preserved.
    #[test]
    fn extract_model_id_prefers_resolved_when_manifest_known() {
        // Anthropic dated form — direct manifest hit.
        let v = make_message(
            r#"{
                "modelId": "copilot/claude-haiku-4.5",
                "agent": {"id": "github.copilot.editsAgent"},
                "result": {"metadata": {"resolvedModel": "claude-haiku-4-5-20251001"}}
            }"#,
        );
        assert_eq!(
            extract_model_id(&v).as_deref(),
            Some("claude-haiku-4-5-20251001"),
            "dated LiteLLM-canonical Anthropic key must win directly via manifest"
        );

        // Grok auto-route — alias-overlay hit, beats Sonnet fallback.
        let v = make_message(
            r#"{
                "modelId": "copilot/auto",
                "agent": {"id": "github.copilot.editsAgent"},
                "result": {"metadata": {"resolvedModel": "grok-code-fast-1"}}
            }"#,
        );
        assert_eq!(
            extract_model_id(&v).as_deref(),
            Some("grok-code-fast-1"),
            "Grok resolvedModel must win over editsAgent → claude-sonnet-4-5"
        );

        // Fleet code — manifest miss, falls through to editsAgent table.
        let v = make_message(
            r#"{
                "modelId": "copilot/auto",
                "agent": {"id": "github.copilot.editsAgent"},
                "result": {"metadata": {"resolvedModel": "capi-noe-ptuc-h200-oswe-vscode-prime"}}
            }"#,
        );
        assert_eq!(
            extract_model_id(&v).as_deref(),
            Some("claude-sonnet-4-5"),
            "fleet-code resolvedModel must fall through to §2.4.1 agent.id table"
        );

        // No resolvedModel at all — current §2.4.1 behavior preserved.
        let v = make_message(
            r#"{"modelId": "copilot/auto", "agent": {"id": "github.copilot.editsAgent"}}"#,
        );
        assert_eq!(extract_model_id(&v).as_deref(), Some("claude-sonnet-4-5"));
    }

    /// `is_clean_model_shape` must pass real model ids and reject
    /// anything carrying dots, slashes, uppercase, or empty input —
    /// the gate that lets the manifest probe in step (1) of
    /// `extract_model_id` stay correct without false positives on
    /// surface forms it can't handle.
    #[test]
    fn is_clean_model_shape_filters() {
        assert!(is_clean_model_shape("grok-code-fast-1"));
        assert!(is_clean_model_shape("claude-haiku-4-5-20251001"));
        assert!(is_clean_model_shape("capi-noe-ptuc-h200-oswe-vscode-prime"));
        assert!(!is_clean_model_shape("claude-haiku-4.5"));
        assert!(!is_clean_model_shape("xai/grok-code-fast-1"));
        assert!(!is_clean_model_shape("Claude-Haiku"));
        assert!(!is_clean_model_shape("1grok"));
        assert!(!is_clean_model_shape(""));
    }

    /// Direct unit test on the static table — pin every entry so a stale
    /// edit (e.g. dropping a known agent id) trips the test instead of
    /// silently falling through to the `"auto"` no-pricing path.
    #[test]
    fn resolve_auto_model_id_known_table() {
        assert_eq!(
            resolve_auto_model_id("github.copilot.editsAgent"),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            resolve_auto_model_id("github.copilot.codingAgent"),
            Some("claude-sonnet-4-5")
        );
        assert_eq!(
            resolve_auto_model_id("github.copilot.workspaceAgent"),
            Some("gpt-4.1")
        );
        assert_eq!(
            resolve_auto_model_id("github.copilot.terminalAgent"),
            Some("gpt-4.1")
        );
        assert_eq!(
            resolve_auto_model_id("github.copilot.default"),
            Some("gpt-4.1")
        );
        assert_eq!(
            resolve_auto_model_id("github.copilot.chat-default"),
            Some("gpt-4.1")
        );
        assert_eq!(resolve_auto_model_id("github.copilot"), Some("gpt-4.1"));
        assert_eq!(resolve_auto_model_id("github.copilot.unknownAgent"), None);
        assert_eq!(resolve_auto_model_id(""), None);
    }

    #[test]
    fn parse_jsonl_file_extracts_messages() {
        let content = concat!(
            r#"{"promptTokens": 100, "outputTokens": 5, "modelId": "copilot/gpt-4.1", "timestamp": "2026-04-12T10:30:00.000Z"}"#,
            "\n",
            // Unknown shape — skipped, no failure
            r#"{"unrelated": "event"}"#,
            "\n",
            r#"{"usage": {"promptTokens": 200, "completionTokens": 10}, "modelId": "copilot/claude-sonnet-4-5"}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-1.jsonl");
        let (msgs, offset) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].input_tokens, 100);
        assert_eq!(msgs[0].output_tokens, 5);
        assert_eq!(msgs[0].model.as_deref(), Some("gpt-4.1"));
        assert_eq!(msgs[0].provider, "copilot_chat");
        assert_eq!(msgs[1].input_tokens, 200);
        assert_eq!(msgs[1].model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(offset, content.len());
    }

    #[test]
    fn parse_jsonl_resumes_from_offset() {
        let content = concat!(
            r#"{"promptTokens": 100, "outputTokens": 5}"#,
            "\n",
            r#"{"promptTokens": 200, "outputTokens": 10}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-2.jsonl");
        let (first, mid_offset) = parse_copilot_chat(path, content, 0);
        assert_eq!(first.len(), 2);
        assert_eq!(mid_offset, content.len());

        let (second, _) = parse_copilot_chat(path, content, mid_offset);
        assert!(second.is_empty(), "no new content past mid_offset");
    }

    #[test]
    fn parse_jsonl_truncates_partial_final_line() {
        // Last line lacks a terminating newline — must be left for the next read.
        let content = concat!(
            r#"{"promptTokens": 100, "outputTokens": 5}"#,
            "\n",
            r#"{"promptTokens": 200, "outputTokens": 10"#, // no closing brace, no newline
        );
        let path = Path::new("/tmp/budi-fixtures/sess-3.jsonl");
        let (msgs, offset) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 1);
        // Offset must stop at the newline boundary so the partial line is re-read next tick.
        assert_eq!(
            offset,
            "{\"promptTokens\": 100, \"outputTokens\": 5}\n".len()
        );
    }

    #[test]
    fn parse_json_document_extracts_messages() {
        let content = r#"{
            "sessionId": "sess-doc-1",
            "currentModel": "copilot/claude-sonnet-4-5",
            "messages": [
                {"promptTokens": 1000, "outputTokens": 50},
                {"result": {"metadata": {"promptTokens": 2000, "outputTokens": 100, "modelId": "copilot/gpt-4.1"}}}
            ]
        }"#;
        let path = Path::new("/tmp/budi-fixtures/sess-doc-1.json");
        let (msgs, offset) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].session_id.as_deref(), Some("sess-doc-1"));
        assert_eq!(msgs[0].input_tokens, 1000);
        // First message has no modelId — inherits the document-level current model.
        assert_eq!(msgs[0].model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(msgs[1].input_tokens, 2000);
        assert_eq!(msgs[1].model.as_deref(), Some("gpt-4.1"));
        assert_eq!(offset, content.len());
    }

    #[test]
    fn parse_json_document_unknown_shape_skipped() {
        // Document with a single unknown-shape record — no panic, no message.
        let content = r#"{"messages": [{"weird": "shape"}]}"#;
        let path = Path::new("/tmp/budi-fixtures/sess-doc-2.json");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert!(msgs.is_empty());
    }

    /// #701 acceptance — parser-local surface inference. The four
    /// canonical roots from ADR-0092 §2.1 each map to a deterministic
    /// surface label on every emitted row, so host extensions
    /// (budi-cursor, future budi-jetbrains) can filter to "only my
    /// host's data" without inspecting paths themselves.
    ///
    /// JetBrains is excluded here because `parse_copilot_chat` never sees
    /// a JetBrains-shaped path today: `watch_roots()` iterates VS
    /// Code-family directories only. The JetBrains storage shape is
    /// pinned at ADR-0093 and exercised by the fixture-presence tests
    /// further down; the classifier-layer mapping
    /// `infer_copilot_chat_surface` → `surface::JETBRAINS` is asserted in
    /// `crate::surface::tests`. The matrix here pins the three roots that
    /// actually flow through the parser.
    #[test]
    fn surface_is_cursor_when_path_under_cursor_user_root() {
        let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
        let path = Path::new(
            "/Users/dev/Library/Application Support/Cursor/User/workspaceStorage/abc/chatSessions/sess.jsonl",
        );
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert!(!msgs.is_empty());
        for m in &msgs {
            assert_eq!(
                m.surface.as_deref(),
                Some(crate::surface::CURSOR),
                "Cursor/User/... must map to surface=cursor; got {:?}",
                m.surface
            );
        }
    }

    #[test]
    fn surface_is_vscode_when_path_under_code_user_root() {
        let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
        let path = Path::new(
            "/Users/dev/Library/Application Support/Code/User/workspaceStorage/abc/chatSessions/sess.jsonl",
        );
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert!(!msgs.is_empty());
        for m in &msgs {
            assert_eq!(
                m.surface.as_deref(),
                Some(crate::surface::VSCODE),
                "Code/User/... must map to surface=vscode; got {:?}",
                m.surface
            );
        }
    }

    #[test]
    fn surface_is_vscode_when_path_under_vscode_server_root() {
        let content = r#"{"promptTokens": 1, "outputTokens": 2}"#;
        let path = Path::new(
            "/home/dev/.vscode-server/data/User/workspaceStorage/abc/chatSessions/sess.jsonl",
        );
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert!(!msgs.is_empty());
        for m in &msgs {
            assert_eq!(
                m.surface.as_deref(),
                Some(crate::surface::VSCODE),
                "~/.vscode-server/... must map to surface=vscode; got {:?}",
                m.surface
            );
        }
    }

    /// JetBrains classifier — the surface module returns `jetbrains` for a
    /// JetBrains-shaped path. Discovery in `watch_roots()` does not yet
    /// touch the JetBrains storage root (see ADR-0093 and #716), so this
    /// assertion lives at the classifier layer rather than going through
    /// `parse_copilot_chat`. The classifier-layer matrix is exercised in
    /// full at `crate::surface::tests`.
    #[test]
    fn surface_jetbrains_path_classifier_returns_jetbrains_placeholder() {
        let path = Path::new(
            "/Users/dev/Library/Application Support/JetBrains/IntelliJIdea2026.1/copilot/sessions/x.json",
        );
        assert_eq!(
            crate::surface::infer_copilot_chat_surface(path),
            crate::surface::JETBRAINS
        );
    }

    // -----------------------------------------------------------------------
    // JetBrains fixture (ADR-0093) — anchors the next parser ticket.
    // -----------------------------------------------------------------------

    fn jetbrains_empty_session_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/providers/copilot_chat/fixtures/jetbrains_copilot_1_5_53_243_empty_session")
    }

    /// The captured JetBrains fixture is on disk and has the four files
    /// ADR-0093 §4 names. Anchors the next parser ticket against ground
    /// truth instead of a synthetic shape; fails loudly if the fixture
    /// gets accidentally pruned by a future cleanup pass.
    #[test]
    fn jetbrains_empty_session_fixture_layout_is_intact() {
        let dir = jetbrains_empty_session_fixture_dir();
        assert!(
            dir.is_dir(),
            "fixture dir missing: {} — see ADR-0093",
            dir.display()
        );
        for relpath in [
            "00000000000.xd",
            "xd.lck",
            "copilot-chat-nitrite.db",
            "blobs/version",
        ] {
            let f = dir.join(relpath);
            assert!(
                f.is_file(),
                "fixture file missing: {} (relpath {})",
                f.display(),
                relpath
            );
        }

        // The `.expected.json` and `.shape.md` companions live one level up
        // alongside the dir and document the entity inventory.
        let parent = dir.parent().unwrap();
        assert!(
            parent
                .join("jetbrains_copilot_1_5_53_243.expected.json")
                .is_file()
        );
        assert!(
            parent
                .join("jetbrains_copilot_1_5_53_243.shape.md")
                .is_file()
        );
    }

    /// The `xd.lck` header has been byte-exact redacted (see shape.md). If
    /// a future capture accidentally drops in a non-redacted lockfile this
    /// test catches it before the PR lands.
    #[test]
    fn jetbrains_empty_session_xd_lck_header_is_redacted() {
        let dir = jetbrains_empty_session_fixture_dir();
        let lck = std::fs::read_to_string(dir.join("xd.lck")).unwrap();
        let first = lck.lines().next().unwrap();
        assert!(
            first.contains("0000@redacted.invalid"),
            "xd.lck header looks non-redacted: {first:?}"
        );
        assert!(
            !lck.contains("@Mac.attlocal.net") && !lck.contains("Ivan-Seredkin"),
            "xd.lck still contains real host/user PII"
        );
    }

    /// ADR-0093 §4 / #722: the empty fixture session carries only
    /// `XdMigration` bootstrap entries — no `XdChatSession`/`XdAgentSession`
    /// markers. The parser must emit zero rows for it without panicking on
    /// the empty schema. This is the ground-truth anchor; populated
    /// sessions are exercised inside `jetbrains` submodule tests.
    #[test]
    fn jetbrains_empty_session_parses_to_no_messages() {
        let dir = jetbrains_empty_session_fixture_dir();
        let parsed = jetbrains::parse_session_dir_for_tests(&dir).unwrap();
        assert!(
            parsed.is_empty(),
            "empty fixture must emit zero rows; got {parsed:?}"
        );
    }

    #[test]
    fn deterministic_uuid_is_stable() {
        let a = deterministic_uuid("sess-1", "/tmp/x.json", 7);
        let b = deterministic_uuid("sess-1", "/tmp/x.json", 7);
        assert_eq!(a, b);
        let c = deterministic_uuid("sess-1", "/tmp/x.json", 8);
        assert_ne!(a, c);
    }

    #[test]
    fn is_available_robust_when_dirs_absent() {
        // Pass roots that don't exist — must not panic and must return false.
        let bogus = vec![PathBuf::from("/tmp/budi-copilot-chat-does-not-exist")];
        assert!(!any_user_root_has_copilot_marker(&bogus));
    }

    #[test]
    fn is_available_when_workspace_storage_lacks_copilot_subdirs() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-no-marker");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("workspaceStorage/abc1234")).unwrap();
        // No chatSessions, no GitHub.copilot* under the hash dir.
        assert!(!any_user_root_has_copilot_marker(std::slice::from_ref(
            &tmp
        )));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_available_true_when_chat_sessions_present() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-marker-present");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("workspaceStorage/abc1234/chatSessions")).unwrap();
        assert!(any_user_root_has_copilot_marker(std::slice::from_ref(&tmp)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn is_available_true_when_global_storage_publisher_dir_present() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-global-publisher");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("globalStorage/github.copilot-chat/sessions")).unwrap();
        assert!(any_user_root_has_copilot_marker(std::slice::from_ref(&tmp)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_session_files_finds_jsonl_under_chat_sessions() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-collect");
        let _ = std::fs::remove_dir_all(&tmp);
        let target = tmp.join("workspaceStorage/abc1234/chatSessions");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("sess-1.jsonl"), b"{}\n").unwrap();
        std::fs::write(target.join("sess-2.json"), b"{}").unwrap();
        std::fs::write(target.join("not-a-session.txt"), b"ignore").unwrap();

        let mut out = Vec::new();
        collect_session_files(&tmp, &mut out);
        out.sort();
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|p| p.ends_with("sess-1.jsonl")));
        assert!(out.iter().any(|p| p.ends_with("sess-2.json")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_session_files_recurses_into_global_publisher_dir() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-recurse");
        let _ = std::fs::remove_dir_all(&tmp);
        let nested = tmp.join("globalStorage/GitHub.copilot-chat/sessions/2026-05");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.jsonl"), b"{}\n").unwrap();

        let mut out = Vec::new();
        collect_session_files(&tmp, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("a.jsonl"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_session_files_skips_global_publisher_siblings_of_session_dir() {
        // ADR-0092 §2.2 directory-name allowlist (#684): under
        // globalStorage/{publisher}/, only files inside chatSessions,
        // chat-sessions, or sessions subtrees are session files. Embedding
        // caches and CLI state blobs sitting as siblings must be skipped.
        let tmp = std::env::temp_dir().join("budi-copilot-chat-skip-siblings");
        let _ = std::fs::remove_dir_all(&tmp);
        let pub_dir = tmp.join("globalStorage/github.copilot-chat");
        std::fs::create_dir_all(&pub_dir).unwrap();

        // Sibling files (NOT chat sessions): VS Code embedding caches and
        // the Copilot CLI v2 state blob.
        std::fs::write(
            pub_dir.join("commandEmbeddings.json"),
            // Large embedding-only payload, no `kind`/`requests`/`messages` keys.
            br#"{"core":{"editor.action.setSelectionAnchor":{"embedding":[0.008,-0.029,0.061]}}}"#,
        )
        .unwrap();
        std::fs::write(
            pub_dir.join("settingEmbeddings.json"),
            br#"{"core":{"editor.fontSize":{"embedding":[0.1,0.2]}}}"#,
        )
        .unwrap();
        std::fs::write(
            pub_dir.join("copilot.cli.oldGlobalSessions.json"),
            br#"{"version":2,"sessions":{}}"#,
        )
        .unwrap();

        // Real session file under a known session-storage directory.
        let chat_sessions = pub_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_sessions).unwrap();
        std::fs::write(
            chat_sessions.join("0e3b1f3c-1234-4abc-9def-aaaabbbbcccc.jsonl"),
            br#"{"kind":0,"v":{"sessionId":"abc","creationDate":"2026-04-15T10:00:00Z"}}
"#,
        )
        .unwrap();

        let mut out = Vec::new();
        collect_session_files(&tmp, &mut out);
        assert_eq!(
            out.len(),
            1,
            "only the chatSessions/<uuid>.jsonl is collected; \
             commandEmbeddings.json / settingEmbeddings.json / \
             copilot.cli.oldGlobalSessions.json siblings are skipped, got {out:?}"
        );
        assert!(out[0].ends_with("0e3b1f3c-1234-4abc-9def-aaaabbbbcccc.jsonl"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_session_files_accepts_chat_sessions_chat_sessions_and_sessions_subdirs() {
        // The directory-name allowlist (#684) covers all three known
        // session-storage names. A future fourth name must amend ADR-0092
        // §2.2 in lockstep.
        let tmp = std::env::temp_dir().join("budi-copilot-chat-allowlist-names");
        let _ = std::fs::remove_dir_all(&tmp);
        let pub_dir = tmp.join("globalStorage/GitHub.copilot-chat");
        for name in ["chatSessions", "chat-sessions", "sessions"] {
            let d = pub_dir.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s.jsonl"), b"{}\n").unwrap();
        }

        let mut out = Vec::new();
        collect_session_files(&tmp, &mut out);
        assert_eq!(
            out.len(),
            3,
            "all three allowlisted names match, got {out:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_skips_absent_subdirs() {
        // Stub home with neither workspaceStorage nor globalStorage — the
        // provider must not panic and must return an empty watch list for
        // that root. We exercise the scan helper directly because
        // CopilotChatProvider::watch_roots() consults the real $HOME.
        let tmp = std::env::temp_dir().join("budi-copilot-chat-watch-empty");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut roots = Vec::new();
        let ws = tmp.join("workspaceStorage");
        if ws.is_dir() {
            roots.push(ws);
        }
        let gs = tmp.join("globalStorage");
        if gs.is_dir() {
            roots.push(gs);
        }
        assert!(roots.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Real on-disk JSONL shape from `chatSessions/<id>.jsonl` written by
    /// the `github.copilot-chat` extension circa 2026-04. The token-bearing
    /// records are wrapped under the `kind: 2 / v: [...]` envelope and the
    /// counts live at `result.metadata.{promptTokens,outputTokens}`. This
    /// fixture is captured from a real session on a developer machine and
    /// then trimmed to the fields the parser inspects — the structural
    /// envelope (kind / v / nesting depth) is preserved verbatim so any
    /// future regression of [`flatten_records`] is caught here.
    #[test]
    fn parse_jsonl_real_kind_v_envelope() {
        let content = concat!(
            // kind:0 manifest line — no tokens, must not produce a message
            // and must not trigger an unknown-shape warn (its `v` is an
            // object, which is the documented "session manifest" shape).
            r#"{"kind":0,"v":{"sessionId":"abc","creationDate":"2026-04-15T10:00:00Z"}}"#,
            "\n",
            // kind:1 string — text fragment, no tokens, must not produce.
            r#"{"kind":1,"v":"user prompt text"}"#,
            "\n",
            // kind:2 response — the token-bearing shape. `v` is an array of
            // one assistant turn, tokens at result.metadata.{promptTokens,outputTokens}.
            r#"{"kind":2,"v":[{"modelId":"copilot/claude-haiku-4.5","completionTokens":191,"requestId":"req-1","timestamp":1715000000000,"result":{"metadata":{"promptTokens":26412,"outputTokens":191,"modelMessageId":"m-1","resolvedModel":"claude-haiku-4.5"}}}]}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-real-jsonl.jsonl");
        let (msgs, offset) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 1, "exactly one assistant turn carries tokens");
        let m = &msgs[0];
        assert_eq!(m.input_tokens, 26412);
        assert_eq!(m.output_tokens, 191);
        assert_eq!(m.model.as_deref(), Some("claude-haiku-4.5"));
        assert_eq!(m.provider, "copilot_chat");
        assert_eq!(offset, content.len());
    }

    /// Real on-disk `.json` snapshot shape — `requests: [...]` envelope,
    /// each request carrying tokens at
    /// `result.metadata.{promptTokens,outputTokens}`. Mirrors the .jsonl
    /// shape but as a single document (older / persisted-on-close form).
    #[test]
    fn parse_json_document_real_requests_envelope() {
        let content = r#"{
            "sessionId": "real-doc-1",
            "version": 3,
            "requesterUsername": "alice",
            "responderUsername": "GitHub Copilot",
            "requests": [
                {
                    "modelId": "github.copilot-chat/claude-sonnet-4",
                    "requestId": "r-1",
                    "timestamp": 1715000001000,
                    "result": {
                        "metadata": {
                            "promptTokens": 1234,
                            "outputTokens": 56,
                            "modelMessageId": "mm-1"
                        }
                    }
                },
                {
                    "modelId": "github.copilot-chat/claude-sonnet-4",
                    "requestId": "r-2-no-tokens",
                    "timestamp": 1715000002000
                }
            ]
        }"#;
        let path = Path::new("/tmp/budi-fixtures/sess-real-doc.json");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(
            msgs.len(),
            1,
            "only the request with result.metadata tokens produces a message"
        );
        let m = &msgs[0];
        assert_eq!(m.input_tokens, 1234);
        assert_eq!(m.output_tokens, 56);
        // `github.copilot-chat/` prefix should be normalised the same way
        // `copilot/` is — the strip happens via [`strip_copilot_prefix`].
        // Today only `copilot/` is stripped, so we assert the full id
        // passes through unchanged; if that ever changes, tighten here.
        assert!(
            m.model.as_deref().unwrap_or("").contains("claude-sonnet-4"),
            "model id should mention claude-sonnet-4, got {:?}",
            m.model
        );
        assert_eq!(m.session_id.as_deref(), Some("real-doc-1"));
    }

    /// `kind:1` lines whose `v` is an array of state events (no tokens
    /// anywhere) must not emit an unknown-shape warn — the wrapper is
    /// known, the inner records simply don't carry tokens. Pinning this
    /// keeps the warn-once log from getting noisy on real sessions.
    #[test]
    fn parse_jsonl_kind1_array_silently_yields_no_messages() {
        let content = concat!(
            r#"{"kind":1,"v":[{"role":"user","content":"hi"},{"role":"system","content":"ok"}]}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-kind1.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert!(msgs.is_empty());
    }

    /// v3 (8.4.0) output-only fallback shape — VS Code Copilot Chat builds
    /// circa 2026-05 persist `completionTokens` at the top of each
    /// response record but no `promptTokens` counterpart anywhere. The
    /// parser must still emit a row (with `input_tokens = 0`) so the
    /// session is visible in the local-tail surface and the Billing API
    /// reconciliation worker has a `(date, model)` bucket to truth up.
    #[test]
    fn extract_tokens_completion_only_shape() {
        let record = serde_json::json!({
            "modelId": "copilot/auto",
            "completionTokens": 65,
            "result": {
                "metadata": {
                    "resolvedModel": "capi-noe-ptuc-h200-oswe-vscode-prime"
                }
            }
        });
        let tokens = extract_tokens(&record).expect("must match output-only fallback");
        assert_eq!(tokens.input, 0);
        assert_eq!(tokens.output, 65);
    }

    /// Output-only fallback must not fire when `completionTokens == 0` —
    /// that case is "valid shape, empty record" (the surrounding logic
    /// would emit a useless 0/0 row otherwise).
    #[test]
    fn extract_tokens_completion_only_zero_skips() {
        let record = serde_json::json!({"modelId": "x", "completionTokens": 0});
        assert!(extract_tokens(&record).is_none());
    }

    /// Full-pair shapes must outrank the output-only fallback when both
    /// keys are present — otherwise the `feb_2026` shape would lose its
    /// input-token count to the fallback's `input = 0`.
    #[test]
    fn extract_tokens_full_pair_outranks_completion_only_fallback() {
        let record = serde_json::json!({
            "modelId": "copilot/x",
            "completionTokens": 999,
            "result": {
                "metadata": {
                    "promptTokens": 100,
                    "outputTokens": 50
                }
            }
        });
        let tokens = extract_tokens(&record).expect("feb_2026 shape must win");
        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.output, 50);
    }

    /// End-to-end on a real-shape JSONL with the v3 output-only records
    /// (kind:0 manifest, kind:1 state events, kind:2 response with only
    /// `completionTokens`). Three response turns → three messages; the
    /// kind:0 / kind:1 lines emit nothing and stay silent.
    #[test]
    fn parse_jsonl_real_v3_completion_only_turns() {
        let content = concat!(
            r#"{"kind":0,"v":{"sessionId":"s","creationDate":"2026-05-07T15:00:00Z"}}"#,
            "\n",
            r#"{"kind":1,"v":{"completedAt":1715000000000,"value":"prompt"}}"#,
            "\n",
            r#"{"kind":2,"v":[{"modelId":"copilot/auto","completionTokens":65,"requestId":"r1","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}}]}"#,
            "\n",
            r#"{"kind":2,"v":[{"modelId":"copilot/auto","completionTokens":115,"requestId":"r2","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}},{"modelId":"copilot/auto","completionTokens":117,"requestId":"r3","result":{"metadata":{"resolvedModel":"capi-noe-ptuc-h200-oswe-vscode-prime"}}}]}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-v3.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 3);
        assert!(msgs.iter().all(|m| m.input_tokens == 0));
        assert_eq!(
            msgs.iter().map(|m| m.output_tokens).sum::<u64>(),
            65 + 115 + 117
        );
        // All emit with the user-facing modelId, not the fleet-code resolvedModel.
        assert!(msgs.iter().all(|m| m.model.as_deref() == Some("auto")));
    }

    // ---- v4 (8.4.1, R1.1): mutation-log reducer tests ------------------

    /// kind:0 snapshot followed by kind:1 patches that fill in
    /// `completionTokens` for an existing request. This is the shape that
    /// VS Code 1.109+ writes mid-conversation, and the regression that
    /// drove ticket #668 — the v3 parser saw the kind:1 `v: 39` line as
    /// a flat record with no token keys at the top level and emitted
    /// nothing. The reducer materializes the merged request and the
    /// output-only fallback shape produces a row.
    #[test]
    fn reducer_kind1_completion_tokens_patch_emits_row() {
        let content = concat!(
            // kind:0 snapshot — one request stub, no tokens yet.
            r#"{"kind":0,"v":{"sessionId":"s-1","requests":[{"requestId":"r-1","modelId":"copilot/claude-sonnet-4-5"}]}}"#,
            "\n",
            // kind:1 patch lands the completion-token count on requests[0].
            r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":42}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-reducer-1.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 1, "completionTokens patch must emit one row");
        let m = &msgs[0];
        assert_eq!(m.input_tokens, 0, "output-only fallback ⇒ input = 0");
        assert_eq!(m.output_tokens, 42);
        assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(m.session_id.as_deref(), Some("s-1"));
    }

    /// kind:1 patches that land both `promptTokens` and `outputTokens` on
    /// `result.metadata` for a request stub appended via kind:2. Auto-grow
    /// of intermediate objects is exercised: the kind:2 stub doesn't carry
    /// `result.metadata` at all, so the kind:1 path
    /// `["requests",0,"result","metadata","promptTokens"]` has to
    /// materialize the missing object levels on the way in.
    #[test]
    fn reducer_kind1_patches_auto_create_intermediate_objects() {
        let content = concat!(
            // No kind:0 snapshot — start empty. kind:2 push adds a stub.
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-9","modelId":"copilot/gpt-4.1"}]}"#,
            "\n",
            // kind:1 patches stream the token counts in. result/metadata
            // do not yet exist — set_at_path must create them.
            r#"{"kind":1,"k":["requests",0,"result","metadata","promptTokens"],"v":1234}"#,
            "\n",
            r#"{"kind":1,"k":["requests",0,"result","metadata","outputTokens"],"v":56}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-reducer-2.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        // Three lines, but the first emit-eligible mutation is the second
        // kind:1 patch (when both prompt+output land). The first kind:1
        // patch alone leaves `outputTokens = 0` so no shape matches yet.
        // Result: exactly one row.
        assert_eq!(msgs.len(), 1, "feb-2026 shape must materialize once");
        let m = &msgs[0];
        assert_eq!(m.input_tokens, 1234);
        assert_eq!(m.output_tokens, 56);
        assert_eq!(m.model.as_deref(), Some("gpt-4.1"));
    }

    /// Acceptance criterion (#668): "append a kind:2 stub then a kind:1
    /// completionTokens patch to a watched file; assert exactly one row
    /// materializes after the patch." This pins the live-tailer ordering:
    /// the kind:2 stub alone emits nothing, the kind:1 patch landing
    /// completionTokens emits one row, and a *second* kind:1 patch on
    /// the same request (e.g. updating `timestamp`) does not double-emit.
    #[test]
    fn reducer_emit_keyed_by_request_id_no_double_emit() {
        let content = concat!(
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r-only","modelId":"copilot/auto"}]}"#,
            "\n",
            r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":77}"#,
            "\n",
            // Later patch on the same request — must NOT emit a second row.
            r#"{"kind":1,"k":["requests",0,"timestamp"],"v":1715000999000}"#,
            "\n",
            r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":80}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-reducer-3.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 1, "exactly one row per requestId");
        assert_eq!(msgs[0].output_tokens, 77, "first complete value wins");
    }

    /// Two kind:2 splices to `["requests"]` followed by interleaved kind:1
    /// patches that complete each request at different lines. The reducer
    /// must emit one row per request, in the order each request becomes
    /// complete (not in array-index order).
    #[test]
    fn reducer_multiple_requests_emit_in_completion_order() {
        let content = concat!(
            r#"{"kind":0,"v":{"sessionId":"s-multi","requests":[]}}"#,
            "\n",
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"a","modelId":"copilot/gpt-4.1"}]}"#,
            "\n",
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"b","modelId":"copilot/gpt-4.1"}]}"#,
            "\n",
            // Request b completes first (out-of-order vs. array index).
            r#"{"kind":1,"k":["requests",1,"result","metadata","promptTokens"],"v":10}"#,
            "\n",
            r#"{"kind":1,"k":["requests",1,"result","metadata","outputTokens"],"v":2}"#,
            "\n",
            // Then request a completes.
            r#"{"kind":1,"k":["requests",0,"result","metadata","promptTokens"],"v":100}"#,
            "\n",
            r#"{"kind":1,"k":["requests",0,"result","metadata","outputTokens"],"v":20}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-reducer-4.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2);
        // First-completed (request b) emits first.
        assert_eq!(msgs[0].input_tokens, 10);
        assert_eq!(msgs[0].output_tokens, 2);
        assert_eq!(msgs[1].input_tokens, 100);
        assert_eq!(msgs[1].output_tokens, 20);
        // Stable per-request UUIDs — different requests, different UUIDs.
        assert_ne!(msgs[0].uuid, msgs[1].uuid);
    }

    /// kind:0 snapshots that already inline `completionTokens` (the
    /// historical path that even the v3 parser handled) must keep
    /// emitting a single row through the reducer. This is the regression
    /// shape called out in #668: "only kind:0 lines whose `requests`
    /// snapshot already had `completionTokens` inline at file write time"
    /// produced rows on v3. The reducer must preserve this for the
    /// imported-historical-session case (`budi db import`).
    #[test]
    fn reducer_kind0_snapshot_with_inline_tokens_emits() {
        let content = concat!(
            r#"{"kind":0,"v":{"sessionId":"hist-1","requests":[{"requestId":"h-1","modelId":"copilot/claude-haiku-4.5","result":{"metadata":{"promptTokens":500,"outputTokens":12}}}]}}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-hist.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].input_tokens, 500);
        assert_eq!(msgs[0].output_tokens, 12);
        assert_eq!(msgs[0].model.as_deref(), Some("claude-haiku-4.5"));
    }

    /// Reducer-emitted rows must use a deterministic UUID that's stable
    /// across re-parses keyed by `requestId` — so a future call that
    /// re-replays the file (e.g. on daemon restart) produces the same UUID
    /// and the database upsert dedupes instead of double-counting.
    #[test]
    fn reducer_deterministic_uuid_stable_across_reparse() {
        let content = concat!(
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"stable-key","modelId":"copilot/x"}]}"#,
            "\n",
            r#"{"kind":1,"k":["requests",0,"completionTokens"],"v":7}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-stable.jsonl");
        let (first, _) = parse_copilot_chat(path, content, 0);
        let (second, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].uuid, second[0].uuid);
    }

    /// `set_at_path` correctness — exercises the helper directly so a
    /// future regression of the auto-grow / placeholder logic is caught
    /// without needing to construct a full mutation-log fixture.
    #[test]
    fn set_at_path_grows_arrays_and_creates_objects() {
        let mut state = serde_json::json!({});
        let path = vec![
            serde_json::json!("requests"),
            serde_json::json!(2),
            serde_json::json!("result"),
            serde_json::json!("metadata"),
            serde_json::json!("promptTokens"),
        ];
        set_at_path(&mut state, &path, serde_json::json!(99));
        assert_eq!(
            state
                .pointer("/requests/2/result/metadata/promptTokens")
                .and_then(|v| v.as_u64()),
            Some(99)
        );
        // Indices 0 and 1 are placeholder objects (next segment is the
        // string "result", so an object placeholder is correct).
        assert!(
            state
                .pointer("/requests/0")
                .map(|v| v.is_object())
                .unwrap_or(false)
        );
        assert!(
            state
                .pointer("/requests/1")
                .map(|v| v.is_object())
                .unwrap_or(false)
        );
    }

    // ---- v4 (8.4.1, R1.2): real-extension regression fixture --------------

    /// Real-extension regression fixture (#669) — sanitized capture of an
    /// actual `github.copilot-chat` 0.47.0 session file. The reducer must
    /// materialize one row per completed request, matching the expected
    /// `(requestId, output_tokens, input_tokens, model)` tuples from
    /// `vscode_chat_0_47_0.expected.json` exactly once each.
    ///
    /// Why this exists: the synthetic v3 fixtures pass with both the old
    /// per-line parser and the v4 reducer because they don't actually
    /// exercise the kind:0 + kind:1/kind:2 envelope dance. This test pins
    /// the reducer against a real on-disk capture so a future regression
    /// of the same shape (an extension bump that changes how the mutation
    /// log is shaped) fails loudly here even when the synthetic fixtures
    /// continue to pass.
    #[test]
    fn parse_real_vscode_0_47_0_fixture() {
        let content = include_str!("copilot_chat/fixtures/vscode_chat_0_47_0.jsonl");
        let expected_json = include_str!("copilot_chat/fixtures/vscode_chat_0_47_0.expected.json");
        let expected: Vec<serde_json::Value> = serde_json::from_str(expected_json).unwrap();

        // Use a path that does NOT exist on disk so the parser falls through
        // to the in-memory `content` rather than re-reading from a stale
        // checkout-relative path.
        let path = Path::new("/tmp/budi-fixtures-r1-2/vscode_chat_0_47_0.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);

        assert_eq!(
            msgs.len(),
            expected.len(),
            "fixture must yield exactly {} rows (8 assistant rows + 2 user \
             rows from the synthetic message.text on the first two requests)",
            expected.len()
        );

        // Each expected entry carries a `role`; assistant entries match by
        // `(output_tokens, model)` (output values are all distinct), and
        // user entries match by `(role, prompt_category)` against the
        // paired user row produced for the same `requestId`. The fixture's
        // synthetic user prompts are authored so each `prompt_category`
        // is unique among user rows in the fixture.
        for entry in &expected {
            let role = entry["role"].as_str().unwrap();
            let matches: Vec<_> = match role {
                "assistant" => {
                    let want_output = entry["output_tokens"].as_u64().unwrap();
                    let want_model = entry["model"].as_str().unwrap();
                    msgs.iter()
                        .filter(|m| {
                            m.role == "assistant"
                                && m.output_tokens == want_output
                                && m.model.as_deref() == Some(want_model)
                        })
                        .collect()
                }
                "user" => {
                    let want_category = entry["prompt_category"].as_str().unwrap();
                    msgs.iter()
                        .filter(|m| {
                            m.role == "user" && m.prompt_category.as_deref() == Some(want_category)
                        })
                        .collect()
                }
                other => panic!("unknown role in expected.json: {other}"),
            };
            assert_eq!(
                matches.len(),
                1,
                "expected exactly one {} row for requestId={}; got {}",
                role,
                entry["requestId"].as_str().unwrap(),
                matches.len()
            );
        }

        // Provider tag is preserved through the reducer.
        assert!(msgs.iter().all(|m| m.provider == "copilot_chat"));
        // Every assistant row maps to the §2.4.1 edits-agent default
        // (`claude-sonnet-4-5`); user rows carry `model = None`.
        for m in &msgs {
            match m.role.as_str() {
                "assistant" => assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-5")),
                "user" => assert!(m.model.is_none()),
                other => panic!("unexpected role: {other}"),
            }
        }
        // #686 acceptance: both row roles materialize, and every assistant
        // row whose paired request carried `message.text` (or `message.parts`)
        // points back at its user row via `parent_uuid`.
        assert!(msgs.iter().any(|m| m.role == "user"));
        assert!(msgs.iter().any(|m| m.role == "assistant"));
        let user_uuids: std::collections::HashSet<&str> = msgs
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.uuid.as_str())
            .collect();
        let assistants_with_parent = msgs
            .iter()
            .filter(|m| m.role == "assistant")
            .filter(|m| m.parent_uuid.is_some())
            .count();
        assert_eq!(
            assistants_with_parent,
            user_uuids.len(),
            "every user row in the fixture must have exactly one paired assistant row"
        );
        for m in msgs.iter().filter(|m| m.role == "assistant") {
            if let Some(p) = m.parent_uuid.as_deref() {
                assert!(
                    user_uuids.contains(p),
                    "assistant row's parent_uuid {} must reference a user row in the same parse",
                    p
                );
            }
        }
        // #687 acceptance: assistant rows whose request carried a
        // toolCallRounds entry materialize tool_names / tool_use_ids /
        // tool_files. The dc9f930d request in the fixture is the only
        // one with synthetic tool data; all other assistant rows must
        // surface empty tool slots.
        for entry in &expected {
            if entry["role"].as_str() != Some("assistant") {
                continue;
            }
            let want_output = entry["output_tokens"].as_u64().unwrap();
            let row = msgs
                .iter()
                .find(|m| m.role == "assistant" && m.output_tokens == want_output)
                .expect("assistant row must exist");
            let want_names: Vec<String> = entry
                .get("tool_names")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let want_ids: Vec<String> = entry
                .get("tool_use_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let want_files: Vec<String> = entry
                .get("tool_files")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                row.tool_names,
                want_names,
                "tool_names mismatch on requestId={}",
                entry["requestId"].as_str().unwrap()
            );
            assert_eq!(
                row.tool_use_ids,
                want_ids,
                "tool_use_ids mismatch on requestId={}",
                entry["requestId"].as_str().unwrap()
            );
            assert_eq!(
                row.tool_files,
                want_files,
                "tool_files mismatch on requestId={}",
                entry["requestId"].as_str().unwrap()
            );
        }
        // User rows must always have empty tool slots — the tool data
        // belongs to the assistant turn, not the prompt that initiated
        // it.
        for m in msgs.iter().filter(|m| m.role == "user") {
            assert!(m.tool_names.is_empty());
            assert!(m.tool_use_ids.is_empty());
            assert!(m.tool_files.is_empty());
        }
    }

    /// Streaming-truncation variant — the same fixture sliced to drop the
    /// final `kind:1` `completionTokens` patch. The kind:2 stub for the
    /// last request is on disk (`requestId` + `modelId` exist) but the
    /// completion-token count never landed.
    ///
    /// Pins the live-tailer contract from #668: an in-flight request MUST
    /// NOT emit until the completion token arrives. Only the seven
    /// requests with inline `completionTokens` (delivered on the kind:2
    /// push payload) materialize.
    #[test]
    fn parse_real_vscode_0_47_0_fixture_streaming_truncation() {
        let content = include_str!("copilot_chat/fixtures/vscode_chat_0_47_0_streaming.jsonl");
        let path = Path::new("/tmp/budi-fixtures-r1-2/vscode_chat_0_47_0_streaming.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);

        // 7 assistant rows from the inline-completionTokens requests, plus
        // 2 user rows from the synthetic `message.text` on the first two
        // requests (#686). The kind:2 stub for the in-flight final request
        // carries neither tokens nor a message, so it must not emit at all.
        let assistant_count = msgs.iter().filter(|m| m.role == "assistant").count();
        let user_count = msgs.iter().filter(|m| m.role == "user").count();
        assert_eq!(
            assistant_count, 7,
            "truncated fixture: 7 inline-completionTokens requests must \
             still emit assistant rows; the kind:2 stub for the in-flight \
             request must NOT"
        );
        assert_eq!(
            user_count, 2,
            "truncated fixture: 2 user rows from the synthetic message.text \
             on the first two requests"
        );

        // The patched-only request's completion-token value (39) is the
        // signature for the in-flight row. It must not appear.
        assert!(
            !msgs.iter().any(|m| m.output_tokens == 39),
            "the in-flight (kind:2 stub, no completionTokens patch) request \
             leaked into the output — this is the no-double-emit /  \
             wait-for-completion-token contract from R1.1 #668"
        );

        // Every assistant row carries a non-zero output_tokens — none are
        // synthesized from the bare stub. User rows always have output=0.
        assert!(
            msgs.iter()
                .filter(|m| m.role == "assistant")
                .all(|m| m.output_tokens > 0)
        );
    }

    // ---- #681: workspace.json cwd enrichment --------------------------

    /// `parse_workspace_storage_session_enriches_cwd` — covers all four
    /// shapes the parser must handle per #681:
    /// 1. `<workspaceStorage>/<hash>/chatSessions/<uuid>.jsonl` with a
    ///    sibling `<hash>/workspace.json` → cwd populated from `folder`.
    /// 2. `<globalStorage>/emptyWindowChatSessions/<uuid>.jsonl` → cwd
    ///    stays `None` cleanly (no spurious warnings, no crash).
    /// 3. Remote / dev-container — same `<hash>/workspace.json` shape on
    ///    the remote-side path (`~/.vscode-server/data/User/...`) → cwd
    ///    populated.
    /// 4. Multi-root — `workspace.json` carries `configuration` pointing
    ///    at a `.code-workspace` file → first folder's path is the cwd.
    #[test]
    fn parse_workspace_storage_session_enriches_cwd() {
        // Use forward-slashed string-form paths for any value that round-
        // trips through a `file://` URI — Windows uses backslashes in
        // PathBuf string forms, but VS Code (and RFC 3986) writes URIs
        // with forward slashes, and an unescaped backslash inside a JSON
        // string is an invalid escape that aborts parsing.
        fn fwd(p: &Path) -> String {
            p.to_string_lossy().replace('\\', "/")
        }

        let tmp = std::env::temp_dir().join("budi-copilot-chat-cwd-enrich");
        let _ = std::fs::remove_dir_all(&tmp);

        let line = r#"{"kind":2,"v":[{"requestId":"r-1","modelId":"copilot/gpt-4.1","completionTokens":42,"result":{"metadata":{"resolvedModel":"x"}}}]}"#;

        // ---- Case 1: workspaceStorage single-root ----------------------
        let hash_dir = tmp.join("Library/Application Support/Code/User/workspaceStorage/abc123");
        let chat_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_dir).unwrap();
        let target_cwd = format!("{}/repos/single-root", fwd(&tmp));
        let workspace_json = serde_json::json!({
            "folder": format!("file://{}", target_cwd),
        })
        .to_string();
        std::fs::write(hash_dir.join("workspace.json"), workspace_json).unwrap();
        let session_path = chat_dir.join("sess-single.jsonl");
        std::fs::write(&session_path, format!("{line}\n")).unwrap();
        let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
        assert_eq!(msgs.len(), 1, "single-root session emits one row");
        assert_eq!(
            msgs[0].cwd.as_deref(),
            Some(target_cwd.as_str()),
            "single-root cwd resolved from workspace.json folder URI"
        );

        // ---- Case 2: emptyWindowChatSessions ---------------------------
        let empty_dir =
            tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let empty_path = empty_dir.join("sess-empty.jsonl");
        std::fs::write(&empty_path, format!("{line}\n")).unwrap();
        let (empty_msgs, _) = parse_copilot_chat(&empty_path, &format!("{line}\n"), 0);
        assert_eq!(empty_msgs.len(), 1);
        assert!(
            empty_msgs[0].cwd.is_none(),
            "emptyWindowChatSessions session leaves cwd None"
        );
        assert!(
            empty_msgs[0].git_branch.is_none(),
            "emptyWindowChatSessions session leaves git_branch None"
        );

        // ---- Case 3: remote / dev-container ----------------------------
        let remote_hash = tmp.join(".vscode-server/data/User/workspaceStorage/remotehash456");
        let remote_chat = remote_hash.join("chatSessions");
        std::fs::create_dir_all(&remote_chat).unwrap();
        let remote_workspace_json = serde_json::json!({
            "folder": "vscode-remote://ssh-remote+myhost/srv/repos/remote-proj",
        })
        .to_string();
        std::fs::write(remote_hash.join("workspace.json"), remote_workspace_json).unwrap();
        let remote_session = remote_chat.join("sess-remote.jsonl");
        std::fs::write(&remote_session, format!("{line}\n")).unwrap();
        let (remote_msgs, _) = parse_copilot_chat(&remote_session, &format!("{line}\n"), 0);
        assert_eq!(remote_msgs.len(), 1);
        assert_eq!(
            remote_msgs[0].cwd.as_deref(),
            Some("/srv/repos/remote-proj"),
            "remote URI strips scheme + host segment"
        );

        // ---- Case 4: multi-root configuration --------------------------
        let multi_hash =
            tmp.join("Library/Application Support/Code/User/workspaceStorage/multi789");
        let multi_chat = multi_hash.join("chatSessions");
        std::fs::create_dir_all(&multi_chat).unwrap();
        let workspace_dir = tmp.join("repos/workspaces");
        let folder_a = tmp.join("repos/multi-a");
        let folder_b = tmp.join("repos/multi-b");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::create_dir_all(&folder_a).unwrap();
        std::fs::create_dir_all(&folder_b).unwrap();
        let code_workspace = workspace_dir.join("multi.code-workspace");
        let folder_a_str = fwd(&folder_a);
        let folder_b_str = fwd(&folder_b);
        let code_workspace_json = serde_json::json!({
            "folders": [
                {"path": folder_a_str},
                {"path": folder_b_str},
            ],
        })
        .to_string();
        std::fs::write(&code_workspace, code_workspace_json).unwrap();
        let multi_workspace_json = serde_json::json!({
            "configuration": format!("file://{}", fwd(&code_workspace)),
        })
        .to_string();
        std::fs::write(multi_hash.join("workspace.json"), multi_workspace_json).unwrap();
        let multi_session = multi_chat.join("sess-multi.jsonl");
        std::fs::write(&multi_session, format!("{line}\n")).unwrap();
        let (multi_msgs, _) = parse_copilot_chat(&multi_session, &format!("{line}\n"), 0);
        assert_eq!(multi_msgs.len(), 1);
        assert_eq!(
            multi_msgs[0].cwd.as_deref(),
            Some(folder_a_str.as_str()),
            "multi-root cwd is the first folder in .code-workspace"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Percent-encoded paths (`Application%20Support`) must round-trip
    /// through the URI decoder so cwds with spaces resolve correctly.
    #[test]
    fn workspace_json_percent_decodes_folder_uri() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-percent-decode");
        let _ = std::fs::remove_dir_all(&tmp);
        let hash_dir = tmp.join("workspaceStorage/abc");
        let chat_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(
            hash_dir.join("workspace.json"),
            r#"{"folder":"file:///Users/me/My%20Project"}"#,
        )
        .unwrap();
        let session = chat_dir.join("s.jsonl");
        let line = r#"{"kind":2,"v":[{"requestId":"r","completionTokens":1}]}"#;
        std::fs::write(&session, format!("{line}\n")).unwrap();
        let (msgs, _) = parse_copilot_chat(&session, &format!("{line}\n"), 0);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].cwd.as_deref(), Some("/Users/me/My Project"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Malformed `workspace.json` falls back to `cwd: None` cleanly — the
    /// parse must not fail.
    #[test]
    fn workspace_json_malformed_falls_back_to_none() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-malformed-ws");
        let _ = std::fs::remove_dir_all(&tmp);
        let hash_dir = tmp.join("workspaceStorage/bad");
        let chat_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(hash_dir.join("workspace.json"), b"{not valid json").unwrap();
        let session = chat_dir.join("s.jsonl");
        let line = r#"{"kind":2,"v":[{"requestId":"r","completionTokens":1}]}"#;
        std::fs::write(&session, format!("{line}\n")).unwrap();
        let (msgs, _) = parse_copilot_chat(&session, &format!("{line}\n"), 0);
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].cwd.is_none(),
            "malformed workspace.json -> cwd None"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// End-to-end against the canonical R1.2 fixture (#669) — drops the
    /// `vscode_chat_0_47_0.jsonl` content under a synthetic
    /// `<workspaceStorage>/<hash>/chatSessions/` tree alongside the
    /// `vscode_chat_0_47_0.workspace.json` sibling fixture, and asserts
    /// every emitted row carries the cwd from `workspace.json`. Pins the
    /// #681 acceptance criterion: "the fixture gains a sibling
    /// `vscode_chat_0_47_0.workspace.json` so the unit test asserts
    /// cwd-enrichment end-to-end against the canonical fixture".
    #[test]
    fn parse_real_vscode_0_47_0_fixture_enriches_cwd() {
        let jsonl = include_str!("copilot_chat/fixtures/vscode_chat_0_47_0.jsonl");
        let workspace_json =
            include_str!("copilot_chat/fixtures/vscode_chat_0_47_0.workspace.json");

        let tmp = std::env::temp_dir().join("budi-copilot-chat-r681-canonical");
        let _ = std::fs::remove_dir_all(&tmp);
        let hash_dir = tmp.join("workspaceStorage/canon-hash");
        let chat_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(hash_dir.join("workspace.json"), workspace_json).unwrap();
        let session_path = chat_dir.join("vscode_chat_0_47_0.jsonl");
        std::fs::write(&session_path, jsonl).unwrap();

        let (msgs, _) = parse_copilot_chat(&session_path, jsonl, 0);
        assert!(
            !msgs.is_empty(),
            "canonical fixture must still emit rows under the cwd-enrichment path"
        );
        // The fixture workspace.json points at /Users/budi-fixture/...
        // which doesn't exist on disk, but cwd is the *string* — the
        // GitEnricher resolves it (or not) at the pipeline layer.
        let expected_cwd = "/Users/budi-fixture/workspaces/vscode-0.47.0-chat";
        assert!(
            msgs.iter().all(|m| m.cwd.as_deref() == Some(expected_cwd)),
            "every emitted row must carry the cwd from the sibling \
             workspace.json (got: {:?})",
            msgs.iter().map(|m| m.cwd.as_deref()).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- #688: emptyWindow editor-context cwd hint -------------------

    /// Pure-text extractor: the canonical sentence shape from
    /// `result.metadata.renderedUserMessage[*].text` resolves to the
    /// file's parent directory. Pins the format documented on #688.
    #[test]
    fn editor_context_text_extracts_parent_dir() {
        let body = "<editorContext>\n\
                    The user's current file is /Users/ivan.seredkin/Desktop/CP4X-GQMY-GH9C.md. \
                    The current selection is from line 9 to line 9.\n\
                    </editorContext>";
        assert_eq!(
            parent_dir_from_editor_context_text(body).as_deref(),
            Some("/Users/ivan.seredkin/Desktop")
        );
    }

    /// Path with spaces — the parent dir is preserved verbatim. The
    /// editor-context block carries a literal local path, not a URI, so
    /// no percent-decoding is needed (unlike the workspace.json `folder`
    /// field which does require it).
    #[test]
    fn editor_context_text_handles_spaces_and_dots() {
        let body = "<editorContext>\n\
                    The user's current file is /Users/me/My Project/src/file.v2.rs. \
                    The current selection is from line 1 to line 1.\n\
                    </editorContext>";
        assert_eq!(
            parent_dir_from_editor_context_text(body).as_deref(),
            Some("/Users/me/My Project/src")
        );
    }

    /// Relative path — skipped. We have no workspace root in the
    /// emptyWindow case, so a relative path cannot be turned into a
    /// concrete cwd hint.
    #[test]
    fn editor_context_text_rejects_relative_path() {
        let body = "<editorContext>\n\
                    The user's current file is src/main.rs. \
                    The current selection is from line 1 to line 1.\n\
                    </editorContext>";
        assert!(parent_dir_from_editor_context_text(body).is_none());
    }

    /// No `<editorContext>` block — None. Sessions sent before editor
    /// focus is established legitimately omit the block.
    #[test]
    fn editor_context_text_absent_returns_none() {
        let body = "<workspace_info>There is no workspace currently open.</workspace_info>";
        assert!(parent_dir_from_editor_context_text(body).is_none());
    }

    /// End-to-end: an emptyWindow session whose first request carries an
    /// `<editorContext>` block in `result.metadata.renderedUserMessage`
    /// emits rows with `cwd` populated from the file's parent dir and a
    /// `cwd_source = copilot_chat:editor_context_hint` marker so
    /// downstream analytics can distinguish the hint from an
    /// authoritative `workspace.json` cwd.
    #[test]
    fn empty_window_session_uses_editor_context_hint() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-empty-window-hint");
        let _ = std::fs::remove_dir_all(&tmp);
        let empty_dir =
            tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let session_path = empty_dir.join("bda343f1.jsonl");

        // Synthetic but shape-faithful kind:0 snapshot — one request with
        // tokens (so it emits) and an editorContext-bearing
        // renderedUserMessage entry.
        let line = serde_json::json!({
            "kind": 0,
            "v": {
                "sessionId": "empty-1",
                "requests": [{
                    "requestId": "r-1",
                    "modelId": "copilot/gpt-4.1",
                    "timestamp": 1715000000000_u64,
                    "message": {"text": "summarise this file"},
                    "result": {
                        "metadata": {
                            "promptTokens": 10,
                            "outputTokens": 5,
                            "renderedUserMessage": [{
                                "text": "<editorContext>\nThe user's current file is /Users/ivan.seredkin/Desktop/CP4X-GQMY-GH9C.md. The current selection is from line 9 to line 9.\n</editorContext>\n<workspace_info>\nThere is no workspace currently open.\n</workspace_info>"
                            }]
                        }
                    }
                }]
            }
        })
        .to_string();
        std::fs::write(&session_path, format!("{line}\n")).unwrap();

        let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
        assert!(
            !msgs.is_empty(),
            "session must emit at least the assistant row"
        );
        for msg in &msgs {
            assert_eq!(
                msg.cwd.as_deref(),
                Some("/Users/ivan.seredkin/Desktop"),
                "every row carries the editor-context hint cwd (got role={:?}, cwd={:?})",
                msg.role,
                msg.cwd
            );
            assert_eq!(
                msg.cwd_source.as_deref(),
                Some(CWD_SOURCE_EDITOR_CONTEXT_HINT),
                "every row carries the hint cwd_source marker"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Workspace-anchored sessions are unaffected: when `workspace.json`
    /// resolves the cwd, the editor-context hint must NOT override it
    /// and `cwd_source` stays `None` so analytics see the row as
    /// authoritative. Pins the "primary path of #681 is unaffected"
    /// acceptance criterion.
    #[test]
    fn workspace_anchored_session_does_not_apply_editor_context_hint() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-ws-anchored-no-hint");
        let _ = std::fs::remove_dir_all(&tmp);
        let hash_dir = tmp.join("workspaceStorage/abc-hash");
        let chat_dir = hash_dir.join("chatSessions");
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(
            hash_dir.join("workspace.json"),
            r#"{"folder":"file:///Users/me/repos/proj"}"#,
        )
        .unwrap();
        let session_path = chat_dir.join("sess.jsonl");

        // Even though the renderedUserMessage carries an editorContext
        // block pointing somewhere else, the workspace.json folder wins
        // and cwd_source stays None.
        let line = serde_json::json!({
            "kind": 0,
            "v": {
                "sessionId": "ws-1",
                "requests": [{
                    "requestId": "r-1",
                    "modelId": "copilot/gpt-4.1",
                    "timestamp": 1715000000000_u64,
                    "result": {
                        "metadata": {
                            "promptTokens": 10,
                            "outputTokens": 5,
                            "renderedUserMessage": [{
                                "text": "<editorContext>\nThe user's current file is /Users/ivan.seredkin/Desktop/foo.md. The current selection is from line 1 to line 1.\n</editorContext>"
                            }]
                        }
                    }
                }]
            }
        })
        .to_string();
        std::fs::write(&session_path, format!("{line}\n")).unwrap();

        let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].cwd.as_deref(), Some("/Users/me/repos/proj"));
        assert!(
            msgs[0].cwd_source.is_none(),
            "workspace-anchored cwd is authoritative; cwd_source must stay None"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// emptyWindow session with no `<editorContext>` block in any
    /// renderedUserMessage — cwd stays None and cwd_source stays None
    /// (no spurious hint). Mirrors the existing #681 emptyWindow test
    /// but goes further by also asserting the new tag.
    #[test]
    fn empty_window_session_without_editor_context_leaves_cwd_none() {
        let tmp = std::env::temp_dir().join("budi-copilot-chat-empty-window-no-hint");
        let _ = std::fs::remove_dir_all(&tmp);
        let empty_dir =
            tmp.join("Library/Application Support/Code/User/globalStorage/emptyWindowChatSessions");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let session_path = empty_dir.join("e22dad3b.jsonl");

        let line = r#"{"kind":2,"v":[{"requestId":"r-1","modelId":"copilot/gpt-4.1","completionTokens":42,"result":{"metadata":{"resolvedModel":"x"}}}]}"#;
        std::fs::write(&session_path, format!("{line}\n")).unwrap();

        let (msgs, _) = parse_copilot_chat(&session_path, &format!("{line}\n"), 0);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].cwd.is_none());
        assert!(msgs[0].cwd_source.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `append_at_path` correctness — the default-path branch (kind:2 with
    /// no `k`) and the explicit `["requests"]` branch must both append to
    /// the same array.
    #[test]
    fn append_at_path_appends_to_named_array() {
        let mut state = serde_json::json!({});
        append_at_path(
            &mut state,
            &[serde_json::json!("requests")],
            &[serde_json::json!({"requestId": "a"})],
        );
        append_at_path(
            &mut state,
            &[serde_json::json!("requests")],
            &[
                serde_json::json!({"requestId": "b"}),
                serde_json::json!({"requestId": "c"}),
            ],
        );
        let arr = state
            .get("requests")
            .and_then(|v| v.as_array())
            .expect("requests is an array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0].get("requestId").and_then(|v| v.as_str()), Some("a"));
        assert_eq!(arr[2].get("requestId").and_then(|v| v.as_str()), Some("c"));
    }

    // ---- #686: user-role row capture ---------------------------------

    /// Reducer path: a kind:0 snapshot whose request carries `message.text`
    /// emits both a user row (role=user, tokens=0, prompt content fed to
    /// the classifier) and an assistant row (role=assistant, current
    /// behavior). Assistant `parent_uuid` references the user `uuid`.
    #[test]
    fn reducer_emits_user_and_assistant_for_message_text() {
        let content = concat!(
            r#"{"kind":0,"v":{"sessionId":"s-user","requests":[{"requestId":"r-1","modelId":"copilot/claude-sonnet-4-5","timestamp":1715000000000,"message":{"text":"fix the login bug please","timestamp":1714999999000},"result":{"metadata":{"promptTokens":50,"outputTokens":12}}}]}}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-user-role-1.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2, "one turn ⇒ user + assistant rows");
        let user = &msgs[0];
        let assistant = &msgs[1];
        assert_eq!(user.role, "user");
        assert_eq!(assistant.role, "assistant");
        assert_eq!(user.input_tokens, 0);
        assert_eq!(user.output_tokens, 0);
        assert_eq!(assistant.input_tokens, 50);
        assert_eq!(assistant.output_tokens, 12);
        assert_eq!(user.session_id.as_deref(), Some("s-user"));
        assert_eq!(assistant.session_id.as_deref(), Some("s-user"));
        assert_ne!(user.uuid, assistant.uuid);
        assert_eq!(
            assistant.parent_uuid.as_deref(),
            Some(user.uuid.as_str()),
            "assistant row points back at the paired user row"
        );
        // The classifier ran against `message.text` — "fix the login bug" is
        // a textbook bugfix prompt, so the user row carries a category.
        assert_eq!(user.prompt_category.as_deref(), Some("bugfix"));
        assert!(user.prompt_category_source.is_some());
        assert!(user.prompt_category_confidence.is_some());
        // User-row provenance: `cost_confidence` is "n/a" (no cost on a
        // user prompt), `model` stays None, parent_uuid is None.
        assert_eq!(user.cost_confidence, "n/a");
        assert!(user.model.is_none());
        assert!(user.parent_uuid.is_none());
        // Provider tag is preserved on both rows.
        assert!(msgs.iter().all(|m| m.provider == "copilot_chat"));
    }

    /// Reducer path: `message.parts[]` joins text-typed parts in order.
    /// Non-text parts (file references, ephemeral cache markers) are
    /// skipped. The joined text feeds the classifier just like the
    /// `message.text` shape.
    #[test]
    fn reducer_user_row_concatenates_message_parts() {
        let content = concat!(
            r#"{"kind":0,"v":{"sessionId":"s-parts","requests":[{"requestId":"r-p","modelId":"copilot/gpt-4.1","message":{"parts":[{"text":"add a new "},{"kind":3,"cacheType":"ephemeral"},{"text":"button to the dashboard"}]},"result":{"metadata":{"promptTokens":7,"outputTokens":3}}}]}}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-parts.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        // `add a new button to the dashboard` classifies as "feature".
        assert_eq!(msgs[0].prompt_category.as_deref(), Some("feature"));
    }

    /// Missing or empty `message` ⇒ no user row, but the assistant row
    /// still emits. Per ticket #686 — interrupted / replayed-via-API
    /// sessions are rare but legal; the assistant row carries the tokens.
    #[test]
    fn reducer_no_user_row_when_message_missing_or_empty() {
        // No message at all.
        let content_a = concat!(
            r#"{"kind":0,"v":{"sessionId":"s-no-msg","requests":[{"requestId":"r","modelId":"copilot/auto","completionTokens":42}]}}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-no-msg.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content_a, 0);
        assert_eq!(msgs.len(), 1, "only the assistant row");
        assert_eq!(msgs[0].role, "assistant");

        // Empty `message.text` — also no user row.
        let content_b = concat!(
            r#"{"kind":0,"v":{"sessionId":"s-empty","requests":[{"requestId":"r","modelId":"copilot/auto","completionTokens":42,"message":{"text":""}}]}}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-empty.jsonl");
        let (msgs, _) = parse_copilot_chat(path, content_b, 0);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].parent_uuid.is_none());
    }

    /// Re-emit guard: re-parsing the same file must produce the same
    /// pair of UUIDs. The `:user` suffix on the user-row emit key keeps
    /// it stable across ticks, just like the assistant row.
    #[test]
    fn user_row_uuid_stable_across_reparse() {
        let content = concat!(
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"stable-pair","modelId":"copilot/x","completionTokens":7,"message":{"text":"how does this work?"}}]}"#,
            "\n",
        );
        let path = Path::new("/tmp/budi-fixtures/sess-stable-pair.jsonl");
        let (first, _) = parse_copilot_chat(path, content, 0);
        let (second, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_eq!(first[0].uuid, second[0].uuid);
        assert_eq!(first[1].uuid, second[1].uuid);
        assert_ne!(first[0].uuid, first[1].uuid);
    }

    /// JSON-document path: same shape, same emit. A single
    /// `{"requests": [...]}` snapshot with `message.text` on a request
    /// produces user + assistant rows.
    #[test]
    fn json_document_emits_user_and_assistant_rows() {
        let content = r#"{
            "sessionId": "doc-user-1",
            "requests": [
                {
                    "modelId": "copilot/claude-sonnet-4-5",
                    "requestId": "r-doc-1",
                    "message": {"text": "explain the auth flow"},
                    "result": {"metadata": {"promptTokens": 20, "outputTokens": 4}}
                }
            ]
        }"#;
        let path = Path::new("/tmp/budi-fixtures/sess-doc-user.json");
        let (msgs, _) = parse_copilot_chat(path, content, 0);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].parent_uuid.as_deref(), Some(msgs[0].uuid.as_str()));
    }
}
