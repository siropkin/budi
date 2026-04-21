//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Primary data source: Cursor Usage API (`/api/dashboard/get-filtered-usage-events`)
//! — returns exact per-request tokens and cost. Auth token extracted from state.vscdb.
//!
//! Legacy fallback: composerData from state.vscdb (will be removed).
//! Secondary fallback: JSONL agent transcripts under `~/.cursor/projects/*/agent-transcripts/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::analytics;
use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, Provider};

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

    fn sync_direct(
        &self,
        conn: &mut Connection,
        pipeline: &mut crate::pipeline::Pipeline,
        max_age_days: Option<u64>,
    ) -> Option<Result<(usize, usize, Vec<String>)>> {
        // Sync from Cursor Usage API (exact per-request tokens and cost)
        sync_from_usage_api(conn, pipeline, max_age_days)
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let Ok(home) = crate::config::home_dir() else {
            return Vec::new();
        };
        watch_roots_for_home(&home)
    }
}

/// Compute Cursor's tailer watch roots relative to the given home dir.
///
/// Cursor writes JSONL transcripts under
/// `~/.cursor/projects/<encoded-cwd>/agent-transcripts/**/*.jsonl`. The
/// tailer attaches a recursive watcher to `~/.cursor/projects` so that new
/// per-project subdirs are picked up automatically.
///
/// The Cursor Usage API is intentionally **not** a watch root. Per ADR-0089
/// §7 it remains a pull-mode reconciliation handled by
/// [`Provider::sync_direct`] and is scheduled independently of the live
/// tailer; its lag profile is the subject of #321.
///
/// `state.vscdb` is also not a watch root — the JSONL transcripts already
/// carry the messages the pipeline needs, and `state.vscdb` is a SQLite
/// database whose write semantics are not tail-friendly.
fn watch_roots_for_home(home: &Path) -> Vec<PathBuf> {
    let projects = home.join(".cursor").join("projects");
    if projects.is_dir() {
        vec![projects]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// state.vscdb paths (cross-platform) — globalStorage + workspaceStorage
// ---------------------------------------------------------------------------

/// Returns all state.vscdb paths found on the system: globalStorage and
/// every workspace under workspaceStorage, across macOS/Linux/Windows layouts.
fn all_state_vscdb_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let home = match crate::config::home_dir() {
        Ok(h) => h,
        Err(_) => return paths,
    };
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);

    for root in cursor_user_state_roots(&home, appdata.as_deref()) {
        let global = root.join("globalStorage/state.vscdb");
        if global.exists() {
            push_unique_path(&mut paths, global);
        }

        let ws_dir = root.join("workspaceStorage");
        scan_workspace_dbs(&ws_dir, &mut paths);
    }

    paths
}

fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|p| p == &candidate) {
        paths.push(candidate);
    }
}

/// Cursor stores state.vscdb under OS-specific "User" roots:
/// - macOS: ~/Library/Application Support/Cursor/User
/// - Linux: ~/.config/Cursor/User
/// - Windows: %APPDATA%/Cursor/User (or ~/AppData/Roaming/Cursor/User fallback)
fn cursor_user_state_roots(home: &Path, appdata: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_unique_path(
        &mut roots,
        home.join("Library/Application Support/Cursor/User"),
    );
    push_unique_path(&mut roots, home.join(".config/Cursor/User"));
    push_unique_path(&mut roots, home.join("AppData/Roaming/Cursor/User"));
    if let Some(appdata_dir) = appdata {
        push_unique_path(&mut roots, appdata_dir.join("Cursor/User"));
    }
    roots
}

/// Scan a workspaceStorage directory for `*/state.vscdb` files.
fn scan_workspace_dbs(ws_dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(ws_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let db = entry.path().join("state.vscdb");
        if db.exists() {
            push_unique_path(paths, db);
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor Usage API — exact per-request tokens and cost
// ---------------------------------------------------------------------------

/// Auth credentials extracted from Cursor's state.vscdb.
struct CursorAuth {
    user_id: String,
    jwt: String,
}

/// A single usage event from the Cursor API.
#[derive(Debug)]
struct CursorUsageEvent {
    timestamp_ms: i64,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    /// None when cost is not provided (e.g. subscription "Included" plan).
    total_cents: Option<f64>,
}

/// Decode a base64url-encoded string (no padding required).
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: [u8; 128] = {
        let mut t = [255u8; 128];
        let mut i = 0u8;
        while i < 26 {
            t[(b'A' + i) as usize] = i;
            i += 1;
        }
        i = 0;
        while i < 26 {
            t[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        i = 0;
        while i < 10 {
            t[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        t[b'+' as usize] = 62;
        t[b'-' as usize] = 62;
        t[b'/' as usize] = 63;
        t[b'_' as usize] = 63;
        t
    };
    let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut buf = [0u32; 4];
        for (i, &b) in chunk.iter().enumerate() {
            if b >= 128 {
                return None;
            }
            let v = TABLE[b as usize];
            if v == 255 {
                return None;
            }
            buf[i] = v as u32;
        }
        let n = (buf[0] << 18) | (buf[1] << 12) | (buf[2] << 6) | buf[3];
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Extract auth credentials from Cursor's state.vscdb ItemTable.
/// Returns `None` when auth is unavailable (not installed, empty, expired, etc.).
/// Expired tokens are logged via `tracing::warn`.
fn extract_cursor_auth() -> Option<CursorAuth> {
    let paths = all_state_vscdb_paths();
    let global_path = paths
        .into_iter()
        .find(|p| p.to_string_lossy().contains("globalStorage"))?;

    let vscdb = Connection::open_with_flags(
        &global_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;

    let jwt: String = vscdb
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
            [],
            |row| row.get(0),
        )
        .ok()?;

    if jwt.is_empty() {
        return None;
    }

    // Decode JWT payload to extract user_id from `sub` field.
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let decoded = base64url_decode(parts[1])?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;

    // Check JWT expiry — `exp` is assumed to be in seconds (standard JWT).
    // If it looks like milliseconds (> 1_700_000_000_000), convert first.
    if let Some(raw_exp) = payload.get("exp").and_then(|v| v.as_i64()) {
        let exp = if raw_exp > 1_700_000_000_000 {
            raw_exp / 1000
        } else {
            raw_exp
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if now > exp {
            tracing::warn!(
                "Cursor auth token expired — restart Cursor to refresh it. Using estimated costs from local files."
            );
            return None;
        }
    }

    let sub = payload.get("sub").and_then(|v| v.as_str())?;
    let user_id = sub.split('|').next_back().unwrap_or(sub).to_string();

    Some(CursorAuth { user_id, jwt })
}

fn parse_timestamp_ms(value: &Value) -> Option<i64> {
    let ts = match value {
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())),
        _ => None,
    }?;
    (ts > 0).then_some(ts)
}

/// Parse a single usage event JSON value into a CursorUsageEvent.
/// Returns None if the event should be skipped.
fn parse_usage_event(ev: &Value) -> Option<CursorUsageEvent> {
    let ts: i64 = ev.get("timestamp").and_then(parse_timestamp_ms)?;

    let model = ev
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let token_usage = ev.get("tokenUsage");
    let input_tokens = token_usage
        .and_then(|t: &Value| t.get("inputTokens"))
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);
    let output_tokens = token_usage
        .and_then(|t: &Value| t.get("outputTokens"))
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = token_usage
        .and_then(|t: &Value| t.get("cacheWriteTokens"))
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = token_usage
        .and_then(|t: &Value| t.get("cacheReadTokens"))
        .and_then(|v: &Value| v.as_u64())
        .unwrap_or(0);

    let total_cents_raw = token_usage
        .and_then(|t: &Value| t.get("totalCents"))
        .and_then(|v: &Value| v.as_f64());

    let is_subscription = ev
        .get("kind")
        .and_then(|v| v.as_str())
        .is_some_and(|k| k.eq_ignore_ascii_case("included"));

    let total_cents = match total_cents_raw {
        Some(c) if c == 0.0 && is_subscription => Some(0.0),
        Some(c) if c < 0.0 => {
            tracing::warn!("Cursor API totalCents={c} is negative, clamping to 0.0");
            Some(0.0)
        }
        Some(c) if c > 100_000.0 => {
            tracing::warn!(
                "Cursor API totalCents={c} exceeds $1000 — skipping event as likely corrupt"
            );
            return None;
        }
        Some(c) if c > 5000.0 => {
            let dollars = c / 100.0;
            tracing::warn!(
                "Cursor API totalCents={c} unusually high for a single request (>${dollars:.0} dollars)"
            );
            Some(c)
        }
        Some(c) => Some(c),
        None => None,
    };

    let total_tokens = input_tokens
        .saturating_add(output_tokens)
        .saturating_add(cache_creation_tokens)
        .saturating_add(cache_read_tokens);
    if total_tokens == 0 && total_cents.is_none() {
        return None;
    }

    Some(CursorUsageEvent {
        timestamp_ms: ts,
        model,
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        total_cents,
    })
}

/// Extract usage-event timestamp in milliseconds from raw API JSON.
fn usage_event_timestamp_ms(ev: &Value) -> Option<i64> {
    ev.get("timestamp").and_then(parse_timestamp_ms)
}

struct UsageFetchResult {
    events: Vec<CursorUsageEvent>,
    pages_fetched: u32,
}

fn fetch_usage_events_with_page_loader<F>(
    since_ms: Option<i64>,
    paginate_all: bool,
    mut load_page: F,
) -> Result<UsageFetchResult>
where
    F: FnMut(u32) -> Result<Vec<Value>>,
{
    let since = since_ms.unwrap_or(0);
    let mut all_events: Vec<CursorUsageEvent> = Vec::new();

    // API returns 100 events per page, newest first. In quick mode we still
    // paginate when a watermark exists, until we cross that watermark.
    let should_paginate = paginate_all || since_ms.is_some();
    let max_pages: u32 = if should_paginate { 200 } else { 1 };
    let mut pages_fetched = 0;

    for page in 1..=max_pages {
        let events_arr = load_page(page)?;
        if events_arr.is_empty() {
            break;
        }
        pages_fetched = page;

        // Track whether all events on this page were older than watermark.
        // Use raw timestamps so malformed events do not force an early stop.
        let mut all_below_watermark = true;

        for ev in &events_arr {
            if usage_event_timestamp_ms(ev).is_some_and(|ts| ts > since) {
                all_below_watermark = false;
            }

            if let Some(parsed) = parse_usage_event(ev)
                && parsed.timestamp_ms > since
            {
                all_events.push(parsed);
            }
        }

        // If every event on this page was already synced, no need to fetch older pages.
        if all_below_watermark {
            break;
        }

        // Last page: fewer than 100 events means no more pages.
        if events_arr.len() < 100 {
            break;
        }

        if page > 1 {
            tracing::info!(
                "Cursor API: fetched page {page} ({} new events so far)",
                all_events.len()
            );
        }
    }

    all_events.sort_by_key(|e| e.timestamp_ms);
    Ok(UsageFetchResult {
        events: all_events,
        pages_fetched,
    })
}

/// Fetch usage events from Cursor's API with pagination.
/// `since_ms`: only return events newer than this timestamp.
/// `paginate_all`: when true, fetches all pages; when false, quick-sync mode.
fn fetch_usage_events(
    auth: &CursorAuth,
    since_ms: Option<i64>,
    paginate_all: bool,
) -> Result<UsageFetchResult> {
    let cookie = format!(
        "WorkosCursorSessionToken={}%3A%3A{}",
        auth.user_id, auth.jwt
    );

    // Keep API probes bounded so sync does not look "stuck" when Cursor's
    // endpoint is slow/unreachable. We fall back to local transcript files.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .into();

    fetch_usage_events_with_page_loader(since_ms, paginate_all, |page| {
        let body_json = if page == 1 {
            serde_json::json!({})
        } else {
            serde_json::json!({"page": page})
        };

        let mut response = agent
            .post("https://cursor.com/api/dashboard/get-filtered-usage-events")
            .header("Cookie", &cookie)
            .header("Origin", "https://cursor.com")
            .header("Referer", "https://cursor.com/dashboard")
            .send_json(body_json)
            .with_context(|| format!("Cursor Usage API request failed (page {page})"))?;

        let body: Value = response.body_mut().read_json()?;

        Ok(body
            .get("usageEventsDisplay")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    })
}

/// Find the session whose time range contains `ts_ms`, using strict containment
/// first, then falling back to a ±5s clock-skew window.
fn find_matching_session(ts_ms: i64, sessions: &[SessionContext]) -> Option<&SessionContext> {
    const CLOCK_SKEW_MS: i64 = 5000;
    sessions
        .iter()
        .filter(|s| ts_ms >= s.start_ms && ts_ms <= s.end_ms)
        .min_by_key(|s| (ts_ms - s.start_ms).abs())
        .or_else(|| {
            sessions
                .iter()
                .filter(|s| {
                    ts_ms >= (s.start_ms - CLOCK_SKEW_MS) && ts_ms <= (s.end_ms + CLOCK_SKEW_MS)
                })
                .min_by_key(|s| {
                    let d_start = (ts_ms - s.start_ms).abs();
                    let d_end = (ts_ms - s.end_ms).abs();
                    d_start.min(d_end)
                })
        })
}

/// Session context for correlating API events to hook sessions.
struct SessionContext {
    start_ms: i64,
    end_ms: i64, // i64::MAX if session still open
    session_id: String,
    workspace_root: Option<String>,
    repo_id: Option<String>,
    git_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComposerHeadersPayload {
    #[serde(default, rename = "allComposers")]
    all_composers: Vec<ComposerHeader>,
}

#[derive(Debug, Deserialize)]
struct ComposerHeader {
    #[serde(rename = "composerId")]
    composer_id: String,
    #[serde(rename = "createdAt")]
    created_at: i64,
    #[serde(default, rename = "lastUpdatedAt")]
    last_updated_at: Option<i64>,
    #[serde(default, rename = "isArchived")]
    is_archived: bool,
    #[serde(default, rename = "workspaceIdentifier")]
    workspace_identifier: Option<ComposerWorkspaceIdentifier>,
}

#[derive(Debug, Deserialize)]
struct ComposerWorkspaceIdentifier {
    #[serde(default)]
    uri: Option<ComposerWorkspaceUri>,
}

#[derive(Debug, Deserialize)]
struct ComposerWorkspaceUri {
    #[serde(default, rename = "fsPath")]
    fs_path: Option<String>,
}

/// Build Cursor session contexts from global state.vscdb composer headers.
/// This is more reliable than relying on our own sessions table timestamps
/// when hooks were missing or late.
fn load_composer_header_contexts(now_ms: i64) -> Vec<SessionContext> {
    const LOOKBACK_MS: i64 = 30 * 24 * 60 * 60 * 1000;
    const END_SKEW_MS: i64 = 5 * 60 * 1000;

    let global_path = match all_state_vscdb_paths()
        .into_iter()
        .find(|p| p.to_string_lossy().contains("globalStorage"))
    {
        Some(p) => p,
        None => return Vec::new(),
    };

    let vscdb = match Connection::open_with_flags(
        &global_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(db) => db,
        Err(_) => return Vec::new(),
    };

    let raw_headers: String = match vscdb.query_row(
        "SELECT value FROM ItemTable WHERE key = 'composer.composerHeaders'",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let payload: ComposerHeadersPayload = match serde_json::from_str(&raw_headers) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for composer in payload.all_composers {
        if composer.is_archived {
            continue;
        }
        if composer.composer_id.trim().is_empty() {
            continue;
        }
        let start_ms = composer.created_at;
        let mut end_ms = composer.last_updated_at.unwrap_or(start_ms);
        if end_ms < start_ms {
            end_ms = start_ms;
        }
        end_ms = end_ms.saturating_add(END_SKEW_MS);
        if end_ms < now_ms - LOOKBACK_MS {
            continue;
        }

        let workspace_root = composer
            .workspace_identifier
            .and_then(|w| w.uri)
            .and_then(|u| u.fs_path)
            .filter(|p| !p.trim().is_empty());
        let repo_id = workspace_root
            .as_deref()
            .map(std::path::Path::new)
            .map(crate::repo_id::resolve_repo_id);
        let git_branch = workspace_root
            .as_deref()
            .and_then(resolve_git_branch_from_head);

        out.push(SessionContext {
            start_ms,
            end_ms,
            session_id: crate::identity::normalize_session_id(&composer.composer_id),
            workspace_root,
            repo_id,
            git_branch,
        });
    }

    out.sort_by_key(|s| s.start_ms);
    out
}

/// Load session contexts from the sessions table.
fn load_session_contexts(conn: &Connection) -> Vec<SessionContext> {
    // Only load sessions from the last 30 days to avoid stale attribution.
    // Without this filter, API events could match sessions from months ago.
    let mut stmt = match conn.prepare(
        "SELECT id, started_at, ended_at, workspace_root, repo_id, git_branch
         FROM sessions WHERE provider = 'cursor'
           AND started_at >= datetime('now', '-30 days')
         ORDER BY started_at ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let db_contexts = stmt
        .query_map([], |row| {
            let cid: String = row.get(0)?;
            let started: String = row.get(1)?;
            let ended: Option<String> = row.get(2)?;

            let start_ms = started
                .parse::<DateTime<Utc>>()
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0);
            let end_ms = ended
                .and_then(|e| e.parse::<DateTime<Utc>>().ok())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(i64::MAX);

            Ok(SessionContext {
                start_ms,
                end_ms,
                session_id: crate::identity::normalize_session_id(&cid),
                workspace_root: row.get(3)?,
                repo_id: row.get(4)?,
                git_branch: row.get(5)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default();

    let mut merged: std::collections::HashMap<String, SessionContext> = db_contexts
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();

    // Merge in authoritative local Cursor composer windows from state.vscdb.
    // They provide real conversation timing even when hooks were missing.
    let now_ms = Utc::now().timestamp_millis();
    for local in load_composer_header_contexts(now_ms) {
        if let Some(existing) = merged.get_mut(&local.session_id) {
            existing.start_ms = existing.start_ms.min(local.start_ms);
            existing.end_ms = existing.end_ms.max(local.end_ms);
            if existing.workspace_root.is_none() {
                existing.workspace_root = local.workspace_root.clone();
            }
            let repo_missing = existing
                .repo_id
                .as_deref()
                .map(|v| v.is_empty() || v == "unknown")
                .unwrap_or(true);
            if repo_missing {
                existing.repo_id = local.repo_id.clone();
            }
            if existing.git_branch.is_none() {
                existing.git_branch = local.git_branch.clone();
            }
        } else {
            merged.insert(local.session_id.clone(), local);
        }
    }

    let mut contexts: Vec<SessionContext> = merged.into_values().collect();
    contexts.sort_by_key(|s| s.start_ms);
    contexts
}

fn deterministic_cursor_message_uuid(session_id: &str, line_index: usize, line: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(line_index.to_le_bytes());
    hasher.update(b"\n");
    hasher.update(line.as_bytes());
    let hash = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    // RFC4122 version 4/variant bits for canonical UUID-like representation.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ])
    )
}

fn deterministic_cursor_usage_uuid(ev: &CursorUsageEvent) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ev.timestamp_ms.to_le_bytes());
    hasher.update(b"\n");
    hasher.update(ev.model.as_bytes());
    hasher.update(b"\n");
    hasher.update(ev.input_tokens.to_le_bytes());
    hasher.update(ev.output_tokens.to_le_bytes());
    hasher.update(ev.cache_creation_tokens.to_le_bytes());
    hasher.update(ev.cache_read_tokens.to_le_bytes());
    let hash = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        u16::from_be_bytes([bytes[8], bytes[9]]),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
        ])
    )
}

/// Convert API usage events into ParsedMessages, correlating with hook sessions.
fn usage_events_to_messages(
    events: &[CursorUsageEvent],
    sessions: &[SessionContext],
) -> Vec<ParsedMessage> {
    events
        .iter()
        .map(|ev| {
            let matched = find_matching_session(ev.timestamp_ms, sessions);

            let session_id = matched.map(|s| crate::identity::normalize_session_id(&s.session_id));

            let timestamp =
                DateTime::from_timestamp_millis(ev.timestamp_ms).unwrap_or_else(Utc::now);

            // Deterministic UUID-like id derived from event fields.
            // Keeps IDs stable across re-syncs while enforcing canonical UUID format.
            let uuid = deterministic_cursor_usage_uuid(ev);

            ParsedMessage {
                uuid,
                session_id,
                timestamp,
                cwd: matched.and_then(|s| s.workspace_root.clone()),
                role: "assistant".to_string(),
                model: Some(ev.model.clone()),
                input_tokens: ev.input_tokens,
                output_tokens: ev.output_tokens,
                cache_creation_tokens: ev.cache_creation_tokens,
                cache_read_tokens: ev.cache_read_tokens,
                git_branch: matched.and_then(|s| s.git_branch.clone()),
                repo_id: matched.and_then(|s| s.repo_id.clone()),
                provider: "cursor".to_string(),
                cost_cents: ev.total_cents,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: if ev.total_cents.is_some() {
                    "exact".to_string()
                } else {
                    "estimated".to_string()
                },
                // ADR-0091 §1 / #376: Cursor exact-cost rows come from
                // Cursor's Usage API, not from the LiteLLM manifest. Tag
                // them with `upstream:api` so `pricing_source` stays
                // honest; the CostEnricher will tag manifest-estimated
                // rows (those without `total_cents`) on its pass.
                pricing_source: if ev.total_cents.is_some() {
                    Some(crate::pricing::COLUMN_VALUE_UPSTREAM_API.to_string())
                } else {
                    None
                },
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
            }
        })
        .collect()
}

/// Sync from Cursor's Usage API (exact per-request tokens and cost).
/// `max_age_days`: Some(N) for quick sync (paginate until prior watermark),
/// None for full history (all pages).
fn sync_from_usage_api(
    conn: &mut Connection,
    pipeline: &mut crate::pipeline::Pipeline,
    max_age_days: Option<u64>,
) -> Option<Result<(usize, usize, Vec<String>)>> {
    let auth = match extract_cursor_auth() {
        Some(a) => a,
        None => {
            // No valid auth — fall back to file-based sync (returns None).
            return None;
        }
    };

    let watermark_key = "cursor-api-usage";
    let watermark = analytics::get_sync_offset(conn, watermark_key)
        .ok()
        .and_then(|v| {
            let ts = v as i64;
            if ts > 0 { Some(ts) } else { None }
        });

    // Quick sync (max_age_days=Some): fetch pages until crossing prior watermark.
    // Full history (max_age_days=None): paginate all pages back to watermark.
    let paginate_all = max_age_days.is_none();

    let fetched = match fetch_usage_events(&auth, watermark, paginate_all) {
        Ok(result) => result,
        Err(e) => {
            // API can be unavailable transiently (network/VPN/outage). Fall back
            // to local transcript files so Cursor sessions still appear.
            tracing::warn!(
                "Cursor Usage API unavailable ({e:#}); falling back to local transcript sync"
            );
            return None;
        }
    };

    let api_calls = fetched.pages_fetched.max(1) as usize;
    let warnings = Vec::new();
    if fetched.pages_fetched > 1 {
        tracing::info!(
            "Cursor Usage API returned {} pages in one sync tick (watermark catch-up active)",
            fetched.pages_fetched
        );
    }

    if fetched.events.is_empty() {
        return Some(Ok((0, 0, warnings)));
    }

    // Session repair/backfill is only needed when new Cursor data arrives.
    run_cursor_repairs(conn);

    let sessions = load_session_contexts(conn);
    let mut messages = usage_events_to_messages(&fetched.events, &sessions);
    let tags = pipeline.process(&mut messages);
    let count = match analytics::ingest_messages(conn, &messages, Some(&tags)) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };

    // Update watermark to latest event timestamp.
    if let Some(newest_ts) = fetched.events.iter().map(|e| e.timestamp_ms).max() {
        match analytics::set_sync_offset(conn, watermark_key, newest_ts as usize) {
            Ok(()) => {}
            Err(e) => return Some(Err(e)),
        }
    }

    Some(Ok((api_calls, count, warnings)))
}

/// Session repair and backfill for Cursor data.
pub(crate) fn run_cursor_repairs(conn: &mut Connection) {
    // Persist session windows/metadata discovered from Cursor local composer
    // headers so session rows stay useful even when hooks were missing.
    repair_cursor_sessions_from_composer_headers(conn);

    // Upgrade legacy Cursor-internal cwd paths (`~/.cursor/projects/<slug>`) to
    // real workspace roots discovered in worker.log, then backfill repo/branch.
    repair_cursor_workspace_metadata(conn);

    // Backfill orphaned messages (ingested before their session row existed).
    let sessions = load_session_contexts(conn);
    if !sessions.is_empty() {
        let orphaned = backfill_cursor_session_ids(conn, &sessions);
        if orphaned > 0 {
            tracing::info!(
                "Cursor session backfill: assigned session_id to {orphaned} orphaned messages"
            );
        }
    }

    // Repair messages that have a session_id but stale metadata (repo_id=unknown,
    // missing cwd/branch) — propagate from the now-correct session row.
    let _ = conn.execute(
        "UPDATE messages SET
            cwd = COALESCE(cwd, (SELECT workspace_root FROM sessions WHERE id = messages.session_id)),
            repo_id = (SELECT COALESCE(repo_id, 'unknown') FROM sessions WHERE id = messages.session_id),
            git_branch = COALESCE(git_branch, (SELECT git_branch FROM sessions WHERE id = messages.session_id))
         WHERE provider = 'cursor'
           AND session_id IS NOT NULL
           AND (repo_id IS NULL OR repo_id = 'unknown')
           AND EXISTS (
             SELECT 1 FROM sessions s
             WHERE s.id = messages.session_id AND s.repo_id IS NOT NULL AND s.repo_id != 'unknown'
           )",
        [],
    );
}

fn repair_cursor_sessions_from_composer_headers(conn: &mut Connection) {
    let contexts = load_composer_header_contexts(Utc::now().timestamp_millis());
    if contexts.is_empty() {
        return;
    }

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return,
    };
    for s in &contexts {
        let start_iso = DateTime::from_timestamp_millis(s.start_ms)
            .unwrap_or_else(Utc::now)
            .to_rfc3339();
        let end_iso = DateTime::from_timestamp_millis(s.end_ms)
            .unwrap_or_else(Utc::now)
            .to_rfc3339();
        let _ = tx.execute(
            "UPDATE sessions SET
                started_at = COALESCE(started_at, ?2),
                ended_at = COALESCE(ended_at, ?3),
                workspace_root = COALESCE(NULLIF(workspace_root, ''), ?4),
                repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?5),
                git_branch = COALESCE(NULLIF(git_branch, ''), ?6)
             WHERE id = ?1 AND provider = 'cursor'",
            params![
                s.session_id,
                start_iso,
                end_iso,
                s.workspace_root,
                s.repo_id,
                s.git_branch
            ],
        );
    }
    let _ = tx.commit();
}

fn repair_cursor_workspace_metadata(conn: &mut Connection) {
    let legacy_cwds: Vec<String> = {
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT cwd
             FROM messages
             WHERE provider = 'cursor'
               AND cwd IS NOT NULL
               AND cwd != ''
               AND cwd LIKE '%/.cursor/projects/%'",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([], |row| row.get(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    if legacy_cwds.is_empty() {
        return;
    }

    for old_cwd in legacy_cwds {
        let project_dir = std::path::Path::new(&old_cwd);
        let Some(workspace_root) = workspace_root_from_project_dir(project_dir) else {
            continue;
        };

        let repo_id = crate::repo_id::resolve_repo_id(std::path::Path::new(&workspace_root));
        let git_branch = resolve_git_branch_from_head(&workspace_root);

        let session_ids: Vec<String> = {
            let mut stmt = match conn.prepare(
                "SELECT DISTINCT session_id
                 FROM messages
                 WHERE provider = 'cursor' AND cwd = ?1 AND session_id IS NOT NULL",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            stmt.query_map([&old_cwd], |row| row.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        };

        let _ = conn.execute(
            "UPDATE messages SET
                cwd = ?1,
                repo_id = ?2,
                git_branch = COALESCE(NULLIF(git_branch, ''), ?3)
             WHERE provider = 'cursor' AND cwd = ?4",
            params![workspace_root, repo_id, git_branch, old_cwd],
        );

        for sid in &session_ids {
            let _ = conn.execute(
                "UPDATE sessions SET
                    workspace_root = COALESCE(NULLIF(workspace_root, ''), ?2),
                    repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?3),
                    git_branch = COALESCE(NULLIF(git_branch, ''), ?4)
                 WHERE id = ?1 AND provider = 'cursor'",
                params![sid, workspace_root, repo_id, git_branch],
            );
        }
    }
}

/// Retroactively assign session_id to Cursor messages that have NULL session_id.
/// Uses the same timestamp-overlap logic as `usage_events_to_messages`.
fn backfill_cursor_session_ids(conn: &mut Connection, sessions: &[SessionContext]) -> usize {
    let orphans: Vec<(String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT id, timestamp FROM messages
             WHERE provider = 'cursor' AND session_id IS NULL AND role = 'assistant'
             LIMIT 5000",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    if orphans.is_empty() {
        return 0;
    }

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return 0,
    };

    let mut updated = 0;
    {
        let mut update_stmt = match tx.prepare_cached(
            "UPDATE messages SET session_id = ?1,
             cwd = COALESCE(NULLIF(cwd, ''), ?2),
             repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?3),
             git_branch = COALESCE(NULLIF(git_branch, ''), ?4)
             WHERE id = ?5",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };

        for (uuid, ts_str) in &orphans {
            let Ok(ts) = ts_str.parse::<DateTime<Utc>>() else {
                continue;
            };
            let matched = find_matching_session(ts.timestamp_millis(), sessions);

            if let Some(session) = matched {
                let _ = update_stmt.execute(params![
                    session.session_id,
                    session.workspace_root,
                    session.repo_id,
                    session.git_branch,
                    uuid,
                ]);
                updated += 1;
            }
        }
    }

    let _ = tx.commit();
    updated
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

// ---------------------------------------------------------------------------
// JSONL fallback helpers
// ---------------------------------------------------------------------------

fn cursor_home() -> Result<PathBuf> {
    Ok(crate::config::home_dir()?.join(".cursor"))
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
        .map(crate::identity::normalize_session_id)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Extract the Cursor project slug from the transcript path.
fn cwd_from_path(path: &Path) -> Option<String> {
    let mut current = path;
    while let Some(parent) = current.parent() {
        if parent.file_name().is_some_and(|n| n == "agent-transcripts")
            && let Some(project_dir) = parent.parent()
        {
            return workspace_root_from_project_dir(project_dir);
        }
        current = parent;
    }
    None
}

/// Best-effort workspace root lookup from Cursor's per-project `worker.log`.
///
/// This keeps repo_id/git metadata aligned with Claude sessions by using the
/// real workspace path (e.g. `/Users/me/repo`) instead of Cursor's internal
/// project storage path (`~/.cursor/projects/<slug>`).
fn workspace_root_from_project_dir(project_dir: &Path) -> Option<String> {
    let worker_log = project_dir.join("worker.log");
    let content = std::fs::read_to_string(worker_log).ok()?;

    let mut last_seen: Option<String> = None;
    for line in content.lines() {
        let Some(idx) = line.find("workspacePath=") else {
            continue;
        };
        let tail = &line[idx + "workspacePath=".len()..];
        let candidate = tail.split_whitespace().next().unwrap_or("").trim();
        if !candidate.is_empty() {
            last_seen = Some(candidate.to_string());
        }
    }
    last_seen
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
    #[serde(rename = "toolCalls")]
    tool_calls: Option<Vec<CursorToolCall>>,
    #[serde(rename = "tool_calls")]
    tool_calls_alt: Option<Vec<CursorToolCall>>,
    uuid: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    cwd: Option<String>,
    /// Nested message envelope. Cursor transcripts wrap user/assistant text
    /// inside `message.content` (same shape as the Anthropic wire format).
    /// We only look at user entries to classify the prompt; assistant text
    /// is not used. See R1.2 (#222).
    message: Option<CursorMessage>,
}

#[derive(Debug, Deserialize)]
struct CursorMessage {
    content: Option<CursorMessageContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CursorMessageContent {
    Text(String),
    Blocks(Vec<serde_json::Value>),
}

/// Extract plain text from a Cursor message content payload. Returns `None`
/// when the payload is empty. Only textual blocks are read; code blocks and
/// tool-use payloads are ignored so the classifier only sees prompt prose.
fn cursor_prompt_text(message: Option<&CursorMessage>) -> Option<String> {
    let content = message.and_then(|m| m.content.as_ref())?;
    let text = match content {
        CursorMessageContent::Text(s) => s.clone(),
        CursorMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[derive(Debug, Deserialize)]
struct CursorToolCall {
    name: Option<String>,
    /// Tool-call arguments. Cursor version churn means the exact shape is
    /// not stable, so we accept any JSON value and let
    /// `crate::file_attribution` pick out file-path fields it recognises
    /// (`file_path`, `target_file`, `path`, `pattern`). Added in R1.4 (#292).
    #[serde(default, alias = "arguments", alias = "input")]
    args: Option<serde_json::Value>,
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

    let msg_session_id =
        crate::identity::normalize_session_id(entry.session_id.as_deref().unwrap_or(session_id));
    let request_id = entry.request_id;
    let uuid = entry
        .uuid
        .or_else(|| request_id.clone().filter(|id| !id.is_empty()))
        .unwrap_or_else(|| deterministic_cursor_message_uuid(&msg_session_id, line_index, line));
    let msg_cwd = entry.cwd.or_else(|| cwd.map(|s| s.to_string()));
    let git_branch = msg_cwd.as_deref().and_then(resolve_git_branch_from_head);

    // R1.4 (#292): collect tool names + raw file paths from tool args. We
    // walk the list once so we pick both up atomically per message.
    let mut tool_names: Vec<String> = Vec::new();
    let mut tool_files: Vec<String> = Vec::new();
    for call in entry
        .tool_calls
        .or(entry.tool_calls_alt)
        .unwrap_or_default()
    {
        let name = call.name.unwrap_or_default();
        let trimmed = name.trim().to_string();
        if !trimmed.is_empty() {
            tool_names.push(trimmed.clone());
        }
        if let Some(args) = call.args.as_ref() {
            crate::file_attribution::collect_cursor_tool_paths(&trimmed, args, &mut tool_files);
        }
    }
    tool_names.sort();
    tool_names.dedup();

    match role {
        "user" | "human" => {
            // R1.2 (#222): classify Cursor user prompts so the `activity`
            // taxonomy is populated for Cursor history too. The classifier
            // runs on prompt text in-memory only; no content is stored.
            let classification = cursor_prompt_text(entry.message.as_ref())
                .as_deref()
                .and_then(crate::hooks::classify_prompt_detailed);
            let (prompt_category, prompt_category_source, prompt_category_confidence) =
                match classification {
                    Some(c) => (
                        Some(c.category),
                        Some(c.source.to_string()),
                        Some(c.confidence.to_string()),
                    ),
                    None => (None, None, None),
                };
            Some(ParsedMessage {
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
                git_branch: git_branch.clone(),
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "n/a".to_string(),
                pricing_source: None,
                request_id: request_id.clone(),
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
            })
        }
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
                git_branch: git_branch.clone(),
                repo_id: None,
                provider: "cursor".to_string(),
                cost_cents: None,
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
                pricing_source: None,
                request_id,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
                prompt_category: None,
                prompt_category_source: None,
                prompt_category_confidence: None,
                tool_names,
                tool_use_ids: Vec::new(),
                tool_files,
                tool_outcomes: Vec::new(),
            })
        }
        _ => None,
    }
}

/// Parse all lines from a Cursor JSONL string with incremental offset support.
pub(crate) fn parse_cursor_transcript(
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

    let remaining = &content[start_offset..];
    let mut pos = 0;
    for line in remaining.lines() {
        let line_end = pos + line.len();
        let has_newline = line_end < remaining.len() && remaining.as_bytes()[line_end] == b'\n';
        if !has_newline && line_end == remaining.len() {
            break;
        }
        pos = line_end + if has_newline { 1 } else { 0 };
        if let Some(msg) = parse_cursor_line(line, line_index, session_id, cwd, fallback_ts) {
            messages.push(msg);
        }
        offset = start_offset + pos;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn looks_like_uuid(s: &str) -> bool {
        if s.len() != 36 {
            return false;
        }
        for (i, ch) in s.chars().enumerate() {
            if [8, 13, 18, 23].contains(&i) {
                if ch != '-' {
                    return false;
                }
            } else if !ch.is_ascii_hexdigit() {
                return false;
            }
        }
        true
    }

    // --- JSONL parsing tests ---

    #[test]
    fn parse_real_cursor_user_message() {
        let line = r#"{"role":"user","message":{"content":[{"type":"text","text":"fix the bug in main.rs"}]}}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 0, "cursor-abc", Some("/proj"), ts).unwrap();
        assert_eq!(msg.role, "user");
        assert!(looks_like_uuid(&msg.uuid));
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
        assert!(looks_like_uuid(&msg.uuid));
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
        assert!(msgs.iter().all(|m| looks_like_uuid(&m.uuid)));
        assert_ne!(msgs[0].uuid, msgs[1].uuid);
        assert_ne!(msgs[1].uuid, msgs[2].uuid);
        assert_ne!(msgs[0].uuid, msgs[2].uuid);

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
        assert_eq!(msg.tool_names, vec!["edit_file".to_string()]);
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
        assert_eq!(session_id_from_path(path), "abc-def-123");
    }

    #[test]
    fn session_id_from_path_flat() {
        let path = Path::new("/home/.cursor/projects/proj/agent-transcripts/xyz.jsonl");
        assert_eq!(session_id_from_path(path), "xyz");
    }

    #[test]
    fn parse_cursor_line_normalizes_prefixed_session_uuid() {
        let line =
            r#"{"role":"assistant","sessionId":"cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb"}"#;
        let ts = Utc::now();
        let msg = parse_cursor_line(line, 1, "fallback", None, ts).unwrap();
        assert_eq!(
            msg.session_id.as_deref(),
            Some("d99dfe22-d05c-4c78-8698-015d06e5dabb")
        );
    }

    #[test]
    fn workspace_root_from_project_dir_reads_worker_log() {
        let dir = make_test_dir("cursor-worker-log");
        std::fs::write(
            dir.join("worker.log"),
            "[info] foo\n[info] Getting tree structure for workspacePath=/Users/test/repo\n",
        )
        .unwrap();

        let workspace = workspace_root_from_project_dir(&dir);
        assert_eq!(workspace.as_deref(), Some("/Users/test/repo"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- git branch tests ---

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

    // --- Usage API tests ---

    #[test]
    fn usage_events_to_messages_basic() {
        let events = vec![
            CursorUsageEvent {
                timestamp_ms: 1774455909363,
                model: "composer-2-fast".to_string(),
                input_tokens: 2958,
                output_tokens: 1663,
                cache_creation_tokens: 0,
                cache_read_tokens: 48214,
                total_cents: Some(1.68),
            },
            CursorUsageEvent {
                timestamp_ms: 1774455910000,
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 10000,
                output_tokens: 5000,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_cents: Some(12.50),
            },
        ];

        let session_ranges = vec![SessionContext {
            start_ms: 1774455900000,
            end_ms: 1774455920000,
            session_id: "session-abc".to_string(),
            workspace_root: Some("/projects/webapp".to_string()),
            repo_id: Some("github.com/acme/webapp".to_string()),
            git_branch: Some("feature/PROJ-42-fix".to_string()),
        }];

        let msgs = usage_events_to_messages(&events, &session_ranges);
        assert_eq!(msgs.len(), 2);

        // First event
        assert_eq!(msgs[0].model.as_deref(), Some("composer-2-fast"));
        assert_eq!(msgs[0].input_tokens, 2958);
        assert_eq!(msgs[0].output_tokens, 1663);
        assert_eq!(msgs[0].cache_read_tokens, 48214);
        assert_eq!(msgs[0].cost_cents, Some(1.68));
        assert_eq!(msgs[0].cost_confidence, "exact");
        assert_eq!(msgs[0].session_id.as_deref(), Some("session-abc"));
        assert_eq!(msgs[0].provider, "cursor");
        assert_eq!(msgs[0].role, "assistant");
        // Session context flows through to message
        assert_eq!(msgs[0].cwd.as_deref(), Some("/projects/webapp"));
        assert_eq!(msgs[0].repo_id.as_deref(), Some("github.com/acme/webapp"));
        assert_eq!(msgs[0].git_branch.as_deref(), Some("feature/PROJ-42-fix"));

        // Second event
        assert_eq!(msgs[1].model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(msgs[1].cost_cents, Some(12.50));
        assert_eq!(msgs[1].session_id.as_deref(), Some("session-abc"));
        assert_eq!(msgs[1].git_branch.as_deref(), Some("feature/PROJ-42-fix"));
    }

    #[test]
    fn usage_events_orphan_when_no_session_match() {
        let events = vec![CursorUsageEvent {
            timestamp_ms: 1774455909363,
            model: "gpt-4o".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_cents: Some(0.5),
        }];

        // No sessions at all
        let msgs = usage_events_to_messages(&events, &[]);
        assert_eq!(msgs[0].session_id, None);
        assert!(msgs[0].cwd.is_none());
        assert!(msgs[0].repo_id.is_none());
        assert!(msgs[0].git_branch.is_none());
    }

    #[test]
    fn usage_events_deterministic_uuid() {
        let events = vec![CursorUsageEvent {
            timestamp_ms: 1774455909363,
            model: "gpt-4o".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_cents: Some(0.5),
        }];

        let msgs = usage_events_to_messages(&events, &[]);
        assert!(looks_like_uuid(&msgs[0].uuid));
    }

    #[test]
    fn usage_events_subscription_no_cost() {
        // Subscription ("Included") plan: tokens present but no cost
        let events = vec![CursorUsageEvent {
            timestamp_ms: 1774455909363,
            model: "composer-2".to_string(),
            input_tokens: 22770,
            output_tokens: 6509,
            cache_creation_tokens: 0,
            cache_read_tokens: 236544,
            total_cents: None,
        }];

        let msgs = usage_events_to_messages(&events, &[]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].input_tokens, 22770);
        assert_eq!(msgs[0].output_tokens, 6509);
        assert_eq!(msgs[0].cache_read_tokens, 236544);
        // cost_cents is None so CostEnricher will estimate
        assert_eq!(msgs[0].cost_cents, None);
        assert_eq!(msgs[0].cost_confidence, "estimated");
    }

    fn usage_event_json(ts_ms: i64) -> Value {
        serde_json::json!({
            "timestamp": ts_ms.to_string(),
            "model": "composer-2-fast",
            "tokenUsage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheWriteTokens": 0,
                "cacheReadTokens": 0,
                "totalCents": 0.2
            }
        })
    }

    fn usage_event_json_numeric(ts_ms: i64) -> Value {
        serde_json::json!({
            "timestamp": ts_ms,
            "model": "composer-2-fast",
            "tokenUsage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheWriteTokens": 0,
                "cacheReadTokens": 0,
                "totalCents": 0.2
            }
        })
    }

    #[test]
    fn parse_usage_event_accepts_numeric_timestamp() {
        let ev = usage_event_json_numeric(1_774_455_909_363);
        let parsed = parse_usage_event(&ev).expect("numeric timestamp should be accepted");
        assert_eq!(parsed.timestamp_ms, 1_774_455_909_363);
        assert_eq!(parsed.model, "composer-2-fast");
    }

    #[test]
    fn quick_sync_paginates_until_existing_watermark() {
        // 200 new events after watermark=1000, spread across two full pages.
        let page1: Vec<Value> = (1101..=1200).rev().map(usage_event_json).collect();
        let page2: Vec<Value> = (1001..=1100).rev().map(usage_event_json).collect();
        let page3: Vec<Value> = (901..=1000).rev().map(usage_event_json).collect();
        let pages = [page1, page2, page3];

        let fetched = fetch_usage_events_with_page_loader(Some(1000), false, |page| {
            Ok(pages
                .get((page.saturating_sub(1)) as usize)
                .cloned()
                .unwrap_or_default())
        })
        .unwrap();

        assert_eq!(fetched.pages_fetched, 3);
        assert_eq!(fetched.events.len(), 200);
        assert_eq!(fetched.events.first().map(|e| e.timestamp_ms), Some(1001));
        assert_eq!(fetched.events.last().map(|e| e.timestamp_ms), Some(1200));
    }

    #[test]
    fn quick_sync_handles_numeric_timestamps() {
        // Cursor has shipped timestamp as both JSON string and number.
        // Numeric timestamps must still drive watermark pagination + parsing.
        let page1: Vec<Value> = (1101..=1200).rev().map(usage_event_json_numeric).collect();
        let page2: Vec<Value> = (1001..=1100).rev().map(usage_event_json_numeric).collect();
        let page3: Vec<Value> = (901..=1000).rev().map(usage_event_json_numeric).collect();
        let pages = [page1, page2, page3];

        let fetched = fetch_usage_events_with_page_loader(Some(1000), false, |page| {
            Ok(pages
                .get((page.saturating_sub(1)) as usize)
                .cloned()
                .unwrap_or_default())
        })
        .unwrap();

        assert_eq!(fetched.pages_fetched, 3);
        assert_eq!(fetched.events.len(), 200);
        assert_eq!(fetched.events.first().map(|e| e.timestamp_ms), Some(1001));
        assert_eq!(fetched.events.last().map(|e| e.timestamp_ms), Some(1200));
    }

    #[test]
    fn quick_sync_without_watermark_stays_on_page_one() {
        let page1: Vec<Value> = (1101..=1200).rev().map(usage_event_json).collect();
        let page2: Vec<Value> = (1001..=1100).rev().map(usage_event_json).collect();
        let pages = [page1, page2];

        let fetched = fetch_usage_events_with_page_loader(None, false, |page| {
            Ok(pages
                .get((page.saturating_sub(1)) as usize)
                .cloned()
                .unwrap_or_default())
        })
        .unwrap();

        assert_eq!(fetched.pages_fetched, 1);
        assert_eq!(fetched.events.len(), 100);
        assert_eq!(fetched.events.first().map(|e| e.timestamp_ms), Some(1101));
        assert_eq!(fetched.events.last().map(|e| e.timestamp_ms), Some(1200));
    }

    #[test]
    fn cursor_user_state_roots_include_windows_variants_without_duplicates() {
        let home = Path::new("/tmp/home");
        let appdata = home.join("AppData/Roaming");
        let roots = cursor_user_state_roots(home, Some(appdata.as_path()));

        assert!(roots.contains(&home.join("Library/Application Support/Cursor/User")));
        assert!(roots.contains(&home.join(".config/Cursor/User")));
        assert!(roots.contains(&home.join("AppData/Roaming/Cursor/User")));
        assert_eq!(
            roots
                .iter()
                .filter(|p| *p == &home.join("AppData/Roaming/Cursor/User"))
                .count(),
            1
        );
    }

    #[test]
    fn watch_roots_returns_projects_dir_when_present() {
        let tmp = std::env::temp_dir().join("budi-cursor-watch-roots-present");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".cursor/projects")).unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert_eq!(roots, vec![tmp.join(".cursor/projects")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_empty_when_projects_dir_absent() {
        let tmp = std::env::temp_dir().join("budi-cursor-watch-roots-absent");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert!(roots.is_empty(), "expected empty roots, got {roots:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn watch_roots_excludes_state_vscdb_and_usage_api() {
        // ADR-0089 §7: Usage API stays in sync_direct; state.vscdb is not a
        // watch root. Even when both exist, the only watch root is the JSONL
        // projects dir.
        let tmp = std::env::temp_dir().join("budi-cursor-watch-roots-jsonl-only");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".cursor/projects")).unwrap();
        std::fs::create_dir_all(tmp.join("Library/Application Support/Cursor/User/globalStorage"))
            .unwrap();

        let roots = watch_roots_for_home(&tmp);
        assert_eq!(roots, vec![tmp.join(".cursor/projects")]);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
