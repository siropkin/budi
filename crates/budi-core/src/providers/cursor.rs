//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Primary data source: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
//! — a SQLite database containing composerData sessions with per-model cost, request counts,
//! context token usage, timestamps, lines changed, model names, and session titles.
//!
//! Fallback: JSONL agent transcripts under `~/.cursor/projects/*/agent-transcripts/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

use crate::analytics;
use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, ModelPricing, Provider};

/// Resolve the Cursor default model from ~/.cursor/cli-config.json.
/// Returns None if the file doesn't exist or can't be parsed.
fn resolve_default_model() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".cursor/cli-config.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed
        .get("model")
        .and_then(|m| m.get("modelId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// The Cursor provider.
pub struct CursorProvider;

impl Provider for CursorProvider {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
    }

    fn is_available(&self) -> bool {
        !all_state_vscdb_paths().is_empty() || cursor_home().map(|p| p.exists()).unwrap_or(false)
    }

    fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
        let home = cursor_home()?;
        let projects_dir = home.join("projects");
        let mut files = Vec::new();
        collect_cursor_transcripts(&projects_dir, &mut files);
        // Sort by mtime descending (newest first) for progressive sync.
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
        let session_id = session_id_from_path(path);
        let cwd = cwd_from_path(path);
        let file_ts = file_mtime(path);

        Ok(parse_cursor_transcript(
            content,
            offset,
            &session_id,
            cwd.as_deref(),
            file_ts,
        ))
    }

    fn pricing_for_model(&self, model: &str) -> ModelPricing {
        cursor_pricing_for_model(model)
    }

    fn sync_direct(
        &self,
        conn: &mut Connection,
        pipeline: &mut crate::pipeline::Pipeline,
    ) -> Option<Result<(usize, usize)>> {
        let paths = all_state_vscdb_paths();
        if paths.is_empty() {
            return None; // Fall back to JSONL
        }
        let mut total_sessions = 0;
        let mut total_messages = 0;
        for path in &paths {
            match sync_from_state_vscdb(conn, path, pipeline) {
                Ok((s, m)) => {
                    total_sessions += s;
                    total_messages += m;
                }
                Err(e) => {
                    tracing::warn!("Cursor sync failed for {}: {}", path.display(), e);
                }
            }
        }

        Some(Ok((total_sessions, total_messages)))
    }
}

// ---------------------------------------------------------------------------
// state.vscdb paths (cross-platform) — globalStorage + workspaceStorage
// ---------------------------------------------------------------------------

/// Returns all state.vscdb paths found on the system: globalStorage and
/// every workspace under workspaceStorage, for both macOS and Linux.
fn all_state_vscdb_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return paths,
    };

    // macOS globalStorage
    let mac_global = home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    if mac_global.exists() {
        paths.push(mac_global);
    }

    // Linux globalStorage
    let linux_global = home.join(".config/Cursor/User/globalStorage/state.vscdb");
    if linux_global.exists() {
        paths.push(linux_global);
    }

    // macOS workspaceStorage
    let mac_ws = home.join("Library/Application Support/Cursor/User/workspaceStorage");
    scan_workspace_dbs(&mac_ws, &mut paths);

    // Linux workspaceStorage
    let linux_ws = home.join(".config/Cursor/User/workspaceStorage");
    scan_workspace_dbs(&linux_ws, &mut paths);

    paths
}

/// Scan a workspaceStorage directory for `*/state.vscdb` files.
fn scan_workspace_dbs(ws_dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(ws_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let db = entry.path().join("state.vscdb");
        if db.exists() {
            paths.push(db);
        }
    }
}

// ---------------------------------------------------------------------------
// state.vscdb sync — composerData sessions with rich analytics
// ---------------------------------------------------------------------------

/// Sync from Cursor's state.vscdb SQLite database.
/// Reads composerData and bubbleId entries from the cursorDiskKV table.
fn sync_from_state_vscdb(
    budi_conn: &mut Connection,
    vscdb_path: &Path,
    pipeline: &mut crate::pipeline::Pipeline,
) -> Result<(usize, usize)> {
    // Open state.vscdb read-only (Cursor may be running with WAL mode)
    let vscdb = Connection::open_with_flags(
        vscdb_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Failed to open {}", vscdb_path.display()))?;

    // Get watermark for incremental sync
    let watermark_key = format!("cursor-vscdb:{}", vscdb_path.display());
    let last_watermark = analytics::get_sync_offset(budi_conn, &watermark_key).unwrap_or(0) as i64;

    let mut total_messages = 0usize;
    let mut total_sessions = 0usize;

    // Parse composerData entries (rich session data with cost, model, lines)
    let (sessions, new_watermark) = parse_composer_sessions(&vscdb, last_watermark)?;

    for session in &sessions {
        let mut messages = composer_session_to_messages(session);
        if !messages.is_empty() {
            let tags = pipeline.process(&mut messages);
            let count = analytics::ingest_messages(budi_conn, &messages, Some(&tags))?;
            if count > 0 {
                total_sessions += 1;
                total_messages += count;
            }
        }
    }

    // Note: bubbleId entries have token counts but no timestamps, so they
    // can't be properly time-bucketed. composerData already provides cost,
    // model, lines, and context data — which is what matters for analytics.

    // Save the watermark
    let final_watermark = new_watermark;
    if final_watermark > last_watermark {
        analytics::set_sync_offset(budi_conn, &watermark_key, final_watermark as usize)?;
    }

    Ok((total_sessions, total_messages))
}

/// A parsed composer session from state.vscdb.
#[derive(Debug)]
struct ComposerSession {
    key: String,
    name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    last_updated_at: Option<i64>, // Unix millis, used as watermark
    #[allow(dead_code)]
    is_agentic: bool,
    /// Per-model usage data within this session.
    usage_entries: Vec<ComposerUsageEntry>,
    /// Context token counts from the session.
    context_tokens_used: Option<u64>,
    context_token_limit: Option<u64>,
    /// Model name from modelConfig (fallback when usageData is empty).
    model_name: Option<String>,
    /// Git branch from `createdOnBranch` or resolved from workspace .git/HEAD.
    git_branch: Option<String>,
    /// Workspace folder path, extracted from file URIs in the session.
    cwd: Option<String>,
}

#[derive(Debug)]
struct ComposerUsageEntry {
    model: String,
    cost_cents: f64,
    #[allow(dead_code)]
    num_requests: u64,
}

fn parse_composer_sessions(
    vscdb: &Connection,
    since_watermark: i64,
) -> Result<(Vec<ComposerSession>, i64)> {
    let mut sessions = Vec::new();
    let mut max_watermark = since_watermark;

    // Resolve "default" model name once for the entire sync
    let default_model = resolve_default_model();

    // Query all composerData:* keys from cursorDiskKV
    let mut stmt =
        vscdb.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")?;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (key, value) = match row {
            Ok(r) => r,
            Err(_) => continue,
        };

        let parsed: Value = match serde_json::from_str(&value) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check watermark — skip sessions we've already processed
        let last_updated = parsed.get("lastUpdatedAt").and_then(|v| v.as_i64());
        if let Some(ts) = last_updated {
            if ts <= since_watermark {
                continue;
            }
            max_watermark = max_watermark.max(ts);
        }

        let name = parsed
            .get("name")
            .or_else(|| parsed.get("title"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let created_at = parsed
            .get("createdAt")
            .and_then(|v| v.as_i64())
            .and_then(DateTime::from_timestamp_millis);

        let is_agentic = parsed
            .get("isAgentic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let context_token_limit = parsed
            .get("contextTokenLimit")
            .or_else(|| parsed.get("maxContextTokens"))
            .and_then(|v| v.as_u64());

        // Try contextTokensUsed first, then estimate from contextUsagePercent
        let context_tokens_used = parsed
            .get("contextTokensUsed")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                let pct = parsed.get("contextUsagePercent").and_then(|v| v.as_f64())?;
                let limit = context_token_limit.unwrap_or(200_000); // default 200K
                Some((pct / 100.0 * limit as f64) as u64)
            });

        let raw_model = parsed
            .get("modelConfig")
            .and_then(|v| v.get("modelName"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Resolve "default" to the actual configured model
        let model_name = match raw_model.as_deref() {
            Some("default") | None => default_model.clone().or(raw_model),
            _ => raw_model,
        };

        // Parse usageData — per-model cost breakdown
        let mut usage_entries = Vec::new();
        if let Some(usage_data) = parsed.get("usageData")
            && let Some(obj) = usage_data.as_object()
        {
            for (model, model_data) in obj {
                let cost = model_data
                    .get("costInCents")
                    .or_else(|| model_data.get("cost"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                let requests = model_data
                    .get("numRequests")
                    .or_else(|| model_data.get("requestCount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1);

                if cost > 0.0 || requests > 0 {
                    usage_entries.push(ComposerUsageEntry {
                        model: model.clone(),
                        cost_cents: cost,
                        num_requests: requests,
                    });
                }
            }
        }

        // Extract git branch — prefer createdOnBranch (stored by Cursor), fall
        // back to reading .git/HEAD from the workspace folder (pure file read).
        let created_on_branch = parsed
            .get("createdOnBranch")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // Extract workspace folder from file URIs in the session.
        let cwd = extract_folder_from_file_uris(&parsed);

        let git_branch = created_on_branch.or_else(|| {
            cwd.as_deref().and_then(resolve_git_branch_from_head)
        });

        sessions.push(ComposerSession {
            key,
            name,
            created_at,
            last_updated_at: last_updated,
            is_agentic,
            usage_entries,
            context_tokens_used,
            context_token_limit,
            model_name,
            git_branch,
            cwd,
        });
    }

    Ok((sessions, max_watermark))
}

/// Extract a workspace folder path from file URIs in composerData.
/// Checks `allAttachedFileCodeChunksUris` (string URIs) and
/// `newlyCreatedFiles` (objects with `uri.fsPath`).
fn extract_folder_from_file_uris(parsed: &Value) -> Option<String> {
    // Try allAttachedFileCodeChunksUris first (array of "file:///..." strings)
    if let Some(uris) = parsed.get("allAttachedFileCodeChunksUris").and_then(|v| v.as_array()) {
        for uri in uris {
            if let Some(path) = uri.as_str().and_then(file_uri_to_path) {
                return find_git_root(&path);
            }
        }
    }

    // Try newlyCreatedFiles (array of objects with uri.fsPath)
    if let Some(files) = parsed.get("newlyCreatedFiles").and_then(|v| v.as_array()) {
        for file in files {
            if let Some(fs_path) = file
                .get("uri")
                .and_then(|u| u.get("fsPath"))
                .and_then(|v| v.as_str())
            {
                return find_git_root(&PathBuf::from(fs_path));
            }
        }
    }

    None
}

/// Convert a `file:///path` URI to a PathBuf.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    uri.strip_prefix("file://").map(PathBuf::from)
}

/// Walk up from a file path to find the nearest directory containing `.git`.
fn find_git_root(path: &Path) -> Option<String> {
    let mut dir = if path.is_file() || !path.exists() {
        path.parent()?
    } else {
        path
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_string_lossy().to_string());
        }
        dir = dir.parent()?;
    }
}

/// Read `.git/HEAD` to resolve the current branch name without spawning a subprocess.
/// Returns `None` for detached HEAD or if the file can't be read.
pub fn resolve_git_branch_from_head(dir: &str) -> Option<String> {
    let head_path = Path::new(dir).join(".git/HEAD");
    let contents = std::fs::read_to_string(&head_path).ok()?;
    let trimmed = contents.trim();
    trimmed
        .strip_prefix("ref: refs/heads/")
        .map(|b| b.to_string())
}

/// Convert a ComposerSession into ParsedMessages for ingestion.
fn composer_session_to_messages(session: &ComposerSession) -> Vec<ParsedMessage> {
    let mut messages = Vec::new();

    let session_id = format!(
        "cursor-composer-{}",
        session.key.replace("composerData:", "")
    );
    let timestamp = session
        .created_at
        .or_else(|| {
            session
                .last_updated_at
                .map(|ms| chrono::DateTime::from_timestamp_millis(ms).unwrap_or_else(Utc::now))
        })
        .unwrap_or_else(Utc::now);

    // Create a user message for the session
    messages.push(ParsedMessage {
        uuid: format!("{}-user", session_id),
        session_id: Some(session_id.clone()),
        timestamp,
        cwd: session.cwd.clone(),
        role: "user".to_string(),
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        git_branch: session.git_branch.clone(),
        repo_id: None,
        provider: "cursor".to_string(),
        cost_cents: None,
        context_tokens_used: None,
        context_token_limit: None,
        session_title: session.name.clone(),
        parent_uuid: None,
        user_name: None,
        machine_name: None,
    });

    // Create an assistant message per model used in the session
    for (i, usage) in session.usage_entries.iter().enumerate() {
        // Reverse-calculate approximate tokens from cost and pricing
        let pricing = cursor_pricing_for_model(&usage.model);
        let (input_tokens, output_tokens) = estimate_tokens_from_cost(usage.cost_cents, &pricing);

        messages.push(ParsedMessage {
            uuid: format!("{}-assistant-{}", session_id, i),
            session_id: Some(session_id.clone()),
            timestamp,
            cwd: session.cwd.clone(),
            role: "assistant".to_string(),
            model: Some(usage.model.clone()),
            input_tokens,
            output_tokens,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: session.git_branch.clone(),
            repo_id: None,
            provider: "cursor".to_string(),
            cost_cents: Some(usage.cost_cents),
            context_tokens_used: session.context_tokens_used,
            context_token_limit: session.context_token_limit,
            session_title: session.name.clone(),
            parent_uuid: None,
            user_name: None,
            machine_name: None,
        });
    }

    // If no usage entries but session exists, use contextTokensUsed as input_tokens fallback
    if session.usage_entries.is_empty() {
        let input_tokens = session.context_tokens_used.unwrap_or(0);
        // Estimate output as ~25% of input (typical coding ratio)
        let output_tokens = input_tokens / 4;

        // Estimate cost from tokens using provider pricing
        let model = session.model_name.as_deref().unwrap_or("unknown");
        let pricing = cursor_pricing_for_model(model);
        let cost = input_tokens as f64 * pricing.input / 1_000_000.0
            + output_tokens as f64 * pricing.output / 1_000_000.0;
        let cost_cents = (cost * 100.0 * 100.0).round() / 100.0; // cents with 2 decimal places

        messages.push(ParsedMessage {
            uuid: format!("{}-assistant-0", session_id),
            session_id: Some(session_id.clone()),
            timestamp,
            cwd: session.cwd.clone(),
            role: "assistant".to_string(),
            model: session.model_name.clone(),
            input_tokens,
            output_tokens,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: session.git_branch.clone(),
            repo_id: None,
            provider: "cursor".to_string(),
            cost_cents: if cost_cents > 0.0 {
                Some(cost_cents)
            } else {
                None
            },
            context_tokens_used: session.context_tokens_used,
            context_token_limit: session.context_token_limit,
            session_title: session.name.clone(),
            parent_uuid: None,
            user_name: None,
            machine_name: None,
        });
    }

    messages
}

/// Rough estimate of tokens from cost in cents using model pricing.
/// Assumes a 3:1 input:output ratio (typical for coding).
fn estimate_tokens_from_cost(cost_cents: f64, pricing: &ModelPricing) -> (u64, u64) {
    if cost_cents <= 0.0 {
        return (0, 0);
    }
    let cost_dollars = cost_cents / 100.0;
    // Assume 75% input, 25% output by cost
    let input_cost = cost_dollars * 0.75;
    let output_cost = cost_dollars * 0.25;
    let input_tokens = if pricing.input > 0.0 {
        (input_cost * 1_000_000.0 / pricing.input) as u64
    } else {
        0
    };
    let output_tokens = if pricing.output > 0.0 {
        (output_cost * 1_000_000.0 / pricing.output) as u64
    } else {
        0
    };
    (input_tokens, output_tokens)
}

// Note: bubbleId entries contain per-message content but lack timestamps and
// have zero token counts in practice. composerData already provides all the
// aggregated analytics we need (cost, model, lines, context). Bubble parsing
// was removed to avoid timestamp issues (all bubbles would get Utc::now()).

// ---------------------------------------------------------------------------
// JSONL fallback helpers (kept for when state.vscdb is unavailable)
// ---------------------------------------------------------------------------

fn cursor_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".cursor"))
}

/// Walk `~/.cursor/projects/*/agent-transcripts/` for JSONL files.
fn collect_cursor_transcripts(projects_dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(projects_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let transcripts_dir = entry.path().join("agent-transcripts");
        if !transcripts_dir.is_dir() {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(&transcripts_dir) else {
            continue;
        };
        for inner_entry in inner.flatten() {
            let path = inner_entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            } else if path.is_dir() {
                let Ok(nested) = std::fs::read_dir(&path) else {
                    continue;
                };
                for nested_entry in nested.flatten() {
                    let nested_path = nested_entry.path();
                    if nested_path.extension().is_some_and(|e| e == "jsonl") {
                        files.push(nested_path);
                    }
                }
            }
        }
    }
}

/// Extract a session ID from the transcript file path.
fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("cursor-{}", s))
        .unwrap_or_else(|| "cursor-unknown".to_string())
}

/// Extract the Cursor project slug from the transcript path.
fn cwd_from_path(path: &Path) -> Option<String> {
    let mut current = path;
    while let Some(parent) = current.parent() {
        if parent.file_name().is_some_and(|n| n == "agent-transcripts")
            && let Some(project_dir) = parent.parent()
        {
            return project_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|_| project_dir.display().to_string());
        }
        current = parent;
    }
    None
}

/// Get file modification time as a UTC DateTime.
fn file_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| {
            let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
            Utc.timestamp_opt(dur.as_secs() as i64, 0).single()
        })
        .unwrap_or_else(Utc::now)
}

// ---------------------------------------------------------------------------
// Cursor JSONL parsing
// ---------------------------------------------------------------------------

/// A Cursor transcript entry.
#[derive(Debug, Deserialize)]
struct CursorEntry {
    role: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    model: Option<String>,
    timestamp: Option<String>,
    usage: Option<CursorUsage>,
    uuid: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CursorUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_creation_input_tokens_alt: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read_input_tokens_alt: Option<u64>,
}

impl CursorUsage {
    fn cache_creation(&self) -> u64 {
        self.cache_creation_input_tokens
            .or(self.cache_creation_input_tokens_alt)
            .unwrap_or(0)
    }

    fn cache_read(&self) -> u64 {
        self.cache_read_input_tokens
            .or(self.cache_read_input_tokens_alt)
            .unwrap_or(0)
    }
}

/// Parse a single Cursor JSONL line into a `ParsedMessage`.
fn parse_cursor_line(
    line: &str,
    line_index: usize,
    session_id: &str,
    cwd: Option<&str>,
    fallback_ts: DateTime<Utc>,
) -> Option<ParsedMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let entry: CursorEntry = serde_json::from_str(line).ok()?;

    let role = entry.role.as_deref().or(entry.entry_type.as_deref())?;

    let timestamp = entry
        .timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(fallback_ts);

    let uuid = entry
        .uuid
        .or(entry.request_id)
        .unwrap_or_else(|| format!("{}-{}", session_id, line_index));

    let msg_session_id = entry.session_id.unwrap_or_else(|| session_id.to_string());
    let msg_cwd = entry.cwd.or_else(|| cwd.map(|s| s.to_string()));

    match role {
        "user" | "human" => Some(ParsedMessage {
            uuid,
            session_id: Some(msg_session_id),
            timestamp,
            cwd: msg_cwd,
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "cursor".to_string(),
            cost_cents: None,
            context_tokens_used: None,
            context_token_limit: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
        }),
        "assistant" | "ai" | "model" => {
            let usage = entry.usage.as_ref();
            Some(ParsedMessage {
                uuid,
                session_id: Some(msg_session_id),
                timestamp,
                cwd: msg_cwd,
                role: "assistant".to_string(),
                model: entry.model,
                input_tokens: usage.and_then(|u| u.input_tokens).unwrap_or(0),
                output_tokens: usage.and_then(|u| u.output_tokens).unwrap_or(0),
                cache_creation_tokens: usage.map(|u| u.cache_creation()).unwrap_or(0),
                cache_read_tokens: usage.map(|u| u.cache_read()).unwrap_or(0),
                git_branch: None,
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                context_tokens_used: None,
                context_token_limit: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
            })
        }
        _ => None,
    }
}

/// Parse all lines from a Cursor JSONL string with incremental offset support.
pub fn parse_cursor_transcript(
    content: &str,
    start_offset: usize,
    session_id: &str,
    cwd: Option<&str>,
    fallback_ts: DateTime<Utc>,
) -> (Vec<ParsedMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = start_offset;
    let mut line_index = 0usize;

    if start_offset > 0 {
        line_index = content[..start_offset].lines().count();
    }

    for line in content[start_offset..].lines() {
        let line_end = offset + line.len() + 1;
        if let Some(msg) = parse_cursor_line(line, line_index, session_id, cwd, fallback_ts) {
            messages.push(msg);
        }
        offset = line_end;
        line_index += 1;
    }

    (messages, offset)
}

/// Try parsing a timestamp string — supports ISO 8601 and Unix millis.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = ts.parse::<DateTime<Utc>>() {
        return Some(dt);
    }
    if let Ok(millis) = ts.parse::<i64>() {
        return DateTime::from_timestamp_millis(millis);
    }
    None
}

/// Cursor model pricing lookup.
/// Prices are per MTok (million tokens), matching actual API rates that Cursor
/// bills against credit pools.
pub fn cursor_pricing_for_model(model: &str) -> ModelPricing {
    let m = model.to_lowercase();
    // GPT-5.x models
    if m.contains("gpt-5") {
        ModelPricing {
            input: 2.50,
            output: 15.0,
            cache_write: 2.50,
            cache_read: 1.25,
        }
    // Cursor "Auto" / "composer" / "default" — uses cheapest routing
    } else if m == "default" || m == "composer-1" || m.contains("auto") {
        ModelPricing {
            input: 1.25,
            output: 6.0,
            cache_write: 1.25,
            cache_read: 0.25,
        }
    } else if m.contains("gpt-4o-mini") {
        ModelPricing {
            input: 0.15,
            output: 0.60,
            cache_write: 0.15,
            cache_read: 0.075,
        }
    } else if m.contains("gpt-4o") || m.contains("gpt-4-turbo") {
        ModelPricing {
            input: 2.50,
            output: 10.0,
            cache_write: 2.50,
            cache_read: 1.25,
        }
    } else if m.contains("gpt-4") {
        ModelPricing {
            input: 30.0,
            output: 60.0,
            cache_write: 30.0,
            cache_read: 15.0,
        }
    } else if m.contains("o1-mini") || m.contains("o3-mini") {
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_write: 1.10,
            cache_read: 0.55,
        }
    } else if m.contains("o1") || m.contains("o3") {
        ModelPricing {
            input: 10.0,
            output: 40.0,
            cache_write: 10.0,
            cache_read: 5.0,
        }
    } else if m.contains("opus") {
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_write: 6.25,
            cache_read: 0.50,
        }
    } else if m.contains("sonnet") {
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_write: 1.25,
            cache_read: 0.10,
        }
    } else if m.contains("gemini") {
        ModelPricing {
            input: 2.0,
            output: 12.0,
            cache_write: 2.0,
            cache_read: 0.50,
        }
    } else if m.contains("deepseek") {
        ModelPricing {
            input: 0.27,
            output: 1.10,
            cache_write: 0.27,
            cache_read: 0.07,
        }
    } else {
        // Unknown model — use GPT-4o pricing as reasonable default
        ModelPricing {
            input: 2.50,
            output: 10.0,
            cache_write: 2.50,
            cache_read: 1.25,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- JSONL parsing tests ---

    #[test]
    fn parse_real_cursor_user_message() {
        let line = r#"{"role":"user","message":{"content":[{"type":"text","text":"fix the bug in main.rs"}]}}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 0, "cursor-abc", Some("/proj"), ts).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.uuid, "cursor-abc-0");
        assert_eq!(msg.session_id.as_deref(), Some("cursor-abc"));
        assert_eq!(msg.cwd.as_deref(), Some("/proj"));
        assert_eq!(msg.provider, "cursor");
        assert_eq!(msg.model, None);
        assert_eq!(msg.input_tokens, 0);
    }

    #[test]
    fn parse_real_cursor_assistant_message() {
        let line = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Here is the fix for main.rs"}]}}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 1, "cursor-abc", Some("/proj"), ts).unwrap();
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.uuid, "cursor-abc-1");
        assert_eq!(msg.model, None);
        assert_eq!(msg.input_tokens, 0);
    }

    #[test]
    fn parse_real_cursor_transcript() {
        let content = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"let me help"}]}}"#,
            "\n",
        );
        let ts = Utc::now();
        let (msgs, offset) = parse_cursor_transcript(content, 0, "cursor-s1", Some("/proj"), ts);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[2].role, "assistant");
        assert!(
            msgs.iter()
                .all(|m| m.session_id.as_deref() == Some("cursor-s1"))
        );
        assert!(msgs.iter().all(|m| m.provider == "cursor"));
        assert_eq!(msgs[0].uuid, "cursor-s1-0");
        assert_eq!(msgs[1].uuid, "cursor-s1-1");
        assert_eq!(msgs[2].uuid, "cursor-s1-2");

        let (msgs2, _) = parse_cursor_transcript(content, offset, "cursor-s1", Some("/proj"), ts);
        assert!(msgs2.is_empty());
    }

    #[test]
    fn parse_cursor_with_optional_fields() {
        let line = r#"{"role":"assistant","model":"gpt-4o","message":{"content":[{"type":"text","text":"done"}]},"uuid":"ca-456","timestamp":"2026-03-20T10:01:00.000Z","sessionId":"cs-1","usage":{"input_tokens":500,"output_tokens":200},"toolCalls":[{"name":"edit_file"}],"stopReason":"end_turn"}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 0, "fallback", None, ts).unwrap();
        assert_eq!(msg.uuid, "ca-456");
        assert_eq!(msg.session_id.as_deref(), Some("cs-1"));
        assert_eq!(msg.model.as_deref(), Some("gpt-4o"));
        assert_eq!(msg.input_tokens, 500);
        assert_eq!(msg.output_tokens, 200);
    }

    #[test]
    fn skip_system_role() {
        let line =
            r#"{"role":"system","message":{"content":[{"type":"text","text":"You are helpful"}]}}"#;
        let ts = Utc::now();
        assert!(parse_cursor_line(line, 0, "s", None, ts).is_none());
    }

    #[test]
    fn skip_empty_and_whitespace() {
        let ts = Utc::now();
        assert!(parse_cursor_line("", 0, "s", None, ts).is_none());
        assert!(parse_cursor_line("  ", 0, "s", None, ts).is_none());
    }

    #[test]
    fn session_id_from_path_uuid() {
        let path = Path::new(
            "/home/.cursor/projects/proj/agent-transcripts/abc-def-123/abc-def-123.jsonl",
        );
        assert_eq!(session_id_from_path(path), "cursor-abc-def-123");
    }

    #[test]
    fn session_id_from_path_flat() {
        let path = Path::new("/home/.cursor/projects/proj/agent-transcripts/xyz.jsonl");
        assert_eq!(session_id_from_path(path), "cursor-xyz");
    }

    #[test]
    fn cursor_pricing_gpt4o() {
        let p = cursor_pricing_for_model("gpt-4o");
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 10.0);
    }

    #[test]
    fn cursor_pricing_sonnet() {
        let p = cursor_pricing_for_model("claude-sonnet-4-6");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn cursor_pricing_unknown_defaults_to_gpt4o() {
        let p = cursor_pricing_for_model("some-new-model");
        assert_eq!(p.input, 2.50);
    }

    #[test]
    fn cursor_pricing_deepseek() {
        let p = cursor_pricing_for_model("deepseek-v3");
        assert_eq!(p.input, 0.27);
        assert_eq!(p.output, 1.10);
    }

    // --- state.vscdb parsing tests ---

    #[test]
    fn composer_session_to_messages_with_usage() {
        let session = ComposerSession {
            key: "composerData:test-uuid-123".to_string(),
            name: Some("Fix login bug".to_string()),
            created_at: Some("2026-03-20T10:00:00Z".parse().unwrap()),
            last_updated_at: Some(1742468400000),
            is_agentic: true,
            usage_entries: vec![ComposerUsageEntry {
                model: "claude-sonnet-4-6".to_string(),
                cost_cents: 2.40,
                num_requests: 3,
            }],
            context_tokens_used: Some(50000),
            context_token_limit: Some(200000),
            model_name: Some("claude-sonnet-4-6".to_string()),
            git_branch: Some("feature/PAVA-123-fix-login".to_string()),
            cwd: Some("/projects/webapp".to_string()),
        };

        let msgs = composer_session_to_messages(&session);
        assert_eq!(msgs.len(), 2); // 1 user + 1 assistant (1 model)

        // User message
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].session_title.as_deref(), Some("Fix login bug"));

        // Assistant message
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(msgs[1].cost_cents, Some(2.40));
        assert_eq!(msgs[1].context_tokens_used, Some(50000));
        assert_eq!(msgs[1].context_token_limit, Some(200000));
        assert!(msgs[1].input_tokens > 0); // Reverse-calculated from cost

        // Git branch and cwd flow through to all messages
        assert_eq!(
            msgs[0].git_branch.as_deref(),
            Some("feature/PAVA-123-fix-login")
        );
        assert_eq!(msgs[0].cwd.as_deref(), Some("/projects/webapp"));
        assert_eq!(
            msgs[1].git_branch.as_deref(),
            Some("feature/PAVA-123-fix-login")
        );
        assert_eq!(msgs[1].cwd.as_deref(), Some("/projects/webapp"));
    }

    #[test]
    fn composer_session_to_messages_no_usage() {
        let session = ComposerSession {
            key: "composerData:empty-session".to_string(),
            name: None,
            created_at: None,
            last_updated_at: None,
            is_agentic: false,
            usage_entries: vec![],
            context_tokens_used: None,
            context_token_limit: None,
            model_name: None,
            git_branch: None,
            cwd: None,
        };

        let msgs = composer_session_to_messages(&session);
        assert_eq!(msgs.len(), 2); // 1 user + 1 minimal assistant
        assert_eq!(msgs[1].cost_cents, None);
    }

    #[test]
    fn estimate_tokens_from_cost_basic() {
        let pricing = ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            cache_read: 0.30,
        };
        let (inp, outp) = estimate_tokens_from_cost(100.0, &pricing); // $1.00
        assert!(inp > 0);
        assert!(outp > 0);
        // Verify the cost adds up approximately
        let recalc = inp as f64 * 3.0 / 1_000_000.0 + outp as f64 * 15.0 / 1_000_000.0;
        assert!((recalc - 1.0).abs() < 0.01);
    }

    #[test]
    fn estimate_tokens_zero_cost() {
        let pricing = cursor_pricing_for_model("gpt-4o");
        let (inp, outp) = estimate_tokens_from_cost(0.0, &pricing);
        assert_eq!(inp, 0);
        assert_eq!(outp, 0);
    }

    #[test]
    fn parse_composer_sessions_from_vscdb() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();

        // Insert a mock composerData entry with contextTokensUsed
        let data = serde_json::json!({
            "allComposers": [{
                "composerId": "test-id-1",
                "createdAt": 1742468400000i64,
                "lastUpdatedAt": 1742472000000i64,
                "name": "Fix login bug",
                "totalLinesAdded": 10,
                "totalLinesRemoved": 3,
                "isArchived": false,
                "type": 1
            }],
            "selectedComposerIds": [],
            "lastFocusedComposerIds": []
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('composer.composerData', ?1)",
            [data.to_string()],
        )
        .unwrap();

        // Insert the actual composerData for the session with contextUsagePercent fallback
        let session_data = serde_json::json!({
            "_v": 11,
            "composerId": "test-id-1",
            "name": "Fix login bug",
            "modelConfig": { "modelName": "claude-4-sonnet" },
            "contextUsagePercent": 25.0,
            "usageData": {},
            "fullConversationHeadersOnly": [{"type": 1, "bubbleId": "b1"}, {"type": 2, "bubbleId": "b2"}],
            "conversationState": "",
            "status": "completed",
            "createdAt": 1742468400000i64,
            "lastUpdatedAt": 1742472000000i64,
            "totalLinesAdded": 10,
            "totalLinesRemoved": 3,
            "isArchived": false,
            "isAgentic": true,
            "createdOnBranch": "feature/PAVA-42-login-fix",
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES ('composerData:test-id-1', ?1)",
            [session_data.to_string()],
        )
        .unwrap();

        let (sessions, watermark) = parse_composer_sessions(&conn, 0).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(watermark > 0);

        let s = &sessions[0];
        assert_eq!(s.name.as_deref(), Some("Fix login bug"));
        assert_eq!(s.model_name.as_deref(), Some("claude-4-sonnet"));
        assert!(s.is_agentic);
        // contextUsagePercent=25% of default 200K = 50,000 tokens
        assert_eq!(s.context_tokens_used, Some(50000));
        // createdOnBranch is extracted
        assert_eq!(
            s.git_branch.as_deref(),
            Some("feature/PAVA-42-login-fix")
        );

        // Verify messages are generated correctly
        let msgs = composer_session_to_messages(s);
        assert_eq!(msgs.len(), 2); // user + assistant
        assert_eq!(msgs[1].model.as_deref(), Some("claude-4-sonnet"));
        assert_eq!(msgs[1].input_tokens, 50000);
        assert!(msgs[1].cost_cents.is_some());
        // Branch flows through to messages
        assert_eq!(
            msgs[0].git_branch.as_deref(),
            Some("feature/PAVA-42-login-fix")
        );
        assert_eq!(
            msgs[1].git_branch.as_deref(),
            Some("feature/PAVA-42-login-fix")
        );
    }

    // --- git branch / folder extraction tests ---

    /// Create a temp dir with a unique name for testing.
    fn make_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("budi-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_git_branch_reads_head_file() {
        let dir = make_test_dir("git-head");
        let git_dir = dir.join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/my-branch\n").unwrap();

        let branch = resolve_git_branch_from_head(dir.to_str().unwrap());
        assert_eq!(branch.as_deref(), Some("feature/my-branch"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_git_branch_detached_head_returns_none() {
        let dir = make_test_dir("detached");
        let git_dir = dir.join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("HEAD"),
            "abc123def456789012345678901234567890abcd\n",
        )
        .unwrap();

        let branch = resolve_git_branch_from_head(dir.to_str().unwrap());
        assert!(branch.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_git_branch_missing_dir_returns_none() {
        let branch = resolve_git_branch_from_head("/nonexistent/path");
        assert!(branch.is_none());
    }

    #[test]
    fn file_uri_to_path_strips_prefix() {
        let p = file_uri_to_path("file:///Users/me/project/src/main.rs");
        assert_eq!(p.unwrap(), PathBuf::from("/Users/me/project/src/main.rs"));
    }

    #[test]
    fn file_uri_to_path_rejects_non_file_uri() {
        assert!(file_uri_to_path("https://example.com").is_none());
    }

    #[test]
    fn find_git_root_walks_up() {
        let dir = make_test_dir("git-root");
        let git_dir = dir.join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let nested = dir.join("src/components");
        std::fs::create_dir_all(&nested).unwrap();

        let root = find_git_root(&nested.join("App.tsx"));
        assert_eq!(root.as_deref(), Some(dir.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_folder_from_attached_uris() {
        let dir = make_test_dir("attached-uris");
        let git_dir = dir.join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();

        let uri = format!("file://{}/src/main.rs", dir.display());
        let parsed = serde_json::json!({
            "allAttachedFileCodeChunksUris": [uri],
        });

        let folder = extract_folder_from_file_uris(&parsed);
        assert_eq!(folder.as_deref(), Some(dir.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_folder_from_newly_created_files() {
        let dir = make_test_dir("newly-created");
        let git_dir = dir.join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let src = dir.join("src");
        std::fs::create_dir(&src).unwrap();

        let fs_path = format!("{}/src/new.rs", dir.display());
        let parsed = serde_json::json!({
            "newlyCreatedFiles": [{"uri": {"fsPath": fs_path}}],
        });

        let folder = extract_folder_from_file_uris(&parsed);
        assert_eq!(folder.as_deref(), Some(dir.to_str().unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn created_on_branch_preferred_over_head_fallback() {
        // When createdOnBranch is present, it should be used even if
        // we could also read .git/HEAD from the cwd
        let session = ComposerSession {
            key: "composerData:branch-test".to_string(),
            name: None,
            created_at: None,
            last_updated_at: None,
            is_agentic: false,
            usage_entries: vec![],
            context_tokens_used: None,
            context_token_limit: None,
            model_name: None,
            git_branch: Some("main".to_string()),
            cwd: Some("/some/project".to_string()),
        };

        let msgs = composer_session_to_messages(&session);
        assert_eq!(msgs[0].git_branch.as_deref(), Some("main"));
        assert_eq!(msgs[0].cwd.as_deref(), Some("/some/project"));
    }
}
