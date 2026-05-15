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
    let path =
        Path::new("/home/.cursor/projects/proj/agent-transcripts/abc-def-123/abc-def-123.jsonl");
    assert_eq!(session_id_from_path(path), "abc-def-123");
}

#[test]
fn session_id_from_path_flat() {
    let path = Path::new("/home/.cursor/projects/proj/agent-transcripts/xyz.jsonl");
    assert_eq!(session_id_from_path(path), "xyz");
}

#[test]
fn parse_cursor_line_normalizes_prefixed_session_uuid() {
    let line = r#"{"role":"assistant","sessionId":"cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb"}"#;
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
    let inserted = analytics::ingest_messages(&mut analytics_conn, &messages, Some(&tags)).unwrap();
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

// ---------------------------------------------------------------------------
// ADR-0090 §1 Usage API event fixtures — one fixture per documented
// `kind` variant (#819). Each fixture exercises [`parse_usage_event`]
// against the exact shape ADR-0090 records, so a future upstream rename
// fails at this test layer instead of silently zero-ing a column.
// ---------------------------------------------------------------------------

/// Build a Usage API event JSON value matching ADR-0090 §1's
/// `usageEventsDisplay` element shape, parameterized by `kind`.
fn adr0090_event(kind: &str, total_cents: Option<f64>) -> Value {
    let mut ev = serde_json::json!({
        "timestamp": "1774455909363",
        "model": "composer-2-fast",
        "kind": kind,
        "tokenUsage": {
            "inputTokens": 2958,
            "outputTokens": 1663,
            "cacheWriteTokens": 0,
            "cacheReadTokens": 48214,
        },
        "chargedCents": 0,
        "isChargeable": false,
        "isTokenBasedCall": false,
        "owningUser": "273223875",
        "owningTeam": "9890257",
    });
    if let Some(c) = total_cents {
        ev["tokenUsage"]["totalCents"] = serde_json::json!(c);
    }
    ev
}

#[test]
fn adr0090_variant_included_in_business_parses() {
    // The exact response example in ADR-0090 §1.
    let ev = adr0090_event("USAGE_EVENT_KIND_INCLUDED_IN_BUSINESS", Some(1.68));
    let parsed = parse_usage_event(&ev).expect("ADR fixture must parse");
    assert_eq!(parsed.timestamp_ms, 1_774_455_909_363);
    assert_eq!(parsed.model, "composer-2-fast");
    assert_eq!(parsed.input_tokens, 2958);
    assert_eq!(parsed.output_tokens, 1663);
    assert_eq!(parsed.cache_read_tokens, 48214);
    assert_eq!(parsed.total_cents, Some(1.68));
}

#[test]
fn adr0090_variant_free_credit_parses() {
    let ev = adr0090_event("FREE_CREDIT", Some(0.0));
    let parsed = parse_usage_event(&ev).expect("FREE_CREDIT must parse");
    assert_eq!(parsed.model, "composer-2-fast");
    assert_eq!(parsed.total_cents, Some(0.0));
}

#[test]
fn adr0090_variant_usage_based_parses() {
    let ev = adr0090_event("USAGE_BASED", Some(12.50));
    let parsed = parse_usage_event(&ev).expect("USAGE_BASED must parse");
    assert_eq!(parsed.total_cents, Some(12.50));
}

#[test]
fn adr0090_variant_subscription_included_zero_cents_keeps_zero() {
    // ADR-0090 caveat: `kind` vocabulary includes opaque values. When
    // `kind` matches the "Included" subscription marker AND cost is 0,
    // the parser keeps the zero rather than dropping the row, because
    // the tokens are the meaningful signal.
    let ev = serde_json::json!({
        "timestamp": "1774455909363",
        "model": "composer-2",
        "kind": "Included",
        "tokenUsage": {
            "inputTokens": 22770,
            "outputTokens": 6509,
            "cacheWriteTokens": 0,
            "cacheReadTokens": 236544,
            "totalCents": 0.0,
        }
    });
    let parsed = parse_usage_event(&ev).expect("subscription event must parse");
    assert_eq!(parsed.total_cents, Some(0.0));
    assert_eq!(parsed.input_tokens, 22770);
}

#[test]
fn adr0090_variant_unknown_kind_is_opaque() {
    // ADR-0090 caveat: anything outside the documented vocabulary is
    // treated as opaque — parser must still emit the event.
    let ev = adr0090_event("UNKNOWN_KIND_NOT_YET_OBSERVED", Some(5.0));
    let parsed = parse_usage_event(&ev).expect("opaque kind must parse");
    assert_eq!(parsed.total_cents, Some(5.0));
}

#[test]
fn parse_usage_event_drops_negative_cents_to_zero() {
    let ev = adr0090_event("USAGE_BASED", Some(-2.5));
    let parsed = parse_usage_event(&ev).expect("negative cents clamp, not drop");
    assert_eq!(parsed.total_cents, Some(0.0));
}

#[test]
fn parse_usage_event_drops_event_above_one_thousand_dollars() {
    // ADR-0090 caveat: $1000+ in a single request is treated as corrupt
    // and the event is dropped entirely (likely upstream bug).
    let ev = adr0090_event("USAGE_BASED", Some(200_000.0));
    assert!(parse_usage_event(&ev).is_none());
}

#[test]
fn parse_usage_event_keeps_high_cents_with_warn() {
    // Between $50 and $1000 the parser keeps the event and emits a warn.
    // We can't assert the warn fired from here, but we assert the event
    // survives — the warn path is exercised, the row is not dropped.
    let ev = adr0090_event("USAGE_BASED", Some(7500.0));
    let parsed = parse_usage_event(&ev).expect("high but plausible cost must parse");
    assert_eq!(parsed.total_cents, Some(7500.0));
}

#[test]
fn parse_usage_event_drops_event_with_no_tokens_and_no_cents() {
    // No tokens AND no cost → nothing to price, drop the row.
    let ev = serde_json::json!({
        "timestamp": "1774455909363",
        "model": "composer-2-fast",
        "tokenUsage": {
            "inputTokens": 0,
            "outputTokens": 0,
            "cacheWriteTokens": 0,
            "cacheReadTokens": 0,
        }
    });
    assert!(parse_usage_event(&ev).is_none());
}

#[test]
fn parse_usage_event_drops_event_without_timestamp() {
    let ev = serde_json::json!({
        "model": "composer-2-fast",
        "tokenUsage": {"inputTokens": 100, "outputTokens": 50, "totalCents": 0.5}
    });
    assert!(parse_usage_event(&ev).is_none());
}

#[test]
fn parse_usage_event_handles_missing_model_as_unknown() {
    let ev = serde_json::json!({
        "timestamp": "1774455909363",
        "tokenUsage": {"inputTokens": 100, "outputTokens": 50, "totalCents": 0.5}
    });
    let parsed = parse_usage_event(&ev).expect("event without model still parses");
    assert_eq!(parsed.model, "unknown");
}

#[test]
fn parse_usage_event_captures_cache_write_tokens() {
    let ev = serde_json::json!({
        "timestamp": "1774455909363",
        "model": "claude-sonnet-4-6",
        "tokenUsage": {
            "inputTokens": 100,
            "outputTokens": 50,
            "cacheWriteTokens": 4000,
            "cacheReadTokens": 8000,
            "totalCents": 1.0,
        }
    });
    let parsed = parse_usage_event(&ev).expect("must parse");
    assert_eq!(parsed.cache_creation_tokens, 4000);
    assert_eq!(parsed.cache_read_tokens, 8000);
}

#[test]
fn parse_timestamp_ms_string_and_number() {
    assert_eq!(
        parse_timestamp_ms(&Value::String("1234".into())),
        Some(1234)
    );
    assert_eq!(
        parse_timestamp_ms(&Value::Number(serde_json::Number::from(9_876_543_210_i64))),
        Some(9_876_543_210),
    );
    assert_eq!(parse_timestamp_ms(&Value::Null), None);
    assert_eq!(parse_timestamp_ms(&Value::Bool(true)), None);
    assert_eq!(parse_timestamp_ms(&Value::String("0".into())), None);
    assert_eq!(
        parse_timestamp_ms(&Value::String("not-a-number".into())),
        None
    );
}

#[test]
fn usage_event_timestamp_ms_passthrough() {
    let ev = serde_json::json!({"timestamp": "1774455909363"});
    assert_eq!(usage_event_timestamp_ms(&ev), Some(1_774_455_909_363));
    let ev = serde_json::json!({});
    assert_eq!(usage_event_timestamp_ms(&ev), None);
}

// ---------------------------------------------------------------------------
// `find_matching_session` — strict containment, clock-skew, no match.
// ---------------------------------------------------------------------------

fn session_ctx(start_ms: i64, end_ms: i64, id: &str) -> SessionContext {
    SessionContext {
        start_ms,
        end_ms,
        session_id: id.to_string(),
        workspace_root: None,
        repo_id: None,
        git_branch: None,
    }
}

#[test]
fn find_matching_session_strict_containment_wins() {
    let sessions = [session_ctx(1000, 2000, "a"), session_ctx(1500, 2500, "b")];
    // 1700 falls in both; pick the one with start_ms closest to ts.
    let matched = find_matching_session(1700, &sessions).expect("must match");
    assert_eq!(matched.session_id, "b");
}

#[test]
fn find_matching_session_clock_skew_fallback() {
    let sessions = [session_ctx(1000, 2000, "a")];
    // 2003 is past the session end but within the ±5s skew window.
    let matched = find_matching_session(2003, &sessions).expect("must match via skew");
    assert_eq!(matched.session_id, "a");
}

#[test]
fn find_matching_session_returns_none_when_far_outside() {
    let sessions = [session_ctx(1000, 2000, "a")];
    // 10000ms > 5000ms skew window from end_ms=2000.
    assert!(find_matching_session(7100, &sessions).is_none());
}

#[test]
fn find_matching_session_returns_none_for_empty_input() {
    assert!(find_matching_session(1234, &[]).is_none());
}

#[test]
fn usage_events_to_messages_matches_via_clock_skew() {
    // Event lands 2 seconds after session.end_ms — outside strict
    // containment but inside the ±5s skew window. The message must
    // still pick up the session metadata.
    let events = vec![CursorUsageEvent {
        timestamp_ms: 2002,
        model: "claude".to_string(),
        input_tokens: 1,
        output_tokens: 1,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        total_cents: Some(0.1),
    }];
    let sessions = vec![SessionContext {
        start_ms: 1000,
        end_ms: 2000,
        session_id: "skewed".to_string(),
        workspace_root: Some("/proj".to_string()),
        repo_id: Some("repo".to_string()),
        git_branch: Some("main".to_string()),
    }];
    let msgs = usage_events_to_messages(&events, &sessions);
    assert_eq!(msgs[0].session_id.as_deref(), Some("skewed"));
    assert_eq!(msgs[0].repo_id.as_deref(), Some("repo"));
}

// ---------------------------------------------------------------------------
// `base64url_decode` — JWT payload helper.
// ---------------------------------------------------------------------------

#[test]
fn base64url_decodes_standard_alphabet() {
    // "hello" → "aGVsbG8" (no padding required).
    assert_eq!(
        base64url_decode("aGVsbG8").as_deref(),
        Some(b"hello".as_ref())
    );
    // url-safe chars `-` / `_` decode like `+` / `/`.
    let with_url_safe = base64url_decode("a-_-").expect("url-safe decodes");
    assert_eq!(with_url_safe.len(), 3);
}

#[test]
fn base64url_decodes_with_padding_stripped() {
    // Standard "any carnal pleasure." → "YW55IGNhcm5hbCBwbGVhc3VyZS4=".
    // Trailing `=` is stripped.
    let decoded = base64url_decode("YW55IGNhcm5hbCBwbGVhc3VyZS4=").unwrap();
    assert_eq!(decoded, b"any carnal pleasure.");
}

#[test]
fn base64url_rejects_invalid_chars() {
    // `!` is not in the alphabet.
    assert!(base64url_decode("a!bc").is_none());
    // High-byte character (above ASCII 128).
    assert!(base64url_decode("aあbc").is_none());
}

// ---------------------------------------------------------------------------
// `parse_bubble_created_at` — three shapes Cursor has shipped.
// ---------------------------------------------------------------------------

#[test]
fn parse_bubble_created_at_iso8601() {
    let ms = parse_bubble_created_at("2026-04-22T10:00:00Z").expect("ISO 8601 must parse");
    assert!(ms > 1_700_000_000_000);
}

#[test]
fn parse_bubble_created_at_epoch_ms() {
    assert_eq!(
        parse_bubble_created_at("1774555000000"),
        Some(1_774_555_000_000)
    );
}

#[test]
fn parse_bubble_created_at_rejects_empty_and_garbage() {
    assert!(parse_bubble_created_at("").is_none());
    assert!(parse_bubble_created_at("   ").is_none());
    assert!(parse_bubble_created_at("not-a-timestamp").is_none());
    // Zero / negative epoch values are rejected — they're degenerate.
    assert!(parse_bubble_created_at("0").is_none());
    assert!(parse_bubble_created_at("-100").is_none());
}

// ---------------------------------------------------------------------------
// `cursor_prompt_text` — text / blocks / empty.
// ---------------------------------------------------------------------------

#[test]
fn cursor_prompt_text_extracts_plain_text() {
    let msg: CursorMessage = serde_json::from_str(r#"{"content":"hello world"}"#).unwrap();
    assert_eq!(
        cursor_prompt_text(Some(&msg)).as_deref(),
        Some("hello world")
    );
}

#[test]
fn cursor_prompt_text_extracts_block_text() {
    let msg: CursorMessage = serde_json::from_str(
        r#"{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}"#,
    )
    .unwrap();
    assert_eq!(
        cursor_prompt_text(Some(&msg)).as_deref(),
        Some("first second")
    );
}

#[test]
fn cursor_prompt_text_returns_none_for_empty_text() {
    let msg: CursorMessage = serde_json::from_str(r#"{"content":"   "}"#).unwrap();
    assert!(cursor_prompt_text(Some(&msg)).is_none());
}

#[test]
fn cursor_prompt_text_returns_none_for_no_message() {
    assert!(cursor_prompt_text(None).is_none());
}

// ---------------------------------------------------------------------------
// `attach_session_context_to_bubbles` — fills cwd/repo/branch on bubble msgs.
// ---------------------------------------------------------------------------

fn dummy_parsed_message(session_id: Option<&str>, ts_ms: i64) -> ParsedMessage {
    ParsedMessage {
        uuid: "test-uuid".to_string(),
        session_id: session_id.map(|s| s.to_string()),
        timestamp: DateTime::from_timestamp_millis(ts_ms).unwrap_or_else(Utc::now),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude".to_string()),
        input_tokens: 1,
        output_tokens: 1,
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
        surface: Some(crate::surface::CURSOR.to_string()),
    }
}

#[test]
fn attach_session_context_no_op_for_empty_sessions() {
    let mut msgs = vec![dummy_parsed_message(Some("a"), 1500)];
    attach_session_context_to_bubbles(&mut msgs, &[]);
    assert!(msgs[0].cwd.is_none());
    assert!(msgs[0].repo_id.is_none());
}

#[test]
fn attach_session_context_matches_by_direct_session_id() {
    let mut msgs = vec![dummy_parsed_message(Some("direct-match"), 99999)];
    let sessions = vec![SessionContext {
        start_ms: 0,
        end_ms: 1,
        session_id: "direct-match".to_string(),
        workspace_root: Some("/proj".to_string()),
        repo_id: Some("acme/repo".to_string()),
        git_branch: Some("main".to_string()),
    }];
    attach_session_context_to_bubbles(&mut msgs, &sessions);
    assert_eq!(msgs[0].cwd.as_deref(), Some("/proj"));
    assert_eq!(msgs[0].repo_id.as_deref(), Some("acme/repo"));
    assert_eq!(msgs[0].git_branch.as_deref(), Some("main"));
}

#[test]
fn attach_session_context_falls_back_to_timestamp_match() {
    let mut msgs = vec![dummy_parsed_message(Some("no-match"), 1500)];
    let sessions = vec![SessionContext {
        start_ms: 1000,
        end_ms: 2000,
        session_id: "by-time".to_string(),
        workspace_root: Some("/proj".to_string()),
        repo_id: Some("repo".to_string()),
        git_branch: None,
    }];
    attach_session_context_to_bubbles(&mut msgs, &sessions);
    // Direct id lookup misses → timestamp window matches.
    assert_eq!(msgs[0].cwd.as_deref(), Some("/proj"));
    assert_eq!(msgs[0].repo_id.as_deref(), Some("repo"));
}

#[test]
fn attach_session_context_preserves_existing_values() {
    let mut msgs = vec![ParsedMessage {
        cwd: Some("/preset".to_string()),
        repo_id: Some("preset-repo".to_string()),
        git_branch: Some("dev".to_string()),
        ..dummy_parsed_message(Some("direct-match"), 1500)
    }];
    let sessions = vec![SessionContext {
        start_ms: 0,
        end_ms: i64::MAX,
        session_id: "direct-match".to_string(),
        workspace_root: Some("/other".to_string()),
        repo_id: Some("other-repo".to_string()),
        git_branch: Some("main".to_string()),
    }];
    attach_session_context_to_bubbles(&mut msgs, &sessions);
    assert_eq!(msgs[0].cwd.as_deref(), Some("/preset"));
    assert_eq!(msgs[0].repo_id.as_deref(), Some("preset-repo"));
    assert_eq!(msgs[0].git_branch.as_deref(), Some("dev"));
}

// ---------------------------------------------------------------------------
// `combine_cursor_sync_results` — every variant of the (bubbles, api) pair.
// ---------------------------------------------------------------------------

#[test]
fn combine_results_both_none_returns_none() {
    assert!(combine_cursor_sync_results(None, None).is_none());
}

#[test]
fn combine_results_bubbles_only() {
    let r = combine_cursor_sync_results(Some(Ok((1, 2, vec!["w".to_string()]))), None);
    let (a, c, w) = r.unwrap().unwrap();
    assert_eq!((a, c), (1, 2));
    assert_eq!(w, vec!["w".to_string()]);
}

#[test]
fn combine_results_api_only() {
    let r = combine_cursor_sync_results(None, Some(Ok((3, 4, vec!["x".to_string()]))));
    let (a, c, _) = r.unwrap().unwrap();
    assert_eq!((a, c), (3, 4));
}

#[test]
fn combine_results_sums_when_both_ok() {
    let r = combine_cursor_sync_results(
        Some(Ok((1, 10, vec!["b".to_string()]))),
        Some(Ok((2, 20, vec!["a".to_string()]))),
    );
    let (a, c, w) = r.unwrap().unwrap();
    assert_eq!((a, c), (3, 30));
    assert_eq!(w, vec!["b".to_string(), "a".to_string()]);
}

#[test]
fn combine_results_propagates_bubbles_error() {
    let err = anyhow::anyhow!("bubbles failed");
    let r = combine_cursor_sync_results(Some(Err(err)), Some(Ok((1, 1, Vec::new()))));
    assert!(r.unwrap().is_err());
}

#[test]
fn combine_results_propagates_api_error() {
    let err = anyhow::anyhow!("api failed");
    let r = combine_cursor_sync_results(Some(Ok((1, 1, Vec::new()))), Some(Err(err)));
    assert!(r.unwrap().is_err());
}

// ---------------------------------------------------------------------------
// `parse_cursor_line` extended coverage — entry_type, blocks, alt fields.
// ---------------------------------------------------------------------------

#[test]
fn parse_cursor_line_uses_entry_type_when_role_absent() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(msg.role, "assistant");
}

#[test]
fn parse_cursor_line_accepts_human_role() {
    let line = r#"{"role":"human","message":{"content":"fix it"}}"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(msg.role, "user");
}

#[test]
fn parse_cursor_line_accepts_ai_role() {
    let line = r#"{"role":"ai","model":"gpt-4"}"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(msg.role, "assistant");
    assert_eq!(msg.model.as_deref(), Some("gpt-4"));
}

#[test]
fn parse_cursor_line_rejects_unknown_role() {
    let line = r#"{"role":"observer"}"#;
    assert!(parse_cursor_line(line, 0, "s", None, Utc::now()).is_none());
}

#[test]
fn parse_cursor_line_parses_iso_and_epoch_ms_timestamps() {
    // ISO 8601 — exact epoch verified independently.
    let iso = r#"{"role":"user","timestamp":"2026-04-22T10:00:00Z"}"#;
    let m = parse_cursor_line(iso, 0, "s", None, Utc::now()).unwrap();
    let expected = "2026-04-22T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
    assert_eq!(m.timestamp, expected);
    // Unix millis (string form is what the parser accepts).
    let ms = r#"{"role":"user","timestamp":"1774455909363"}"#;
    let m = parse_cursor_line(ms, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(m.timestamp.timestamp_millis(), 1_774_455_909_363);
    // Bad timestamp → falls back.
    let fallback_ts = Utc::now();
    let bad = r#"{"role":"user","timestamp":"not-a-timestamp"}"#;
    let m = parse_cursor_line(bad, 0, "s", None, fallback_ts).unwrap();
    assert_eq!(
        m.timestamp.timestamp_millis(),
        fallback_ts.timestamp_millis()
    );
}

#[test]
fn parse_cursor_line_uses_request_id_as_uuid_when_explicit_uuid_missing() {
    let line = r#"{"role":"assistant","requestId":"req-xyz"}"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(msg.uuid, "req-xyz");
    assert_eq!(msg.request_id.as_deref(), Some("req-xyz"));
}

#[test]
fn parse_cursor_line_uses_entry_cwd_over_fallback() {
    let line = r#"{"role":"user","cwd":"/from/entry"}"#;
    let msg = parse_cursor_line(line, 0, "s", Some("/from/path"), Utc::now()).unwrap();
    assert_eq!(msg.cwd.as_deref(), Some("/from/entry"));
}

#[test]
fn parse_cursor_line_assistant_reads_cache_usage_alt_keys() {
    // Some Cursor JSONLs use snake_case `cache_*_input_tokens` instead
    // of camelCase. Both must populate the same field.
    let line = r#"{
        "role":"assistant",
        "usage":{
            "input_tokens":10,
            "output_tokens":5,
            "cache_creation_input_tokens":40,
            "cache_read_input_tokens":80
        }
    }"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(msg.cache_creation_tokens, 40);
    assert_eq!(msg.cache_read_tokens, 80);
}

#[test]
fn parse_cursor_line_collects_tool_names_and_dedups() {
    let line = r#"{
        "role":"assistant",
        "toolCalls":[
            {"name":"edit_file","arguments":{"file_path":"/x/y.rs"}},
            {"name":"edit_file","input":{"file_path":"/x/z.rs"}},
            {"name":"read","arguments":{"path":"/a/b.rs"}}
        ]
    }"#;
    let msg = parse_cursor_line(line, 0, "s", None, Utc::now()).unwrap();
    assert_eq!(
        msg.tool_names,
        vec!["edit_file".to_string(), "read".to_string()]
    );
}

#[test]
fn parse_cursor_line_skips_malformed_json() {
    let line = r#"{not valid json"#;
    assert!(parse_cursor_line(line, 0, "s", None, Utc::now()).is_none());
}

#[test]
fn parse_cursor_transcript_handles_partial_trailing_line() {
    // Last line without `\n` is treated as not-yet-flushed — leave it.
    let content = concat!(
        r#"{"role":"user","message":{"content":"a"}}"#,
        "\n",
        r#"{"role":"assistant","model":"gpt-4""#, // missing closing brace + newline
    );
    let (msgs, offset) = parse_cursor_transcript(content, 0, "s", None, Utc::now());
    assert_eq!(msgs.len(), 1);
    // Offset advanced only through the complete first line.
    assert!(offset < content.len());
}

#[test]
fn parse_cursor_transcript_resume_advances_line_index() {
    // After resuming from a non-zero offset, the line index continues
    // from the previously parsed lines so deterministic UUIDs stay
    // distinct.
    let content = concat!(
        r#"{"role":"user","message":{"content":"hello"}}"#,
        "\n",
        r#"{"role":"assistant","message":{"content":"hi"}}"#,
        "\n",
    );
    let (first, mid) = parse_cursor_transcript(content, 0, "s", None, Utc::now());
    assert_eq!(first.len(), 2);

    let extra = format!(
        "{content}{}",
        r#"{"role":"assistant","message":{"content":"again"}}"#.to_string() + "\n"
    );
    let (second, end) = parse_cursor_transcript(&extra, mid, "s", None, Utc::now());
    assert_eq!(second.len(), 1);
    assert_eq!(end, extra.len());
}

// ---------------------------------------------------------------------------
// `cwd_from_path` + `collect_cursor_transcripts` + `file_mtime`.
// ---------------------------------------------------------------------------

#[test]
fn cwd_from_path_reads_workspace_from_worker_log() {
    let root = make_test_dir("cursor-cwd-from-path");
    let project = root.join("projects/slug");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("worker.log"),
        "[info] workspacePath=/Users/me/repo\n",
    )
    .unwrap();
    let transcripts = project.join("agent-transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let file_path = transcripts.join("abc.jsonl");
    std::fs::write(&file_path, "").unwrap();

    assert_eq!(cwd_from_path(&file_path).as_deref(), Some("/Users/me/repo"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cwd_from_path_returns_none_when_no_agent_transcripts_segment() {
    let p = Path::new("/some/random/path/file.jsonl");
    assert!(cwd_from_path(p).is_none());
}

#[test]
fn collect_cursor_transcripts_walks_flat_and_nested() {
    let root = make_test_dir("cursor-collect-transcripts");
    let projects = root.join("projects");
    let flat = projects.join("proj1/agent-transcripts");
    let nested = projects.join("proj2/agent-transcripts/session-uuid");
    std::fs::create_dir_all(&flat).unwrap();
    std::fs::create_dir_all(&nested).unwrap();

    let flat_jsonl = flat.join("flat-session.jsonl");
    let nested_jsonl = nested.join("nested-session.jsonl");
    std::fs::write(&flat_jsonl, "").unwrap();
    std::fs::write(&nested_jsonl, "").unwrap();
    // A non-jsonl sibling must be skipped.
    std::fs::write(flat.join("ignore.txt"), "x").unwrap();

    let mut files = Vec::new();
    collect_cursor_transcripts(&projects, &mut files);
    assert!(files.contains(&flat_jsonl), "missing flat: {files:?}");
    assert!(files.contains(&nested_jsonl), "missing nested: {files:?}");
    assert_eq!(files.len(), 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn collect_cursor_transcripts_missing_root_is_safe() {
    let mut files = Vec::new();
    collect_cursor_transcripts(Path::new("/nonexistent/path"), &mut files);
    assert!(files.is_empty());
}

#[test]
fn file_mtime_returns_some_time_for_existing_file() {
    let dir = make_test_dir("cursor-file-mtime");
    let f = dir.join("a.txt");
    std::fs::write(&f, "x").unwrap();
    // Just ensure it doesn't panic and returns a value close-to-now.
    let mt = file_mtime(&f);
    let drift = (Utc::now().timestamp() - mt.timestamp()).abs();
    assert!(drift < 60, "mtime drift too large: {drift}s");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_mtime_falls_back_to_now_for_missing_file() {
    // No file at this path — function must not panic, should land at "now".
    let mt = file_mtime(Path::new("/nonexistent/cursor/file.jsonl"));
    let drift = (Utc::now().timestamp() - mt.timestamp()).abs();
    assert!(drift < 60);
}

// ---------------------------------------------------------------------------
// Provider trait surface — `parse_file` + `discover_files`.
// ---------------------------------------------------------------------------

#[test]
fn provider_parse_file_delegates_to_transcript_parser() {
    let dir = make_test_dir("cursor-provider-parse-file");
    let project = dir.join("projects/p1");
    let transcripts = project.join("agent-transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(project.join("worker.log"), "workspacePath=/work\n").unwrap();
    let file = transcripts.join("s-1.jsonl");
    let content = concat!(r#"{"role":"user","message":{"content":"hi"}}"#, "\n",);
    std::fs::write(&file, content).unwrap();

    let provider = CursorProvider;
    let (msgs, _) = provider.parse_file(&file, content, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].provider, "cursor");
    assert_eq!(msgs[0].cwd.as_deref(), Some("/work"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provider_name_and_display_name() {
    let provider = CursorProvider;
    assert_eq!(provider.name(), "cursor");
    assert_eq!(provider.display_name(), "Cursor");
}

// ---------------------------------------------------------------------------
// `load_bubble_timestamp_fallbacks` — composer header fallback timestamps.
// ---------------------------------------------------------------------------

#[test]
fn load_bubble_timestamp_fallbacks_picks_last_updated_over_created() {
    let dir = make_test_dir("cursor-bubble-ts-fallback-last");
    let db = dir.join("state.vscdb");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    let payload = serde_json::json!({
        "allComposers": [
            {"composerId": "c1", "createdAt": 1000, "lastUpdatedAt": 5000},
            {"composerId": "c2", "createdAt": 2000},
            {"composerId": "  ", "createdAt": 3000},
            // Degenerate values are skipped.
            {"composerId": "c3", "createdAt": 0, "lastUpdatedAt": 0},
        ]
    });
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerHeaders', ?1)",
        params![payload.to_string()],
    )
    .unwrap();

    let map = load_bubble_timestamp_fallbacks(&conn);
    assert_eq!(map.get("c1"), Some(&5000));
    assert_eq!(map.get("c2"), Some(&2000));
    assert!(!map.contains_key("c3"));
    assert!(!map.contains_key("  "));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_bubble_timestamp_fallbacks_returns_empty_when_row_absent() {
    let dir = make_test_dir("cursor-bubble-ts-fallback-empty");
    let db = dir.join("state.vscdb");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    assert!(load_bubble_timestamp_fallbacks(&conn).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_bubble_timestamp_fallbacks_returns_empty_for_invalid_json() {
    let dir = make_test_dir("cursor-bubble-ts-fallback-bad-json");
    let db = dir.join("state.vscdb");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
        .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerHeaders', '{ not json')",
        [],
    )
    .unwrap();
    assert!(load_bubble_timestamp_fallbacks(&conn).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// `read_cursor_bubbles` with watermark filter.
// ---------------------------------------------------------------------------

#[test]
fn read_cursor_bubbles_respects_since_ms_watermark() {
    let dir = make_test_dir("cursor-bubbles-since-ms");
    let db = dir.join("state.vscdb");
    let rows = [
        (
            bubble_key(FIXTURE_CONV_1, FIXTURE_BUBBLE_A),
            r#"{"tokenCount":{"inputTokens":100,"outputTokens":50},"modelInfo":{"modelName":"claude-sonnet-4-6"},"createdAt":1000000000000,"type":2}"#.to_string(),
        ),
        (
            bubble_key(FIXTURE_CONV_2, FIXTURE_BUBBLE_A),
            r#"{"tokenCount":{"inputTokens":200,"outputTokens":100},"modelInfo":{"modelName":"claude-sonnet-4-6"},"createdAt":2000000000000,"type":2}"#.to_string(),
        ),
    ];
    let row_refs: Vec<(&str, &str)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    seed_bubble_db(&db, &row_refs);

    let parsed = read_cursor_bubbles(&db, Some(1_500_000_000_000)).expect("read ok");
    assert_eq!(parsed.len(), 1, "watermark must drop the older row");
    assert_eq!(parsed[0].session_id.as_deref(), Some(FIXTURE_CONV_2));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Pagination loader — empty pages and short last page.
// ---------------------------------------------------------------------------

#[test]
fn fetch_loader_short_page_terminates() {
    // First page returns fewer than 100 events → no further pages requested.
    let page1: Vec<Value> = (1101..=1150).rev().map(usage_event_json).collect();
    let mut pages_requested = 0;
    let fetched = fetch_usage_events_with_page_loader(Some(0), false, |_page| {
        pages_requested += 1;
        Ok(page1.clone())
    })
    .unwrap();
    assert_eq!(pages_requested, 1);
    assert_eq!(fetched.events.len(), 50);
}

#[test]
fn fetch_loader_empty_first_page_returns_empty() {
    let fetched =
        fetch_usage_events_with_page_loader(None, false, |_| Ok::<Vec<Value>, _>(Vec::new()))
            .unwrap();
    assert!(fetched.events.is_empty());
    assert_eq!(fetched.pages_fetched, 0);
}

#[test]
fn fetch_loader_propagates_loader_error() {
    let result = fetch_usage_events_with_page_loader(None, true, |_| -> Result<Vec<Value>> {
        Err(anyhow::anyhow!("network blew up"))
    });
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// `load_session_contexts` against an in-memory SQLite — exercises the
// SQL query, the 30-day filter, and the timestamp parsing branch.
// ---------------------------------------------------------------------------

fn open_test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    crate::migration::migrate(&conn).unwrap();
    conn
}

#[allow(clippy::too_many_arguments)]
fn insert_session(
    conn: &Connection,
    id: &str,
    provider: &str,
    started_at: &str,
    ended_at: Option<&str>,
    workspace_root: Option<&str>,
    repo_id: Option<&str>,
    git_branch: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, workspace_root, repo_id, git_branch, surface)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'cursor')",
        params![id, provider, started_at, ended_at, workspace_root, repo_id, git_branch],
    )
    .unwrap();
}

#[test]
fn load_session_contexts_returns_only_cursor_provider() {
    let conn = open_test_db();
    let now = Utc::now();
    let recent = now.to_rfc3339();
    insert_session(
        &conn,
        "c1",
        "cursor",
        &recent,
        None,
        Some("/proj"),
        Some("repo"),
        Some("main"),
    );
    insert_session(&conn, "cc1", "claude_code", &recent, None, None, None, None);

    let ctxs = load_session_contexts(&conn);
    assert!(ctxs.iter().any(|s| s.session_id == "c1"));
    assert!(!ctxs.iter().any(|s| s.session_id == "cc1"));
}

#[test]
fn load_session_contexts_drops_sessions_older_than_30_days() {
    let conn = open_test_db();
    let now = Utc::now();
    let recent = now.to_rfc3339();
    let old = (now - chrono::Duration::days(45)).to_rfc3339();
    insert_session(&conn, "recent", "cursor", &recent, None, None, None, None);
    insert_session(&conn, "old", "cursor", &old, None, None, None, None);

    let ctxs = load_session_contexts(&conn);
    assert!(ctxs.iter().any(|s| s.session_id == "recent"));
    assert!(
        !ctxs.iter().any(|s| s.session_id == "old"),
        "stale session must be filtered: {ctxs:?}",
        ctxs = ctxs.iter().map(|c| &c.session_id).collect::<Vec<_>>(),
    );
}

#[test]
fn load_session_contexts_parses_started_and_ended() {
    let conn = open_test_db();
    let now = Utc::now();
    let started = now.to_rfc3339();
    let ended = (now + chrono::Duration::hours(1)).to_rfc3339();
    insert_session(
        &conn,
        "with-end",
        "cursor",
        &started,
        Some(&ended),
        None,
        None,
        None,
    );
    insert_session(&conn, "open", "cursor", &started, None, None, None, None);

    let ctxs = load_session_contexts(&conn);
    let with_end = ctxs.iter().find(|s| s.session_id == "with-end").unwrap();
    assert!(with_end.end_ms > with_end.start_ms);
    let open = ctxs.iter().find(|s| s.session_id == "open").unwrap();
    assert_eq!(open.end_ms, i64::MAX);
}

// ---------------------------------------------------------------------------
// `backfill_cursor_session_ids` — orphan adoption via timestamp window.
// ---------------------------------------------------------------------------

fn insert_message(
    conn: &Connection,
    id: &str,
    provider: &str,
    role: &str,
    timestamp: &str,
    session_id: Option<&str>,
) {
    conn.execute(
        "INSERT INTO messages (id, provider, role, timestamp, session_id, surface)
         VALUES (?1, ?2, ?3, ?4, ?5, 'cursor')",
        params![id, provider, role, timestamp, session_id],
    )
    .unwrap();
}

#[test]
fn backfill_cursor_session_ids_assigns_matching_session() {
    let mut conn = open_test_db();
    let now = Utc::now();
    let ts = now.to_rfc3339();
    insert_message(&conn, "orphan-1", "cursor", "assistant", &ts, None);

    let sessions = vec![SessionContext {
        start_ms: now.timestamp_millis() - 1000,
        end_ms: now.timestamp_millis() + 1000,
        session_id: "matched".to_string(),
        workspace_root: Some("/work".to_string()),
        repo_id: Some("acme/repo".to_string()),
        git_branch: Some("main".to_string()),
    }];

    let updated = backfill_cursor_session_ids(&mut conn, &sessions);
    assert_eq!(updated, 1);

    let (sid, cwd, repo, branch): (String, Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT session_id, cwd, repo_id, git_branch FROM messages WHERE id = 'orphan-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(sid, "matched");
    assert_eq!(cwd.as_deref(), Some("/work"));
    assert_eq!(repo.as_deref(), Some("acme/repo"));
    assert_eq!(branch.as_deref(), Some("main"));
}

#[test]
fn backfill_cursor_session_ids_zero_when_no_orphans() {
    let mut conn = open_test_db();
    let updated = backfill_cursor_session_ids(&mut conn, &[]);
    assert_eq!(updated, 0);
}

#[test]
fn backfill_cursor_session_ids_skips_when_no_matching_session() {
    let mut conn = open_test_db();
    let now = Utc::now();
    let ts = now.to_rfc3339();
    insert_message(&conn, "orphan-far", "cursor", "assistant", &ts, None);

    // Session window is months away from the orphan's timestamp.
    let far = now - chrono::Duration::days(40);
    let sessions = vec![SessionContext {
        start_ms: far.timestamp_millis(),
        end_ms: far.timestamp_millis() + 1000,
        session_id: "stale".to_string(),
        workspace_root: None,
        repo_id: None,
        git_branch: None,
    }];

    let updated = backfill_cursor_session_ids(&mut conn, &sessions);
    assert_eq!(updated, 0);
}

// ---------------------------------------------------------------------------
// `repair_cursor_workspace_metadata` — replaces legacy `~/.cursor/projects/<slug>`
// cwd with a real workspace path discovered in worker.log.
// ---------------------------------------------------------------------------

// Windows: the repair filter is `cwd LIKE '%/.cursor/projects/%'` (forward
// slashes), but `PathBuf::join` on Windows yields backslashes for the temp-dir
// prefix, so the LIKE never fires. The production code path is the same on
// both platforms; the test fixture is what's Unix-shaped.
#[cfg(not(windows))]
#[test]
fn repair_cursor_workspace_metadata_upgrades_legacy_cwd() {
    let dir = make_test_dir("cursor-repair-workspace");
    let project_dir = dir.join(".cursor/projects/slug-abc");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("worker.log"),
        "[info] workspacePath=/Users/me/real-repo\n",
    )
    .unwrap();

    let mut conn = open_test_db();
    let legacy_cwd = project_dir.to_string_lossy().to_string();
    let now = Utc::now().to_rfc3339();

    // Seed: session with empty workspace_root + message with the legacy cwd.
    // workspace_root left empty so repair fills it via the COALESCE(NULLIF…) path.
    insert_session(
        &conn,
        "sess-x",
        "cursor",
        &now,
        None,
        None,
        Some("unknown"),
        None,
    );
    conn.execute(
        "INSERT INTO messages (id, provider, role, timestamp, session_id, cwd, surface)
         VALUES ('m-x', 'cursor', 'assistant', ?1, 'sess-x', ?2, 'cursor')",
        params![now, legacy_cwd],
    )
    .unwrap();

    repair_cursor_workspace_metadata(&mut conn);

    let new_cwd: String = conn
        .query_row("SELECT cwd FROM messages WHERE id = 'm-x'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(new_cwd, "/Users/me/real-repo");

    let session_cwd: Option<String> = conn
        .query_row(
            "SELECT workspace_root FROM sessions WHERE id = 'sess-x'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_cwd.as_deref(), Some("/Users/me/real-repo"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repair_cursor_workspace_metadata_noop_without_legacy_rows() {
    let mut conn = open_test_db();
    // No-op should not panic.
    repair_cursor_workspace_metadata(&mut conn);
}

// ---------------------------------------------------------------------------
// `deterministic_*_uuid` — UUID-shape stability across re-syncs.
// ---------------------------------------------------------------------------

#[test]
fn deterministic_cursor_message_uuid_is_stable_and_uuid_shaped() {
    let a = deterministic_cursor_message_uuid("session", 3, "line content");
    let b = deterministic_cursor_message_uuid("session", 3, "line content");
    assert_eq!(a, b);
    assert!(looks_like_uuid(&a), "expected UUID shape, got {a}");
    let differ = deterministic_cursor_message_uuid("session", 4, "line content");
    assert_ne!(a, differ);
}

#[test]
fn deterministic_cursor_usage_uuid_is_stable_and_uuid_shaped() {
    let ev = CursorUsageEvent {
        timestamp_ms: 12345,
        model: "claude".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        total_cents: Some(0.1),
    };
    let a = deterministic_cursor_usage_uuid(&ev);
    let b = deterministic_cursor_usage_uuid(&ev);
    assert_eq!(a, b);
    assert!(looks_like_uuid(&a));

    let ev2 = CursorUsageEvent {
        timestamp_ms: 99999,
        ..CursorUsageEvent {
            timestamp_ms: 12345,
            model: "claude".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_cents: Some(0.1),
        }
    };
    assert_ne!(a, deterministic_cursor_usage_uuid(&ev2));
}
