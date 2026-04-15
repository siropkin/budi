//! Sync pipeline: discovers transcript files across providers and ingests them.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;

use super::{
    Tag, get_sync_offset, ingest_messages_with_sync, mark_sync_completed, set_sync_offset,
};
use crate::jsonl::ParsedMessage;

const INGEST_BATCH_SIZE: usize = 1000;

/// Quick sync: only files modified in the last 30 days.
/// Used by `budi import` and the daemon's 30s auto-sync.
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize, Vec<String>)> {
    sync_with_max_age(conn, Some(30))
}

/// Full history sync: process ALL transcript files regardless of age.
/// Used by `budi import` — may take minutes on large histories.
pub fn sync_history(conn: &mut Connection) -> Result<(usize, usize, Vec<String>)> {
    sync_with_max_age(conn, None)
}

/// Internal sync implementation with optional max_age filter.
/// When `max_age_days` is Some(N), only files modified in the last N days are processed.
/// When None, all files are processed.
fn sync_with_max_age(
    conn: &mut Connection,
    max_age_days: Option<u64>,
) -> Result<(usize, usize, Vec<String>)> {
    let providers = crate::provider::enabled_providers();
    let tags_config = crate::config::load_tags_config();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config);
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut total_messages_skipped_after_proxy_cutoff = 0usize;
    let mut cursor_file_messages_ingested = 0usize;
    let mut warnings: Vec<String> = Vec::new();
    let proxy_cutoff = first_proxy_message_timestamp(conn);

    if let Some(cutoff) = proxy_cutoff {
        warnings.push(format!(
            "Proxy data detected; importing transcript history only before {} to avoid double-counting.",
            cutoff.to_rfc3339()
        ));
    }

    let cutoff = max_age_days
        .map(|days| std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400));

    for provider in &providers {
        // Try direct sync first (e.g. Cursor Usage API).
        if let Some(result) = provider.sync_direct(conn, &mut pipeline, max_age_days) {
            match result {
                Ok((files, messages, w)) => {
                    total_files += files;
                    total_messages += messages;
                    warnings.extend(w);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Provider sync_direct failed: {e:#}");
                    continue;
                }
            }
        }

        let files = provider.discover_files()?;

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
            if let Some(cutoff) = proxy_cutoff {
                let before = messages.len();
                messages.retain(|msg| msg.timestamp < cutoff);
                total_messages_skipped_after_proxy_cutoff += before.saturating_sub(messages.len());
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
            }
        }
    }

    if cursor_file_messages_ingested > 0 {
        crate::providers::cursor::run_cursor_repairs(conn);
    }

    if total_messages_skipped_after_proxy_cutoff > 0 {
        warnings.push(format!(
            "Skipped {} transcript messages at/after the first proxy event to prevent duplicate accounting.",
            total_messages_skipped_after_proxy_cutoff
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

        let removed_legacy_auto_tags = cleanup_legacy_auto_tags(conn);
        if removed_legacy_auto_tags > 0 {
            tracing::info!(
                "Removed {removed_legacy_auto_tags} legacy auto tags (dominant_tool/repo/branch)"
            );
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
    Ok((total_files, total_messages, warnings))
}

fn first_proxy_message_timestamp(conn: &Connection) -> Option<DateTime<Utc>> {
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
    use super::{first_proxy_message_timestamp, read_transcript_tail};

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
    fn first_proxy_message_timestamp_returns_none_without_proxy_rows() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");
        assert!(first_proxy_message_timestamp(&conn).is_none());
    }

    #[test]
    fn first_proxy_message_timestamp_returns_earliest_proxy_row() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        crate::migration::migrate(&conn).expect("migrate schema");
        conn.execute(
            "INSERT INTO messages
             (id, role, timestamp, model, provider, input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_cents, cost_confidence)
             VALUES
             ('m2', 'assistant', '2026-04-10T10:00:00Z', 'gpt-4o', 'openai', 10, 5, 0, 0, 1.0, 'proxy_estimated'),
             ('m1', 'assistant', '2026-04-10T09:00:00Z', 'gpt-4o', 'openai', 10, 5, 0, 0, 1.0, 'proxy_estimated'),
             ('m3', 'assistant', '2026-04-10T08:00:00Z', 'claude-sonnet-4-6', 'claude_code', 10, 5, 0, 0, 1.0, 'estimated')",
            [],
        )
        .expect("insert messages");

        let ts = first_proxy_message_timestamp(&conn).expect("timestamp exists");
        assert_eq!(ts.to_rfc3339(), "2026-04-10T09:00:00+00:00");
    }
}
