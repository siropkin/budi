//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Primary data source: Cursor Usage API (`/api/dashboard/get-filtered-usage-events`)
//! — returns exact per-request tokens and cost. Auth token extracted from state.vscdb.
//!
//! Legacy fallback: composerData from state.vscdb (will be removed).
//! Secondary fallback: JSONL agent transcripts under `~/.cursor/projects/*/agent-transcripts/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::Value;

use crate::analytics;
use crate::jsonl::ParsedMessage;
use crate::provider::{DiscoveredFile, ModelPricing, Provider};

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
}

// ---------------------------------------------------------------------------
// state.vscdb paths (cross-platform) — globalStorage + workspaceStorage
// ---------------------------------------------------------------------------

/// Returns all state.vscdb paths found on the system: globalStorage and
/// every workspace under workspaceStorage, for both macOS and Linux.
fn all_state_vscdb_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = match crate::config::home_dir() {
        Ok(h) => h,
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

/// Result of auth extraction: credentials (if valid) plus any warnings.
struct CursorAuthResult {
    auth: Option<CursorAuth>,
    warnings: Vec<String>,
}

/// Extract auth credentials from Cursor's state.vscdb ItemTable.
fn extract_cursor_auth() -> CursorAuthResult {
    let mut warnings = Vec::new();

    let paths = all_state_vscdb_paths();
    // Only global state.vscdb has ItemTable with auth
    let Some(global_path) = paths
        .into_iter()
        .find(|p| p.to_string_lossy().contains("globalStorage"))
    else {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    };

    let Ok(vscdb) = Connection::open_with_flags(
        &global_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    };

    let Ok(jwt) = vscdb.query_row(
        "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
        [],
        |row| row.get::<_, String>(0),
    ) else {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    };

    if jwt.is_empty() {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    }

    // Decode JWT payload to extract user_id from `sub` field.
    // JWT is header.payload.signature — we need the payload (base64url).
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    }

    let Some(decoded) = base64url_decode(parts[1]) else {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&decoded) else {
        return CursorAuthResult {
            auth: None,
            warnings,
        };
    };

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
            let msg = "Cursor auth token expired. Re-authenticate in Cursor to restore exact cost tracking.";
            tracing::warn!("{}", msg);
            warnings.push(msg.to_string());
            return CursorAuthResult {
                auth: None,
                warnings,
            };
        }
    }

    let sub = match payload.get("sub").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return CursorAuthResult {
                auth: None,
                warnings,
            };
        }
    };
    let user_id = sub.split('|').next_back().unwrap_or(sub).to_string();

    CursorAuthResult {
        auth: Some(CursorAuth { user_id, jwt }),
        warnings,
    }
}

/// Parse a single usage event JSON value into a CursorUsageEvent.
/// Returns None if the event should be skipped.
fn parse_usage_event(ev: &Value) -> Option<CursorUsageEvent> {
    let ts_str = ev.get("timestamp").and_then(|v| v.as_str()).unwrap_or("0");
    let ts: i64 = ts_str.parse().unwrap_or(0);

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

    let total_tokens = input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens;
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

/// Fetch usage events from Cursor's API with pagination.
/// `since_ms`: only return events newer than this timestamp.
/// `paginate_all`: when true, fetches all pages; when false, fetches only page 1.
fn fetch_usage_events(
    auth: &CursorAuth,
    since_ms: Option<i64>,
    paginate_all: bool,
) -> Result<Vec<CursorUsageEvent>> {
    let cookie = format!(
        "WorkosCursorSessionToken={}%3A%3A{}",
        auth.user_id, auth.jwt
    );

    let since = since_ms.unwrap_or(0);
    let mut all_events: Vec<CursorUsageEvent> = Vec::new();
    let agent = ureq::agent();

    // API returns 100 events per page, newest first. Page 1 is default (no param needed).
    let max_pages: u32 = if paginate_all { 200 } else { 1 };

    for page in 1..=max_pages {
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

        let events_arr = body
            .get("usageEventsDisplay")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if events_arr.is_empty() {
            break;
        }

        // Track whether all events on this page were older than watermark.
        let mut all_below_watermark = true;

        for ev in &events_arr {
            if let Some(parsed) = parse_usage_event(ev)
                && parsed.timestamp_ms > since
            {
                all_below_watermark = false;
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

    // Sort by timestamp ascending
    all_events.sort_by_key(|e| e.timestamp_ms);

    Ok(all_events)
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

/// Load session contexts from the sessions table.
fn load_session_contexts(conn: &Connection) -> Vec<SessionContext> {
    // Only load sessions from the last 30 days to avoid stale attribution.
    // Without this filter, API events could match sessions from months ago.
    let mut stmt = match conn.prepare(
        "SELECT session_id, started_at, ended_at, workspace_root, repo_id, git_branch
         FROM sessions WHERE provider = 'cursor'
           AND started_at >= datetime('now', '-30 days')
         ORDER BY started_at ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([], |row| {
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
            session_id: cid,
            workspace_root: row.get(3)?,
            repo_id: row.get(4)?,
            git_branch: row.get(5)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Convert API usage events into ParsedMessages, correlating with hook sessions.
fn usage_events_to_messages(
    events: &[CursorUsageEvent],
    sessions: &[SessionContext],
) -> Vec<ParsedMessage> {
    events
        .iter()
        .map(|ev| {
            // Find matching session by timestamp — prefer strict containment,
            // fall back to clock-skew window with closest-timestamp tiebreak.
            const CLOCK_SKEW_MS: i64 = 5000;
            let strict_match = sessions
                .iter()
                .filter(|s| ev.timestamp_ms >= s.start_ms && ev.timestamp_ms <= s.end_ms)
                .min_by_key(|s| (ev.timestamp_ms - s.start_ms).abs());
            let matched = strict_match.or_else(|| {
                let fallback = sessions
                    .iter()
                    .filter(|s| {
                        ev.timestamp_ms >= (s.start_ms - CLOCK_SKEW_MS)
                            && ev.timestamp_ms <= (s.end_ms + CLOCK_SKEW_MS)
                    })
                    .min_by_key(|s| {
                        let d_start = (ev.timestamp_ms - s.start_ms).abs();
                        let d_end = (ev.timestamp_ms - s.end_ms).abs();
                        d_start.min(d_end)
                    });
                if let Some(s) = &fallback {
                    tracing::warn!(
                        "Cursor session correlation: clock-skew fallback used for event at ts={}, matched session '{}'",
                        ev.timestamp_ms,
                        s.session_id
                    );
                }
                fallback
            });

            let session_id = matched.map(|s| s.session_id.clone());

            let timestamp =
                DateTime::from_timestamp_millis(ev.timestamp_ms).unwrap_or_else(Utc::now);

            // Deterministic UUID from timestamp + model + all token counts.
            // Uses all 4 token fields to avoid collisions when two requests share
            // the same millisecond and model (sequential counter was unstable when
            // previously-skipped events changed the ordering).
            let uuid = format!(
                "cursor-api-{}-{}-{}-{}-{}-{}",
                ev.timestamp_ms,
                ev.model,
                ev.input_tokens,
                ev.output_tokens,
                ev.cache_creation_tokens,
                ev.cache_read_tokens
            );

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
                request_id: None,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
            }
        })
        .collect()
}

/// Sync from Cursor's Usage API (exact per-request tokens and cost).
/// `max_age_days`: Some(N) for quick sync (page 1 only), None for full history (all pages).
fn sync_from_usage_api(
    conn: &mut Connection,
    pipeline: &mut crate::pipeline::Pipeline,
    max_age_days: Option<u64>,
) -> Option<Result<(usize, usize, Vec<String>)>> {
    let auth_result = extract_cursor_auth();
    let warnings = auth_result.warnings;

    let auth = match auth_result.auth {
        Some(a) => a,
        None => {
            // No valid auth — return warnings (e.g. JWT expired) but no error.
            // Fall back to file-based sync by returning None when there are no warnings,
            // or return success with 0 counts + warnings if we have something to report.
            if warnings.is_empty() {
                return None;
            }
            return Some(Ok((0, 0, warnings)));
        }
    };

    let watermark_key = "cursor-api-usage";
    let watermark = analytics::get_sync_offset(conn, watermark_key)
        .ok()
        .and_then(|v| {
            let ts = v as i64;
            if ts > 0 { Some(ts) } else { None }
        });

    // Quick sync (max_age_days=Some): fetch page 1 only (100 most recent events).
    // Full history (max_age_days=None): paginate all pages back to watermark.
    let paginate_all = max_age_days.is_none();

    let events = match fetch_usage_events(&auth, watermark, paginate_all) {
        Ok(e) => e,
        Err(e) => return Some(Err(e)),
    };

    // Repair sessions whose started_at was set by a late-arriving hook instead
    // of the earliest hook. Use MIN(hook_events.timestamp) as the true start.
    let _ = conn.execute(
        "UPDATE sessions SET started_at = (
            SELECT MIN(h.timestamp) FROM hook_events h WHERE h.session_id = sessions.session_id
         )
         WHERE provider = 'cursor'
           AND EXISTS (
             SELECT 1 FROM hook_events h
             WHERE h.session_id = sessions.session_id AND h.timestamp < sessions.started_at
           )",
        [],
    );

    // Always run backfill for orphaned messages (ingested before their session
    // row existed). Must run even when there are no new API events.
    let sessions = load_session_contexts(conn);
    if !sessions.is_empty() {
        let orphaned = backfill_cursor_session_ids(conn, &sessions);
        if orphaned > 0 {
            tracing::info!("Cursor session backfill: assigned session_id to {orphaned} orphaned messages");
        }
    }

    // Repair messages that have a session_id but stale metadata (repo_id=unknown,
    // missing cwd/branch) — propagate from the now-correct session row.
    let _ = conn.execute(
        "UPDATE messages SET
            cwd = COALESCE(cwd, (SELECT workspace_root FROM sessions WHERE session_id = messages.session_id)),
            repo_id = (SELECT COALESCE(repo_id, 'unknown') FROM sessions WHERE session_id = messages.session_id),
            git_branch = COALESCE(git_branch, (SELECT git_branch FROM sessions WHERE session_id = messages.session_id))
         WHERE provider = 'cursor'
           AND session_id IS NOT NULL
           AND (repo_id IS NULL OR repo_id = 'unknown')
           AND EXISTS (
             SELECT 1 FROM sessions s
             WHERE s.session_id = messages.session_id AND s.repo_id IS NOT NULL AND s.repo_id != 'unknown'
           )",
        [],
    );

    if events.is_empty() {
        return Some(Ok((0, 0, warnings)));
    }

    let mut messages = usage_events_to_messages(&events, &sessions);
    let tags = pipeline.process(&mut messages);
    let count = match analytics::ingest_messages(conn, &messages, Some(&tags)) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };

    // Update watermark to latest event timestamp.
    if let Some(newest_ts) = events.iter().map(|e| e.timestamp_ms).max() {
        let _ = analytics::set_sync_offset(conn, watermark_key, newest_ts as usize);
    }

    let api_calls = if paginate_all {
        events.len().div_ceil(100).max(1)
    } else {
        1
    };
    Some(Ok((api_calls, count, warnings)))
}

/// Retroactively assign session_id to Cursor messages that have NULL session_id.
/// Uses the same timestamp-overlap logic as `usage_events_to_messages`.
fn backfill_cursor_session_ids(conn: &Connection, sessions: &[SessionContext]) -> usize {
    let mut stmt = match conn.prepare(
        "SELECT uuid, timestamp FROM messages
         WHERE provider = 'cursor' AND session_id IS NULL AND role = 'assistant'",
    ) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let orphans: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    let mut updated = 0;
    for (uuid, ts_str) in &orphans {
        let Ok(ts) = ts_str.parse::<DateTime<Utc>>() else {
            continue;
        };
        let ts_ms = ts.timestamp_millis();

        const CLOCK_SKEW_MS: i64 = 5000;
        let matched = sessions
            .iter()
            .filter(|s| ts_ms >= s.start_ms && ts_ms <= s.end_ms)
            .min_by_key(|s| (ts_ms - s.start_ms).abs())
            .or_else(|| {
                sessions
                    .iter()
                    .filter(|s| {
                        ts_ms >= (s.start_ms - CLOCK_SKEW_MS)
                            && ts_ms <= (s.end_ms + CLOCK_SKEW_MS)
                    })
                    .min_by_key(|s| {
                        let d_start = (ts_ms - s.start_ms).abs();
                        let d_end = (ts_ms - s.end_ms).abs();
                        d_start.min(d_end)
                    })
            });

        if let Some(session) = matched {
            let _ = conn.execute(
                "UPDATE messages SET session_id = ?1,
                 cwd = COALESCE(NULLIF(cwd, ''), ?2),
                 repo_id = COALESCE(NULLIF(NULLIF(repo_id, ''), 'unknown'), ?3),
                 git_branch = COALESCE(NULLIF(git_branch, ''), ?4)
                 WHERE uuid = ?5",
                params![
                    session.session_id,
                    session.workspace_root,
                    session.repo_id,
                    session.git_branch,
                    uuid,
                ],
            );
            updated += 1;
        }
    }
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

    let request_id = entry.request_id;
    let uuid = entry
        .uuid
        .or_else(|| request_id.clone())
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
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "n/a".to_string(),
            request_id: request_id.clone(),
            speed: None,
            cache_creation_1h_tokens: 0,
            web_search_requests: 0,
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
                session_title: None,
                parent_uuid: None,
                user_name: None,
                machine_name: None,
                cost_confidence: "estimated".to_string(),
                request_id,
                speed: None,
                cache_creation_1h_tokens: 0,
                web_search_requests: 0,
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
/// Prices are per MTok (million tokens), sourced from https://cursor.com/docs/models
/// Last updated: 2026-03-26
pub fn cursor_pricing_for_model(model: &str) -> ModelPricing {
    if model.is_empty() {
        tracing::warn!("Cursor model is empty, using Composer 2 default pricing");
        return ModelPricing {
            input: 0.50,
            output: 2.50,
            cache_write: 0.50,
            cache_read: 0.20,
        };
    }
    let m = model.to_lowercase();

    // --- Cursor native models ---
    // Composer 2 (latest, cheapest)
    if m.contains("composer-2") || m.contains("composer_2") {
        ModelPricing {
            input: 0.50,
            output: 2.50,
            cache_write: 0.50,
            cache_read: 0.20,
        }
    // Composer 1.5
    } else if m.contains("composer-1.5") || m.contains("composer_1.5") {
        ModelPricing {
            input: 3.50,
            output: 17.50,
            cache_write: 3.50,
            cache_read: 0.35,
        }
    // Composer 1 / generic "composer" / "auto" / "default"
    } else if m == "default" || m.starts_with("composer") || m.contains("auto") {
        ModelPricing {
            input: 1.25,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.125,
        }

    // --- OpenAI GPT-5.x models ---
    } else if m.contains("gpt-5.4") && m.contains("nano") {
        ModelPricing {
            input: 0.20,
            output: 1.25,
            cache_write: 0.20,
            cache_read: 0.02,
        }
    } else if m.contains("gpt-5.4") && m.contains("mini") {
        ModelPricing {
            input: 0.75,
            output: 4.50,
            cache_write: 0.75,
            cache_read: 0.075,
        }
    } else if m.contains("gpt-5.4") {
        ModelPricing {
            input: 2.50,
            output: 15.0,
            cache_write: 2.50,
            cache_read: 0.25,
        }
    } else if m.contains("gpt-5.2") || m.contains("gpt-5.3") {
        ModelPricing {
            input: 1.75,
            output: 14.0,
            cache_write: 1.75,
            cache_read: 0.175,
        }
    } else if m.contains("gpt-5") && m.contains("mini") {
        ModelPricing {
            input: 0.25,
            output: 2.0,
            cache_write: 0.25,
            cache_read: 0.025,
        }
    } else if m.contains("gpt-5") && m.contains("fast") {
        ModelPricing {
            input: 2.50,
            output: 20.0,
            cache_write: 2.50,
            cache_read: 0.25,
        }
    } else if m.contains("gpt-5") {
        ModelPricing {
            input: 1.25,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.125,
        }

    // --- OpenAI GPT-4.x models ---
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

    // --- OpenAI reasoning models ---
    } else if m.contains("o3-mini") || m.contains("o1-mini") {
        ModelPricing {
            input: 1.10,
            output: 4.40,
            cache_write: 1.10,
            cache_read: 0.55,
        }
    } else if m.contains("o3") {
        ModelPricing {
            input: 2.0,
            output: 8.0,
            cache_write: 2.0,
            cache_read: 0.20,
        }
    } else if m.contains("o1") {
        ModelPricing {
            input: 15.0,
            output: 60.0,
            cache_write: 15.0,
            cache_read: 7.50,
        }

    // --- Anthropic models ---
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

    // --- Google models ---
    } else if m.contains("gemini") && m.contains("flash") {
        // Gemini 3 Flash / 2.5 Flash — use 3 Flash pricing
        ModelPricing {
            input: 0.50,
            output: 3.0,
            cache_write: 0.50,
            cache_read: 0.05,
        }
    } else if m.contains("gemini") {
        // Gemini Pro (3 Pro, 3.1 Pro)
        ModelPricing {
            input: 2.0,
            output: 12.0,
            cache_write: 2.0,
            cache_read: 0.20,
        }

    // --- xAI ---
    } else if m.contains("grok") {
        ModelPricing {
            input: 2.0,
            output: 6.0,
            cache_write: 2.0,
            cache_read: 0.20,
        }

    // --- Moonshot ---
    } else if m.contains("kimi") {
        ModelPricing {
            input: 0.60,
            output: 3.0,
            cache_write: 0.60,
            cache_read: 0.10,
        }

    // --- DeepSeek ---
    } else if m.contains("deepseek") {
        ModelPricing {
            input: 0.27,
            output: 1.10,
            cache_write: 0.27,
            cache_read: 0.07,
        }
    } else {
        // Unknown model — use Composer 2 pricing as default (most common Cursor model)
        tracing::warn!(
            "Unknown Cursor model '{}', using Composer 2 default pricing",
            model
        );
        ModelPricing {
            input: 0.50,
            output: 2.50,
            cache_write: 0.50,
            cache_read: 0.20,
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
    fn cursor_pricing_composer_2() {
        let p = cursor_pricing_for_model("composer-2");
        assert_eq!(p.input, 0.50);
        assert_eq!(p.output, 2.50);
        assert_eq!(p.cache_read, 0.20);
        // "composer-2-fast" also matches composer-2
        let p2 = cursor_pricing_for_model("composer-2-fast");
        assert_eq!(p2.input, 0.50);
    }

    #[test]
    fn cursor_pricing_gpt5() {
        let p = cursor_pricing_for_model("gpt-5");
        assert_eq!(p.input, 1.25);
        assert_eq!(p.output, 10.0);
        // GPT-5.4
        let p2 = cursor_pricing_for_model("gpt-5.4");
        assert_eq!(p2.input, 2.50);
        assert_eq!(p2.output, 15.0);
    }

    #[test]
    fn cursor_pricing_gpt4o() {
        let p = cursor_pricing_for_model("gpt-4o");
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 10.0);
    }

    #[test]
    fn cursor_pricing_o3() {
        let p = cursor_pricing_for_model("o3");
        assert_eq!(p.input, 2.0);
        assert_eq!(p.output, 8.0);
    }

    #[test]
    fn cursor_pricing_sonnet() {
        let p = cursor_pricing_for_model("claude-sonnet-4-6");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn cursor_pricing_unknown_defaults_to_composer2() {
        let p = cursor_pricing_for_model("some-new-model");
        assert_eq!(p.input, 0.50);
        assert_eq!(p.output, 2.50);
    }

    #[test]
    fn cursor_pricing_deepseek() {
        let p = cursor_pricing_for_model("deepseek-v3");
        assert_eq!(p.input, 0.27);
        assert_eq!(p.output, 1.10);
    }

    #[test]
    fn cursor_pricing_gemini_flash() {
        let p = cursor_pricing_for_model("gemini-3-flash");
        assert_eq!(p.input, 0.50);
        assert_eq!(p.output, 3.0);
    }

    #[test]
    fn cursor_pricing_grok() {
        let p = cursor_pricing_for_model("grok-4.20");
        assert_eq!(p.input, 2.0);
        assert_eq!(p.output, 6.0);
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
        assert_eq!(msgs[0].uuid, "cursor-api-1774455909363-gpt-4o-100-50-0-0");
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
}
