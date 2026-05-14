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
