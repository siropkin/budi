//! Sync pipeline: discovers transcript files across providers and ingests them.

use anyhow::Result;
use rusqlite::Connection;

use super::{get_sync_offset, ingest_messages_with_sync, set_sync_offset};

/// Quick sync: only files modified in the last 30 days.
/// Used by `budi sync` and the daemon's 30s auto-sync.
pub fn sync_all(conn: &mut Connection) -> Result<(usize, usize, Vec<String>)> {
    sync_with_max_age(conn, Some(30))
}

/// Full history sync: process ALL transcript files regardless of age.
/// Used by `budi history` — may take minutes on large histories.
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
    let providers = crate::provider::available_providers();
    let tags_config = crate::config::load_tags_config();
    let session_cache = crate::hooks::load_session_meta(conn, max_age_days).unwrap_or_default();
    let mut pipeline = crate::pipeline::Pipeline::default_pipeline(tags_config, session_cache);
    let mut total_files = 0;
    let mut total_messages = 0;
    let mut warnings: Vec<String> = Vec::new();

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

            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Skipping {}: {e}", file_path.display());
                    warnings.push(format!("Skipped {}: {e}", file_path.display()));
                    continue;
                }
            };

            if offset >= content.len() {
                continue; // Already fully synced.
            }

            let (mut messages, new_offset) = provider.parse_file(file_path, &content, offset)?;
            if messages.is_empty() {
                set_sync_offset(conn, &path_str, new_offset)?;
                continue;
            }

            let tags = pipeline.process(&mut messages);
            let count = ingest_messages_with_sync(
                conn,
                &messages,
                Some(&tags),
                Some((&path_str, new_offset)),
            )?;

            if count > 0 {
                total_files += 1;
                total_messages += count;
            }
        }
    }

    // Repair messages with NULL git_branch from two sources:
    // 1) The session row itself (populated by hooks or earlier ingestion)
    // 2) Sibling messages in the same session (e.g., user entries in CC JSONL
    //    carry gitBranch but assistant entries may not if parsed by older code)
    let repaired_from_session = conn.execute(
        "UPDATE messages SET git_branch = (
            SELECT s.git_branch FROM sessions s WHERE s.session_id = messages.session_id
         )
         WHERE git_branch IS NULL
           AND session_id IS NOT NULL
           AND timestamp >= datetime('now', '-30 days')
           AND EXISTS (
             SELECT 1 FROM sessions s
             WHERE s.session_id = messages.session_id
               AND s.git_branch IS NOT NULL AND s.git_branch != ''
           )",
        [],
    ).unwrap_or(0);
    let repaired_from_siblings = conn.execute(
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
    ).unwrap_or(0);
    let repaired = repaired_from_session + repaired_from_siblings;
    if repaired > 0 {
        tracing::info!("Repaired git_branch on {repaired} messages ({repaired_from_session} from sessions, {repaired_from_siblings} from siblings)");
    }

    Ok((total_files, total_messages, warnings))
}
