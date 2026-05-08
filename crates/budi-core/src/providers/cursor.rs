//! Cursor provider — implements the Provider trait for Cursor AI editor.
//!
//! Primary data source: Cursor Usage API (`/api/dashboard/get-filtered-usage-events`)
//! — returns exact per-request tokens and cost. Auth token extracted from state.vscdb.
//!
//! Legacy fallback: composerData from state.vscdb (will be removed).
//! Secondary fallback: JSONL agent transcripts under `~/.cursor/projects/*/agent-transcripts/`.
//!
//! Contract: [ADR-0090 — Cursor Usage API Contract](../../../../docs/adr/0090-cursor-usage-api-contract.md).
//! Any breaking change to the undocumented upstream must land as a paired
//! edit to ADR-0090 and this module so the two never disagree. Lag
//! characterization is pinned in ADR-0089 §7 (verdict comment on #321).

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

/// Model id we substitute for Cursor "Auto" mode bubble rows whose
/// `modelInfo.modelName` is empty or the literal `"default"`.
///
/// Cursor's public stance is that Auto mode prices at Sonnet rates
/// (see [#553] scope-update and the CodeBurn reference implementation's
/// `CURSOR_DEFAULT_MODEL`). Without this substitution the bubble would
/// look up as an unknown model and land with `pricing_source = "unknown"`
/// plus $0 cost, which defeats the whole point of the bubbles path.
/// The constant is kept single-sourced so a future "Auto ≠ Sonnet" pivot
/// lands as a one-line change with a visible blame trail.
///
/// [#553]: https://github.com/siropkin/budi/issues/553
const CURSOR_AUTO_MODEL_FALLBACK: &str = "claude-sonnet-4-5";

/// `sync_state` watermark key for the `cursorDiskKV` bubbles path.
///
/// Intentionally distinct from [`CURSOR_USAGE_API_WATERMARK_KEY`] so the
/// two Cursor data paths advance independently: a bubble ingest tick does
/// not acknowledge Usage API events, and vice versa. Dedup between the
/// two paths is handled at the row-id level (bubble UUIDs vs usage-event
/// UUIDs collide deterministically only when they describe the same
/// activity; the first-seen row wins per `ingest_messages`).
const CURSOR_BUBBLES_WATERMARK_KEY: &str = "cursor-bubbles";

/// `sync_state` watermark key for the Cursor Usage API path.
const CURSOR_USAGE_API_WATERMARK_KEY: &str = "cursor-api-usage";

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
        // #553: prefer the local `cursorDiskKV` bubble rows — they carry
        // real per-message tokens and model without any network call,
        // and the whole subscription consumption (not just overage)
        // shows up there. The Usage API path still runs afterwards as a
        // supplementary signal for overage attribution during the
        // validation window documented in ADR-0090.
        let bubbles = sync_from_bubbles(conn, pipeline);
        let api = sync_from_usage_api(conn, pipeline, max_age_days);
        combine_cursor_sync_results(bubbles, api)
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
///
/// #504 (RC-4 Part A): every early-return emits a structured
/// `cursor_auth` warn with a typed reason tag the first time it's
/// observed, so operators can find out why `/analytics/providers`
/// shows zero cost for Cursor without having to instrument the
/// daemon. The `budi_core::providers::cursor::auth_probe` warn-once
/// dedup (below) keeps the daemon log to one line per reason per
/// process rather than flooding it every sync tick.
///
/// Does NOT log the JWT contents — only the reason code / category.
/// Does NOT distinguish expired-by-N-minutes from expired-by-N-days
/// beyond "expired vs not".
fn extract_cursor_auth() -> Option<CursorAuth> {
    let paths = all_state_vscdb_paths();
    let global_path = match paths
        .into_iter()
        .find(|p| p.to_string_lossy().contains("globalStorage"))
    {
        Some(p) => p,
        None => {
            warn_auth_once(CursorAuthIssue::NoStateVscdb);
            return None;
        }
    };

    let vscdb = match Connection::open_with_flags(
        &global_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => {
            warn_auth_once(CursorAuthIssue::StateVscdbOpenFailed);
            return None;
        }
    };

    let jwt: String = match vscdb.query_row(
        "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => {
            // Covers both "row absent" and other query errors. Most
            // commonly fires when the user is signed out of Cursor.
            warn_auth_once(CursorAuthIssue::TokenRowMissing);
            return None;
        }
    };

    if jwt.is_empty() {
        warn_auth_once(CursorAuthIssue::TokenEmpty);
        return None;
    }

    // Decode JWT payload to extract user_id from `sub` field.
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() < 2 {
        warn_auth_once(CursorAuthIssue::TokenMalformed);
        return None;
    }

    let decoded = match base64url_decode(parts[1]) {
        Some(b) => b,
        None => {
            warn_auth_once(CursorAuthIssue::TokenMalformed);
            return None;
        }
    };
    let payload: Value = match serde_json::from_slice(&decoded) {
        Ok(p) => p,
        Err(_) => {
            warn_auth_once(CursorAuthIssue::TokenMalformed);
            return None;
        }
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
            warn_auth_once(CursorAuthIssue::TokenExpired);
            return None;
        }
    }

    let sub = match payload.get("sub").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            warn_auth_once(CursorAuthIssue::TokenMissingSubject);
            return None;
        }
    };
    let user_id = sub.split('|').next_back().unwrap_or(sub).to_string();

    Some(CursorAuth { user_id, jwt })
}

/// #504 (RC-4): reasons `extract_cursor_auth` can return `None`.
/// Each variant surfaces as a stable structured-log reason tag so
/// operators grepping `daemon.log` for `cursor_auth` see exactly one
/// of these strings and can map it to a fix without reading source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CursorAuthIssue {
    /// `state.vscdb` under a `globalStorage` root does not exist —
    /// Cursor isn't installed, or the install is on a different user
    /// account, or it's a machine where Cursor data lives outside the
    /// paths `all_state_vscdb_paths()` probes.
    NoStateVscdb,
    /// The path exists but SQLite can't open it read-only. Most commonly
    /// a permissions issue; rare in practice.
    StateVscdbOpenFailed,
    /// `ItemTable` has no row keyed `cursorAuth/accessToken`. Fires
    /// whenever the user is signed out of Cursor, or Cursor's auth-key
    /// schema changed upstream.
    TokenRowMissing,
    /// The row exists but the value is the empty string. Fires right
    /// after sign-out on some Cursor versions before the row is deleted.
    TokenEmpty,
    /// JWT doesn't parse as three dot-separated base64-url parts, or
    /// the payload isn't valid JSON. Fires on upstream auth-format
    /// changes we haven't tracked yet.
    TokenMalformed,
    /// `exp` claim is in the past. User needs to restart Cursor to
    /// refresh the token.
    TokenExpired,
    /// Payload parses but has no `sub` claim — can't anchor the Usage
    /// API to a specific user account. Never seen in practice; the
    /// variant exists so the warn is self-describing if it ever fires.
    TokenMissingSubject,
}

impl CursorAuthIssue {
    fn reason_tag(self) -> &'static str {
        match self {
            Self::NoStateVscdb => "no_state_vscdb",
            Self::StateVscdbOpenFailed => "state_vscdb_open_failed",
            Self::TokenRowMissing => "token_row_missing",
            Self::TokenEmpty => "token_empty",
            Self::TokenMalformed => "token_malformed",
            Self::TokenExpired => "token_expired",
            Self::TokenMissingSubject => "token_missing_subject",
        }
    }

    fn human_message(self) -> &'static str {
        match self {
            Self::NoStateVscdb => {
                "Cursor state.vscdb not found — Cursor not installed, or install is \
                 under a different user account. `budi stats` shows Cursor cost only \
                 when the Cursor Usage API path has run at least once."
            }
            Self::StateVscdbOpenFailed => {
                "Cursor state.vscdb exists but could not be opened read-only \
                 (permissions?). Usage API path skipped; falling back to local \
                 transcript sync (which has no per-message cost metadata)."
            }
            Self::TokenRowMissing => {
                "Cursor state.vscdb has no cursorAuth/accessToken row — signed out \
                 of Cursor, or auth-key schema changed upstream. Usage API path \
                 skipped until Cursor is signed back in."
            }
            Self::TokenEmpty => {
                "Cursor auth token is empty (just signed out?). Usage API path \
                 skipped until Cursor is signed back in."
            }
            Self::TokenMalformed => {
                "Cursor auth token could not be decoded as a JWT. Upstream format \
                 may have changed; Usage API path skipped."
            }
            Self::TokenExpired => {
                "Cursor auth token expired — restart Cursor to refresh it. \
                 Usage API path skipped; falling back to local transcript sync."
            }
            Self::TokenMissingSubject => {
                "Cursor auth token has no `sub` claim — cannot anchor Usage API \
                 to a user. Upstream format may have changed."
            }
        }
    }
}

fn auth_warn_cache() -> &'static std::sync::Mutex<std::collections::HashSet<CursorAuthIssue>> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashSet<CursorAuthIssue>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Emit a structured `cursor_auth` warn on the first hit for this
/// reason per daemon process. Subsequent hits are dedup'd silently so
/// a 24h worker loop that fires every hour doesn't spam the log.
fn warn_auth_once(issue: CursorAuthIssue) {
    let mut guard = match auth_warn_cache().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !guard.insert(issue) {
        return;
    }
    tracing::warn!(
        target: "budi_core::providers::cursor",
        event = "cursor_auth_skipped",
        reason = issue.reason_tag(),
        "Cursor Usage API not running: {}",
        issue.human_message(),
    );
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
            .and_then(crate::repo_id::resolve_repo_id);
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
                cwd_source: None,
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

    let watermark_key = CURSOR_USAGE_API_WATERMARK_KEY;
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

// ---------------------------------------------------------------------------
// `cursorDiskKV` bubbles — local per-message tokens/model (#553)
// ---------------------------------------------------------------------------

/// One decoded `bubbleId:*` row from `cursorDiskKV`.
///
/// Field ordering mirrors the SQL column order in [`read_cursor_bubbles`]
/// so the `query_map` closure stays obviously in sync with the SELECT.
///
/// Schema lessons from the v8.3.7 live-smoke (#553 follow-up):
/// - `$.conversationId` is never written by Cursor — the conversation id
///   is embedded in the **row key** (`bubbleId:<conv-uuid>:<bubble-uuid>`),
///   so we parse it out of `substr(key, 10, 36)` rather than
///   `json_extract`.
/// - `$.createdAt` is optional. On a live maintainer DB, 131 of 1,565
///   token-bearing bubbles had no `createdAt`, and every `type=1` user
///   bubble was missing it too. Those rows still need to ship tokens +
///   model, so the reader falls back to the composer header's
///   `createdAt` / `lastUpdatedAt` for a conversation-level timestamp,
///   and dedup ids include the unique per-row `bubble_id` from the key.
#[derive(Debug)]
struct BubbleRow {
    input_tokens: u64,
    output_tokens: u64,
    model: Option<String>,
    /// Raw `$.createdAt` JSON value, rendered as TEXT by the SQL CAST.
    /// Cursor has shipped this as either an ISO-8601 string, epoch ms,
    /// or absent entirely; [`parse_bubble_created_at`] + the composer
    /// fallback handle all three shapes.
    created_at: Option<String>,
    /// Conversation id parsed from the row key, not the JSON value.
    /// Every key observed in the wild is exactly 82 chars shaped
    /// `bubbleId:<36-char conv-uuid>:<36-char bubble-uuid>`; shorter keys
    /// degrade to `None` and the row is dropped.
    conversation_id: Option<String>,
    /// Bubble id parsed from the same key. Used in the dedup uuid so
    /// every distinct bubble gets a unique row even when `created_at`
    /// is missing and tokens collide within a conversation.
    bubble_id: Option<String>,
    /// Cursor's internal type code. `1` = user message, other values =
    /// assistant. Matches the CodeBurn reference implementation's
    /// convention, cross-verified against live rows.
    type_code: Option<i64>,
}

/// Parse Cursor's `$.createdAt` field in either of the two shapes we have
/// observed: an ISO-8601 string, or an epoch-millis integer (rendered as
/// a decimal string by the SQL CAST).
///
/// Returns `None` when the input can't be parsed as either shape —
/// callers skip rows whose timestamp is unreadable rather than pinning
/// them to "now".
fn parse_bubble_created_at(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ms) = s.parse::<i64>() {
        // Guard against degenerate zero/negative values; valid epoch-ms
        // timestamps for the Cursor-era are well past 10^12.
        if ms > 0 {
            return Some(ms);
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        return Some(dt.timestamp_millis());
    }
    None
}

/// Emit a one-time `cursor_bubble_schema_unrecognized` warn, deduplicated
/// for the life of the process. Mirrors [`warn_auth_once`] in spirit but
/// keyed on "we've warned at least once" rather than on a reason enum —
/// there is exactly one schema-missing signal to surface here.
fn warn_bubble_schema_once() {
    use std::sync::{Mutex, OnceLock};
    static FIRED: OnceLock<Mutex<bool>> = OnceLock::new();
    let mutex = FIRED.get_or_init(|| Mutex::new(false));
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *guard {
        return;
    }
    *guard = true;
    tracing::warn!(
        target: "budi_core::providers::cursor",
        event = "cursor_bubble_schema_unrecognized",
        "Cursor state.vscdb cursorDiskKV bubble rows not found — schema may have changed \
         or the DB is empty. Falling back to the Usage API path for Cursor pricing.",
    );
}

/// Fallback timestamps for bubble rows whose `$.createdAt` is missing,
/// keyed on `conversation_id`. Populated once per call from
/// `composer.composerHeaders` in `ItemTable`.
type ComposerTsMap = std::collections::HashMap<String, i64>;

/// Read `composer.composerHeaders` from the same `state.vscdb` and build
/// a `conversation_id -> fallback_timestamp_ms` map. The fallback prefers
/// `lastUpdatedAt` (latest conversation activity) because bubbles without
/// an explicit `createdAt` in the JSON are the newest ones Cursor writes;
/// if that's missing too, fall back to `createdAt`.
///
/// Errors are treated as "no fallback available" — the caller will drop
/// bubbles without an explicit timestamp rather than pretending they
/// landed at epoch zero.
fn load_bubble_timestamp_fallbacks(vscdb: &Connection) -> ComposerTsMap {
    let raw: String = match vscdb.query_row(
        "SELECT value FROM ItemTable WHERE key = 'composer.composerHeaders'",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return ComposerTsMap::new(),
    };

    let payload: ComposerHeadersPayload = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(_) => return ComposerTsMap::new(),
    };

    let mut out = ComposerTsMap::new();
    for composer in payload.all_composers {
        if composer.composer_id.trim().is_empty() {
            continue;
        }
        let ts = composer
            .last_updated_at
            .filter(|v| *v > 0)
            .unwrap_or(composer.created_at);
        if ts > 0 {
            out.insert(composer.composer_id, ts);
        }
    }
    out
}

/// Read Cursor per-message usage rows from the `cursorDiskKV` table in
/// `state.vscdb`.
///
/// Cursor stores per-bubble JSON under keys shaped
/// `bubbleId:<conv-uuid>:<bubble-uuid>` (every observed key is exactly
/// 82 chars). The JSON value carries `tokenCount.inputTokens`,
/// `tokenCount.outputTokens`, `modelInfo.modelName`, and `type`.
/// `createdAt` is optional and `conversationId` is never present — the
/// schema-fix follow-up to #553 discovered both on the v8.3.7
/// live-smoke, where the CodeBurn-derived assumption pointed at the
/// wrong JSON paths.
///
/// Reading these rows directly gives exact per-message tokens and model
/// without any network call, which is the whole point of #553 so
/// Cursor's subscription-included traffic stops reading as $0.
///
/// - `db_path` points at the `globalStorage/state.vscdb` we already
///   probed in `all_state_vscdb_paths`.
/// - `since_ms` is an optional watermark in epoch-millis; rows whose
///   effective timestamp parses to `<= since_ms` are skipped.
///
/// When the `cursorDiskKV` table is missing (schema drift, empty DB, or
/// we're pointed at a non-Cursor sqlite), returns `Ok(vec![])` after
/// emitting a one-time schema-unrecognized warn. The Usage API path
/// still runs in the same sync tick so the provider degrades gracefully.
///
/// See [ADR-0090 §2026-04-23](../../../../docs/adr/0090-cursor-usage-api-contract.md)
/// for the dual-path policy during the #553 validation window.
pub(crate) fn read_cursor_bubbles(
    db_path: &Path,
    since_ms: Option<i64>,
) -> Result<Vec<ParsedMessage>> {
    let vscdb = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", db_path.display()))?;

    let has_table: i64 = vscdb
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'cursorDiskKV'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if has_table == 0 {
        warn_bubble_schema_once();
        return Ok(Vec::new());
    }

    let fallbacks = load_bubble_timestamp_fallbacks(&vscdb);

    // Key layout: `bubbleId:<conv-uuid>:<bubble-uuid>`. SQLite `substr`
    // is 1-indexed, so `substr(key, 10, 36)` extracts the 36-char
    // conversation uuid and `substr(key, 47, 36)` extracts the 36-char
    // bubble uuid. `length(key) = 82` guards against malformed keys.
    //
    // CAST `$.createdAt` AS TEXT so numeric timestamps (epoch ms) and
    // string timestamps (ISO 8601) both deserialize into the same
    // `Option<String>` — `parse_bubble_created_at` handles the split.
    let mut stmt = match vscdb.prepare(
        "SELECT
            COALESCE(json_extract(value, '$.tokenCount.inputTokens'), 0)  AS input_tokens,
            COALESCE(json_extract(value, '$.tokenCount.outputTokens'), 0) AS output_tokens,
            json_extract(value, '$.modelInfo.modelName')                  AS model,
            CAST(json_extract(value, '$.createdAt') AS TEXT)              AS created_at,
            substr(key, 10, 36)                                           AS conversation_id,
            substr(key, 47, 36)                                           AS bubble_id,
            json_extract(value, '$.type')                                 AS type_code
         FROM cursorDiskKV
         WHERE key LIKE 'bubbleId:%'
           AND length(key) = 82
           AND (
             json_extract(value, '$.tokenCount.inputTokens') > 0
             OR json_extract(value, '$.type') = 1
           )",
    ) {
        Ok(s) => s,
        Err(e) => {
            warn_bubble_schema_once();
            tracing::debug!("cursorDiskKV prepare failed: {e:#}");
            return Ok(Vec::new());
        }
    };

    let rows = stmt.query_map([], |row| {
        let input_raw: i64 = row.get::<_, Option<i64>>(0)?.unwrap_or(0);
        let output_raw: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
        Ok(BubbleRow {
            input_tokens: input_raw.max(0) as u64,
            output_tokens: output_raw.max(0) as u64,
            model: row.get::<_, Option<String>>(2)?,
            created_at: row.get::<_, Option<String>>(3)?,
            conversation_id: row.get::<_, Option<String>>(4)?,
            bubble_id: row.get::<_, Option<String>>(5)?,
            type_code: row.get::<_, Option<i64>>(6)?,
        })
    })?;

    let mut parsed: Vec<ParsedMessage> = Vec::new();
    for row_r in rows {
        let row = match row_r {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("cursorDiskKV row decode failed: {e:#}");
                continue;
            }
        };
        if let Some(msg) = bubble_to_parsed_message(row, since_ms, &fallbacks) {
            parsed.push(msg);
        }
    }

    Ok(parsed)
}

/// Translate one decoded `BubbleRow` into a `ParsedMessage` ready for
/// the pipeline. Returns `None` when the row is unreadable (malformed
/// key, or filtered by `since_ms` after resolving a timestamp).
fn bubble_to_parsed_message(
    row: BubbleRow,
    since_ms: Option<i64>,
    fallbacks: &ComposerTsMap,
) -> Option<ParsedMessage> {
    let conversation_id = row.conversation_id.as_deref().unwrap_or("").trim();
    if conversation_id.is_empty() {
        return None;
    }
    let bubble_id = row.bubble_id.as_deref().unwrap_or("").trim();
    if bubble_id.is_empty() {
        return None;
    }

    // Prefer the bubble's own `createdAt`; fall back to the composer
    // header's `lastUpdatedAt` / `createdAt` (ms) when absent. The
    // fallback buckets bubbles by conversation-level activity — a few
    // ms off the true per-bubble time, but enough to keep date-buckets
    // correct and the cost surface non-zero.
    let created_ms = row
        .created_at
        .as_deref()
        .and_then(parse_bubble_created_at)
        .or_else(|| fallbacks.get(conversation_id).copied());
    let created_ms = created_ms?;
    if let Some(w) = since_ms
        && created_ms <= w
    {
        return None;
    }

    let timestamp = DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now);
    let is_user = row.type_code == Some(1);

    // Dedup id: `cursor:bubble:<conv-uuid>:<bubble-uuid>`. The
    // bubble-uuid from the key is globally unique, so two distinct
    // bubbles can't collide even when they share a conversation and
    // identical token counts. Bubbles that later appear on the Usage
    // API keep the deterministic `cursor-api-usage` uuid shape and
    // live in a disjoint id namespace.
    let uuid = format!("cursor:bubble:{conversation_id}:{bubble_id}");

    let session_id = Some(crate::identity::normalize_session_id(conversation_id));

    if is_user {
        // #533 keeps user rows at zero tokens / no cost; `CostEnricher`
        // will tag them `unpriced:no_tokens` on its pass. Flowing them
        // through the pipeline keeps prompt-category classification
        // and tool-outcome retention consistent with other providers.
        Some(ParsedMessage {
            uuid,
            session_id,
            timestamp,
            cwd: None,
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
            cwd_source: None,
        })
    } else {
        let resolved_model = match row.model.as_deref().map(str::trim) {
            None | Some("") | Some("default") => CURSOR_AUTO_MODEL_FALLBACK.to_string(),
            Some(other) => other.to_string(),
        };
        Some(ParsedMessage {
            uuid,
            session_id,
            timestamp,
            cwd: None,
            role: "assistant".to_string(),
            model: Some(resolved_model),
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            // Cursor's bubble schema exposes tokenCount.{input,output}Tokens
            // only; cache tiers are not visible from the local DB. Leaving
            // them at zero undercounts cache-read savings slightly for
            // Cursor vs reality — documented caveat in the ADR amendment.
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
            // Empty string → `CostEnricher` sets it to "estimated" when
            // it prices the row via the manifest. If the model is
            // unknown (manifest miss), the enricher falls back to
            // "estimated_unknown_model" — both paths end up with a
            // non-empty value.
            cost_confidence: String::new(),
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
            cwd_source: None,
        })
    }
}

/// Fill missing `cwd` / `repo_id` / `git_branch` on freshly-read bubble
/// messages from session context. Bubble rows only carry
/// `conversationId`, not workspace metadata; we look each one up against
/// the session contexts we already compute for the Usage API path so
/// downstream attribution tags match.
fn attach_session_context_to_bubbles(msgs: &mut [ParsedMessage], sessions: &[SessionContext]) {
    if sessions.is_empty() {
        return;
    }
    for m in msgs {
        let ts_ms = m.timestamp.timestamp_millis();
        let direct = m
            .session_id
            .as_deref()
            .and_then(|sid| sessions.iter().find(|s| s.session_id == sid));
        let matched = direct.or_else(|| find_matching_session(ts_ms, sessions));
        let Some(s) = matched else {
            continue;
        };
        if m.cwd.is_none() {
            m.cwd = s.workspace_root.clone();
        }
        if m.repo_id.is_none() {
            m.repo_id = s.repo_id.clone();
        }
        if m.git_branch.is_none() {
            m.git_branch = s.git_branch.clone();
        }
    }
}

/// Sync Cursor per-message usage from local `cursorDiskKV` bubble rows.
///
/// Returns the same `(api_calls, message_count, warnings)` tuple shape as
/// [`sync_from_usage_api`] so it can share the `combine_cursor_sync_results`
/// merger. `api_calls` is always 0 here — the bubbles path makes no
/// network calls. `None` when no state.vscdb path is discoverable on
/// this machine, so the Usage API path alone drives the sync tick.
fn sync_from_bubbles(
    conn: &mut Connection,
    pipeline: &mut crate::pipeline::Pipeline,
) -> Option<Result<(usize, usize, Vec<String>)>> {
    let global_path = all_state_vscdb_paths()
        .into_iter()
        .find(|p| p.to_string_lossy().contains("globalStorage"))?;

    let watermark_key = CURSOR_BUBBLES_WATERMARK_KEY;
    let watermark = analytics::get_sync_offset(conn, watermark_key)
        .ok()
        .and_then(|v| {
            let ts = v as i64;
            if ts > 0 { Some(ts) } else { None }
        });

    let mut messages = match read_cursor_bubbles(&global_path, watermark) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Cursor bubbles read failed: {e:#}; Usage API path will still run");
            return None;
        }
    };

    if messages.is_empty() {
        return Some(Ok((0, 0, Vec::new())));
    }

    // Session repair/backfill is only needed when new Cursor data arrives,
    // and the bubbles path is often the first path to notice a new
    // composer. Running this before `load_session_contexts` means the
    // composer-header merge picks up repo/branch for freshly-seen sessions.
    run_cursor_repairs(conn);

    let sessions = load_session_contexts(conn);
    attach_session_context_to_bubbles(&mut messages, &sessions);

    let newest_ts_ms = messages
        .iter()
        .map(|m| m.timestamp.timestamp_millis())
        .max()
        .unwrap_or(0);

    let tags = pipeline.process(&mut messages);
    let count = match analytics::ingest_messages(conn, &messages, Some(&tags)) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };

    if newest_ts_ms > 0
        && let Err(e) = analytics::set_sync_offset(conn, watermark_key, newest_ts_ms as usize)
    {
        return Some(Err(e));
    }

    Some(Ok((0, count, Vec::new())))
}

/// Merge the results of the two Cursor sync paths (bubbles + Usage API)
/// into the single `(api_calls, message_count, warnings)` tuple the
/// provider trait expects. Returning `None` here leaves the JSONL
/// fallback free to run for the tick, same as before #553.
fn combine_cursor_sync_results(
    bubbles: Option<Result<(usize, usize, Vec<String>)>>,
    api: Option<Result<(usize, usize, Vec<String>)>>,
) -> Option<Result<(usize, usize, Vec<String>)>> {
    match (bubbles, api) {
        (None, None) => None,
        (Some(Err(e)), _) => Some(Err(e)),
        (_, Some(Err(e))) => Some(Err(e)),
        (Some(Ok(b)), None) => Some(Ok(b)),
        (None, Some(Ok(a))) => Some(Ok(a)),
        (Some(Ok((ba, bc, mut bw))), Some(Ok((aa, ac, aw)))) => {
            bw.extend(aw);
            Some(Ok((ba + aa, bc + ac, bw)))
        }
    }
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
                cwd_source: None,
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
                cwd_source: None,
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

    /// #504 (RC-4): reason tags are a semi-stable wire contract — they
    /// show up in `daemon.log` (`event=cursor_auth_skipped reason=...`),
    /// so operator doc / troubleshooting scripts key off these strings.
    /// Pinning the exact literal strings keeps a rename from silently
    /// breaking downstream matchers.
    #[test]
    fn cursor_auth_issue_reason_tags_are_stable() {
        assert_eq!(CursorAuthIssue::NoStateVscdb.reason_tag(), "no_state_vscdb");
        assert_eq!(
            CursorAuthIssue::StateVscdbOpenFailed.reason_tag(),
            "state_vscdb_open_failed"
        );
        assert_eq!(
            CursorAuthIssue::TokenRowMissing.reason_tag(),
            "token_row_missing"
        );
        assert_eq!(CursorAuthIssue::TokenEmpty.reason_tag(), "token_empty");
        assert_eq!(
            CursorAuthIssue::TokenMalformed.reason_tag(),
            "token_malformed"
        );
        assert_eq!(CursorAuthIssue::TokenExpired.reason_tag(), "token_expired");
        assert_eq!(
            CursorAuthIssue::TokenMissingSubject.reason_tag(),
            "token_missing_subject"
        );
        // Every variant's human_message must also mention the Usage API
        // path explicitly so an operator grepping for it finds the
        // single remediation surface (sign back in to Cursor).
        for issue in [
            CursorAuthIssue::NoStateVscdb,
            CursorAuthIssue::StateVscdbOpenFailed,
            CursorAuthIssue::TokenRowMissing,
            CursorAuthIssue::TokenEmpty,
            CursorAuthIssue::TokenMalformed,
            CursorAuthIssue::TokenExpired,
            CursorAuthIssue::TokenMissingSubject,
        ] {
            let msg = issue.human_message();
            assert!(
                msg.contains("Usage API") || msg.contains("Cursor"),
                "reason `{:?}` must mention Usage API or Cursor in its message, got {msg:?}",
                issue,
            );
        }
    }

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

    // --- cursorDiskKV bubble path (#553) ---

    /// Populate a brand-new `state.vscdb`-shaped SQLite file with a
    /// `cursorDiskKV` + `ItemTable` fixture shaped like a real Cursor
    /// `state.vscdb`. `bubble_rows` use the production key layout
    /// (`bubbleId:<36-char conv>:<36-char bubble>`); `composer_ids` plants
    /// a matching `composer.composerHeaders` row so the fallback
    /// timestamp path has data to read.
    fn seed_bubble_db(path: &Path, rows: &[(&str, &str)]) {
        let conn = Connection::open(path).expect("open fixture db");
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        for (key, value) in rows {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .unwrap();
        }
    }

    /// Insert a `composer.composerHeaders` row covering the given
    /// (composer_id, created_ms, last_updated_ms) triples so the
    /// fallback-timestamp path has data to read.
    fn seed_composer_headers(path: &Path, composers: &[(&str, i64, i64)]) {
        let payload = serde_json::json!({
            "allComposers": composers
                .iter()
                .map(|(id, c, u)| serde_json::json!({
                    "composerId": id,
                    "createdAt": c,
                    "lastUpdatedAt": u,
                }))
                .collect::<Vec<_>>(),
        });
        let conn = Connection::open(path).expect("reopen fixture db");
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('composer.composerHeaders', ?1)",
            params![payload.to_string()],
        )
        .unwrap();
    }

    /// Pin the exact key layout every real `state.vscdb` on the
    /// maintainer machine has — 82 chars, two 36-char UUIDs joined by
    /// `bubbleId:` and a single colon. Tests use names like
    /// `"00000000-0000-0000-0000-000000000001"` so the keys still read.
    fn bubble_key(conv: &str, bubble: &str) -> String {
        let k = format!("bubbleId:{conv}:{bubble}");
        assert_eq!(k.len(), 82, "test key not 82 chars: {k}");
        k
    }

    const FIXTURE_CONV_1: &str = "11111111-1111-1111-1111-111111111111";
    const FIXTURE_CONV_2: &str = "22222222-2222-2222-2222-222222222222";
    const FIXTURE_CONV_3: &str = "33333333-3333-3333-3333-333333333333";
    const FIXTURE_BUBBLE_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const FIXTURE_BUBBLE_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

    #[test]
    fn read_cursor_bubbles_returns_parsed_messages_from_fixture_db() {
        let dir = make_test_dir("cursor-bubbles-fixture");
        let db = dir.join("state.vscdb");
        let rows = [
            (
                bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":5000,"outputTokens":1200},"modelInfo":{"modelName":"claude-sonnet-4-6"},"createdAt":"2026-04-22T10:00:00.000Z","type":2}"#.to_string(),
            ),
            (
                bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_B),
                r#"{"tokenCount":{"inputTokens":0,"outputTokens":0},"modelInfo":{"modelName":""},"createdAt":"2026-04-22T10:00:05.000Z","type":1}"#.to_string(),
            ),
            (
                bubble_key(FIXTURE_CONV_2, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":10000,"outputTokens":500},"modelInfo":{"modelName":"gpt-5"},"createdAt":1774555000000,"type":2}"#.to_string(),
            ),
            // Noise: zero tokens + non-user type — filtered at the SQL WHERE.
            (
                bubble_key(FIXTURE_CONV_3, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":0,"outputTokens":0},"createdAt":"2026-04-22T10:00:10.000Z","type":2}"#.to_string(),
            ),
        ];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);

        let parsed = read_cursor_bubbles(&db, None).expect("read bubbles ok");

        // Assistant rows + the single user row survive; the zero-token
        // non-user noise row is filtered at the SQL WHERE.
        assert_eq!(parsed.len(), 3, "got: {parsed:?}");

        let assistant_sonnet = parsed
            .iter()
            .find(|m| m.model.as_deref() == Some("claude-sonnet-4-6"))
            .expect("sonnet row present");
        assert_eq!(assistant_sonnet.input_tokens, 5000);
        assert_eq!(assistant_sonnet.output_tokens, 1200);
        assert_eq!(assistant_sonnet.role, "assistant");
        assert_eq!(assistant_sonnet.session_id.as_deref(), Some(FIXTURE_CONV_1));
        assert_eq!(assistant_sonnet.provider, "cursor");
        assert!(assistant_sonnet.cost_cents.is_none());
        let expected_uuid = format!("cursor:bubble:{FIXTURE_CONV_1}:{FIXTURE_BUBBLE_A}");
        assert_eq!(
            assistant_sonnet.uuid, expected_uuid,
            "uuid must carry conv+bubble ids from the row key",
        );

        // Numeric epoch-ms createdAt is accepted too.
        let gpt = parsed
            .iter()
            .find(|m| m.model.as_deref() == Some("gpt-5"))
            .expect("gpt-5 row present");
        assert_eq!(gpt.input_tokens, 10000);
        assert_eq!(gpt.session_id.as_deref(), Some(FIXTURE_CONV_2));

        // User row: role=user, zero tokens. Uuid embeds its own bubble id
        // so a tokens-bearing assistant reply in the same conversation
        // cannot collide with it.
        let user_row = parsed
            .iter()
            .find(|m| m.role == "user")
            .expect("user row present");
        assert_eq!(user_row.session_id.as_deref(), Some(FIXTURE_CONV_1));
        assert_eq!(user_row.input_tokens, 0);
        assert_eq!(user_row.output_tokens, 0);
        let expected_user_uuid = format!("cursor:bubble:{FIXTURE_CONV_1}:{FIXTURE_BUBBLE_B}");
        assert_eq!(user_row.uuid, expected_user_uuid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_mode_falls_back_to_claude_sonnet_4_5() {
        let dir = make_test_dir("cursor-bubbles-auto");
        let db = dir.join("state.vscdb");
        let rows = [
            (
                bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":100,"outputTokens":50},"modelInfo":{"modelName":""},"createdAt":"2026-04-22T10:00:00.000Z","type":2}"#.to_string(),
            ),
            (
                bubble_key(FIXTURE_CONV_2, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":200,"outputTokens":80},"modelInfo":{"modelName":"default"},"createdAt":"2026-04-22T10:01:00.000Z","type":2}"#.to_string(),
            ),
            (
                bubble_key(FIXTURE_CONV_3, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":300,"outputTokens":120},"createdAt":"2026-04-22T10:02:00.000Z","type":2}"#.to_string(),
            ),
        ];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);

        let parsed = read_cursor_bubbles(&db, None).expect("read bubbles ok");
        assert_eq!(parsed.len(), 3);
        for msg in &parsed {
            assert_eq!(
                msg.model.as_deref(),
                Some(CURSOR_AUTO_MODEL_FALLBACK),
                "Auto-mode bubble did not fall back to Sonnet: {msg:?}",
            );
            assert_eq!(msg.role, "assistant");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression test for the v8.3.7 live-smoke finding: bubbles with
    /// no `$.createdAt` in the JSON value must still ingest, using the
    /// composer header's `lastUpdatedAt` as the conversation-level
    /// fallback timestamp. Pre-fix these rows returned `None` and the
    /// bulk of real-world traffic dropped silently.
    #[test]
    fn bubbles_without_created_at_fall_back_to_composer_timestamp() {
        let dir = make_test_dir("cursor-bubbles-composer-fallback");
        let db = dir.join("state.vscdb");
        let rows = [(
            bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
            r#"{"tokenCount":{"inputTokens":500,"outputTokens":200},"modelInfo":{"modelName":"claude-sonnet-4-6"},"type":2}"#.to_string(),
        )];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);
        seed_composer_headers(
            &db,
            &[(FIXTURE_CONV_1, 1_774_000_000_000, 1_774_555_000_000)],
        );

        let parsed = read_cursor_bubbles(&db, None).expect("read bubbles ok");
        assert_eq!(
            parsed.len(),
            1,
            "composer fallback must cover missing createdAt"
        );
        let msg = &parsed[0];
        assert_eq!(msg.input_tokens, 500);
        assert_eq!(
            msg.timestamp.timestamp_millis(),
            1_774_555_000_000,
            "fallback must use composer.lastUpdatedAt",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bubbles with neither `$.createdAt` nor a composer-header match
    /// drop on the floor — pre-fix they'd land at `Utc::now()` and
    /// pollute today's totals.
    #[test]
    fn bubbles_without_any_timestamp_are_dropped() {
        let dir = make_test_dir("cursor-bubbles-no-ts");
        let db = dir.join("state.vscdb");
        let rows = [(
            bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
            r#"{"tokenCount":{"inputTokens":500,"outputTokens":200},"type":2}"#.to_string(),
        )];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);
        // No composer headers seeded → no fallback available.

        let parsed = read_cursor_bubbles(&db, None).expect("read bubbles ok");
        assert!(
            parsed.is_empty(),
            "rows without any timestamp source must not be invented",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Malformed keys (not the 82-char `bubbleId:<conv>:<bubble>` shape)
    /// never reach `bubble_to_parsed_message` — the SQL guard drops them.
    #[test]
    fn malformed_bubble_keys_are_filtered_at_sql() {
        let dir = make_test_dir("cursor-bubbles-malformed-key");
        let db = dir.join("state.vscdb");
        let rows = [
            (
                "bubbleId:too-short".to_string(),
                r#"{"tokenCount":{"inputTokens":1,"outputTokens":1},"createdAt":"2026-04-22T10:00:00.000Z","type":2}"#.to_string(),
            ),
            (
                bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
                r#"{"tokenCount":{"inputTokens":1,"outputTokens":1},"createdAt":"2026-04-22T10:00:00.000Z","type":2}"#.to_string(),
            ),
        ];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);

        let parsed = read_cursor_bubbles(&db, None).expect("read bubbles ok");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].session_id.as_deref(), Some(FIXTURE_CONV_1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_missing_returns_empty_not_panic() {
        let dir = make_test_dir("cursor-bubbles-no-schema");
        let db = dir.join("state.vscdb");
        // Plausible, non-Cursor DB: has an ItemTable but no cursorDiskKV.
        // Mirrors the failure mode where a user points us at an sqlite
        // file that isn't (or is no longer) a Cursor state.vscdb.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', '');",
        )
        .unwrap();
        drop(conn);

        let parsed = read_cursor_bubbles(&db, None).expect("Ok even when schema is missing");
        assert!(
            parsed.is_empty(),
            "expected empty vec when cursorDiskKV is missing, got {parsed:?}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_roundtrip_writes_embedded_or_manifest_source() {
        use crate::pipeline::Pipeline;

        let dir = make_test_dir("cursor-bubbles-ingest");
        let db = dir.join("state.vscdb");
        let rows = [(
            bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
            r#"{"tokenCount":{"inputTokens":1000000,"outputTokens":100000},"modelInfo":{"modelName":"claude-sonnet-4-6"},"createdAt":"2026-04-22T10:00:00.000Z","type":2}"#.to_string(),
        )];
        let row_refs: Vec<(&str, &str)> =
            rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        seed_bubble_db(&db, &row_refs);

        let mut messages = read_cursor_bubbles(&db, None).expect("read ok");
        assert_eq!(messages.len(), 1);

        let mut pipeline = Pipeline::default_pipeline(None);
        let tags = pipeline.process(&mut messages);
        assert_eq!(tags.len(), messages.len());

        let msg = &messages[0];
        let src = msg
            .pricing_source
            .as_deref()
            .expect("CostEnricher sets pricing_source for priced rows");
        assert!(
            src.starts_with("embedded:v") || src.starts_with("manifest:v"),
            "unexpected pricing_source: {src}",
        );
        let cost = msg.cost_cents.expect("cost_cents populated");
        assert!(cost > 0.0, "expected non-zero cost_cents, got {cost}");

        // Ingest round-trips into an in-memory analytics DB without panicking.
        let mut analytics_conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&analytics_conn).unwrap();
        let inserted =
            analytics::ingest_messages(&mut analytics_conn, &messages, Some(&tags)).unwrap();
        assert_eq!(inserted, 1);

        let _ = std::fs::remove_dir_all(&dir);
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
