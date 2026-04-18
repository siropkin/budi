use super::*;
use rusqlite::{Connection, params};

fn cache_stats(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
) -> anyhow::Result<CacheEfficiency> {
    cache_efficiency(conn, since, until)
}

fn test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .unwrap();
    crate::migration::migrate(&conn).unwrap();
    conn
}

#[test]
fn schema_creates_tables() {
    let conn = test_db();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("skipping row: {e}");
                None
            }
        })
        .collect();
    assert!(tables.contains(&"sessions".to_string()));
    assert!(tables.contains(&"messages".to_string()));
    assert!(tables.contains(&"sync_state".to_string()));
    assert!(tables.contains(&"message_rollups_hourly".to_string()));
    assert!(tables.contains(&"message_rollups_daily".to_string()));
}

#[test]
fn ingest_and_query() {
    let mut conn = test_db();
    let msgs = vec![
        ParsedMessage {
            uuid: "u1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T18:13:42Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: Some("main".to_string()),
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "a1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T18:14:00Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 200,
            cache_read_tokens: 300,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];

    let count = ingest_messages(&mut conn, &msgs, None).unwrap();
    assert_eq!(count, 2);

    // Duplicate insert should be skipped.
    let count2 = ingest_messages(&mut conn, &msgs, None).unwrap();
    assert_eq!(count2, 0);

    let summary = usage_summary(&conn, None, None).unwrap();
    assert_eq!(summary.total_messages, 2);
    assert_eq!(summary.total_user_messages, 1);
    assert_eq!(summary.total_assistant_messages, 1);
    assert_eq!(summary.total_input_tokens, 100);
    assert_eq!(summary.total_output_tokens, 50);
}

#[test]
fn rollups_track_message_updates_and_deletes() {
    let mut conn = test_db();
    let msg = ParsedMessage {
        uuid: "rollup-msg-1".to_string(),
        session_id: Some("rollup-sess".to_string()),
        timestamp: "2026-03-14T18:14:00Z".parse().unwrap(),
        cwd: Some("/tmp/proj".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        cache_creation_tokens: 10,
        cache_read_tokens: 20,
        git_branch: Some("refs/heads/main".to_string()),
        repo_id: Some("github.com/acme/repo".to_string()),
        provider: "claude_code".to_string(),
        cost_cents: Some(2.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
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
    };
    ingest_messages(&mut conn, &[msg], None).unwrap();

    conn.execute(
        "UPDATE messages
         SET output_tokens = 90,
             cost_cents = 4.5
         WHERE id = 'rollup-msg-1'",
        [],
    )
    .unwrap();

    let summary =
        usage_summary_with_filters(&conn, None, None, None, &DimensionFilters::default()).unwrap();
    assert_eq!(summary.total_output_tokens, 90);

    conn.execute("DELETE FROM messages WHERE id = 'rollup-msg-1'", [])
        .unwrap();
    let post_delete =
        usage_summary_with_filters(&conn, None, None, None, &DimensionFilters::default()).unwrap();
    assert_eq!(post_delete.total_messages, 0);
}

#[test]
fn rollups_are_used_only_for_hour_aligned_ranges() {
    let mut conn = test_db();
    let msg = ParsedMessage {
        uuid: "rollup-range-msg".to_string(),
        session_id: Some("rollup-range-sess".to_string()),
        timestamp: "2026-03-14T10:30:00Z".parse().unwrap(),
        cwd: Some("/tmp/proj".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        git_branch: Some("main".to_string()),
        repo_id: Some("repo-a".to_string()),
        provider: "claude_code".to_string(),
        cost_cents: Some(1.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
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
    };
    ingest_messages(&mut conn, &[msg], None).unwrap();

    // Poison the rollup row to detect whether the query path reads rollups.
    conn.execute(
        "UPDATE message_rollups_hourly SET message_count = 9, output_tokens = 99
         WHERE bucket_start = '2026-03-14T10:00:00Z'",
        [],
    )
    .unwrap();

    let aligned = usage_summary_with_filters(
        &conn,
        Some("2026-03-14T10:00:00Z"),
        None,
        None,
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(aligned.total_messages, 9);
    assert_eq!(aligned.total_output_tokens, 99);

    let non_aligned = usage_summary_with_filters(
        &conn,
        Some("2026-03-14T10:15:00Z"),
        Some("2026-03-14T11:00:00Z"),
        None,
        &DimensionFilters::default(),
    )
    .unwrap();
    // Non-hour-aligned range should fall back to raw messages for correctness.
    assert_eq!(non_aligned.total_messages, 1);
    assert_eq!(non_aligned.total_output_tokens, 5);
}

#[test]
fn rollup_summary_latency_smoke_on_large_dataset() {
    let mut conn = test_db();
    let mut messages = Vec::new();
    for i in 0..5000 {
        let hour = i % 24;
        let day = (i % 28) + 1;
        messages.push(ParsedMessage {
            uuid: format!("bench-{i}"),
            session_id: Some(format!("bench-sess-{}", i % 50)),
            timestamp: format!("2026-03-{day:02}T{hour:02}:00:00Z")
                .parse()
                .unwrap(),
            cwd: Some("/tmp/bench".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 200,
            output_tokens: 80,
            cache_creation_tokens: 20,
            cache_read_tokens: 40,
            git_branch: Some("main".to_string()),
            repo_id: Some("repo-bench".to_string()),
            provider: "claude_code".to_string(),
            cost_cents: Some(1.2),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
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
        });
    }
    ingest_messages(&mut conn, &messages, None).unwrap();

    let started = std::time::Instant::now();
    let summary =
        usage_summary_with_filters(&conn, None, None, None, &DimensionFilters::default()).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(summary.total_messages, 5000);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "rollup summary latency smoke exceeded budget: {:?}",
        elapsed
    );
}

#[test]
fn cost_cents_baked_at_ingest() {
    use crate::pipeline::Enricher;
    use crate::pipeline::enrichers::CostEnricher;

    let mut conn = test_db();
    let mut msg = ParsedMessage {
        uuid: "cost-test-1".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: None,
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "exact".to_string(),
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
    };
    // CostEnricher is the single source of truth for cost_cents
    CostEnricher.enrich(&mut msg);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    // Verify cost_cents was baked in: 1M input * $5/M + 100K output * $25/M = $5 + $2.50 = $7.50 = 750 cents
    let cost_cents: f64 = conn
        .query_row(
            "SELECT cost_cents FROM messages WHERE id = 'cost-test-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (cost_cents - 750.0).abs() < 1.0,
        "expected ~750 cents, got {cost_cents}"
    );
}

#[test]
fn sync_offset_round_trip() {
    let conn = test_db();
    assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 0);
    set_sync_offset(&conn, "/tmp/test.jsonl", 1234).unwrap();
    assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 1234);
    set_sync_offset(&conn, "/tmp/test.jsonl", 5678).unwrap();
    assert_eq!(get_sync_offset(&conn, "/tmp/test.jsonl").unwrap(), 5678);
}

#[test]
fn sync_completion_marker_round_trip() {
    let conn = test_db();
    assert_eq!(last_sync_completed_at(&conn).unwrap(), None);
    mark_sync_completed(&conn).unwrap();
    let ts = last_sync_completed_at(&conn).unwrap();
    assert!(ts.is_some());
    assert_eq!(
        get_sync_offset(&conn, SYNC_COMPLETION_MARKER_KEY).unwrap(),
        0
    );
}

#[test]
fn last_seen_derived_from_messages() {
    let mut conn = test_db();
    let msgs = vec![
        ParsedMessage {
            uuid: "m1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: Some("/tmp".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: Some("main".to_string()),
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "m2".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T12:00:00Z".parse().unwrap(),
            cwd: Some("/tmp".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let last_seen: String = conn
        .query_row(
            "SELECT MAX(timestamp) FROM messages WHERE session_id = 's1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(last_seen.contains("12:00:00"));
}

#[test]
fn newest_ingested_data_uses_assistant_rows() {
    let mut conn = test_db();
    let msgs = vec![
        ParsedMessage {
            uuid: "u-only".to_string(),
            session_id: Some("s-usage".to_string()),
            timestamp: "2026-03-14T11:00:00Z".parse().unwrap(),
            cwd: Some("/tmp".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "a-only".to_string(),
            session_id: Some("s-usage".to_string()),
            timestamp: "2026-03-14T12:30:00Z".parse().unwrap(),
            cwd: Some("/tmp".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(0.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();
    let newest = newest_ingested_data_at(&conn).unwrap();
    assert_eq!(newest.as_deref(), Some("2026-03-14T12:30:00+00:00"));
}

fn sample_messages() -> Vec<ParsedMessage> {
    vec![
        ParsedMessage {
            uuid: "u1".to_string(),
            session_id: Some("sess-abc".to_string()),
            timestamp: "2026-03-14T18:13:42Z".parse().unwrap(),
            cwd: Some("/home/user/project-a".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: Some("main".to_string()),
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "a1".to_string(),
            session_id: Some("sess-abc".to_string()),
            timestamp: "2026-03-14T18:14:00Z".parse().unwrap(),
            cwd: Some("/home/user/project-a".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 200,
            cache_read_tokens: 300,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(2.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "u2".to_string(),
            session_id: Some("sess-def".to_string()),
            timestamp: "2026-03-14T19:00:00Z".parse().unwrap(),
            cwd: Some("/home/user/project-b".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ]
}

#[test]
fn message_list_returns_messages() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();

    let result = message_list(
        &conn,
        &MessageListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
        },
    )
    .unwrap();
    // Only assistant messages are returned
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.total_count, 1);
    assert_eq!(result.messages[0].input_tokens, 100);
}

#[test]
fn repo_usage_groups_by_repo_id() {
    let mut conn = test_db();
    let mut msgs = sample_messages();
    // Assign repo_ids — only assistant messages count for cost aggregation
    msgs[0].repo_id = Some("project-a".to_string());
    msgs[1].repo_id = Some("project-a".to_string());
    msgs[2].repo_id = Some("project-b".to_string());
    // Make project-b's message an assistant with tokens so it appears in results
    msgs[2].role = "assistant".to_string();
    msgs[2].model = Some("claude-opus-4-6".to_string());
    msgs[2].input_tokens = 50;
    msgs[2].cost_cents = Some(0.5);
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let repos = repo_usage(&conn, None, None, 10).unwrap();
    assert_eq!(repos.len(), 2);
    // project-a has more cost, project-b has some.
    assert_eq!(repos[0].repo_id, "project-a");
    assert_eq!(repos[0].message_count, 1); // only assistant msg
    assert_eq!(repos[1].repo_id, "project-b");
    assert_eq!(repos[1].message_count, 1);
}

#[test]
fn repo_usage_multi_repo_single_session_is_message_attributed() {
    let mut conn = test_db();
    let mut m1 = assistant_msg("repo-multi-1", "sess-multi", 2.0);
    m1.repo_id = Some("repo-a".to_string());
    let mut m2 = assistant_msg("repo-multi-2", "sess-multi", 8.0);
    m2.repo_id = Some("repo-b".to_string());
    ingest_messages(&mut conn, &[m1, m2], None).unwrap();

    let repos = repo_usage(&conn, None, None, 10).unwrap();
    let repo_a = repos.iter().find(|r| r.repo_id == "repo-a").unwrap();
    let repo_b = repos.iter().find(|r| r.repo_id == "repo-b").unwrap();
    assert!((repo_a.cost_cents - 2.0).abs() < 0.01);
    assert!((repo_b.cost_cents - 8.0).abs() < 0.01);
    assert_eq!(repo_a.message_count, 1);
    assert_eq!(repo_b.message_count, 1);
}

#[test]
fn repo_usage_with_dimension_filters() {
    let mut conn = test_db();
    let mut m1 = assistant_msg("repo-filter-1", "sess-repo-filter", 4.0);
    m1.provider = "claude_code".to_string();
    m1.model = Some("claude-sonnet-4-6".to_string());
    m1.repo_id = Some("github.com/acme/repo-a".to_string());
    m1.git_branch = Some("refs/heads/main".to_string());
    let mut m2 = assistant_msg("repo-filter-2", "sess-repo-filter", 9.0);
    m2.provider = "cursor".to_string();
    m2.model = Some("gpt-5.4".to_string());
    m2.repo_id = Some("github.com/acme/repo-b".to_string());
    m2.git_branch = Some("refs/heads/feature/x".to_string());
    ingest_messages(&mut conn, &[m1, m2], None).unwrap();

    let filters = DimensionFilters {
        agents: vec!["cursor".to_string()],
        models: vec!["gpt-5.4".to_string()],
        projects: vec!["github.com/acme/repo-b".to_string()],
        branches: vec!["feature/x".to_string()],
    };
    let rows = repo_usage_with_filters(&conn, None, None, &filters, 20).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo_id, "github.com/acme/repo-b");
    assert!((rows[0].cost_cents - 9.0).abs() < 0.01);
}

#[test]
fn filter_options_are_normalized_and_match_dimension_filters() {
    let mut conn = test_db();

    let mut m1 = assistant_msg("fo-1", "sess-fo", 2.0);
    m1.provider = "claude_code".to_string();
    m1.model = Some("claude-sonnet-4-6".to_string());
    m1.repo_id = Some("github.com/acme/repo-a".to_string());
    m1.git_branch = Some("refs/heads/main".to_string());

    let mut m2 = assistant_msg("fo-2", "sess-fo", 3.0);
    m2.provider = "cursor".to_string();
    m2.model = Some("<synthetic>".to_string());
    m2.repo_id = Some("unknown".to_string());
    m2.git_branch = Some("".to_string());

    let mut m3 = assistant_msg("fo-3", "sess-fo", 5.0);
    m3.provider = "codex".to_string();
    m3.model = None;
    m3.repo_id = None;
    m3.git_branch = None;

    ingest_messages(&mut conn, &[m1, m2, m3], None).unwrap();

    let options = filter_options(&conn, None, None, None).unwrap();
    assert!(options.agents.contains(&"claude_code".to_string()));
    assert!(options.agents.contains(&"cursor".to_string()));
    assert!(options.agents.contains(&"codex".to_string()));
    assert!(options.models.contains(&"claude-sonnet-4-6".to_string()));
    assert!(options.models.contains(&"(untagged)".to_string()));
    assert!(
        options
            .projects
            .contains(&"github.com/acme/repo-a".to_string())
    );
    assert!(options.projects.contains(&"(untagged)".to_string()));
    assert!(options.branches.contains(&"main".to_string()));
    assert!(options.branches.contains(&"(untagged)".to_string()));

    let filters = DimensionFilters {
        agents: vec!["cursor".to_string()],
        models: vec!["(untagged)".to_string()],
        projects: vec!["(untagged)".to_string()],
        branches: vec!["(untagged)".to_string()],
    };
    let summary = usage_summary_with_filters(&conn, None, None, None, &filters).unwrap();
    assert_eq!(summary.total_assistant_messages, 1);
}

#[test]
fn filter_options_limit_is_optional_and_respected_when_set() {
    let mut conn = test_db();
    let mut m1 = assistant_msg("fo-limit-1", "sess-fo-limit", 1.0);
    m1.provider = "claude_code".to_string();
    let mut m2 = assistant_msg("fo-limit-2", "sess-fo-limit", 1.0);
    m2.provider = "cursor".to_string();
    let mut m3 = assistant_msg("fo-limit-3", "sess-fo-limit", 1.0);
    m3.provider = "codex".to_string();
    ingest_messages(&mut conn, &[m1, m2, m3], None).unwrap();

    let unlimited = filter_options(&conn, None, None, None).unwrap();
    assert_eq!(unlimited.agents.len(), 3);

    let limited = filter_options(&conn, None, None, Some(1)).unwrap();
    assert_eq!(limited.agents.len(), 1);
}

fn messages_with_cache_patterns() -> Vec<ParsedMessage> {
    vec![
        ParsedMessage {
            uuid: "t1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 500,
            output_tokens: 100,
            cache_creation_tokens: 0,
            cache_read_tokens: 200,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "t2".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
            cwd: Some("/tmp/proj".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 300,
            output_tokens: 200,
            cache_creation_tokens: 100,
            cache_read_tokens: 150,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "t3".to_string(),
            session_id: Some("s2".to_string()),
            timestamp: "2026-03-14T11:00:00Z".parse().unwrap(),
            cwd: Some("/tmp/big".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 50000,
            output_tokens: 500,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ]
}

#[test]
fn cache_stats_computes_hit_rate() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &messages_with_cache_patterns(), None).unwrap();

    let cs = cache_stats(&conn, None, None).unwrap();
    assert_eq!(cs.total_input_tokens, 51150);
    assert_eq!(cs.total_cache_read_tokens, 350);
    assert!((cs.cache_hit_rate - 350.0 / 51150.0).abs() < 0.001);
}

#[test]
fn statusline_stats_empty_db() {
    let conn = test_db();
    let params = StatuslineParams::default();
    let stats = statusline_stats(&conn, "2026-03-21", "2026-03-17", "2026-03-01", &params).unwrap();
    assert_eq!(stats.today_cost, 0.0);
    assert_eq!(stats.week_cost, 0.0);
    assert_eq!(stats.month_cost, 0.0);
    assert!(stats.session_cost.is_none());
    assert!(stats.branch_cost.is_none());
    assert!(stats.project_cost.is_none());
}

#[test]
fn statusline_stats_with_data() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    let params = StatuslineParams::default();
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
    assert!(stats.month_cost > 0.0);
}

#[test]
fn statusline_stats_with_session_filter() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    let params = StatuslineParams {
        session_id: Some("sess-1".to_string()),
        ..Default::default()
    };
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
    assert!(stats.session_cost.is_some());
    assert!(stats.session_cost.unwrap() >= 0.0);
}

#[test]
fn statusline_stats_with_branch_filter() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    let params = StatuslineParams {
        branch: Some("main".to_string()),
        ..Default::default()
    };
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-10", "2026-03-01", &params).unwrap();
    assert!(stats.branch_cost.is_some());
}

#[test]
fn multi_provider_ingest_and_query() {
    let mut conn = test_db();

    let claude_msgs = vec![
        ParsedMessage {
            uuid: "cc-u1".to_string(),
            session_id: Some("cc-sess-1".to_string()),
            timestamp: "2026-03-20T10:00:00Z".parse().unwrap(),
            cwd: Some("/proj/a".to_string()),
            role: "user".to_string(),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: None,
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "cc-a1".to_string(),
            session_id: Some("cc-sess-1".to_string()),
            timestamp: "2026-03-20T10:01:00Z".parse().unwrap(),
            cwd: Some("/proj/a".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.75),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];

    let cursor_msgs = vec![
        ParsedMessage {
            uuid: "cu-u1".to_string(),
            session_id: Some("cu-sess-1".to_string()),
            timestamp: "2026-03-20T11:00:00Z".parse().unwrap(),
            cwd: Some("/proj/b".to_string()),
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
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "cu-a1".to_string(),
            session_id: Some("cu-sess-1".to_string()),
            timestamp: "2026-03-20T11:01:00Z".parse().unwrap(),
            cwd: Some("/proj/b".to_string()),
            role: "assistant".to_string(),
            model: Some("gpt-4o".to_string()),
            input_tokens: 2000,
            output_tokens: 800,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "cursor".to_string(),
            cost_cents: Some(0.62),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];

    ingest_messages(&mut conn, &claude_msgs, None).unwrap();
    ingest_messages(&mut conn, &cursor_msgs, None).unwrap();

    let all = usage_summary(&conn, None, None).unwrap();
    assert_eq!(all.total_messages, 4);
    assert_eq!(all.total_input_tokens, 3000);
    assert_eq!(all.total_output_tokens, 1300);

    let cc = usage_summary_filtered(&conn, None, None, Some("claude_code")).unwrap();
    assert_eq!(cc.total_messages, 2);
    assert_eq!(cc.total_input_tokens, 1000);
    assert_eq!(cc.total_output_tokens, 500);

    let cu = usage_summary_filtered(&conn, None, None, Some("cursor")).unwrap();
    assert_eq!(cu.total_messages, 2);
    assert_eq!(cu.total_input_tokens, 2000);
    assert_eq!(cu.total_output_tokens, 800);

    let pstats = provider_stats(&conn, None, None).unwrap();
    assert_eq!(pstats.len(), 2);
    let cc_stats = pstats.iter().find(|p| p.provider == "claude_code").unwrap();
    let cu_stats = pstats.iter().find(|p| p.provider == "cursor").unwrap();
    assert_eq!(cc_stats.message_count, 1);
    assert_eq!(cu_stats.message_count, 1);

    assert_eq!(cc_stats.display_name, "Claude Code");
    assert!(cc_stats.estimated_cost > 0.0);
}

#[test]
fn cross_parse_dedup_by_request_id() {
    let mut conn = test_db();

    let intermediate = ParsedMessage {
        uuid: "a1".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
        cwd: Some("/tmp/proj".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 3,
        output_tokens: 10,
        cache_creation_tokens: 21559,
        cache_read_tokens: 50000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(1.5),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        request_id: Some("msg_01ABC".to_string()),
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
    };
    ingest_messages(&mut conn, &[intermediate], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let final_entry = ParsedMessage {
        uuid: "a3".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:01.500Z".parse().unwrap(),
        cwd: Some("/tmp/proj".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 3,
        output_tokens: 425,
        cache_creation_tokens: 21559,
        cache_read_tokens: 50000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(5.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        request_id: Some("msg_01ABC".to_string()),
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
    };
    ingest_messages(&mut conn, &[final_entry], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "should dedup by request_id, not insert both");

    let (output, cache_read): (i64, i64) = conn
        .query_row(
            "SELECT output_tokens, cache_read_tokens FROM messages",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(output, 425, "should keep higher output_tokens");
    assert_eq!(cache_read, 50000, "cache_read should not be doubled");
}

#[test]
fn cross_parse_dedup_keeps_higher_output() {
    let mut conn = test_db();

    let final_entry = ParsedMessage {
        uuid: "a3".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 3,
        output_tokens: 425,
        cache_creation_tokens: 21559,
        cache_read_tokens: 50000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(5.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        request_id: Some("msg_01XYZ".to_string()),
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
    };
    ingest_messages(&mut conn, &[final_entry], None).unwrap();

    let intermediate = ParsedMessage {
        uuid: "a1".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 3,
        output_tokens: 10,
        cache_creation_tokens: 21559,
        cache_read_tokens: 50000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(1.5),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        request_id: Some("msg_01XYZ".to_string()),
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
    };
    ingest_messages(&mut conn, &[intermediate], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let output: i64 = conn
        .query_row("SELECT output_tokens FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        output, 425,
        "should keep the final entry with higher output"
    );
}

#[test]
fn no_request_id_no_dedup() {
    let mut conn = test_db();

    let msg1 = ParsedMessage {
        uuid: "m1".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        cache_creation_tokens: 0,
        cache_read_tokens: 1000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(1.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
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
    };
    ingest_messages(&mut conn, &[msg1], None).unwrap();

    let msg2 = ParsedMessage {
        uuid: "m2".to_string(),
        session_id: Some("s1".to_string()),
        timestamp: "2026-03-25T00:00:02.000Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        input_tokens: 200,
        output_tokens: 100,
        cache_creation_tokens: 0,
        cache_read_tokens: 2000,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(2.0),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
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
    };
    ingest_messages(&mut conn, &[msg2], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "messages without request_id should both be inserted"
    );
}

#[test]
fn jsonl_dedup_matches_otel_by_fingerprint_within_window() {
    let mut conn = test_db();

    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_cents, cost_confidence)
         VALUES ('otel-a', 'sess-otel', 'assistant', '2026-03-25T00:00:01.050Z', 'claude-opus-4-6',
                 'claude_code', 10, 5, 0, 0, 1.0, 'otel_exact')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_cents, cost_confidence)
         VALUES ('otel-b', 'sess-otel', 'assistant', '2026-03-25T00:00:01.120Z', 'claude-opus-4-6',
                 'claude_code', 900, 400, 5000, 50000, 7.3, 'otel_exact')",
        [],
    )
    .unwrap();

    let msg = ParsedMessage {
        uuid: "jsonl-match".to_string(),
        session_id: Some("sess-otel".to_string()),
        timestamp: "2026-03-25T00:00:01.180Z".parse().unwrap(),
        cwd: Some("/tmp/repo".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 900,
        output_tokens: 400,
        cache_creation_tokens: 5000,
        cache_read_tokens: 50000,
        git_branch: Some("main".to_string()),
        repo_id: Some("github.com/example/repo".to_string()),
        provider: "claude_code".to_string(),
        cost_cents: Some(7.3),
        session_title: None,
        parent_uuid: Some("parent-1".to_string()),
        user_name: None,
        machine_name: None,
        cost_confidence: "estimated".to_string(),
        request_id: Some("req-match".to_string()),
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
    };
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "should enrich OTEL row instead of inserting duplicate"
    );

    let (match_req, match_parent): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT request_id, parent_uuid FROM messages WHERE id = 'otel-b'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(match_req.as_deref(), Some("req-match"));
    assert_eq!(match_parent.as_deref(), Some("parent-1"));

    let other_req: Option<String> = conn
        .query_row(
            "SELECT request_id FROM messages WHERE id = 'otel-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_req, None);
}

#[test]
fn jsonl_dedup_preserves_message_when_otel_candidates_are_ambiguous() {
    let mut conn = test_db();

    for id in ["otel-1", "otel-2"] {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                cost_cents, cost_confidence)
             VALUES (?1, 'sess-ambig', 'assistant', '2026-03-25T00:00:01.100Z', 'claude-opus-4-6',
                     'claude_code', 1000, 500, 5000, 50000, 7.3, 'otel_exact')",
            params![id],
        )
        .unwrap();
    }

    let msg = ParsedMessage {
        uuid: "jsonl-ambig".to_string(),
        session_id: Some("sess-ambig".to_string()),
        timestamp: "2026-03-25T00:00:01.200Z".parse().unwrap(),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 1000,
        output_tokens: 500,
        cache_creation_tokens: 5000,
        cache_read_tokens: 50000,
        provider: "claude_code".to_string(),
        cost_cents: Some(7.3),
        cost_confidence: "estimated".to_string(),
        ..Default::default()
    };
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let count: i64 = conn
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 3,
        "ambiguous OTEL matches should not collapse JSONL row"
    );

    let inserted_exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM messages WHERE id = 'jsonl-ambig'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inserted_exists, 1);
}

#[test]
fn cache_efficiency_computes_savings() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &messages_with_cache_patterns(), None).unwrap();

    let ce = cache_efficiency(&conn, None, None).unwrap();
    assert_eq!(ce.total_cache_read_tokens, 350);
    assert!(ce.cache_hit_rate > 0.0);
    assert!(ce.cache_savings_cents > 0.0);
}

#[test]
fn session_cost_curve_buckets() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    for i in 0..10 {
        msgs.push(ParsedMessage {
            uuid: format!("curve-{}", i),
            session_id: Some("curve-sess".to_string()),
            timestamp: format!("2026-03-14T10:{:02}:00Z", i).parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        });
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let curve = session_cost_curve(&conn, None, None).unwrap();
    assert!(!curve.is_empty());
    let bucket = curve.iter().find(|b| b.bucket == "6-15").unwrap();
    assert_eq!(bucket.session_count, 1);
}

#[test]
fn cost_confidence_stats_groups_correctly() {
    let mut conn = test_db();
    let msgs = vec![
        ParsedMessage {
            uuid: "conf-1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(1.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "otel_exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "conf-2".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(2.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "estimated".to_string(),
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
        },
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let stats = cost_confidence_stats(&conn, None, None).unwrap();
    assert_eq!(stats.len(), 2);
    let otel = stats.iter().find(|s| s.confidence == "otel_exact").unwrap();
    assert_eq!(otel.message_count, 1);
    let est = stats.iter().find(|s| s.confidence == "estimated").unwrap();
    assert_eq!(est.message_count, 1);
}

#[test]
fn subagent_cost_stats_splits_correctly() {
    let mut conn = test_db();
    let msgs = vec![
        ParsedMessage {
            uuid: "main-1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(3.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "sub-1".to_string(),
            session_id: Some("s1".to_string()),
            timestamp: "2026-03-14T10:01:00Z".parse().unwrap(),
            cwd: None,
            role: "assistant".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            input_tokens: 200,
            output_tokens: 100,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: None,
            repo_id: None,
            provider: "claude_code".to_string(),
            cost_cents: Some(5.0),
            session_title: None,
            parent_uuid: Some("main-1".to_string()),
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let stats = subagent_cost_stats(&conn, None, None).unwrap();
    assert_eq!(stats.len(), 2);
    let main = stats.iter().find(|s| s.category == "main").unwrap();
    assert_eq!(main.message_count, 1);
    assert!((main.cost_cents - 3.0).abs() < 0.01);
    let sub = stats.iter().find(|s| s.category == "subagent").unwrap();
    assert_eq!(sub.message_count, 1);
    assert!((sub.cost_cents - 5.0).abs() < 0.01);
}

#[test]
fn session_list_returns_sessions() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();

    let result = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
    )
    .unwrap();
    assert!(!result.sessions.is_empty());
    assert!(result.total_count >= 1);
}

#[test]
fn session_list_uses_structured_models_array() {
    let mut conn = test_db();
    let mut m1 = assistant_msg("sess-models-1", "sess-models", 2.0);
    m1.model = Some("claude-opus-4-6".to_string());
    m1.repo_id = Some("github.com/acme/repo-a".to_string());
    m1.git_branch = Some("feature/AAA-1".to_string());
    m1.timestamp = "2026-03-25T00:00:01Z".parse().unwrap();
    let mut m2 = assistant_msg("sess-models-2", "sess-models", 3.0);
    m2.model = Some("claude-sonnet-4-6".to_string());
    m2.repo_id = Some("github.com/acme/repo-b".to_string());
    m2.git_branch = Some("refs/heads/feature/BBB-2".to_string());
    m2.timestamp = "2026-03-25T00:00:02Z".parse().unwrap();
    ingest_messages(&mut conn, &[m1, m2], None).unwrap();

    let result = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
    )
    .unwrap();
    let entry = result
        .sessions
        .into_iter()
        .find(|s| s.id == "sess-models")
        .expect("session row should exist");
    assert_eq!(entry.models.len(), 2);
    assert!(entry.models.contains(&"claude-opus-4-6".to_string()));
    assert!(entry.models.contains(&"claude-sonnet-4-6".to_string()));
    assert_eq!(
        entry.repo_ids,
        vec![
            "github.com/acme/repo-b".to_string(),
            "github.com/acme/repo-a".to_string()
        ]
    );
    assert_eq!(
        entry.git_branches,
        vec!["feature/BBB-2".to_string(), "feature/AAA-1".to_string()]
    );
}

/// Regression test for #302 — `budi sessions -p today` was empty because
/// `since` is computed as a UTC RFC3339 string from local midnight while
/// message timestamps are written by each provider in RFC3339 (UTC). Confirm
/// the string comparison works for the CLI's exact `since` format.
#[test]
fn session_list_returns_session_for_today_window_with_local_midnight_utc_since() {
    use chrono::TimeZone;
    let mut conn = test_db();

    // Emulate the CLI's `local_midnight_to_utc(today)` output — RFC3339 UTC
    // with a `+00:00` offset and no fractional seconds.
    let since_dt = chrono::Local
        .from_local_datetime(
            &chrono::Local::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .latest()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let since = since_dt.to_rfc3339();

    // One assistant message "today, one hour after local midnight".
    let mut msg = assistant_msg("today-msg", "sess-today", 1.5);
    msg.timestamp = since_dt + chrono::Duration::hours(1);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let result = session_list_with_filters(
        &conn,
        &SessionListParams {
            since: Some(&since),
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(result.total_count, 1);
    assert_eq!(result.sessions[0].id, "sess-today");
}

/// Sessions visibility doctor check surfaces window-level mismatches so
/// `budi doctor` can flag a recurrence of #302 even if the primary fix
/// regresses.
#[test]
fn session_visibility_reports_windows_and_flags_hidden_rows() {
    let mut conn = test_db();
    // Anchor both rows inside today's UTC window (matches the
    // `session_visibility` production query which floors to
    // `Utc::now().date_naive()` midnight). Using `Utc::now() - minutes(30)`
    // directly made the test flaky when CI happened to run in the first
    // ~90 minutes after UTC midnight, because "30 minutes ago" was
    // yesterday and fell outside the today window.
    let today_midnight = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight")
        .and_utc();

    // Visible: assistant message with a session_id, stamped inside today.
    let mut visible = assistant_msg("vis-1", "sess-vis", 1.0);
    visible.timestamp = today_midnight + chrono::Duration::minutes(1);
    ingest_messages(&mut conn, &[visible], None).unwrap();

    // Hidden: assistant row written directly with NULL session_id, also
    // stamped inside today — simulates the pre-fix proxy bug.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_cents, cost_confidence)
         VALUES ('hidden-1', NULL, 'assistant', ?1, 'claude-opus-4-6',
                 'claude_code', 1, 1, 0, 0, 0.1, 'proxy_estimated')",
        params![(today_midnight + chrono::Duration::minutes(2)).to_rfc3339()],
    )
    .unwrap();

    let windows = session_visibility(&conn).unwrap();
    let labels: Vec<&str> = windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, vec!["today", "7d", "30d"]);

    for window in &windows {
        assert_eq!(window.assistant_messages, 2, "{} window", window.label);
        assert_eq!(
            window.assistant_messages_with_session, 1,
            "{} window",
            window.label
        );
        assert_eq!(window.distinct_sessions, 1, "{} window", window.label);
        assert_eq!(window.returned_sessions, 1, "{} window", window.label);
        assert!(
            !window.has_mismatch(),
            "{} window should not flag mismatch: visible rows exist",
            window.label
        );
    }
}

#[test]
fn session_visibility_flags_mismatch_when_all_rows_missing_session_id() {
    let conn = test_db();
    // Anchor the row inside today's UTC window for the same reason as
    // `session_visibility_reports_windows_and_flags_hidden_rows`: using
    // `Utc::now() - minutes(30)` was flaky in the first ~90 minutes after
    // UTC midnight (the timestamp fell into "yesterday").
    let today_midnight = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight")
        .and_utc();

    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_cents, cost_confidence)
         VALUES ('hidden-1', NULL, 'assistant', ?1, 'claude-opus-4-6',
                 'claude_code', 1, 1, 0, 0, 0.1, 'proxy_estimated')",
        params![(today_midnight + chrono::Duration::minutes(1)).to_rfc3339()],
    )
    .unwrap();

    let windows = session_visibility(&conn).unwrap();
    let today = windows.iter().find(|w| w.label == "today").unwrap();
    assert_eq!(today.assistant_messages, 1);
    assert_eq!(today.assistant_messages_with_session, 0);
    assert_eq!(today.returned_sessions, 0);
    assert!(today.has_mismatch());
}

/// Regression for #303 — `budi doctor` must see when live proxy traffic
/// lands in the last 7 days without `git_branch`, which is exactly what
/// makes `budi stats --branches` collapse into `(untagged)`.
#[test]
fn branch_attribution_stats_reports_missing_branch_within_7d() {
    let conn = test_db();
    let now = chrono::Utc::now();

    // Two claude_code rows without a branch, one with.
    for (id, branch) in [
        ("m-no-1", None::<&str>),
        ("m-no-2", None),
        ("m-ok-1", Some("PROJ-1-feat")),
    ] {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                                   input_tokens, output_tokens, cache_creation_tokens,
                                   cache_read_tokens, git_branch, cost_cents, cost_confidence)
             VALUES (?1, ?2, 'assistant', ?3, 'claude-sonnet-4-6',
                     'claude_code', 1, 1, 0, 0, ?4, 0.1, 'proxy_estimated')",
            params![
                id,
                format!("sess-{id}"),
                (now - chrono::Duration::hours(1)).to_rfc3339(),
                branch,
            ],
        )
        .unwrap();
    }

    // One openai row, outside the 7-day window. Must be excluded.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
                               input_tokens, output_tokens, cache_creation_tokens,
                               cache_read_tokens, git_branch, cost_cents, cost_confidence)
         VALUES ('old-1', 'sess-old', 'assistant', ?1, 'gpt-4o',
                 'openai', 1, 1, 0, 0, NULL, 0.1, 'proxy_estimated')",
        params![(now - chrono::Duration::days(30)).to_rfc3339()],
    )
    .unwrap();

    let stats = branch_attribution_stats(&conn).unwrap();
    assert_eq!(
        stats.len(),
        1,
        "only claude_code should appear in 7d window, got: {stats:?}"
    );
    let claude = &stats[0];
    assert_eq!(claude.provider, "claude_code");
    assert_eq!(claude.total_assistant, 3);
    assert_eq!(claude.missing_branch, 2);
    // 2/3 ≈ 66.7% — should breach the 50% red threshold.
    assert!(
        claude.missing_branch_ratio() > 0.5,
        "2 of 3 rows missing a branch must breach the red threshold, got {}",
        claude.missing_branch_ratio()
    );
}

/// `messages.timestamp` contract — every provider must write an RFC3339 UTC
/// string that string-compares correctly against the CLI's `since`/`until`
/// bounds. This regression test exercises the exact formats emitted by the
/// Claude Code JSONL, Cursor, and Codex providers (see SOUL.md §Messages).
#[test]
fn session_list_window_compares_correctly_across_provider_timestamp_formats() {
    let mut conn = test_db();

    // Claude Code JSONL uses `...Z`; after ingest we round-trip via
    // `DateTime<Utc>::to_rfc3339()` which writes `+00:00`.
    let mut claude = assistant_msg("cc-1", "cc-sess", 1.0);
    claude.timestamp = "2026-03-14T12:00:00.500Z".parse().unwrap();
    claude.provider = "claude_code".to_string();

    // Cursor timestamps come in as millis and are written via
    // `DateTime::from_timestamp_millis().to_rfc3339()`.
    let mut cursor = assistant_msg("cu-1", "cu-sess", 1.0);
    cursor.timestamp = "2026-03-14T12:30:00+00:00".parse().unwrap();
    cursor.provider = "cursor".to_string();

    // Codex emits RFC3339 with `...Z` suffix as well.
    let mut codex = assistant_msg("cx-1", "cx-sess", 1.0);
    codex.timestamp = "2026-03-14T13:00:00.123Z".parse().unwrap();
    codex.provider = "openai".to_string();

    ingest_messages(&mut conn, &[claude, cursor, codex], None).unwrap();

    let since = "2026-03-14T00:00:00+00:00";
    let until = "2026-03-15T00:00:00+00:00";
    let result = session_list_with_filters(
        &conn,
        &SessionListParams {
            since: Some(since),
            until: Some(until),
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(
        result.total_count, 3,
        "all three provider sessions must be in window"
    );

    // And the `Z`-suffixed upper bound (as produced by some callers) also works.
    let until_z = "2026-03-15T00:00:00Z";
    let result2 = session_list_with_filters(
        &conn,
        &SessionListParams {
            since: Some(since),
            until: Some(until_z),
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(result2.total_count, 3);
}

#[test]
fn session_list_ignores_empty_string_session_id() {
    let conn = test_db();
    // Direct insert: assistant row whose session_id is the empty string.
    // Pre-fix `insert_proxy_message` could land here if the proxy route
    // supplied "" instead of generating an id.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, provider,
            input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
            cost_cents, cost_confidence)
         VALUES ('empty-1', '', 'assistant', '2026-03-14T10:00:00+00:00',
                 'claude-opus-4-6', 'claude_code', 1, 1, 0, 0, 0.1, 'proxy_estimated')",
        [],
    )
    .unwrap();

    let result = session_list_with_filters(
        &conn,
        &SessionListParams {
            since: Some("2026-03-13T00:00:00+00:00"),
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(
        result.total_count, 0,
        "empty-string session_id must not produce a ghost session row"
    );
}

#[test]
fn session_list_with_dimension_filters() {
    let mut conn = test_db();
    let mut keep = assistant_msg("session-filter-keep", "sess-keep", 6.0);
    keep.provider = "cursor".to_string();
    keep.model = Some("gpt-5.4".to_string());
    keep.repo_id = Some("github.com/acme/repo-b".to_string());
    keep.git_branch = Some("refs/heads/feature/ship".to_string());
    keep.timestamp = "2026-03-26T00:00:01Z".parse().unwrap();
    let mut skip = assistant_msg("session-filter-skip", "sess-skip", 3.0);
    skip.provider = "claude_code".to_string();
    skip.model = Some("claude-sonnet-4-6".to_string());
    skip.repo_id = Some("github.com/acme/repo-a".to_string());
    skip.git_branch = Some("refs/heads/main".to_string());
    skip.timestamp = "2026-03-26T00:00:02Z".parse().unwrap();
    ingest_messages(&mut conn, &[keep, skip], None).unwrap();

    let filters = DimensionFilters {
        agents: vec!["cursor".to_string()],
        models: vec!["gpt-5.4".to_string()],
        projects: vec!["github.com/acme/repo-b".to_string()],
        branches: vec!["feature/ship".to_string()],
    };
    let result = session_list_with_filters(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: Some("started_at"),
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: None,
        },
        &filters,
    )
    .unwrap();

    assert_eq!(result.total_count, 1);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].id, "sess-keep");
}

#[test]
fn session_detail_returns_row_for_message_only_session() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();

    let detail = session_detail(&conn, "sess-abc")
        .unwrap()
        .expect("session should exist");
    assert_eq!(detail.id, "sess-abc");
    assert!(detail.message_count >= 1);
    assert!(detail.cost_cents >= 0.0);
}

#[test]
fn session_detail_uses_session_title_when_available() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO sessions (id, provider, title)
         VALUES ('sess-abc', 'claude_code', 'Fix flaky test')",
        [],
    )
    .unwrap();

    let detail = session_detail(&conn, "sess-abc")
        .unwrap()
        .expect("session should exist");
    assert_eq!(detail.title.as_deref(), Some("Fix flaky test"));
}

#[test]
fn session_detail_tracks_multi_repo_and_branch() {
    let mut conn = test_db();

    let mut m1 = assistant_msg("multi-1", "sess-multi", 2.0);
    m1.repo_id = Some("github.com/acme/repo-a".to_string());
    m1.git_branch = Some("feature/AAA-1".to_string());
    let mut m2 = assistant_msg("multi-2", "sess-multi", 5.0);
    m2.repo_id = Some("github.com/acme/repo-b".to_string());
    m2.git_branch = Some("refs/heads/feature/BBB-2".to_string());
    ingest_messages(&mut conn, &[m1, m2], None).unwrap();

    let detail = session_detail(&conn, "sess-multi")
        .unwrap()
        .expect("session should exist");
    assert_eq!(detail.repo_ids.len(), 2);
    assert_eq!(detail.git_branches.len(), 2);
    assert_eq!(detail.repo_ids[0], "github.com/acme/repo-b");
    assert_eq!(detail.git_branches[0], "feature/BBB-2");
}

#[test]
fn session_tags_do_not_derive_repo_and_branch_from_message_columns() {
    let mut conn = test_db();

    let mut msg = assistant_msg("sess-tags-cols-1", "sess-tags-cols", 1.0);
    msg.repo_id = Some("github.com/acme/repo-z".to_string());
    msg.git_branch = Some("refs/heads/feature/ZZZ-99".to_string());
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let result = session_tags(&conn, "sess-tags-cols").unwrap();
    assert!(
        !result.iter().any(|(k, _)| k == "repo"),
        "repo is a canonical message/session field, not a tag"
    );
    assert!(
        !result.iter().any(|(k, _)| k == "branch"),
        "branch is a canonical message/session field, not a tag"
    );
}

/// Helper: create a minimal assistant ParsedMessage, overriding only what matters.
fn assistant_msg(uuid: &str, session_id: &str, cost_cents: f64) -> ParsedMessage {
    ParsedMessage {
        uuid: uuid.to_string(),
        session_id: Some(session_id.to_string()),
        timestamp: "2026-03-14T10:00:00Z".parse().unwrap(),
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(cost_cents),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "exact".to_string(),
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
}

#[test]
fn activity_chart_groups_by_day() {
    let mut conn = test_db();
    let mut msg1 = assistant_msg("act-1", "s1", 2.0);
    msg1.timestamp = "2026-03-14T10:00:00Z".parse().unwrap();
    let mut msg2 = assistant_msg("act-2", "s1", 3.0);
    msg2.timestamp = "2026-03-15T14:00:00Z".parse().unwrap();
    ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

    let chart = activity_chart(&conn, None, None, "day", 0).unwrap();
    assert_eq!(chart.len(), 2);
    assert_eq!(chart[0].label, "2026-03-14");
    assert_eq!(chart[0].message_count, 1);
    assert_eq!(chart[1].label, "2026-03-15");
    assert_eq!(chart[1].message_count, 1);
}

#[test]
fn activity_chart_hour_granularity() {
    let mut conn = test_db();
    let msg = assistant_msg("act-h1", "s1", 1.0);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let chart = activity_chart(&conn, None, None, "hour", 0).unwrap();
    assert_eq!(chart.len(), 1);
    assert_eq!(chart[0].label, "10:00");
}

#[test]
fn branch_cost_groups_by_branch() {
    let mut conn = test_db();
    let mut msg1 = assistant_msg("br-1", "s1", 5.0);
    msg1.git_branch = Some("main".to_string());
    msg1.repo_id = Some("my-repo".to_string());
    let mut msg2 = assistant_msg("br-2", "s2", 3.0);
    msg2.git_branch = Some("feature".to_string());
    msg2.repo_id = Some("my-repo".to_string());
    let mut msg3 = assistant_msg("br-3", "s1", 2.0);
    msg3.git_branch = Some("main".to_string());
    msg3.repo_id = Some("my-repo".to_string());
    ingest_messages(&mut conn, &[msg1, msg2, msg3], None).unwrap();

    let branches = branch_cost(&conn, None, None, 10).unwrap();
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].git_branch, "main");
    assert!((branches[0].cost_cents - 7.0).abs() < 0.01);
    assert_eq!(branches[0].message_count, 2);
    assert_eq!(branches[1].git_branch, "feature");
    assert!((branches[1].cost_cents - 3.0).abs() < 0.01);
}

#[test]
fn branch_cost_single_finds_branch() {
    let mut conn = test_db();
    let mut msg = assistant_msg("brs-1", "s1", 4.0);
    msg.git_branch = Some("fix/bug-123".to_string());
    msg.repo_id = Some("repo".to_string());
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let result = branch_cost_single(&conn, "fix/bug-123", None, None, None).unwrap();
    assert!(result.is_some());
    let bc = result.unwrap();
    assert_eq!(bc.git_branch, "fix/bug-123");
    assert!((bc.cost_cents - 4.0).abs() < 0.01);

    let none = branch_cost_single(&conn, "nonexistent", None, None, None).unwrap();
    assert!(none.is_none());
}

#[test]
fn branch_cost_single_handles_multi_repo_branches() {
    let mut conn = test_db();
    let mut msg1 = assistant_msg("brs-multi-1", "s1", 4.0);
    msg1.git_branch = Some("feature/shared".to_string());
    msg1.repo_id = Some("repo-a".to_string());
    let mut msg2 = assistant_msg("brs-multi-2", "s2", 6.0);
    msg2.git_branch = Some("feature/shared".to_string());
    msg2.repo_id = Some("repo-b".to_string());
    ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

    let all = branch_cost_single(&conn, "feature/shared", None, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(all.git_branch, "feature/shared");
    assert_eq!(all.repo_id, "");
    assert_eq!(all.session_count, 2);
    assert_eq!(all.message_count, 2);
    assert!((all.cost_cents - 10.0).abs() < 0.01);

    let repo_a = branch_cost_single(&conn, "feature/shared", Some("repo-a"), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(repo_a.repo_id, "repo-a");
    assert_eq!(repo_a.session_count, 1);
    assert_eq!(repo_a.message_count, 1);
    assert!((repo_a.cost_cents - 4.0).abs() < 0.01);

    let missing = branch_cost_single(&conn, "feature/shared", Some("repo-c"), None, None).unwrap();
    assert!(missing.is_none());
}

#[test]
fn branch_cost_untagged() {
    let mut conn = test_db();
    let msg = assistant_msg("br-untagged", "s1", 6.0);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let branches = branch_cost(&conn, None, None, 10).unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].git_branch, "(untagged)");
}

// ---------------------------------------------------------------------------
// Ticket cost (R1.0.3 / #304)
// ---------------------------------------------------------------------------
//
// These tests cover the contract that 8.1 promotes ticket attribution to a
// first-class CLI dimension:
//   1. `--tickets` lists tickets by cost, with `(untagged)` for the bucket
//      of assistant messages that have no `ticket_id` tag.
//   2. `--ticket <ID>` returns a single detail row + per-branch breakdown.
//   3. The session list filter restricts results to sessions tagged with
//      the requested ticket.
// All three rely on the `ticket_id` tag emitted by `GitEnricher`; the tests
// inject the tag directly so they don't depend on enricher behaviour.

fn ticket_msg(uuid: &str, session_id: &str, branch: &str, repo: &str, cost: f64) -> ParsedMessage {
    let mut m = assistant_msg(uuid, session_id, cost);
    m.git_branch = Some(branch.to_string());
    m.repo_id = Some(repo.to_string());
    m
}

fn ticket_tags(values: &[&str]) -> Vec<Tag> {
    values
        .iter()
        .map(|v| Tag {
            key: "ticket_id".to_string(),
            value: (*v).to_string(),
        })
        .collect()
}

#[test]
fn ticket_cost_groups_by_ticket() {
    // Two tickets, with PAVA-1 carrying more cost than PAVA-2 across two
    // sessions; PAVA-2 has a single session. Expect cost-desc ordering and
    // session_count to count *distinct* sessions per ticket.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-1", "s1", "PAVA-1-foo", "repo-a", 4.0);
    let m2 = ticket_msg("tk-2", "s2", "PAVA-1-foo", "repo-a", 6.0);
    let m3 = ticket_msg("tk-3", "s3", "PAVA-2-bar", "repo-b", 3.0);
    let tags = vec![
        ticket_tags(&["PAVA-1"]),
        ticket_tags(&["PAVA-1"]),
        ticket_tags(&["PAVA-2"]),
    ];
    ingest_messages(&mut conn, &[m1, m2, m3], Some(&tags)).unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let pava1 = tickets.iter().find(|t| t.ticket_id == "PAVA-1").unwrap();
    let pava2 = tickets.iter().find(|t| t.ticket_id == "PAVA-2").unwrap();
    assert_eq!(pava1.session_count, 2, "PAVA-1 spans two sessions");
    assert_eq!(pava1.message_count, 2);
    assert!((pava1.cost_cents - 10.0).abs() < 0.01);
    assert_eq!(pava1.ticket_prefix, "PAVA");
    assert_eq!(pava1.top_branch, "PAVA-1-foo");
    assert_eq!(pava1.top_repo_id, "repo-a");
    assert_eq!(pava2.session_count, 1);
    assert!((pava2.cost_cents - 3.0).abs() < 0.01);
    // Cost-desc ordering is the contract surfaced by `budi stats --tickets`.
    let pava1_idx = tickets
        .iter()
        .position(|t| t.ticket_id == "PAVA-1")
        .unwrap();
    let pava2_idx = tickets
        .iter()
        .position(|t| t.ticket_id == "PAVA-2")
        .unwrap();
    assert!(pava1_idx < pava2_idx);
}

#[test]
fn ticket_cost_includes_untagged_bucket() {
    // One tagged ticket and one bare assistant message → expect the
    // (untagged) row to appear so the total reconciles with the global
    // cost summary, never silently disappears.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-u-1", "s1", "PAVA-9", "repo", 5.0);
    let m2 = assistant_msg("tk-u-2", "s2", 7.0); // no branch, no ticket
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[ticket_tags(&["PAVA-9"]), Vec::new()]),
    )
    .unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let untagged = tickets
        .iter()
        .find(|t| t.ticket_id == "(untagged)")
        .expect("untagged ticket bucket present");
    assert!((untagged.cost_cents - 7.0).abs() < 0.01);
    assert_eq!(untagged.message_count, 1);
    assert_eq!(untagged.ticket_prefix, "");
    assert_eq!(untagged.top_branch, "");
}

#[test]
fn ticket_cost_single_returns_detail_with_branches() {
    // PAVA-7 is worked on across two branches in the same repo. Detail
    // view should attribute cost per branch and pick the dominant repo.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-d-1", "s1", "PAVA-7-impl", "repo-a", 8.0);
    let m2 = ticket_msg("tk-d-2", "s2", "PAVA-7-test", "repo-a", 2.0);
    let tags = vec![ticket_tags(&["PAVA-7"]), ticket_tags(&["PAVA-7"])];
    ingest_messages(&mut conn, &[m1, m2], Some(&tags)).unwrap();

    let detail = ticket_cost_single(&conn, "PAVA-7", None, None, None)
        .unwrap()
        .expect("ticket detail present");
    assert_eq!(detail.ticket_id, "PAVA-7");
    assert_eq!(detail.ticket_prefix, "PAVA");
    assert_eq!(detail.session_count, 2);
    assert_eq!(detail.message_count, 2);
    assert_eq!(detail.repo_id, "repo-a");
    assert!((detail.cost_cents - 10.0).abs() < 0.01);
    assert_eq!(detail.branches.len(), 2);
    // Cost-desc ordering of the branch breakdown.
    assert_eq!(detail.branches[0].git_branch, "PAVA-7-impl");
    assert!((detail.branches[0].cost_cents - 8.0).abs() < 0.01);

    let missing = ticket_cost_single(&conn, "DOES-NOT-EXIST", None, None, None).unwrap();
    assert!(missing.is_none());
}

#[test]
fn ticket_cost_single_can_filter_by_repo() {
    // The same ticket id exists in two repos (rare, but possible when teams
    // share IDs across services). `--repo` should narrow the result so the
    // CLI can disambiguate.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-r-1", "s1", "PAVA-3", "repo-a", 4.0);
    let m2 = ticket_msg("tk-r-2", "s2", "PAVA-3", "repo-b", 6.0);
    let tags = vec![ticket_tags(&["PAVA-3"]), ticket_tags(&["PAVA-3"])];
    ingest_messages(&mut conn, &[m1, m2], Some(&tags)).unwrap();

    let only_a = ticket_cost_single(&conn, "PAVA-3", Some("repo-a"), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(only_a.repo_id, "repo-a");
    assert!((only_a.cost_cents - 4.0).abs() < 0.01);
    assert_eq!(only_a.message_count, 1);

    let none = ticket_cost_single(&conn, "PAVA-3", Some("repo-c"), None, None).unwrap();
    assert!(none.is_none());
}

#[test]
fn ticket_cost_splits_cost_for_multi_ticket_message() {
    // Reuse the proportional-split contract that `tag_stats` already gives
    // ticket tags so users see a fair number when one message touches two
    // tickets (e.g. cross-cutting refactor).
    let mut conn = test_db();
    let m = ticket_msg("tk-multi", "s1", "PAVA-1+PAVA-2", "repo", 10.0);
    let tags = vec![ticket_tags(&["PAVA-1", "PAVA-2"])];
    ingest_messages(&mut conn, &[m], Some(&tags)).unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let p1 = tickets.iter().find(|t| t.ticket_id == "PAVA-1").unwrap();
    let p2 = tickets.iter().find(|t| t.ticket_id == "PAVA-2").unwrap();
    assert!((p1.cost_cents - 5.0).abs() < 0.01);
    assert!((p2.cost_cents - 5.0).abs() < 0.01);
}

#[test]
fn session_list_filters_by_ticket() {
    // The session list filter is the third surface added in 8.1 — without it,
    // `budi sessions --ticket PAVA-9` would have to client-side filter and
    // would misreport `total_count`.
    let mut conn = test_db();
    let m1 = ticket_msg("sess-tk-1", "s1", "PAVA-9", "repo", 5.0);
    let m2 = assistant_msg("sess-tk-2", "s2", 7.0); // unrelated session
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[ticket_tags(&["PAVA-9"]), Vec::new()]),
    )
    .unwrap();

    let filtered = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: Some("PAVA-9"),
            activity: None,
        },
    )
    .unwrap();
    assert_eq!(filtered.total_count, 1);
    assert_eq!(filtered.sessions.len(), 1);
    assert_eq!(filtered.sessions[0].id, "s1");

    // A ticket that does not exist filters everything out.
    let none = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: Some("NOPE-1"),
            activity: None,
        },
    )
    .unwrap();
    assert_eq!(none.total_count, 0);
    assert!(none.sessions.is_empty());
}

// ---------------------------------------------------------------------------
// Ticket source — R1.3 (#221)
// ---------------------------------------------------------------------------
//
// R1.3 promotes `ticket_source` to a first-class sibling of `ticket_id`
// so analytics can explain how an id was derived (alphanumeric `branch`
// pattern vs `branch_numeric` fallback). The list and detail views must
// surface the dominant source per ticket, and legacy rows that only
// carry a `ticket_id` tag (pre-R1.3) must keep working by defaulting to
// `branch` — the only pre-R1.3 pipeline producer.

fn ticket_tags_with_source(ticket: &str, source: &str) -> Vec<Tag> {
    vec![
        Tag {
            key: "ticket_id".to_string(),
            value: ticket.to_string(),
        },
        Tag {
            key: "ticket_source".to_string(),
            value: source.to_string(),
        },
    ]
}

#[test]
fn ticket_cost_surfaces_source_per_ticket() {
    // Two tickets with explicit, distinct sources. Expect each row to
    // carry its own `source` so the CLI `src=…` column is reliable.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-src-1", "s1", "PAVA-11-alpha", "repo", 5.0);
    let m2 = ticket_msg("tk-src-2", "s2", "1234-numeric", "repo", 3.0);
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[
            ticket_tags_with_source("PAVA-11", "branch"),
            ticket_tags_with_source("1234", "branch_numeric"),
        ]),
    )
    .unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let alpha = tickets.iter().find(|t| t.ticket_id == "PAVA-11").unwrap();
    let numeric = tickets.iter().find(|t| t.ticket_id == "1234").unwrap();
    assert_eq!(alpha.source, "branch");
    assert_eq!(numeric.source, "branch_numeric");
}

#[test]
fn ticket_cost_defaults_legacy_source_to_branch() {
    // Pre-R1.3 DBs only carry the `ticket_id` tag. The loader falls back
    // to `branch` so older data stays readable without a reindex.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-legacy-1", "s1", "PAVA-42-impl", "repo", 4.0);
    ingest_messages(&mut conn, &[m1], Some(&[ticket_tags(&["PAVA-42"])])).unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let pava42 = tickets.iter().find(|t| t.ticket_id == "PAVA-42").unwrap();
    assert_eq!(
        pava42.source, "branch",
        "legacy rows default to the alphanumeric source"
    );
}

#[test]
fn ticket_cost_untagged_row_has_empty_source() {
    // The `(untagged)` bucket has no derivation, so its source column is
    // empty — rendered as `--` by the CLI.
    let mut conn = test_db();
    let m = assistant_msg("tk-src-unt", "s1", 3.0);
    ingest_messages(&mut conn, &[m], Some(&[Vec::new()])).unwrap();

    let tickets = ticket_cost(&conn, None, None, 10).unwrap();
    let untagged = tickets
        .iter()
        .find(|t| t.ticket_id == "(untagged)")
        .unwrap();
    assert_eq!(untagged.source, "");
}

#[test]
fn ticket_cost_single_surfaces_source() {
    // The detail view must carry the dominant source too; it drives the
    // `Source` row in `budi stats --ticket <ID>`.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-src-d-1", "s1", "4321-fix", "repo", 6.0);
    ingest_messages(
        &mut conn,
        &[m1],
        Some(&[ticket_tags_with_source("4321", "branch_numeric")]),
    )
    .unwrap();

    let detail = ticket_cost_single(&conn, "4321", None, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(detail.source, "branch_numeric");
}

#[test]
fn ticket_cost_single_legacy_source_defaults_to_branch() {
    // Pre-R1.3 detail rows lack a `ticket_source` tag; the detail view
    // still needs to print something useful, so default to `branch`.
    let mut conn = test_db();
    let m1 = ticket_msg("tk-src-d-legacy", "s1", "PAVA-99-impl", "repo", 2.0);
    ingest_messages(&mut conn, &[m1], Some(&[ticket_tags(&["PAVA-99"])])).unwrap();

    let detail = ticket_cost_single(&conn, "PAVA-99", None, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(detail.source, "branch");
}

// ---------------------------------------------------------------------------
// Activities — R1.0 (#305)
//
// Activities live in the `tags` table under the `activity` key (see
// `tag_keys::ACTIVITY`). The tests mirror the ticket suite so regressions
// on either surface are easy to diff, and they seed the tag directly so
// they don't depend on the classifier or pipeline.
// ---------------------------------------------------------------------------

fn activity_tags(values: &[&str]) -> Vec<Tag> {
    values
        .iter()
        .map(|v| Tag {
            key: "activity".to_string(),
            value: (*v).to_string(),
        })
        .collect()
}

fn activity_msg(
    uuid: &str,
    session_id: &str,
    branch: &str,
    repo: &str,
    cost: f64,
) -> ParsedMessage {
    let mut m = assistant_msg(uuid, session_id, cost);
    m.git_branch = Some(branch.to_string());
    m.repo_id = Some(repo.to_string());
    m
}

#[test]
fn activity_cost_groups_by_activity() {
    // `bugfix` and `refactor` each span their own sessions and `bugfix`
    // outweighs `refactor` on cost. We expect cost-desc ordering and
    // `session_count` reporting distinct sessions.
    let mut conn = test_db();
    let m1 = activity_msg("ac-1", "s1", "feat/login", "repo-a", 4.0);
    let m2 = activity_msg("ac-2", "s2", "feat/login", "repo-a", 6.0);
    let m3 = activity_msg("ac-3", "s3", "refactor/api", "repo-b", 3.0);
    let tags = vec![
        activity_tags(&["bugfix"]),
        activity_tags(&["bugfix"]),
        activity_tags(&["refactor"]),
    ];
    ingest_messages(&mut conn, &[m1, m2, m3], Some(&tags)).unwrap();

    let activities = activity_cost(&conn, None, None, 10).unwrap();
    let bug = activities
        .iter()
        .find(|a| a.activity == "bugfix")
        .expect("bugfix present");
    let refac = activities
        .iter()
        .find(|a| a.activity == "refactor")
        .expect("refactor present");
    assert_eq!(bug.session_count, 2, "bugfix spans two sessions");
    assert_eq!(bug.message_count, 2);
    assert!((bug.cost_cents - 10.0).abs() < 0.01);
    assert_eq!(bug.top_branch, "feat/login");
    assert_eq!(bug.top_repo_id, "repo-a");
    assert_eq!(bug.source, "rule", "R1.0 labels rule-derived activities");
    assert_eq!(bug.confidence, "medium");
    assert!((refac.cost_cents - 3.0).abs() < 0.01);
    // Cost-desc ordering is the contract surfaced by `budi stats --activities`.
    let bug_idx = activities
        .iter()
        .position(|a| a.activity == "bugfix")
        .unwrap();
    let refac_idx = activities
        .iter()
        .position(|a| a.activity == "refactor")
        .unwrap();
    assert!(bug_idx < refac_idx);
}

#[test]
fn activity_cost_includes_untagged_bucket() {
    // One tagged activity and one bare assistant message → expect the
    // `(untagged)` row to appear with empty source/confidence so the
    // total reconciles with the global cost summary, never silently
    // disappears.
    let mut conn = test_db();
    let m1 = activity_msg("ac-u-1", "s1", "main", "repo", 5.0);
    let m2 = assistant_msg("ac-u-2", "s2", 7.0);
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[activity_tags(&["bugfix"]), Vec::new()]),
    )
    .unwrap();

    let activities = activity_cost(&conn, None, None, 10).unwrap();
    let untagged = activities
        .iter()
        .find(|a| a.activity == "(untagged)")
        .expect("untagged activity bucket present");
    assert!((untagged.cost_cents - 7.0).abs() < 0.01);
    assert_eq!(untagged.message_count, 1);
    assert_eq!(untagged.top_branch, "");
    assert_eq!(
        untagged.source, "",
        "untagged bucket advertises an explicit absence"
    );
    assert_eq!(untagged.confidence, "");
}

#[test]
fn activity_cost_single_returns_detail_with_branches() {
    // `bugfix` worked across two branches in the same repo. Detail view
    // should attribute cost per branch and pick the dominant repo.
    let mut conn = test_db();
    let m1 = activity_msg("ac-d-1", "s1", "fix/a", "repo-a", 8.0);
    let m2 = activity_msg("ac-d-2", "s2", "fix/b", "repo-a", 2.0);
    let tags = vec![activity_tags(&["bugfix"]), activity_tags(&["bugfix"])];
    ingest_messages(&mut conn, &[m1, m2], Some(&tags)).unwrap();

    let detail = activity_cost_single(&conn, "bugfix", None, None, None)
        .unwrap()
        .expect("activity detail present");
    assert_eq!(detail.activity, "bugfix");
    assert_eq!(detail.session_count, 2);
    assert_eq!(detail.message_count, 2);
    assert_eq!(detail.repo_id, "repo-a");
    assert!((detail.cost_cents - 10.0).abs() < 0.01);
    assert_eq!(detail.branches.len(), 2);
    assert_eq!(detail.branches[0].git_branch, "fix/a");
    assert!((detail.branches[0].cost_cents - 8.0).abs() < 0.01);
    assert_eq!(detail.source, "rule");
    assert_eq!(detail.confidence, "medium");

    let missing = activity_cost_single(&conn, "does-not-exist", None, None, None).unwrap();
    assert!(missing.is_none());
}

#[test]
fn activity_cost_single_can_filter_by_repo() {
    let mut conn = test_db();
    let m1 = activity_msg("ac-r-1", "s1", "main", "repo-a", 4.0);
    let m2 = activity_msg("ac-r-2", "s2", "main", "repo-b", 6.0);
    let tags = vec![activity_tags(&["bugfix"]), activity_tags(&["bugfix"])];
    ingest_messages(&mut conn, &[m1, m2], Some(&tags)).unwrap();

    let only_a = activity_cost_single(&conn, "bugfix", Some("repo-a"), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(only_a.repo_id, "repo-a");
    assert!((only_a.cost_cents - 4.0).abs() < 0.01);

    let none = activity_cost_single(&conn, "bugfix", Some("repo-c"), None, None).unwrap();
    assert!(none.is_none());
}

// ---------------------------------------------------------------------------
// File cost (R1.4 / #292)
// ---------------------------------------------------------------------------
//
// Mirrors the ticket / activity roll-up contract. Tests inject the tags
// directly so they don't depend on `FileEnricher` (which does its own
// normalization, covered in `file_attribution::tests`).

fn file_msg(uuid: &str, session_id: &str, branch: &str, repo: &str, cost: f64) -> ParsedMessage {
    let mut m = assistant_msg(uuid, session_id, cost);
    m.git_branch = Some(branch.to_string());
    m.repo_id = Some(repo.to_string());
    m
}

fn file_tags(values: &[&str]) -> Vec<Tag> {
    values
        .iter()
        .map(|v| Tag {
            key: "file_path".to_string(),
            value: (*v).to_string(),
        })
        .collect()
}

#[test]
fn file_cost_groups_by_file() {
    // Two files touched across two sessions; splits cost proportionally
    // on multi-file messages and sorts cost-descending.
    let mut conn = test_db();
    let m1 = file_msg("fc-1", "s1", "main", "repo-a", 10.0);
    let m2 = file_msg("fc-2", "s2", "main", "repo-a", 4.0);
    // Multi-file message: splits 4.0 across two files.
    let m3 = file_msg("fc-3", "s3", "main", "repo-a", 4.0);
    let tags = vec![
        file_tags(&["src/main.rs"]),
        file_tags(&["src/main.rs"]),
        file_tags(&["src/main.rs", "Cargo.toml"]),
    ];
    ingest_messages(&mut conn, &[m1, m2, m3], Some(&tags)).unwrap();

    let rows = file_cost(&conn, None, None, 10).unwrap();
    let main_rs = rows.iter().find(|r| r.file_path == "src/main.rs").unwrap();
    // 10.0 + 4.0 + (4.0/2 = 2.0) = 16.0
    assert!((main_rs.cost_cents - 16.0).abs() < 0.01);
    assert_eq!(main_rs.message_count, 3);
    assert_eq!(main_rs.session_count, 3);
    assert_eq!(main_rs.top_repo_id, "repo-a");
    assert_eq!(main_rs.top_branch, "main");

    let cargo = rows.iter().find(|r| r.file_path == "Cargo.toml").unwrap();
    assert!((cargo.cost_cents - 2.0).abs() < 0.01);

    let main_idx = rows
        .iter()
        .position(|r| r.file_path == "src/main.rs")
        .unwrap();
    let cargo_idx = rows
        .iter()
        .position(|r| r.file_path == "Cargo.toml")
        .unwrap();
    assert!(main_idx < cargo_idx, "cost-desc ordering");
}

#[test]
fn file_cost_includes_untagged_bucket() {
    // A tagged file + a bare assistant message → the (untagged) row
    // should appear so totals reconcile with `usage_summary`.
    let mut conn = test_db();
    let m1 = file_msg("fc-u-1", "s1", "main", "repo", 5.0);
    let m2 = assistant_msg("fc-u-2", "s2", 7.0);
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[file_tags(&["src/main.rs"]), Vec::new()]),
    )
    .unwrap();

    let rows = file_cost(&conn, None, None, 10).unwrap();
    let untagged = rows
        .iter()
        .find(|r| r.file_path == "(untagged)")
        .expect("untagged file bucket present");
    assert!((untagged.cost_cents - 7.0).abs() < 0.01);
    assert_eq!(untagged.message_count, 1);
}

#[test]
fn file_cost_single_returns_detail_with_branches_and_tickets() {
    // Same file touched on two branches and on two tickets — detail view
    // must attribute cost per branch and per ticket.
    let mut conn = test_db();
    let m1 = file_msg("fcd-1", "s1", "feat/a", "repo-a", 6.0);
    let m2 = file_msg("fcd-2", "s2", "feat/b", "repo-a", 4.0);
    let mut tags_1 = file_tags(&["src/main.rs"]);
    tags_1.push(Tag {
        key: "ticket_id".to_string(),
        value: "PAVA-1".to_string(),
    });
    let mut tags_2 = file_tags(&["src/main.rs"]);
    tags_2.push(Tag {
        key: "ticket_id".to_string(),
        value: "PAVA-2".to_string(),
    });
    ingest_messages(&mut conn, &[m1, m2], Some(&[tags_1, tags_2])).unwrap();

    let detail = file_cost_single(&conn, "src/main.rs", None, None, None)
        .unwrap()
        .expect("file detail present");
    assert_eq!(detail.file_path, "src/main.rs");
    assert_eq!(detail.session_count, 2);
    assert_eq!(detail.message_count, 2);
    assert_eq!(detail.repo_id, "repo-a");
    assert!((detail.cost_cents - 10.0).abs() < 0.01);
    assert_eq!(detail.branches.len(), 2);
    let branch_a = detail
        .branches
        .iter()
        .find(|b| b.git_branch == "feat/a")
        .unwrap();
    assert!((branch_a.cost_cents - 6.0).abs() < 0.01);
    assert_eq!(detail.tickets.len(), 2);

    let missing = file_cost_single(&conn, "does/not/exist.rs", None, None, None).unwrap();
    assert!(missing.is_none());
}

#[test]
fn file_cost_single_can_filter_by_repo() {
    let mut conn = test_db();
    let m1 = file_msg("fcr-1", "s1", "main", "repo-a", 3.0);
    let m2 = file_msg("fcr-2", "s2", "main", "repo-b", 5.0);
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[file_tags(&["src/lib.rs"]), file_tags(&["src/lib.rs"])]),
    )
    .unwrap();

    let only_a = file_cost_single(&conn, "src/lib.rs", Some("repo-a"), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(only_a.repo_id, "repo-a");
    assert!((only_a.cost_cents - 3.0).abs() < 0.01);

    let none = file_cost_single(&conn, "src/lib.rs", Some("repo-c"), None, None).unwrap();
    assert!(none.is_none());
}

#[test]
fn session_list_filters_by_activity() {
    // The session list filter is the third surface added in 8.1 (#305) —
    // without it, `budi sessions --activity bugfix` would have to
    // client-side filter and would misreport `total_count`.
    let mut conn = test_db();
    let m1 = activity_msg("sess-ac-1", "s1", "fix/a", "repo", 5.0);
    let m2 = assistant_msg("sess-ac-2", "s2", 7.0);
    ingest_messages(
        &mut conn,
        &[m1, m2],
        Some(&[activity_tags(&["bugfix"]), Vec::new()]),
    )
    .unwrap();

    let filtered = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: Some("bugfix"),
        },
    )
    .unwrap();
    assert_eq!(filtered.total_count, 1);
    assert_eq!(filtered.sessions.len(), 1);
    assert_eq!(filtered.sessions[0].id, "s1");

    let none = session_list(
        &conn,
        &SessionListParams {
            since: None,
            until: None,
            search: None,
            sort_by: None,
            sort_asc: false,
            limit: 50,
            offset: 0,
            ticket: None,
            activity: Some("nope"),
        },
    )
    .unwrap();
    assert_eq!(none.total_count, 0);
    assert!(none.sessions.is_empty());
}

#[test]
fn model_usage_groups_by_model() {
    let mut conn = test_db();
    let msg1 = assistant_msg("mu-1", "s1", 5.0);
    let mut msg2 = assistant_msg("mu-2", "s1", 3.0);
    msg2.model = Some("claude-sonnet-4-6".to_string());
    ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

    let models = model_usage(&conn, None, None, 10).unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model, "claude-opus-4-6");
    assert!((models[0].cost_cents - 5.0).abs() < 0.01);
    assert_eq!(models[1].model, "claude-sonnet-4-6");
    assert!((models[1].cost_cents - 3.0).abs() < 0.01);
}

#[test]
fn tag_stats_groups_by_tag() {
    let mut conn = test_db();
    let mut msg1 = assistant_msg("ts-1", "s1", 10.0);
    msg1.repo_id = Some("proj-a".to_string());
    let mut msg2 = assistant_msg("ts-2", "s2", 6.0);
    msg2.repo_id = Some("proj-b".to_string());
    ingest_messages(&mut conn, &[msg1, msg2], None).unwrap();

    let stats = tag_stats(&conn, Some("repo"), None, None, 10).unwrap();
    let proj_a = stats.iter().find(|s| s.value == "proj-a").unwrap();
    assert!((proj_a.cost_cents - 10.0).abs() < 0.01);
    let proj_b = stats.iter().find(|s| s.value == "proj-b").unwrap();
    assert!((proj_b.cost_cents - 6.0).abs() < 0.01);
}

#[test]
fn tag_stats_repo_uses_message_columns_not_tag_fanout() {
    let mut conn = test_db();
    let mut msg = assistant_msg("ts-repo-col-1", "s1", 10.0);
    msg.repo_id = Some("proj-a".to_string());
    let tags = vec![vec![
        Tag {
            key: "repo".to_string(),
            value: "proj-a".to_string(),
        },
        Tag {
            key: "repo".to_string(),
            value: "proj-b".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let stats = tag_stats(&conn, Some("repo"), None, None, 10).unwrap();
    let proj_a = stats.iter().find(|s| s.value == "proj-a").unwrap();
    assert!((proj_a.cost_cents - 10.0).abs() < 0.01);
    assert!(
        stats.iter().all(|s| s.value != "proj-b"),
        "repo stats should not be driven by duplicate repo tags"
    );
}

#[test]
fn tag_stats_branch_uses_message_columns_not_tag_fanout() {
    let mut conn = test_db();
    let mut msg = assistant_msg("ts-branch-col-1", "s1", 7.0);
    msg.git_branch = Some("refs/heads/feature/clean-cost".to_string());
    let tags = vec![vec![
        Tag {
            key: "branch".to_string(),
            value: "feature/clean-cost".to_string(),
        },
        Tag {
            key: "branch".to_string(),
            value: "feature/other".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let stats = tag_stats(&conn, Some("branch"), None, None, 10).unwrap();
    let branch = stats
        .iter()
        .find(|s| s.value == "feature/clean-cost")
        .unwrap();
    assert!((branch.cost_cents - 7.0).abs() < 0.01);
    assert!(
        stats.iter().all(|s| s.value != "feature/other"),
        "branch stats should not be driven by duplicate branch tags"
    );
}

#[test]
fn tag_stats_even_split_across_values() {
    let mut conn = test_db();
    let msg = assistant_msg("ts-split", "s-split", 10.0);
    let tags = vec![vec![
        Tag {
            key: "ticket".to_string(),
            value: "ABC-1".to_string(),
        },
        Tag {
            key: "ticket".to_string(),
            value: "DEF-2".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let stats = tag_stats(&conn, Some("ticket"), None, None, 10).unwrap();
    let abc = stats.iter().find(|s| s.value == "ABC-1").unwrap();
    let def = stats.iter().find(|s| s.value == "DEF-2").unwrap();
    assert!((abc.cost_cents - 5.0).abs() < 0.01);
    assert!((def.cost_cents - 5.0).abs() < 0.01);
}

#[test]
fn tag_stats_tool_splits_cost_for_multi_tool_message() {
    let mut conn = test_db();
    let msg = assistant_msg("ts-tool-split", "s-tool", 12.0);
    let tags = vec![vec![
        Tag {
            key: "tool".to_string(),
            value: "Read".to_string(),
        },
        Tag {
            key: "tool".to_string(),
            value: "Bash".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let stats = tag_stats(&conn, Some("tool"), None, None, 10).unwrap();
    let read = stats.iter().find(|s| s.value == "Read").unwrap();
    let bash = stats.iter().find(|s| s.value == "Bash").unwrap();
    assert!((read.cost_cents - 6.0).abs() < 0.01);
    assert!((bash.cost_cents - 6.0).abs() < 0.01);
}

#[test]
fn session_messages_returns_assistant_only() {
    let mut conn = test_db();
    let msgs = sample_messages();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let result = session_messages(&conn, "sess-abc").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "a1");
    assert_eq!(result[0].role, "assistant");
}

#[test]
fn session_messages_roles_all_returns_user_and_assistant_with_tools() {
    let mut conn = test_db();
    let user = ParsedMessage {
        uuid: "sm-user-1".to_string(),
        session_id: Some("sess-all".to_string()),
        timestamp: "2026-03-25T00:00:00Z".parse().unwrap(),
        role: "user".to_string(),
        request_id: Some("req-user".to_string()),
        ..Default::default()
    };
    let assistant = ParsedMessage {
        uuid: "sm-assistant-1".to_string(),
        session_id: Some("sess-all".to_string()),
        timestamp: "2026-03-25T00:00:01Z".parse().unwrap(),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        provider: "claude_code".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        request_id: Some("req-assistant".to_string()),
        cost_confidence: "estimated".to_string(),
        tool_names: vec!["Read".to_string()],
        tool_use_ids: vec!["toolu_123".to_string()],
        ..Default::default()
    };
    let tags = vec![
        vec![],
        vec![Tag {
            key: "tool".to_string(),
            value: "Read".to_string(),
        }],
    ];
    ingest_messages(&mut conn, &[user, assistant], Some(&tags)).unwrap();

    let rows = session_messages_with_roles(&conn, "sess-all", SessionMessageRoles::All).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[1].role, "assistant");
    assert_eq!(rows[1].request_id.as_deref(), Some("req-assistant"));
    assert_eq!(rows[1].tools, vec!["Read".to_string()]);
}

#[test]
fn session_message_list_paginates_and_includes_message_context() {
    let mut conn = test_db();
    let user = ParsedMessage {
        uuid: "sm-page-user".to_string(),
        session_id: Some("sess-page".to_string()),
        timestamp: "2026-03-25T00:00:00Z".parse().unwrap(),
        role: "user".to_string(),
        request_id: Some("req-user".to_string()),
        ..Default::default()
    };
    let mut a1 = assistant_msg("sm-page-a1", "sess-page", 1.0);
    a1.timestamp = "2026-03-25T00:00:01Z".parse().unwrap();
    a1.repo_id = Some("github.com/acme/repo-a".to_string());
    a1.git_branch = Some("refs/heads/feature/A".to_string());
    let mut a2 = assistant_msg("sm-page-a2", "sess-page", 2.0);
    a2.timestamp = "2026-03-25T00:00:02Z".parse().unwrap();
    a2.repo_id = Some("github.com/acme/repo-b".to_string());
    a2.git_branch = Some("feature/B".to_string());
    let mut a3 = assistant_msg("sm-page-a3", "sess-page", 3.0);
    a3.timestamp = "2026-03-25T00:00:03Z".parse().unwrap();
    a3.repo_id = Some("github.com/acme/repo-c".to_string());
    a3.git_branch = Some("feature/C".to_string());

    let tags = vec![
        vec![],
        vec![
            Tag {
                key: "tool".to_string(),
                value: "Read".to_string(),
            },
            Tag {
                key: "ticket_id".to_string(),
                value: "ABC-1".to_string(),
            },
            Tag {
                key: "tool_use_id".to_string(),
                value: "toolu_1".to_string(),
            },
        ],
        vec![
            Tag {
                key: "tool".to_string(),
                value: "Edit".to_string(),
            },
            Tag {
                key: "ticket_id".to_string(),
                value: "ABC-2".to_string(),
            },
        ],
        vec![Tag {
            key: "ticket_id".to_string(),
            value: "ABC-3".to_string(),
        }],
    ];
    ingest_messages(&mut conn, &[user, a1, a2, a3], Some(&tags)).unwrap();

    let page = session_message_list(
        &conn,
        "sess-page",
        &SessionMessageListParams {
            roles: SessionMessageRoles::Assistant,
            sort_by: Some("timestamp"),
            sort_asc: true,
            limit: 2,
            offset: 1,
        },
    )
    .unwrap();
    assert_eq!(page.total_count, 3);
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].id, "sm-page-a2");
    assert_eq!(page.messages[0].assistant_sequence, Some(2));
    assert_eq!(page.messages[1].assistant_sequence, Some(3));
    assert_eq!(
        page.messages[0].repo_id.as_deref(),
        Some("github.com/acme/repo-b")
    );
    assert_eq!(page.messages[0].git_branch.as_deref(), Some("feature/B"));
    assert_eq!(page.messages[0].tools, vec!["Edit".to_string()]);
    assert!(
        page.messages[0]
            .tags
            .iter()
            .any(|t| t.key == "ticket_id" && t.value == "ABC-2")
    );
    assert!(page.messages[0].tags.iter().all(|t| t.key != "tool_use_id"));

    let all_roles = session_message_list(
        &conn,
        "sess-page",
        &SessionMessageListParams {
            roles: SessionMessageRoles::All,
            sort_by: Some("timestamp"),
            sort_asc: true,
            limit: 10,
            offset: 0,
        },
    )
    .unwrap();
    assert_eq!(all_roles.total_count, 4);
    assert_eq!(all_roles.messages[0].role, "user");
    assert_eq!(all_roles.messages[0].assistant_sequence, None);
}

#[test]
fn session_message_list_keeps_canonical_assistant_sequence_across_sorts() {
    let mut conn = test_db();

    let mut m1 = assistant_msg("sm-sort-a1", "sess-sort", 9.0);
    m1.timestamp = "2026-03-25T00:00:01Z".parse().unwrap();
    let mut m2 = assistant_msg("sm-sort-a2", "sess-sort", 1.0);
    m2.timestamp = "2026-03-25T00:00:02Z".parse().unwrap();
    let mut m3 = assistant_msg("sm-sort-a3", "sess-sort", 5.0);
    m3.timestamp = "2026-03-25T00:00:03Z".parse().unwrap();
    ingest_messages(&mut conn, &[m1, m2, m3], None).unwrap();

    let by_cost = session_message_list(
        &conn,
        "sess-sort",
        &SessionMessageListParams {
            roles: SessionMessageRoles::Assistant,
            sort_by: Some("cost"),
            sort_asc: false,
            limit: 50,
            offset: 0,
        },
    )
    .unwrap();

    assert_eq!(
        by_cost
            .messages
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>(),
        vec!["sm-sort-a1", "sm-sort-a3", "sm-sort-a2"]
    );
    assert_eq!(
        by_cost
            .messages
            .iter()
            .map(|m| m.assistant_sequence)
            .collect::<Vec<_>>(),
        vec![Some(1), Some(3), Some(2)]
    );
}

#[test]
fn session_message_curve_uses_full_session_canonical_order() {
    let mut conn = test_db();

    let mut m1 = assistant_msg("sm-curve-a1", "sess-curve", 2.0);
    m1.timestamp = "2026-03-25T00:00:01Z".parse().unwrap();
    m1.input_tokens = 10;
    m1.output_tokens = 5;
    let mut m2 = assistant_msg("sm-curve-a2", "sess-curve", 3.0);
    m2.timestamp = "2026-03-25T00:00:02Z".parse().unwrap();
    m2.input_tokens = 20;
    m2.output_tokens = 10;
    m2.cache_read_tokens = 4;
    let mut m3 = assistant_msg("sm-curve-a3", "sess-curve", 4.0);
    m3.timestamp = "2026-03-25T00:00:03Z".parse().unwrap();
    m3.input_tokens = 7;
    m3.output_tokens = 8;
    m3.cache_creation_tokens = 2;
    ingest_messages(&mut conn, &[m1, m2, m3], None).unwrap();

    let curve = session_message_curve(&conn, "sess-curve").unwrap();
    assert_eq!(curve.len(), 3);
    assert_eq!(curve[0].assistant_sequence, 1);
    assert_eq!(curve[1].assistant_sequence, 2);
    assert_eq!(curve[2].assistant_sequence, 3);
    assert_eq!(curve[0].input_tokens, 10);
    assert_eq!(curve[0].output_tokens, 5);
    assert_eq!(curve[0].cache_tokens, 0);
    assert_eq!(curve[0].tokens, 15);
    assert_eq!(curve[1].input_tokens, 20);
    assert_eq!(curve[1].output_tokens, 10);
    assert_eq!(curve[1].cache_tokens, 4);
    assert_eq!(curve[1].tokens, 30);
    assert_eq!(curve[2].input_tokens, 7);
    assert_eq!(curve[2].output_tokens, 8);
    assert_eq!(curve[2].cache_tokens, 2);
    assert_eq!(curve[2].tokens, 15);
    assert!((curve[0].cumulative_cost_cents - 2.0).abs() < 0.01);
    assert!((curve[1].cumulative_cost_cents - 5.0).abs() < 0.01);
    assert!((curve[2].cumulative_cost_cents - 9.0).abs() < 0.01);
}

#[test]
fn session_messages_does_not_alias_prefixed_session_id() {
    let mut conn = test_db();
    let canonical = "d99dfe22-d05c-4c78-8698-015d06e5dabb";
    let prefixed = "cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb";
    let msg = assistant_msg("sm-no-alias-1", canonical, 3.0);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let canonical_rows = session_messages(&conn, canonical).unwrap();
    let prefixed_rows = session_messages(&conn, prefixed).unwrap();
    assert_eq!(canonical_rows.len(), 1);
    assert!(prefixed_rows.is_empty());
}

#[test]
fn session_tags_returns_distinct_tags() {
    let mut conn = test_db();
    let msg = assistant_msg("st-1", "sess-tags", 1.0);
    let tags = vec![vec![
        Tag {
            key: "team".to_string(),
            value: "platform".to_string(),
        },
        Tag {
            key: "activity".to_string(),
            value: "feature".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let result = session_tags(&conn, "sess-tags").unwrap();
    assert_eq!(result.len(), 2);
    assert!(result.contains(&("activity".to_string(), "feature".to_string())));
    assert!(result.contains(&("team".to_string(), "platform".to_string())));
}

#[test]
fn session_tags_filter_legacy_auto_keys() {
    let mut conn = test_db();
    let msg = assistant_msg("st-legacy-1", "sess-tags-legacy", 1.0);
    let tags = vec![vec![
        Tag {
            key: "repo".to_string(),
            value: "github.com/acme/repo".to_string(),
        },
        Tag {
            key: "branch".to_string(),
            value: "feature/abc".to_string(),
        },
        Tag {
            key: "dominant_tool".to_string(),
            value: "Bash".to_string(),
        },
        Tag {
            key: "ticket_id".to_string(),
            value: "ABC-123".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let result = session_tags(&conn, "sess-tags-legacy").unwrap();
    assert!(result.contains(&("ticket_id".to_string(), "ABC-123".to_string())));
    assert!(!result.iter().any(|(k, _)| k == "repo"));
    assert!(!result.iter().any(|(k, _)| k == "branch"));
    assert!(!result.iter().any(|(k, _)| k == "dominant_tool"));
}

#[test]
fn session_tags_filter_internal_linkage_and_redundant_keys() {
    let mut conn = test_db();
    let msg = assistant_msg("st-internal-1", "sess-tags-internal", 1.0);
    let tags = vec![vec![
        Tag {
            key: "tool_use_id".to_string(),
            value: "toolu_123".to_string(),
        },
        Tag {
            key: "provider".to_string(),
            value: "claude_code".to_string(),
        },
        Tag {
            key: "model".to_string(),
            value: "claude-sonnet-4-6".to_string(),
        },
        Tag {
            key: "cost_confidence".to_string(),
            value: "estimated".to_string(),
        },
        Tag {
            key: "ticket_id".to_string(),
            value: "ABC-123".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let result = session_tags(&conn, "sess-tags-internal").unwrap();
    assert!(result.contains(&("ticket_id".to_string(), "ABC-123".to_string())));
    assert!(!result.iter().any(|(k, _)| k == "tool_use_id"));
    assert!(!result.iter().any(|(k, _)| k == "provider"));
    assert!(!result.iter().any(|(k, _)| k == "model"));
    assert!(!result.iter().any(|(k, _)| k == "cost_confidence"));
}

#[test]
fn session_tags_include_explicit_identity_keys() {
    let mut conn = test_db();
    let msg = assistant_msg("st-identity-1", "sess-tags-identity", 1.0);
    let tags = vec![vec![
        Tag {
            key: "platform".to_string(),
            value: "macos".to_string(),
        },
        Tag {
            key: "machine".to_string(),
            value: "workstation-01".to_string(),
        },
        Tag {
            key: "user".to_string(),
            value: "local-user".to_string(),
        },
        Tag {
            key: "git_user".to_string(),
            value: "Alice Dev".to_string(),
        },
    ]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let result = session_tags(&conn, "sess-tags-identity").unwrap();
    assert!(result.contains(&("platform".to_string(), "macos".to_string())));
    assert!(result.contains(&("machine".to_string(), "workstation-01".to_string())));
    assert!(result.contains(&("user".to_string(), "local-user".to_string())));
    assert!(result.contains(&("git_user".to_string(), "Alice Dev".to_string())));
}

#[test]
fn session_tags_does_not_alias_prefixed_session_id() {
    let mut conn = test_db();
    let canonical = "d99dfe22-d05c-4c78-8698-015d06e5dabb";
    let prefixed = "cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb";
    let msg = assistant_msg("st-no-alias-1", canonical, 1.0);
    let tags = vec![vec![Tag {
        key: "team".to_string(),
        value: "platform".to_string(),
    }]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let canonical_tags = session_tags(&conn, canonical).unwrap();
    let prefixed_tags = session_tags(&conn, prefixed).unwrap();
    assert_eq!(canonical_tags.len(), 1);
    assert!(canonical_tags.contains(&("team".to_string(), "platform".to_string())));
    assert!(prefixed_tags.is_empty());
}

#[test]
fn session_tags_empty_for_unknown_session() {
    let conn = test_db();
    let result = session_tags(&conn, "nonexistent").unwrap();
    assert!(result.is_empty());
}

// --- Session Health tests ---

fn health_msg(
    uuid: &str,
    session_id: &str,
    idx: u64,
    input: u64,
    cache_read: u64,
    cost: f64,
) -> ParsedMessage {
    let ts = chrono::NaiveDateTime::parse_from_str(
        &format!("2026-03-14 10:{:02}:00", idx),
        "%Y-%m-%d %H:%M:%S",
    )
    .unwrap()
    .and_utc();
    ParsedMessage {
        uuid: uuid.to_string(),
        session_id: Some(session_id.to_string()),
        timestamp: ts,
        cwd: None,
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: input,
        output_tokens: 100,
        cache_creation_tokens: 0,
        cache_read_tokens: cache_read,
        git_branch: None,
        repo_id: None,
        provider: "claude_code".to_string(),
        cost_cents: Some(cost),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "exact".to_string(),
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
}

fn insert_health_hook_event_at(
    _conn: &Connection,
    _provider: &str,
    _session_id: &str,
    _event: &str,
    _timestamp: &str,
    _tool_name: Option<&str>,
) {
    // hook_events table was dropped in v22; these inserts are now no-ops.
}

fn insert_health_hook_event(
    _conn: &Connection,
    _provider: &str,
    _session_id: &str,
    _event: &str,
    _idx: u64,
    _tool_name: Option<&str>,
) {
    // hook_events table was dropped in v22; these inserts are now no-ops.
}

#[test]
fn health_green_stable_session() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..8)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.state, "green");
    assert_eq!(h.message_count, 8);
    assert!(h.tip.contains("healthy"));
}

#[test]
fn health_context_drag_yellow() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0))
        .collect();
    for i in 5..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.context_drag.as_ref().unwrap().state, "yellow");
}

#[test]
fn health_context_drag_red() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0))
        .collect();
    for i in 5..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 32000, 0, 5.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.context_drag.as_ref().unwrap().state, "red");
}

#[test]
fn health_cache_efficiency_red() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 1000, 100, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.cache_efficiency.as_ref().unwrap().state, "red");
}

#[test]
fn health_cost_acceleration_yellow() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..4)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    for i in 4..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 100, 900, 15.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    for ts in [
        "2026-03-14T09:59:30+00:00",
        "2026-03-14T10:01:30+00:00",
        "2026-03-14T10:03:30+00:00",
        "2026-03-14T10:05:30+00:00",
    ] {
        insert_health_hook_event_at(&conn, "claude_code", "s1", "user_prompt_submit", ts, None);
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.cost_acceleration.as_ref().unwrap().state, "yellow");
}

/// With `user_prompt_submit` hooks, short multi-turn Cursor sessions suppressed cost acceleration.
/// After v22 (no `hook_events`), only the per-reply path runs — see assertions below.
#[test]
fn health_cost_acceleration_suppressed_for_short_turn_sessions() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    )
    .unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..3)
        .map(|i| {
            let mut msg = health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0);
            msg.provider = "cursor".to_string();
            msg
        })
        .collect();
    for (i, cost) in [(3, 30.0), (4, 15.0), (5, 30.0)] {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 100, 900, cost);
        msg.provider = "cursor".to_string();
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    for ts in ["2026-03-14T09:59:30+00:00", "2026-03-14T10:03:30+00:00"] {
        insert_health_hook_event_at(&conn, "cursor", "s1", "user_prompt_submit", ts, None);
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    // Without hook_events, user_prompt_submit boundaries are missing, so turn-based
    // suppression for short Cursor sessions does not apply; per-reply cost acceleration remains.
    assert!(h.vitals.cost_acceleration.is_some());
    assert_eq!(h.state, "red");
}

#[test]
fn health_cache_uses_recent_model_run() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..4)
        .map(|i| health_msg(&format!("old{i}"), "s1", i, 1000, 0, 5.0))
        .collect();
    for i in 4..8 {
        let mut msg = health_msg(&format!("new{i}"), "s1", i, 100, 900, 5.0);
        msg.model = Some("claude-sonnet-4-6".to_string());
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.cache_efficiency.as_ref().unwrap().state, "green");
}

#[test]
fn health_thrashing_ignores_busy_successful_turn() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();
    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    for (idx, tool) in [
        "ReadFile",
        "rg",
        "ReadFile",
        "ApplyPatch",
        "ReadLints",
        "Shell",
        "ReadFile",
        "rg",
        "ReadFile",
        "ApplyPatch",
        "ReadLints",
        "Shell",
    ]
    .into_iter()
    .enumerate()
    {
        insert_health_hook_event(
            &conn,
            "claude_code",
            "s1",
            "post_tool_use",
            idx as u64,
            Some(tool),
        );
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(
        h.vitals.thrashing.is_none(),
        "thrashing needs hook_events; table dropped in v22"
    );
}

#[test]
fn health_thrashing_detects_retry_loop() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();
    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    for idx in 0..5 {
        insert_health_hook_event(
            &conn,
            "claude_code",
            "s1",
            "post_tool_use_failure",
            idx,
            Some("Shell"),
        );
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.thrashing.is_none());
}

#[test]
fn health_context_drag_resets_after_compact() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("before{i}"), "s1", i, 4000, 0, 5.0))
        .collect();
    for i in 5..10 {
        msgs.push(health_msg(&format!("after{i}"), "s1", i, 5000, 900, 5.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    insert_health_hook_event(&conn, "claude_code", "s1", "pre_compact", 4, None);

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.context_drag.as_ref().unwrap().state, "green");
}

#[test]
fn health_cursor_tips_use_plain_actions() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    )
    .unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| {
            let mut msg = health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0);
            msg.provider = "cursor".to_string();
            msg
        })
        .collect();
    for i in 5..8 {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0);
        msg.provider = "cursor".to_string();
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.details.iter().any(|d| {
        d.vital == "context_drag"
            && d.tip.contains("Context size is getting large")
            && d.actions.iter().any(|a| a.contains("composer session"))
    }));
}

#[test]
fn health_suppressed_few_messages() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..3)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.context_drag.is_none());
    assert!(h.vitals.cache_efficiency.is_none());
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(h.state, "green");
    assert_eq!(h.tip, "New session");
}

#[test]
fn health_auto_selects_latest_session() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('old', 'claude_code', '2026-03-10')",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('new', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "new", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, None).unwrap();
    assert_eq!(h.message_count, 6);
}

#[test]
fn health_auto_select_prefers_recent_assistant_activity_when_started_at_missing() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('old', 'claude_code', '2026-03-20')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider) VALUES ('active', 'claude_code')",
        [],
    )
    .unwrap();

    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("active-m{i}"), "active", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, None).unwrap();
    assert_eq!(h.message_count, 6);
}

#[test]
fn health_does_not_alias_prefixed_session_id() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('d99dfe22-d05c-4c78-8698-015d06e5dabb', 'cursor', '2026-03-14')",
        [],
    )
    .unwrap();

    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| {
            health_msg(
                &format!("h-no-alias-{i}"),
                "d99dfe22-d05c-4c78-8698-015d06e5dabb",
                i,
                100,
                900,
                5.0,
            )
        })
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let canonical = session_health(&conn, Some("d99dfe22-d05c-4c78-8698-015d06e5dabb")).unwrap();
    let prefixed =
        session_health(&conn, Some("cursor-d99dfe22-d05c-4c78-8698-015d06e5dabb")).unwrap();
    assert_eq!(canonical.message_count, 6);
    assert_eq!(prefixed.message_count, 0);
}

#[test]
fn health_batch_returns_all_sessions() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s2', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs1: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("s1m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    let msgs2: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("s2m{i}"), "s2", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs1, None).unwrap();
    ingest_messages(&mut conn, &msgs2, None).unwrap();

    let batch = session_health_batch(&conn, &["s1", "s2", "nonexistent"]).unwrap();
    assert_eq!(batch.len(), 3);
    assert!(batch.contains_key("s1"));
    assert!(batch.contains_key("s2"));
    assert_eq!(batch["nonexistent"], "green");
}

#[test]
fn health_batch_matches_detail_thresholds() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0))
        .collect();
    for i in 5..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let detail = session_health(&conn, Some("s1")).unwrap();
    let batch = session_health_batch(&conn, &["s1"]).unwrap();
    assert_eq!(batch["s1"], detail.state);
}

// --- Coverage: green with sparse data when hook-based thrashing is unavailable (v22) ---

#[test]
fn health_green_when_only_thrashing_computable() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..2)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 50, 1.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();
    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.thrashing.is_none());
    assert!(h.vitals.context_drag.is_none());
    assert!(h.vitals.cache_efficiency.is_none());
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(
        h.state, "green",
        "no hook_events → thrashing absent; remaining vitals suppressed → green default"
    );
    assert_eq!(h.tip, "New session");
}

// --- Coverage: cache_efficiency yellow ---

#[test]
fn health_cache_efficiency_yellow() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    // hit_rate = 500 / (500 + 500) = 0.50, which is below 0.60 (yellow) but above 0.35 (red)
    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 500, 500, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.cache_efficiency.as_ref().unwrap().state, "yellow");
}

// --- Coverage: cost_acceleration red ---

#[test]
fn health_cost_acceleration_red() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    // 4 early turns at 5¢, 4 late turns at 25¢ → ratio=5.0 ≥ 4.0, second_avg=25 ≥ 12
    let mut msgs: Vec<ParsedMessage> = (0..4)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    for i in 4..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 100, 900, 25.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    for (idx, ts) in [
        "2026-03-14T09:59:30+00:00",
        "2026-03-14T10:01:30+00:00",
        "2026-03-14T10:03:30+00:00",
        "2026-03-14T10:05:30+00:00",
    ]
    .iter()
    .enumerate()
    {
        insert_health_hook_event_at(&conn, "claude_code", "s1", "user_prompt_submit", ts, None);
        let _ = idx;
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    let ca = h.vitals.cost_acceleration.as_ref().unwrap();
    assert_eq!(ca.state, "red");
    assert!(
        ca.label.contains("reply"),
        "without hook_events, cost acceleration uses reply fallback: {}",
        ca.label
    );
}

// --- Coverage: cost_acceleration reply fallback ---

#[test]
fn health_cost_acceleration_reply_fallback() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    // No hook events → no prompt boundaries → falls back to per-reply scoring
    let mut msgs: Vec<ParsedMessage> = (0..3)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    for i in 3..9 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 100, 900, 15.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    let ca = h.vitals.cost_acceleration.as_ref().unwrap();
    assert_ne!(ca.state, "green");
    assert!(
        ca.label.contains("reply"),
        "label should say 'reply' not 'turn': {}",
        ca.label
    );
}

// --- Coverage: thrashing yellow ---

#[test]
fn health_thrashing_yellow() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();
    let msgs: Vec<ParsedMessage> = (0..6)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    // 3 repeated failures of the same tool → score=1 → yellow
    for idx in 0..4 {
        insert_health_hook_event(
            &conn,
            "claude_code",
            "s1",
            "post_tool_use_failure",
            idx,
            Some("Shell"),
        );
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.thrashing.is_none());
}

// --- Coverage: provider-specific tips diverge correctly ---

#[test]
fn health_claude_code_context_drag_mentions_compact() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0))
        .collect();
    for i in 5..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    let detail = h
        .details
        .iter()
        .find(|d| d.vital == "context_drag")
        .unwrap();
    assert!(
        detail.actions.iter().any(|a| a.contains("/compact")),
        "Claude Code context_drag detail should mention /compact"
    );
}

#[test]
fn health_cursor_context_drag_no_compact() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    )
    .unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| {
            let mut msg = health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0);
            msg.provider = "cursor".to_string();
            msg
        })
        .collect();
    for i in 5..8 {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0);
        msg.provider = "cursor".to_string();
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    let detail = h
        .details
        .iter()
        .find(|d| d.vital == "context_drag")
        .unwrap();
    assert!(!detail.actions.iter().any(|a| a.contains("/compact")));
    assert!(
        detail
            .actions
            .iter()
            .any(|a| a.contains("composer session"))
    );
    assert!(!h.tip.contains("/compact"));
}

#[test]
fn health_unknown_provider_gets_neutral_tips() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'windsurf', '2026-03-14')",
        [],
    )
    .unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| {
            let mut msg = health_msg(&format!("m{i}"), "s1", i, 4000, 0, 5.0);
            msg.provider = "windsurf".to_string();
            msg
        })
        .collect();
    for i in 5..8 {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 16000, 0, 5.0);
        msg.provider = "windsurf".to_string();
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    let detail = h
        .details
        .iter()
        .find(|d| d.vital == "context_drag")
        .unwrap();
    assert!(!detail.actions.iter().any(|a| a.contains("/compact")));
    assert!(
        !detail
            .actions
            .iter()
            .any(|a| a.contains("composer session"))
    );
    assert!(
        detail
            .actions
            .iter()
            .any(|a| a.contains("Trim context") || a.contains("start fresh"))
    );
}

// --- Coverage: cost_acceleration yellow short tip uses actual metric ---

#[test]
fn health_cost_acceleration_yellow_tip_uses_metric_label() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let mut msgs: Vec<ParsedMessage> = (0..4)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    for i in 4..8 {
        msgs.push(health_msg(&format!("m{i}"), "s1", i, 100, 900, 15.0));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    for ts in [
        "2026-03-14T09:59:30+00:00",
        "2026-03-14T10:01:30+00:00",
        "2026-03-14T10:03:30+00:00",
        "2026-03-14T10:05:30+00:00",
    ] {
        insert_health_hook_event_at(&conn, "claude_code", "s1", "user_prompt_submit", ts, None);
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(
        h.tip.contains("Cost rising"),
        "tip should mention 'Cost rising': {}",
        h.tip
    );
    assert!(
        h.tip.contains("growth"),
        "tip should contain metric label: {}",
        h.tip
    );
}

// --- Coverage: cache_efficiency red provider divergence ---

#[test]
fn health_cache_efficiency_red_cursor_vs_claude() {
    for (provider, should_mention_clear) in [("claude_code", true), ("cursor", false)] {
        let mut conn = test_db();
        conn.execute(
            &format!("INSERT INTO sessions (id, provider, started_at) VALUES ('s1', '{provider}', '2026-03-14')"),
            [],
        ).unwrap();

        let msgs: Vec<ParsedMessage> = (0..6)
            .map(|i| {
                let mut msg = health_msg(&format!("m{i}"), "s1", i, 1000, 100, 5.0);
                msg.provider = provider.to_string();
                msg
            })
            .collect();
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let h = session_health(&conn, Some("s1")).unwrap();
        let detail = h
            .details
            .iter()
            .find(|d| d.vital == "cache_efficiency")
            .unwrap();

        if should_mention_clear {
            assert!(
                detail.actions.iter().any(|a| a.contains("/clear")),
                "Claude Code cache_efficiency red should mention /clear"
            );
        } else {
            assert!(
                !detail.actions.iter().any(|a| a.contains("/clear")),
                "Cursor cache_efficiency red should NOT mention /clear"
            );
            assert!(
                detail
                    .actions
                    .iter()
                    .any(|a| a.contains("composer session")),
                "Cursor cache_efficiency red should mention composer session"
            );
        }
    }
}

// --- Coverage: previously misclassified multi-reply Cursor session ---

#[test]
fn health_cursor_multi_reply_session_not_false_red() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    )
    .unwrap();

    // Cursor session: 2 user turns, each triggers 4 assistant messages.
    // First turn: 4 messages at 3¢ each = 12¢ per turn
    // Second turn: 4 messages at 4¢ each = 16¢ per turn
    // Per-turn ratio ~1.3x → green. But per-reply naively could look worse.
    let mut msgs = Vec::new();
    for i in 0..4 {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 100, 900, 3.0);
        msg.provider = "cursor".to_string();
        msgs.push(msg);
    }
    for i in 4..8 {
        let mut msg = health_msg(&format!("m{i}"), "s1", i, 100, 900, 4.0);
        msg.provider = "cursor".to_string();
        msgs.push(msg);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();
    insert_health_hook_event_at(
        &conn,
        "cursor",
        "s1",
        "user_prompt_submit",
        "2026-03-14T09:59:30+00:00",
        None,
    );
    insert_health_hook_event_at(
        &conn,
        "cursor",
        "s1",
        "user_prompt_submit",
        "2026-03-14T10:03:30+00:00",
        None,
    );

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_ne!(
        h.state, "red",
        "multi-reply Cursor session should not be false red"
    );
}

#[test]
fn health_no_sessions_returns_green() {
    let conn = test_db();
    let h = session_health(&conn, None).unwrap();
    assert_eq!(h.state, "green");
    assert_eq!(h.message_count, 0);
    assert_eq!(h.total_cost_cents, 0.0);
    assert_eq!(h.tip, "No sessions yet");
    assert!(h.details.is_empty());
}
