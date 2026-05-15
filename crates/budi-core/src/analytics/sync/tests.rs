use super::{
    INGEST_BATCH_SIZE, ProviderSyncStats, SyncProgress, SyncReport, backfill_activity_tags,
    backfill_session_titles, backfill_ticket_tags, cleanup_legacy_auto_tags, extract_first_prompt,
    first_legacy_proxy_message_timestamp, ingest_in_batches, read_transcript_tail, truncate_title,
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
    let messages: Vec<ParsedMessage> = (0..n).map(|i| assistant_msg(&format!("m-{i}"))).collect();
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

    let inserted =
        ingest_in_batches(&mut conn, &[], &[], "/tmp/empty.jsonl", 0).expect("empty batch ingest");

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
    let inserted =
        ingest_in_batches(&mut conn, &msgs, &tags, path, final_offset).expect("two-chunk ingest");

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

    let first = ingest_in_batches(&mut conn, &msgs, &tags, path, 1_000).expect("first run ingest");
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
