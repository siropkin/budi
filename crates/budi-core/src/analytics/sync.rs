//! Sync pipeline: discovers transcript files across providers and ingests them.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{
    Tag, get_sync_offset, ingest_messages_with_sync, mark_sync_completed, set_sync_offset,
};
use crate::jsonl::ParsedMessage;

const INGEST_BATCH_SIZE: usize = 1000;

/// Per-provider progress/summary slice. Used both for live polling
/// (via `SyncProgress`) and the final report (via `SyncReport`). `files_total`
/// is the number of transcript files the provider discovered for this window;
/// `files_synced` is the subset that actually had new messages to ingest; and
/// `messages` is how many rows landed in the analytics DB as a result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSyncStats {
    /// Machine-stable name, e.g. `claude_code` (matches [`Provider::name`]).
    pub name: String,
    /// Human label, e.g. `Claude Code` (matches [`Provider::display_name`]).
    pub display_name: String,
    /// Files the provider discovered in this window (0 for direct-sync providers).
    pub files_total: usize,
    /// Files processed so far that actually yielded ingested messages.
    pub files_synced: usize,
    /// Messages ingested into the analytics DB for this provider.
    pub messages: usize,
}

/// Live progress snapshot surfaced via `/sync/status` while a long-running
/// `budi db import` is in flight. The CLI polls this every ~2 s so the user
/// sees per-agent throughput rather than a silent 4-minute wait (#440).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncProgress {
    /// Machine name of the provider currently being synced (`None` once
    /// every provider has finished).
    #[serde(default)]
    pub current_provider: Option<String>,
    /// Running per-provider totals. Ordering matches enabled-providers order
    /// so the CLI can render a stable list even while providers are in flight.
    #[serde(default)]
    pub per_provider: Vec<ProviderSyncStats>,
}

/// Final report returned by `sync_all` / `sync_history`. Replaces the older
/// `(files, messages, warnings)` tuple so the CLI can render a per-agent
/// breakdown after import without a second round-trip (#440).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncReport {
    pub files_synced: usize,
    pub messages_ingested: usize,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub per_provider: Vec<ProviderSyncStats>,
}

/// Quick sync: only files modified in the last 30 days.
/// Used by the daemon's 30 s auto-sync and by `budi db import` when no
/// `--force` flag is set.
pub fn sync_all(conn: &mut Connection) -> Result<SyncReport> {
    sync_with_max_age(conn, Some(30), |_| {})
}

/// Full history sync: process ALL transcript files regardless of age.
/// Used by `budi db import` — may take minutes on large histories.
pub fn sync_history(conn: &mut Connection) -> Result<SyncReport> {
    sync_with_max_age(conn, None, |_| {})
}

/// Variant that fires `on_progress` periodically while providers are being
/// processed. The callback must not block; the daemon's `/sync/status`
/// handler takes a cheap `Mutex` lock and returns. Progress is throttled
/// to ~one call per second (plus one per provider transition) so chatty
/// per-file work never turns the callback into a hot loop.
pub fn sync_history_with_progress<F: FnMut(&SyncProgress)>(
    conn: &mut Connection,
    on_progress: F,
) -> Result<SyncReport> {
    sync_with_max_age(conn, None, on_progress)
}

/// Quick-sync variant with progress. Mirrors `sync_history_with_progress`
/// for the 30-day window used by the daemon's live auto-sync path.
pub fn sync_all_with_progress<F: FnMut(&SyncProgress)>(
    conn: &mut Connection,
    on_progress: F,
) -> Result<SyncReport> {
    sync_with_max_age(conn, Some(30), on_progress)
}

/// Internal sync implementation with optional max_age filter.
/// When `max_age_days` is Some(N), only files modified in the last N days are processed.
/// When None, all files are processed.
fn sync_with_max_age<F: FnMut(&SyncProgress)>(
    conn: &mut Connection,
    max_age_days: Option<u64>,
    mut on_progress: F,
) -> Result<SyncReport> {
    let providers = crate::provider::enabled_providers();
    let tags_config = crate::config::load_tags_config();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config);
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut total_messages_skipped_after_legacy_overlap = 0usize;
    let mut cursor_file_messages_ingested = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    let legacy_overlap_cutoff = first_legacy_proxy_message_timestamp(conn);

    let mut progress = SyncProgress {
        current_provider: None,
        per_provider: providers
            .iter()
            .map(|p| ProviderSyncStats {
                name: p.name().to_string(),
                display_name: p.display_name().to_string(),
                ..Default::default()
            })
            .collect(),
    };
    // 900 ms, not 1000: makes the two-second CLI poll land at least one fresh
    // tick each cycle even with a ~200 ms HTTP round-trip. The per-file loop
    // reads this to throttle its chatty emits; phase-boundary emits
    // (provider-start, post-discovery, post-loop) fire unconditionally.
    const PROGRESS_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(900);
    // Emit a zeroed snapshot before anything runs so the CLI's first poll
    // already sees the full provider list (otherwise the first 0-2 s of
    // progress output is empty on a fresh sync).
    on_progress(&progress);

    if let Some(cutoff) = legacy_overlap_cutoff {
        warnings.push(format!(
            "Retained 8.1 proxy history detected; importing transcript history only before {} to avoid double-counting against legacy proxy-estimated rows.",
            cutoff.to_rfc3339()
        ));
    }

    let cutoff = max_age_days
        .map(|days| std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400));

    for (idx, provider) in providers.iter().enumerate() {
        progress.current_provider = Some(provider.name().to_string());
        on_progress(&progress);

        // Try direct sync first (e.g. Cursor Usage API).
        if let Some(result) = provider.sync_direct(conn, &mut pipeline, max_age_days) {
            match result {
                Ok((files, messages, w)) => {
                    total_files += files;
                    total_messages += messages;
                    warnings.extend(w);
                    let stats = &mut progress.per_provider[idx];
                    stats.files_total = files;
                    stats.files_synced = files;
                    stats.messages = messages;
                    on_progress(&progress);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Provider sync_direct failed: {e:#}");
                    continue;
                }
            }
        }

        let files = provider.discover_files()?;
        progress.per_provider[idx].files_total = files.len();
        on_progress(&progress);
        // Per-provider binding so the Drop-less default (`Instant::now()`)
        // is always the initial read when the file loop spins up and never
        // a stale value from a previous provider's throttle window.
        let mut last_progress_emit = std::time::Instant::now();

        for discovered in &files {
            let file_path = &discovered.path;

            // Skip files older than cutoff (if set)
            if let Some(cutoff_time) = cutoff {
                let mtime = file_path
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                if mtime < cutoff_time {
                    continue; // Too old for quick sync
                }
            }

            let path_str = file_path.display().to_string();
            let offset = get_sync_offset(conn, &path_str)?;

            let (content, parse_start_offset) = match read_transcript_tail(file_path, offset) {
                Ok(slice) => slice,
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", file_path.display());
                    warnings.push(format!("Skipped {}: {e}", file_path.display()));
                    continue;
                }
            };

            if content.is_empty() {
                continue; // Already fully synced.
            }

            // Parse only newly appended content. The parser offset is relative to
            // this slice, so we add the original file offset back afterward.
            let (mut messages, relative_offset) = provider.parse_file(file_path, &content, 0)?;
            let new_offset = parse_start_offset.saturating_add(relative_offset);
            // 8.2 keeps legacy `proxy_estimated` messages queryable after upgrade.
            // Historical transcript imports must avoid re-ingesting the same time
            // window on top of those retained rows.
            if let Some(cutoff) = legacy_overlap_cutoff {
                let before = messages.len();
                messages.retain(|msg| msg.timestamp < cutoff);
                total_messages_skipped_after_legacy_overlap +=
                    before.saturating_sub(messages.len());
            }
            if messages.is_empty() {
                set_sync_offset(conn, &path_str, new_offset)?;
                continue;
            }

            let tags = pipeline.process(&mut messages);
            let count = ingest_in_batches(conn, &messages, &tags, &path_str, new_offset)?;

            if count > 0 {
                total_files += 1;
                total_messages += count;
                if provider.name() == "cursor" {
                    cursor_file_messages_ingested += count;
                }
                let stats = &mut progress.per_provider[idx];
                stats.files_synced += 1;
                stats.messages += count;
            }

            if last_progress_emit.elapsed() >= PROGRESS_EMIT_INTERVAL {
                on_progress(&progress);
                last_progress_emit = std::time::Instant::now();
            }
        }

        // Always flush one final tick so the CLI sees the final per-provider
        // total before moving on to the next agent (it may be that no file
        // ingested new bytes and the in-loop throttle never fired).
        on_progress(&progress);
    }

    progress.current_provider = None;
    on_progress(&progress);

    if cursor_file_messages_ingested > 0 {
        crate::providers::cursor::run_cursor_repairs(conn);
    }

    if total_messages_skipped_after_legacy_overlap > 0 {
        warnings.push(format!(
            "Skipped {} transcript messages at/after the retained legacy overlap boundary to prevent duplicate accounting.",
            total_messages_skipped_after_legacy_overlap
        ));
    }

    if total_messages > 0 {
        // Repair messages with NULL git_branch from two sources:
        // 1) The session row itself (populated by hooks or earlier ingestion)
        // 2) Sibling messages in the same session (e.g., user entries in CC JSONL
        //    carry gitBranch but assistant entries may not if parsed by older code)
        let repaired_from_session = conn
            .execute(
                "UPDATE messages SET git_branch = (
                SELECT s.git_branch FROM sessions s WHERE s.id = messages.session_id
             )
             WHERE git_branch IS NULL
               AND session_id IS NOT NULL
               AND timestamp >= datetime('now', '-30 days')
               AND EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.id = messages.session_id
                   AND s.git_branch IS NOT NULL AND s.git_branch != ''
               )",
                [],
            )
            .unwrap_or(0);
        let repaired_from_siblings = conn
            .execute(
                "UPDATE messages SET git_branch = (
                SELECT m2.git_branch FROM messages m2
                WHERE m2.session_id = messages.session_id
                  AND m2.git_branch IS NOT NULL AND m2.git_branch != ''
                LIMIT 1
             )
             WHERE git_branch IS NULL
               AND session_id IS NOT NULL
               AND timestamp >= datetime('now', '-30 days')
               AND EXISTS (
                 SELECT 1 FROM messages m2
                 WHERE m2.session_id = messages.session_id
                   AND m2.git_branch IS NOT NULL AND m2.git_branch != ''
               )",
                [],
            )
            .unwrap_or(0);
        let repaired = repaired_from_session + repaired_from_siblings;
        if repaired > 0 {
            tracing::info!(
                "Repaired git_branch on {repaired} messages ({repaired_from_session} from sessions, {repaired_from_siblings} from siblings)"
            );
        }

        // Backfill ticket_id / ticket_prefix tags for messages that have a
        // git_branch containing a ticket pattern but no ticket_id tag yet.
        let tickets_backfilled = backfill_ticket_tags(conn);
        if tickets_backfilled > 0 {
            tracing::info!("Backfilled ticket_id tags on {tickets_backfilled} messages");
        }

        let activities_backfilled = backfill_activity_tags(conn);
        if activities_backfilled > 0 {
            tracing::info!("Backfilled activity tags on {activities_backfilled} messages (#616)");
        }

        let removed_legacy_auto_tags = cleanup_legacy_auto_tags(conn);
        if removed_legacy_auto_tags > 0 {
            tracing::info!(
                "Removed {removed_legacy_auto_tags} legacy auto tags (dominant_tool/repo/branch)"
            );
        }

        // Heal sessions that older code paths inserted with NULL
        // started_at/ended_at — without this they never reach
        // `fetch_session_summaries` and the cloud's `session_summaries`
        // stays silently empty (#569). Runs before title backfill since
        // titles filter on `started_at`.
        match crate::migration::backfill_session_timestamps_from_messages(conn) {
            Ok(healed) if healed > 0 => {
                tracing::info!("Backfilled started_at/ended_at on {healed} sessions (#569)");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("Session timestamp backfill failed: {e}"),
        }

        // Backfill session titles from provider-specific sources.
        let titles_backfilled = backfill_session_titles(conn);
        if titles_backfilled > 0 {
            tracing::info!("Backfilled session titles on {titles_backfilled} sessions");
        }
    }

    if let Err(e) = crate::privacy::enforce_retention(conn) {
        tracing::warn!("Privacy retention cleanup failed during sync: {e}");
    }

    mark_sync_completed(conn)?;
    Ok(SyncReport {
        files_synced: total_files,
        messages_ingested: total_messages,
        warnings,
        per_provider: progress.per_provider,
    })
}

fn first_legacy_proxy_message_timestamp(conn: &Connection) -> Option<DateTime<Utc>> {
    let ts: Option<String> = conn
        .query_row(
            "SELECT MIN(timestamp) FROM messages WHERE role = 'assistant' AND cost_confidence = 'proxy_estimated'",
            [],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    ts.and_then(|raw| {
        chrono::DateTime::parse_from_rfc3339(&raw)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}

fn ingest_in_batches(
    conn: &mut Connection,
    messages: &[ParsedMessage],
    tags: &[Vec<Tag>],
    path_str: &str,
    new_offset: usize,
) -> Result<usize> {
    debug_assert_eq!(messages.len(), tags.len());
    let mut total = 0usize;

    let mut start = 0usize;
    while start < messages.len() {
        let end = (start + INGEST_BATCH_SIZE).min(messages.len());
        let sync_file = if end == messages.len() {
            Some((path_str, new_offset))
        } else {
            None
        };
        total += ingest_messages_with_sync(
            conn,
            &messages[start..end],
            Some(&tags[start..end]),
            sync_file,
            None,
        )?;
        start = end;
    }

    Ok(total)
}

fn read_transcript_tail(
    file_path: &std::path::Path,
    stored_offset: usize,
) -> Result<(String, usize)> {
    let file_len = std::fs::metadata(file_path)?.len() as usize;
    let effective_offset = if stored_offset > file_len {
        tracing::info!(
            "Transcript shrank, resetting offset for {} (stored={}, len={})",
            file_path.display(),
            stored_offset,
            file_len
        );
        0
    } else {
        stored_offset
    };

    if effective_offset == file_len {
        return Ok((String::new(), effective_offset));
    }

    let mut file = std::fs::File::open(file_path)?;
    file.seek(SeekFrom::Start(effective_offset as u64))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok((content, effective_offset))
}

/// Scan assistant messages with a git_branch but no ticket_id tag,
/// extract ticket IDs, and insert the missing tags.
/// Limited to recent messages (last 90 days) and batched to cap memory usage.
fn backfill_ticket_tags(conn: &mut Connection) -> usize {
    let rows: Vec<(String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT m.id, m.git_branch
             FROM messages m
             WHERE m.role = 'assistant'
               AND m.git_branch IS NOT NULL AND m.git_branch != ''
               AND m.timestamp >= datetime('now', '-90 days')
               AND NOT EXISTS (
                 SELECT 1 FROM tags t
                 WHERE t.message_id = m.id AND t.key = 'ticket_id'
               )
             LIMIT 10000",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    if rows.is_empty() {
        return 0;
    }

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return 0,
    };

    let mut count = 0usize;
    for (uuid, branch) in &rows {
        if let Some(ticket) = crate::pipeline::extract_ticket_id(branch) {
            if let Err(e) = tx.execute(
                "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'ticket_id', ?2)",
                rusqlite::params![uuid, ticket],
            ) {
                tracing::warn!("backfill_ticket_tags: ticket_id insert failed for {uuid}: {e}");
            }
            if let Some(dash) = ticket.find('-')
                && let Err(e) = tx.execute(
                    "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'ticket_prefix', ?2)",
                    rusqlite::params![uuid, &ticket[..dash]],
                )
            {
                tracing::warn!("backfill_ticket_tags: ticket_prefix insert failed for {uuid}: {e}");
            }
            count += 1;
        }
    }

    if tx.commit().is_err() {
        return 0;
    }
    count
}

/// Backfill `activity` / `activity_source` / `activity_confidence` tags for
/// assistant messages that were ingested without them.
///
/// Root cause (#616): the live tailer processes user and assistant messages
/// in separate batches (the user entry is written to JSONL before Claude
/// responds). `propagate_session_context` inside `Pipeline::process` can
/// only propagate `prompt_category` from user → assistant within a single
/// batch, so an assistant arriving in a later batch never inherits the
/// classification and never receives an activity tag.
///
/// Fix: after each sync cycle, find recent assistant messages without an
/// activity tag whose session carries a `prompt_category` (set when the
/// user message was ingested) and insert the missing tags. The session-
/// level classification is the best available signal when per-message
/// prompt text is not stored in the database.
fn backfill_activity_tags(conn: &mut Connection) -> usize {
    let rows: Vec<(String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT m.id, s.prompt_category
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             WHERE m.role = 'assistant'
               AND m.timestamp >= datetime('now', '-90 days')
               AND s.prompt_category IS NOT NULL AND s.prompt_category != ''
               AND NOT EXISTS (
                 SELECT 1 FROM tags t
                 WHERE t.message_id = m.id AND t.key = 'activity'
               )
             LIMIT 10000",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    if rows.is_empty() {
        return 0;
    }

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return 0,
    };

    let mut count = 0usize;
    for (uuid, category) in &rows {
        if let Err(e) = tx.execute(
            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'activity', ?2)",
            rusqlite::params![uuid, category],
        ) {
            tracing::warn!("backfill_activity_tags: activity insert failed for {uuid}: {e}");
            continue;
        }
        let _ = tx.execute(
            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'activity_source', 'rule')",
            rusqlite::params![uuid],
        );
        let _ = tx.execute(
            "INSERT OR IGNORE INTO tags (message_id, key, value) VALUES (?1, 'activity_confidence', 'medium')",
            rusqlite::params![uuid],
        );
        count += 1;
    }

    if tx.commit().is_err() {
        return 0;
    }
    count
}

fn cleanup_legacy_auto_tags(conn: &mut Connection) -> usize {
    conn.execute(
        "DELETE FROM tags WHERE key IN ('dominant_tool', 'repo', 'branch')",
        [],
    )
    .unwrap_or(0)
}

/// Backfill `sessions.title` for sessions that don't have one yet.
///
/// Sources:
/// - Claude Code: first user prompt from the JSONL transcript file.
/// - Cursor: composer name from state.vscdb `allComposers`.
fn backfill_session_titles(conn: &mut Connection) -> usize {
    let rows: Vec<(String, String)> = {
        let mut stmt = match conn.prepare(
            "SELECT id, provider FROM sessions
             WHERE (title IS NULL OR title = '')
               AND started_at >= datetime('now', '-30 days')",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    if rows.is_empty() {
        return 0;
    }

    let cc_ids: Vec<&str> = rows
        .iter()
        .filter(|(_, p)| p == "claude_code")
        .map(|(id, _)| id.as_str())
        .collect();
    let cursor_ids: Vec<&str> = rows
        .iter()
        .filter(|(_, p)| p == "cursor")
        .map(|(id, _)| id.as_str())
        .collect();

    let mut titles: HashMap<String, String> = HashMap::new();

    if !cc_ids.is_empty() {
        titles.extend(collect_claude_code_titles(&cc_ids));
    }
    if !cursor_ids.is_empty() {
        titles.extend(collect_cursor_titles(&cursor_ids));
    }

    if titles.is_empty() {
        return 0;
    }

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for (sid, title) in &titles {
        if tx
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1 AND (title IS NULL OR title = '')",
                rusqlite::params![sid, title],
            )
            .is_ok()
        {
            count += 1;
        }
    }
    if tx.commit().is_err() {
        return 0;
    }
    count
}

/// Read the first user prompt from Claude Code JSONL transcripts.
fn collect_claude_code_titles(session_ids: &[&str]) -> HashMap<String, String> {
    use std::collections::HashSet;

    let mut needed: HashSet<&str> = session_ids.iter().copied().collect();
    let mut result = HashMap::new();

    let claude_dir = match crate::config::home_dir() {
        Ok(h) => h.join(".claude").join("projects"),
        Err(_) => return result,
    };
    if !claude_dir.exists() {
        return result;
    }

    let project_dirs: Vec<_> = match std::fs::read_dir(&claude_dir) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).collect(),
        Err(_) => return result,
    };

    for project_entry in &project_dirs {
        if needed.is_empty() {
            break;
        }
        let entries = match std::fs::read_dir(project_entry.path()) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if !needed.contains(name) {
                continue;
            }
            if let Some(title) = extract_first_prompt(&path) {
                result.insert(name.to_string(), title);
                needed.remove(name);
            }
        }
    }
    result
}

/// Extract the first user prompt text from a JSONL file (reads only until found).
/// Skips system/synthetic messages like `<local-command-caveat>`.
fn extract_first_prompt(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);

    for line in reader.lines().take(200) {
        let line = line.ok()?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let text = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| {
                if let Some(s) = c.as_str() {
                    return Some(s.to_string());
                }
                if let Some(blocks) = c.as_array() {
                    let t: String = blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
                None
            })?;
        if text.starts_with('<') || text.is_empty() {
            continue;
        }
        let title = truncate_title(&text, 120);
        if !title.is_empty() {
            return Some(title);
        }
    }
    None
}

/// Read Cursor composer names from state.vscdb globalStorage.
///
/// Cursor stores per-composer data in `cursorDiskKV` with keys like
/// `composerData:<composerId>`. Each value is JSON with a `name` field.
fn collect_cursor_titles(session_ids: &[&str]) -> HashMap<String, String> {
    let mut result = HashMap::new();

    let home = match crate::config::home_dir() {
        Ok(h) => h,
        Err(_) => return result,
    };

    let global_dbs = [
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
        home.join(".config/Cursor/User/globalStorage/state.vscdb"),
    ];

    for db_path in &global_dbs {
        if !db_path.exists() {
            continue;
        }
        let Ok(db) = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };

        for sid in session_ids {
            if result.contains_key(*sid) {
                continue;
            }
            let key = format!("composerData:{sid}");
            let json_str: String = match db.query_row(
                "SELECT value FROM cursorDiskKV WHERE key = ?1",
                rusqlite::params![key],
                |row| row.get(0),
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(name) = parsed.get("name").and_then(|v| v.as_str())
                && !name.is_empty()
            {
                result.insert(sid.to_string(), truncate_title(name, 120));
            }
        }
    }
    result
}

fn truncate_title(text: &str, max_len: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if clean.len() <= max_len {
        clean
    } else {
        let mut end = max_len;
        while end > 0 && !clean.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &clean[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderSyncStats, SyncProgress, SyncReport, backfill_activity_tags,
        first_legacy_proxy_message_timestamp, read_transcript_tail,
    };

    fn temp_file_path(test_name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "budi-sync-{test_name}-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn read_transcript_tail_starts_from_offset() {
        let path = temp_file_path("offset");
        std::fs::write(&path, "line1\nline2\n").expect("should write test file");

        let (content, effective_offset) =
            read_transcript_tail(&path, 6).expect("should read transcript tail");
        assert_eq!(effective_offset, 6);
        assert_eq!(content, "line2\n");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_transcript_tail_resets_offset_after_truncate() {
        let path = temp_file_path("truncate");
        std::fs::write(&path, "short\n").expect("should write test file");

        let (content, effective_offset) =
            read_transcript_tail(&path, 100).expect("should read transcript tail");
        assert_eq!(effective_offset, 0);
        assert_eq!(content, "short\n");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn first_legacy_proxy_message_timestamp_returns_none_without_proxy_rows() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");
        assert!(first_legacy_proxy_message_timestamp(&conn).is_none());
    }

    #[test]
    fn sync_report_roundtrips_per_provider_through_json() {
        // The daemon serializes `SyncReport` on `POST /sync/*` and the CLI
        // deserializes it through `crate::client::SyncResponse`. This test
        // pins the wire contract so a future rename of `per_provider` or
        // `files_synced` in `ProviderSyncStats` can't silently break
        // `budi db import`'s per-agent breakdown (#440).
        let report = SyncReport {
            files_synced: 2_159,
            messages_ingested: 152_414,
            warnings: vec!["retained legacy overlap".to_string()],
            per_provider: vec![
                ProviderSyncStats {
                    name: "claude_code".to_string(),
                    display_name: "Claude Code".to_string(),
                    files_total: 2_035,
                    files_synced: 2_035,
                    messages: 118_442,
                },
                ProviderSyncStats {
                    name: "cursor".to_string(),
                    display_name: "Cursor".to_string(),
                    files_total: 154,
                    files_synced: 154,
                    messages: 29_038,
                },
            ],
        };

        let wire = serde_json::to_string(&report).expect("serialize");
        let parsed: SyncReport = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(parsed.files_synced, 2_159);
        assert_eq!(parsed.messages_ingested, 152_414);
        assert_eq!(parsed.per_provider.len(), 2);
        assert_eq!(parsed.per_provider[0].name, "claude_code");
        assert_eq!(parsed.per_provider[0].messages, 118_442);
        assert_eq!(parsed.per_provider[1].files_total, 154);
    }

    #[test]
    fn sync_progress_tolerates_missing_fields() {
        // Deserialization must keep working if an older daemon doesn't know
        // about `current_provider` / `per_provider` yet. This guards the
        // "new CLI talking to old daemon" upgrade path so `budi db import`
        // degrades to "no progress, final summary only" instead of
        // throwing a parse error on every poll.
        let empty: SyncProgress = serde_json::from_str("{}").expect("empty object parses");
        assert!(empty.current_provider.is_none());
        assert!(empty.per_provider.is_empty());
    }

    #[test]
    fn first_legacy_proxy_message_timestamp_returns_earliest_proxy_row() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");
        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents_ingested, cost_cents_effective, cost_confidence)
             VALUES
             ('m2', 'assistant', '2026-04-10T10:00:00Z', 'gpt-4o', 'openai', 10, 5, 0, 0, 1.0, 1.0, 'proxy_estimated'),
             ('m1', 'assistant', '2026-04-10T09:00:00Z', 'gpt-4o', 'openai', 10, 5, 0, 0, 1.0, 1.0, 'proxy_estimated'),
             ('m3', 'assistant', '2026-04-10T08:00:00Z', 'claude-sonnet-4-6', 'claude_code', 10, 5, 0, 0, 1.0, 1.0, 'estimated')",
            [],
        )
        .expect("insert messages");

        let ts = first_legacy_proxy_message_timestamp(&conn).expect("timestamp exists");
        assert_eq!(ts.to_rfc3339(), "2026-04-10T09:00:00+00:00");
    }

    #[test]
    fn backfill_activity_tags_fills_from_session_category() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");

        conn.execute(
            "INSERT INTO sessions (id, provider, prompt_category)
             VALUES ('s1', 'claude_code', 'bugfix')",
            [],
        )
        .expect("insert session");

        conn.execute(
            "INSERT INTO messages
             (id, session_id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence)
             VALUES
             ('a1', 's1', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              100, 50, 0, 0, 5.0, 5.0, 'exact')",
            [],
        )
        .expect("insert assistant message");

        let count = backfill_activity_tags(&mut conn);
        assert_eq!(count, 1, "should backfill exactly one message");

        let tags: Vec<(String, String)> = conn
            .prepare("SELECT key, value FROM tags WHERE message_id = 'a1' ORDER BY key")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(
            tags,
            vec![
                ("activity".to_string(), "bugfix".to_string()),
                ("activity_confidence".to_string(), "medium".to_string()),
                ("activity_source".to_string(), "rule".to_string()),
            ]
        );
    }

    #[test]
    fn backfill_activity_tags_skips_already_tagged() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");

        conn.execute(
            "INSERT INTO sessions (id, provider, prompt_category)
             VALUES ('s1', 'claude_code', 'bugfix')",
            [],
        )
        .expect("insert session");

        conn.execute(
            "INSERT INTO messages
             (id, session_id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence)
             VALUES
             ('a1', 's1', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              100, 50, 0, 0, 5.0, 5.0, 'exact')",
            [],
        )
        .expect("insert assistant message");

        conn.execute(
            "INSERT INTO tags (message_id, key, value) VALUES ('a1', 'activity', 'feature')",
            [],
        )
        .expect("insert existing activity tag");

        let count = backfill_activity_tags(&mut conn);
        assert_eq!(count, 0, "should not backfill already-tagged message");

        let activity: String = conn
            .query_row(
                "SELECT value FROM tags WHERE message_id = 'a1' AND key = 'activity'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(activity, "feature", "original tag should be preserved");
    }

    #[test]
    fn backfill_activity_tags_skips_sessions_without_category() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");

        conn.execute(
            "INSERT INTO sessions (id, provider) VALUES ('s1', 'claude_code')",
            [],
        )
        .expect("insert session without prompt_category");

        conn.execute(
            "INSERT INTO messages
             (id, session_id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence)
             VALUES
             ('a1', 's1', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              100, 50, 0, 0, 5.0, 5.0, 'exact')",
            [],
        )
        .expect("insert assistant message");

        let count = backfill_activity_tags(&mut conn);
        assert_eq!(count, 0, "should not backfill when session has no category");
    }
}
