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
pub const MIN_API_VERSION: u32 = 3;

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
        "copilot_chat"
    }

    fn display_name(&self) -> &'static str {
        "Copilot Chat"
    }

    fn is_available(&self) -> bool {
        any_user_root_has_copilot_marker(&user_root_candidates())
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
        roots.sort();
        roots.dedup();
        roots
    }

    fn sync_direct(
        &self,
        conn: &mut Connection,
        _pipeline: &mut crate::pipeline::Pipeline,
        _max_age_days: Option<u64>,
    ) -> Option<Result<(usize, usize, Vec<String>)>> {
        // R1.5 / ADR-0092 §3: best-effort GitHub Billing API
        // reconciliation. Local-tail is the primary signal; this just
        // truths-up `cost_cents` on existing rows on a (date, model)
        // bucket basis, so we deliberately return `None` and let the
        // dispatcher proceed to the file-based discovery path. The
        // billing pull is a side effect that complements ingest, never
        // a replacement for it.
        let config = crate::config::load_copilot_chat_config();
        config.effective_billing_pat()?;
        if let Err(e) = crate::sync::copilot_chat_billing::run_reconciliation(conn, &config) {
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
    // double-counting both casings.
    for pub_dir in publisher_subdirs(&gs) {
        push_session_files_recursive(&pub_dir, out, 0);
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

fn parse_jsonl(path: &Path, content: &str, start_offset: usize) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;

    if start_offset > content.len() {
        return (messages, content.len());
    }

    let session_id = session_id_for_path(path);
    let mut session_default_model: Option<String> = None;
    let mut line_index: usize = byte_offset_to_line_index(content, start_offset);
    // Per-record counter so deterministic_uuid stays unique across multiple
    // records emitted from the same line (envelope shapes — see
    // [`flatten_records`]).
    let mut record_index: usize = 0;

    let remaining = &content[start_offset..];
    let mut pos = 0;

    for line in remaining.lines() {
        let line_end = pos + line.len();
        let has_newline = line_end < remaining.len() && remaining.as_bytes()[line_end] == b'\n';
        // The last line of a partially-written file has no terminating newline yet;
        // stop here so the next read picks it up once the writer flushes.
        if !has_newline && line_end == remaining.len() {
            break;
        }
        pos = line_end + if has_newline { 1 } else { 0 };
        offset = start_offset + pos;
        line_index += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        // Session-default model can be advertised either on the envelope
        // itself (older flat-line shape) or on the manifest record VS Code
        // writes as `kind: 0` (real on-disk shape — see ADR-0092 §2.3).
        if let Some(model) = extract_session_default_model(&value) {
            session_default_model = Some(model);
        }

        for record in flatten_records(&value) {
            if let Some(model) = extract_session_default_model(record) {
                session_default_model = Some(model);
            }
            record_index += 1;
            let composite_index = line_index
                .wrapping_mul(1_000_000)
                .wrapping_add(record_index);
            if let Some(msg) = build_message(
                path,
                record,
                session_id.as_deref(),
                session_default_model.as_deref(),
                composite_index,
            ) {
                messages.push(msg);
            } else if !shape_matches_any(record) {
                log_unknown_shape_once(path, record);
            }
        }
    }

    (messages, offset)
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

    let records: Vec<&serde_json::Value> = flatten_records(&doc);

    for (index, record) in records.iter().enumerate() {
        if let Some(model) = extract_session_default_model(record) {
            session_default_model = Some(model);
        }
        if let Some(msg) = build_message(
            path,
            record,
            session_id.as_deref(),
            session_default_model.as_deref(),
            index,
        ) {
            messages.push(msg);
        } else if !shape_matches_any(record) {
            log_unknown_shape_once(path, record);
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

fn build_message(
    path: &Path,
    record: &serde_json::Value,
    session_id: Option<&str>,
    session_default_model: Option<&str>,
    index: usize,
) -> Option<ParsedMessage> {
    let tokens = extract_tokens(record)?;

    let model = extract_model_id(record).or_else(|| session_default_model.map(|s| s.to_string()));

    let timestamp = extract_timestamp(record);

    let path_key = path.display().to_string();
    let sid = session_id.unwrap_or(path_key.as_str());
    let uuid = deterministic_uuid(sid, &path_key, index);

    Some(ParsedMessage {
        uuid,
        session_id: session_id.map(String::from),
        timestamp,
        cwd: None,
        role: "assistant".to_string(),
        model,
        input_tokens: tokens.input,
        output_tokens: tokens.output,
        cache_creation_tokens: tokens.cache_write,
        cache_read_tokens: tokens.cache_read,
        git_branch: None,
        repo_id: None,
        provider: "copilot_chat".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid: None,
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
        tool_files: Vec::new(),
        tool_outcomes: Vec::new(),
    })
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

/// Strip a `copilot/` model-id prefix per ADR-0092 §2.4.
fn extract_model_id(record: &serde_json::Value) -> Option<String> {
    // `modelId` is the user-facing label (e.g. `copilot/claude-haiku-4.5`,
    // `copilot/auto`). Prefer it over `result.metadata.resolvedModel` —
    // that field is either a dated version suffix (`claude-haiku-4-5-20251001`)
    // or an internal GPU-fleet code (`capi-noe-ptuc-h200-oswe-vscode-prime`)
    // that does not map to manifest entries. The fleet-code form means
    // `resolvedModel` cannot be trusted as a pricing key.
    if let Some(model) = record.get("modelId").and_then(|v| v.as_str()) {
        return Some(strip_copilot_prefix(model).to_string());
    }
    if let Some(model) = record
        .pointer("/result/metadata/modelId")
        .and_then(|v| v.as_str())
    {
        return Some(strip_copilot_prefix(model).to_string());
    }
    None
}

fn strip_copilot_prefix(model: &str) -> &str {
    model.strip_prefix("copilot/").unwrap_or(model)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
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
}
