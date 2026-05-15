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
    }

    // #779 (post-rc.1 followup): session-title backfill runs on every
    // sync tick, not just when new messages landed. Existing sessions
    // that pre-date a parser update (#766 Phase 1, #778 Phase 2) need
    // their `sessions.title` re-derived from the parser even when no
    // new messages were ingested — otherwise a daemon upgrade leaves
    // the dashboard's Title column empty until the user happens to use
    // their IDE again. The function is idempotent — the `title IS NULL
    // OR title = ''` predicate makes follow-up calls free.
    let titles_backfilled = backfill_session_titles(conn);
    if titles_backfilled > 0 {
        tracing::info!("Backfilled session titles on {titles_backfilled} sessions");
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
/// - Any provider that emits a `session_title` tag (#787): a pure SQL
///   UPDATE pulls the most-frequent tag value per session into
///   `sessions.title`. The `IdentityEnricher` writes this tag whenever
///   `ParsedMessage.session_title` is set, so JetBrains Copilot rows
///   (Phase 1 #766 `projectName` / Phase 2 #778 Nitrite resolution /
///   session-type fallback) light up automatically as new messages
///   ingest. Idempotent — the `title IS NULL OR title = ''` predicate
///   makes follow-up passes free.
///
/// Historical jetbrains rows that pre-date the #787 enricher carry no
/// `session_title` tag and aren't covered by the SQL UPDATE. Run the
/// one-shot helper [`collect_jetbrains_session_titles`] from a manual
/// migration path to fill those.
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

    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let mut count = 0usize;
    for (sid, title) in titles.iter() {
        if tx
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1 AND (title IS NULL OR title = '')",
                rusqlite::params![sid, title],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            count += 1;
        }
    }

    // #787: tag-sourced title backfill. Pulls the most-frequent
    // `session_title` tag value across each session's messages into
    // `sessions.title`. The `IdentityEnricher` writes this tag for any
    // provider whose parser sets `ParsedMessage.session_title` (JetBrains
    // Copilot today, others if/when they wire it up). Pure SQL — no file
    // walks. Runs across every historical row, not just the last 30 days,
    // so dashboard sessions outside the recency window also light up.
    // Idempotent: the `title IS NULL OR title = ''` predicate becomes
    // empty after the first pass.
    if let Ok(updated) = tx.execute(
        "UPDATE sessions
            SET title = (
                SELECT t.value FROM tags t
                JOIN messages m ON m.id = t.message_id
                WHERE m.session_id = sessions.id
                  AND t.key = 'session_title'
                  AND t.value <> ''
                GROUP BY t.value
                ORDER BY COUNT(*) DESC, t.value ASC
                LIMIT 1
            )
          WHERE (title IS NULL OR title = '')
            AND EXISTS (
                SELECT 1 FROM tags t2
                JOIN messages m2 ON m2.id = t2.message_id
                WHERE m2.session_id = sessions.id
                  AND t2.key = 'session_title'
                  AND t2.value <> ''
            )",
        [],
    ) {
        count += updated;
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
        INGEST_BATCH_SIZE, ProviderSyncStats, SyncProgress, SyncReport, backfill_activity_tags,
        backfill_session_titles, backfill_ticket_tags, cleanup_legacy_auto_tags,
        extract_first_prompt, first_legacy_proxy_message_timestamp, ingest_in_batches,
        read_transcript_tail, truncate_title,
    };
    use crate::analytics::{Tag, get_sync_offset};
    use crate::jsonl::ParsedMessage;
    use chrono::{TimeZone, Utc};

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

    // #787: `backfill_session_titles` reads `session_title` tags
    // (emitted by the `IdentityEnricher`) and promotes the most-frequent
    // value per session into `sessions.title`. The pre-existing-title
    // guard is preserved from the original #779 backfill shape.
    #[test]
    fn backfill_session_titles_preserves_existing_title() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO sessions (id, provider, surface, title)
                VALUES ('s-untouched', 'copilot_chat', 'jetbrains', 'do not touch')",
            [],
        )
        .expect("insert session");

        let _ = backfill_session_titles(&mut conn);

        let preserved: Option<String> = conn
            .query_row(
                "SELECT title FROM sessions WHERE id = 's-untouched'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(
            preserved.as_deref(),
            Some("do not touch"),
            "pre-existing title must be preserved by the WHERE predicate"
        );
    }

    // #787: promote `session_title` tags into `sessions.title` via a
    // pure SQL UPDATE. The previous #779 followup walked JetBrains
    // session dirs every sync tick; with the new `IdentityEnricher`
    // path the tag lands in the DB at ingest time and the backfill
    // becomes a one-statement aggregation.
    #[test]
    fn backfill_session_titles_promotes_session_title_tag() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute_batch(
            "INSERT INTO sessions (id, provider, surface, title) VALUES
                ('s-resolved', 'copilot_chat', 'jetbrains', NULL),
                ('s-fallback', 'copilot_chat', 'jetbrains', ''),
                ('s-already',  'copilot_chat', 'jetbrains', 'do not touch');",
        )
        .expect("insert sessions");

        let assistant_row = |id: &str, sess: &str| {
            format!(
                "INSERT INTO messages
                 (id, session_id, role, timestamp, model, provider,
                  input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                  cost_cents_ingested, cost_cents_effective, cost_confidence, surface)
                 VALUES
                 ('{id}', '{sess}', 'assistant', datetime('now'), 'gpt-4o', 'copilot_chat',
                  0, 0, 0, 0, 0.0, 0.0, 'estimated', 'jetbrains');"
            )
        };
        conn.execute_batch(&format!(
            "{}{}{}{}{}",
            assistant_row("m-resolved-1", "s-resolved"),
            assistant_row("m-resolved-2", "s-resolved"),
            assistant_row("m-fallback-1", "s-fallback"),
            assistant_row("m-already-1", "s-already"),
            "INSERT INTO tags (message_id, key, value) VALUES
                ('m-resolved-1', 'session_title', 'Verkada-Web'),
                ('m-resolved-2', 'session_title', 'Verkada-Web'),
                ('m-fallback-1', 'session_title', 'chat-agent'),
                ('m-already-1',  'session_title', 'chat');"
        ))
        .expect("insert messages + tags");

        let updated = backfill_session_titles(&mut conn);
        assert!(
            updated >= 2,
            "expected at least the two empty-title rows to flip, got {updated}"
        );

        let title = |id: &str| -> Option<String> {
            conn.query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .ok()
        };
        assert_eq!(title("s-resolved").as_deref(), Some("Verkada-Web"));
        assert_eq!(title("s-fallback").as_deref(), Some("chat-agent"));
        assert_eq!(title("s-already").as_deref(), Some("do not touch"));

        // Idempotency: a second backfill pass updates nothing.
        let second = backfill_session_titles(&mut conn);
        assert_eq!(second, 0, "second pass should be a no-op");
    }

    /// A session whose messages carry no `session_title` tag at all
    /// must not be set to NULL by the `EXISTS`-guarded UPDATE.
    #[test]
    fn backfill_session_titles_skips_sessions_without_tags() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO sessions (id, provider, surface) VALUES ('s-empty', 'copilot_chat', 'jetbrains')",
            [],
        )
        .expect("insert session");
        conn.execute(
            "INSERT INTO messages
             (id, session_id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence, surface)
             VALUES
             ('m-empty', 's-empty', 'assistant', datetime('now'), 'gpt-4o', 'copilot_chat',
              0, 0, 0, 0, 0.0, 0.0, 'estimated', 'jetbrains')",
            [],
        )
        .expect("insert message without session_title tag");

        let _ = backfill_session_titles(&mut conn);
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = 's-empty'", [], |r| {
                r.get(0)
            })
            .unwrap_or(None);
        assert!(title.is_none(), "title should remain NULL");
    }

    /// #787 regression test: round-trip a synthetic `ParsedMessage` with
    /// `session_title=Some(...)` through the pipeline + ingest path and
    /// assert the `session_title` tag row landed.
    #[test]
    fn pipeline_emits_session_title_tag_through_ingest() {
        use chrono::{TimeZone, Utc};

        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        crate::migration::migrate(&conn).expect("migrate");

        let msg = crate::jsonl::ParsedMessage {
            uuid: "m-787".to_string(),
            session_id: Some("s-787".to_string()),
            timestamp: Utc.with_ymd_and_hms(2026, 5, 12, 18, 0, 0).unwrap(),
            role: "assistant".to_string(),
            provider: "copilot_chat".to_string(),
            cost_confidence: "estimated".to_string(),
            surface: Some("jetbrains".to_string()),
            session_title: Some("Verkada-Web".to_string()),
            ..crate::jsonl::ParsedMessage::default()
        };

        let mut pipeline = crate::pipeline::Pipeline::default_pipeline(None);
        let mut batch = vec![msg];
        let tags = pipeline.process(&mut batch);

        // The enricher emits the tag; pipeline dedup leaves it on the
        // assistant row because this is the first (and only) message
        // for the session.
        assert!(
            tags[0]
                .iter()
                .any(|t| t.key == "session_title" && t.value == "Verkada-Web"),
            "pipeline must emit session_title tag, got: {:?}",
            tags[0]
        );

        let msg_uuid = batch[0].uuid.clone();
        crate::analytics::ingest_messages(&mut conn, &batch, Some(&tags))
            .expect("ingest message + tags");

        let landed: Option<String> = conn
            .query_row(
                "SELECT value FROM tags WHERE message_id = ?1 AND key = 'session_title'",
                rusqlite::params![&msg_uuid],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(
            landed.as_deref(),
            Some("Verkada-Web"),
            "session_title tag must land in the DB after ingest"
        );
    }

    // ---- #823: chunk-bounds tests for ingest_in_batches ----
    //
    // `ingest_in_batches` is the "mints sync chunks" producer at the heart of
    // analytics/sync.rs: it slices a parsed-message vec into `INGEST_BATCH_SIZE`
    // chunks and writes the sync offset only on the final chunk. These tests
    // exercise the boundary cases (empty, exactly one batch, batch + 1) and the
    // idempotency contract (offset advances once at the end; re-running with
    // the same offset is a no-op).

    fn assistant_msg(uuid: &str) -> ParsedMessage {
        ParsedMessage {
            uuid: uuid.to_string(),
            session_id: Some(format!("s-{uuid}")),
            timestamp: Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            provider: "claude_code".to_string(),
            cost_confidence: "exact".to_string(),
            ..ParsedMessage::default()
        }
    }

    fn build_assistant_batch(n: usize) -> (Vec<ParsedMessage>, Vec<Vec<Tag>>) {
        let messages: Vec<ParsedMessage> =
            (0..n).map(|i| assistant_msg(&format!("m-{i}"))).collect();
        let tags: Vec<Vec<Tag>> = (0..n).map(|_| Vec::new()).collect();
        (messages, tags)
    }

    fn count_messages(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .expect("count rows")
    }

    #[test]
    fn ingest_in_batches_handles_empty_input() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        let inserted = ingest_in_batches(&mut conn, &[], &[], "/tmp/empty.jsonl", 0)
            .expect("empty batch ingest");

        assert_eq!(inserted, 0, "no rows should land");
        assert_eq!(count_messages(&conn), 0);
        // Empty input means the final-chunk write never fires, so no sync row.
        let offset = get_sync_offset(&conn, "/tmp/empty.jsonl").expect("offset lookup");
        assert_eq!(offset, 0);
    }

    #[test]
    fn ingest_in_batches_handles_exactly_one_chunk() {
        // INGEST_BATCH_SIZE messages exercises the single-chunk path where
        // `end == messages.len()` on the first iteration and the sync_file
        // write must fire on that one iteration.
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        let (msgs, tags) = build_assistant_batch(INGEST_BATCH_SIZE);
        let path = "/tmp/exactly-one.jsonl";
        let final_offset = 12_345;
        let inserted = ingest_in_batches(&mut conn, &msgs, &tags, path, final_offset)
            .expect("single-chunk ingest");

        assert_eq!(inserted, INGEST_BATCH_SIZE);
        assert_eq!(count_messages(&conn), INGEST_BATCH_SIZE as i64);
        // Sync offset must be persisted on the final (and only) chunk.
        let offset = get_sync_offset(&conn, path).expect("offset lookup");
        assert_eq!(offset, final_offset);
    }

    #[test]
    fn ingest_in_batches_handles_chunk_size_plus_one() {
        // INGEST_BATCH_SIZE + 1 forces a second pass with a single-message
        // chunk — the off-by-one case the issue calls out explicitly.
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        let total = INGEST_BATCH_SIZE + 1;
        let (msgs, tags) = build_assistant_batch(total);
        let path = "/tmp/plus-one.jsonl";
        let final_offset = 99;
        let inserted = ingest_in_batches(&mut conn, &msgs, &tags, path, final_offset)
            .expect("two-chunk ingest");

        assert_eq!(inserted, total, "every message must land exactly once");
        assert_eq!(count_messages(&conn), total as i64);
        let offset = get_sync_offset(&conn, path).expect("offset lookup");
        assert_eq!(
            offset, final_offset,
            "offset is written on the final chunk, not on intermediate chunks"
        );
    }

    #[test]
    fn ingest_in_batches_is_idempotent_across_runs() {
        // The "idempotency-key stability across runs" acceptance bullet:
        // re-running ingest_in_batches with the same UUIDs and same final
        // offset must not double-insert messages. UUIDs act as the
        // idempotency key on the messages table (INSERT OR IGNORE).
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        let (msgs, tags) = build_assistant_batch(50);
        let path = "/tmp/idempotent.jsonl";

        let first =
            ingest_in_batches(&mut conn, &msgs, &tags, path, 1_000).expect("first run ingest");
        let second =
            ingest_in_batches(&mut conn, &msgs, &tags, path, 1_000).expect("second run ingest");

        assert_eq!(first, 50);
        assert_eq!(second, 0, "re-running with same UUIDs must be a no-op");
        assert_eq!(count_messages(&conn), 50);
        let offset = get_sync_offset(&conn, path).expect("offset lookup");
        assert_eq!(offset, 1_000);
    }

    // ---- read_transcript_tail edge cases ----
    //
    // The transcript reader handles the resume-after-failure semantics: it
    // returns the slice from the last stored offset, normalizes truncated
    // files back to offset 0, and returns an empty string when the file is
    // already fully consumed.

    #[test]
    fn read_transcript_tail_returns_empty_when_offset_equals_len() {
        let path = temp_file_path("equals-len");
        std::fs::write(&path, "abc\n").expect("write");

        let (content, off) = read_transcript_tail(&path, 4).expect("read");
        assert_eq!(content, "");
        assert_eq!(off, 4);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_transcript_tail_returns_full_content_when_offset_zero() {
        let path = temp_file_path("zero");
        std::fs::write(&path, "hello\nworld\n").expect("write");

        let (content, off) = read_transcript_tail(&path, 0).expect("read");
        assert_eq!(content, "hello\nworld\n");
        assert_eq!(off, 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_transcript_tail_handles_empty_file() {
        let path = temp_file_path("empty");
        std::fs::write(&path, "").expect("write");

        let (content, off) = read_transcript_tail(&path, 0).expect("read");
        assert_eq!(content, "");
        assert_eq!(off, 0);

        let _ = std::fs::remove_file(path);
    }

    // ---- truncate_title ----
    //
    // Pure function — small, but on the title path for every Claude Code and
    // Cursor session title and so worth pinning the edge cases.

    #[test]
    fn truncate_title_passes_short_strings_through() {
        assert_eq!(truncate_title("fix bug", 120), "fix bug");
    }

    #[test]
    fn truncate_title_collapses_internal_whitespace_and_controls() {
        // Tabs, newlines, and runs of spaces all collapse to single spaces.
        assert_eq!(
            truncate_title("fix\tthe\nbug   please", 120),
            "fix the bug please"
        );
    }

    #[test]
    fn truncate_title_appends_ellipsis_when_too_long() {
        let long: String = "a".repeat(150);
        let out = truncate_title(&long, 100);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().filter(|&c| c == 'a').count(), 100);
    }

    #[test]
    fn truncate_title_respects_utf8_char_boundary() {
        // A multi-byte character straddling the cutoff must not be split:
        // the function walks back to the nearest char boundary first.
        let s = format!("{}é tail", "a".repeat(98));
        let out = truncate_title(&s, 99);
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    // ---- cleanup_legacy_auto_tags ----

    #[test]
    fn cleanup_legacy_auto_tags_removes_only_legacy_keys() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        // Need a message row first because tags.message_id is a foreign key
        // in the schema even if not strictly enforced.
        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence)
             VALUES
             ('m1', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              0, 0, 0, 0, 0.0, 0.0, 'exact')",
            [],
        )
        .expect("insert msg");

        conn.execute_batch(
            "INSERT INTO tags (message_id, key, value) VALUES
                ('m1', 'dominant_tool', 'Edit'),
                ('m1', 'repo',          'budi'),
                ('m1', 'branch',        'main'),
                ('m1', 'activity',      'feature'),
                ('m1', 'ticket_id',     'BUD-42');",
        )
        .expect("insert tags");

        let removed = cleanup_legacy_auto_tags(&mut conn);
        assert_eq!(removed, 3, "three legacy tags should be deleted");

        let remaining: Vec<String> = conn
            .prepare("SELECT key FROM tags ORDER BY key")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(remaining, vec!["activity", "ticket_id"]);
    }

    // ---- backfill_ticket_tags ----

    #[test]
    fn backfill_ticket_tags_extracts_from_branch() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence, git_branch)
             VALUES
             ('m-ticket', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              0, 0, 0, 0, 0.0, 0.0, 'exact', 'fix/BUDI-823-coverage')",
            [],
        )
        .expect("insert msg with branch");

        let n = backfill_ticket_tags(&mut conn);
        assert_eq!(n, 1, "one ticket should be extracted");

        let pairs: Vec<(String, String)> = conn
            .prepare("SELECT key, value FROM tags WHERE message_id = 'm-ticket' ORDER BY key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("ticket_id".to_string(), "BUDI-823".to_string()),
                ("ticket_prefix".to_string(), "BUDI".to_string()),
            ]
        );
    }

    #[test]
    fn backfill_ticket_tags_skips_already_tagged_messages() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence, git_branch)
             VALUES
             ('m-tagged', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              0, 0, 0, 0, 0.0, 0.0, 'exact', 'fix/BUDI-823-coverage')",
            [],
        )
        .expect("insert msg");
        conn.execute(
            "INSERT INTO tags (message_id, key, value) VALUES ('m-tagged', 'ticket_id', 'PREEXISTING-1')",
            [],
        )
        .expect("seed existing tag");

        let n = backfill_ticket_tags(&mut conn);
        assert_eq!(n, 0, "messages with a ticket_id tag must be skipped");

        let val: String = conn
            .query_row(
                "SELECT value FROM tags WHERE message_id = 'm-tagged' AND key = 'ticket_id'",
                [],
                |r| r.get(0),
            )
            .expect("ticket_id row");
        assert_eq!(val, "PREEXISTING-1", "existing tag must not be overwritten");
    }

    // ---- extract_first_prompt ----
    //
    // Pulled from a JSONL transcript to seed `sessions.title` on Claude Code
    // sessions. Skips system/synthetic messages whose text starts with `<`,
    // accepts both plain-string and content-blocks shapes.

    #[test]
    fn extract_first_prompt_reads_plain_string_content() {
        let path = temp_file_path("first-prompt-plain");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"content":"hello world"}}
"#,
        )
        .expect("write");

        let got = extract_first_prompt(&path);
        assert_eq!(got.as_deref(), Some("hello world"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extract_first_prompt_reads_content_blocks_array() {
        let path = temp_file_path("first-prompt-blocks");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"},{"type":"text","text":"there"}]}}
"#,
        )
        .expect("write");

        let got = extract_first_prompt(&path);
        assert_eq!(got.as_deref(), Some("hi there"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extract_first_prompt_skips_synthetic_and_non_user_lines() {
        let path = temp_file_path("first-prompt-skip");
        std::fs::write(
            &path,
            // First a non-user (system) message, then a synthetic user
            // message wrapped in <local-command-…>, then the real prompt.
            "{\"type\":\"assistant\",\"message\":{\"content\":\"a\"}}\n\
             {\"type\":\"user\",\"message\":{\"content\":\"<local-command-stdout></local-command-stdout>\"}}\n\
             {\"type\":\"user\",\"message\":{\"content\":\"real prompt\"}}\n",
        )
        .expect("write");

        let got = extract_first_prompt(&path);
        assert_eq!(got.as_deref(), Some("real prompt"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extract_first_prompt_returns_none_for_missing_file() {
        let path = std::env::temp_dir().join("does-not-exist-budi-sync.jsonl");
        let _ = std::fs::remove_file(&path);
        assert!(extract_first_prompt(&path).is_none());
    }

    #[test]
    fn backfill_ticket_tags_ignores_branch_without_ticket() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open db");
        crate::migration::migrate(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider,
              input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
              cost_cents_ingested, cost_cents_effective, cost_confidence, git_branch)
             VALUES
             ('m-main', 'assistant', datetime('now'), 'claude-opus-4-6', 'claude_code',
              0, 0, 0, 0, 0.0, 0.0, 'exact', 'main')",
            [],
        )
        .expect("insert msg on main");

        let n = backfill_ticket_tags(&mut conn);
        assert_eq!(n, 0);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tags WHERE message_id = 'm-main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
