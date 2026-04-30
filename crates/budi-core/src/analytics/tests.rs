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
    let stats = statusline_stats(&conn, "2026-03-21", "2026-03-14", "2026-02-19", &params).unwrap();
    assert_eq!(stats.cost_1d, 0.0);
    assert_eq!(stats.cost_7d, 0.0);
    assert_eq!(stats.cost_30d, 0.0);
    assert_eq!(stats.today_cost, 0.0);
    assert_eq!(stats.week_cost, 0.0);
    assert_eq!(stats.month_cost, 0.0);
    assert!(stats.session_cost.is_none());
    assert!(stats.branch_cost.is_none());
    assert!(stats.project_cost.is_none());
    assert!(stats.provider_scope.is_none());
}

#[test]
fn statusline_stats_with_data() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    let params = StatuslineParams::default();
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-08", "2026-02-14", &params).unwrap();
    assert!(stats.cost_30d > 0.0);
    assert_eq!(stats.cost_30d, stats.month_cost);
}

#[test]
fn statusline_stats_with_session_filter() {
    let mut conn = test_db();
    ingest_messages(&mut conn, &sample_messages(), None).unwrap();
    let params = StatuslineParams {
        session_id: Some("sess-1".to_string()),
        ..Default::default()
    };
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-08", "2026-02-14", &params).unwrap();
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
    let stats = statusline_stats(&conn, "2026-03-14", "2026-03-08", "2026-02-14", &params).unwrap();
    assert!(stats.branch_cost.is_some());
}

#[test]
fn statusline_stats_branch_cost_scopes_to_repo_id() {
    // Regression guard for #347: developers who use `main` / `master` /
    // `develop` across multiple local repos must see only the current
    // repo's branch spend when `repo_id` is passed alongside `branch`,
    // not a silent sum across every repo with the same branch name.
    let mut conn = test_db();
    let mk = |uuid: &str, session: &str, repo: &str, cost_cents: f64| ParsedMessage {
        uuid: uuid.to_string(),
        session_id: Some(session.to_string()),
        timestamp: "2026-04-15T10:00:00Z".parse().unwrap(),
        cwd: Some("/proj".to_string()),
        role: "assistant".to_string(),
        model: Some("claude-sonnet".to_string()),
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        git_branch: Some("main".to_string()),
        repo_id: Some(repo.to_string()),
        provider: "claude_code".to_string(),
        cost_cents: Some(cost_cents),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "exact".to_string(),
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
    };
    let msgs = vec![
        mk("repo-a-1", "sess-a", "github.com/org/repo-a", 300.0),
        mk("repo-b-1", "sess-b", "github.com/org/repo-b", 400.0),
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let since_1d = "2026-04-15T00:00:00Z";
    let since_7d = "2026-04-10T00:00:00Z";
    let since_30d = "2026-03-20T00:00:00Z";

    // No repo scope → pre-#347 behavior: both repos summed under `main`.
    let blended = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            branch: Some("main".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(blended.branch_cost, Some(7.0));

    // repo-a → only repo-a's `main` spend.
    let repo_a = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            branch: Some("main".to_string()),
            repo_id: Some("github.com/org/repo-a".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(repo_a.branch_cost, Some(3.0));

    // repo-b → only repo-b's `main` spend.
    let repo_b = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            branch: Some("main".to_string()),
            repo_id: Some("github.com/org/repo-b".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(repo_b.branch_cost, Some(4.0));

    // A repo with no matching rows reports $0, not a cross-repo fallback.
    let empty = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            branch: Some("main".to_string()),
            repo_id: Some("github.com/org/does-not-exist".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(empty.branch_cost, Some(0.0));
}

#[test]
fn statusline_stats_with_provider_filter_scopes_all_numeric_fields() {
    // Regression guard for ADR-0088 §4 + #224: the Claude Code statusline
    // must show Claude Code usage only, not blended multi-provider totals.
    let mut conn = test_db();

    let msgs = vec![
        ParsedMessage {
            uuid: "claude-1".to_string(),
            session_id: Some("claude-sess".to_string()),
            timestamp: "2026-04-15T10:00:00Z".parse().unwrap(),
            cwd: Some("/proj/a".to_string()),
            role: "assistant".to_string(),
            model: Some("claude-sonnet".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: Some("main".to_string()),
            repo_id: Some("repo-1".to_string()),
            provider: "claude_code".to_string(),
            cost_cents: Some(500.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
        ParsedMessage {
            uuid: "cursor-1".to_string(),
            session_id: Some("cursor-sess".to_string()),
            timestamp: "2026-04-15T11:00:00Z".parse().unwrap(),
            cwd: Some("/proj/a".to_string()),
            role: "assistant".to_string(),
            model: Some("cursor-gpt-4".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            git_branch: Some("main".to_string()),
            repo_id: Some("repo-1".to_string()),
            provider: "cursor".to_string(),
            cost_cents: Some(700.0),
            session_title: None,
            parent_uuid: None,
            user_name: None,
            machine_name: None,
            cost_confidence: "exact".to_string(),
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
        },
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    // Windows span the ingested data.
    let since_1d = "2026-04-15T00:00:00Z";
    let since_7d = "2026-04-10T00:00:00Z";
    let since_30d = "2026-03-20T00:00:00Z";

    let unscoped = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams::default(),
    )
    .unwrap();
    assert_eq!(unscoped.cost_30d, 12.0); // (500 + 700) / 100
    assert!(unscoped.provider_scope.is_none());

    let claude = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            provider: Some("claude_code".to_string()),
            session_id: Some("claude-sess".to_string()),
            branch: Some("main".to_string()),
            project_dir: Some("/proj/a".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(claude.provider_scope.as_deref(), Some("claude_code"));
    assert_eq!(claude.cost_1d, 5.0);
    assert_eq!(claude.cost_7d, 5.0);
    assert_eq!(claude.cost_30d, 5.0);
    assert_eq!(claude.session_cost, Some(5.0));
    assert_eq!(claude.branch_cost, Some(5.0));
    assert_eq!(claude.project_cost, Some(5.0));
    assert_eq!(claude.active_provider.as_deref(), Some("claude_code"));

    let cursor = statusline_stats(
        &conn,
        since_1d,
        since_7d,
        since_30d,
        &StatuslineParams {
            provider: Some("cursor".to_string()),
            branch: Some("main".to_string()),
            project_dir: Some("/proj/a".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cursor.cost_30d, 7.0);
    assert_eq!(cursor.branch_cost, Some(7.0));
    assert_eq!(cursor.project_cost, Some(7.0));
    assert_eq!(cursor.active_provider.as_deref(), Some("cursor"));
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
    // #482: Agents-block fields — assistant_messages is the pre-8.3.1
    // unit (assistant-only), user_messages is new, total_messages = sum.
    // The Agents block sums back to `UsageSummary.total_messages` via
    // `total_messages`, not `assistant_messages`.
    assert_eq!(cc_stats.assistant_messages, 1);
    assert_eq!(cc_stats.user_messages, 1);
    assert_eq!(cc_stats.total_messages, 2);
    assert_eq!(cu_stats.assistant_messages, 1);
    assert_eq!(cu_stats.user_messages, 1);
    assert_eq!(cu_stats.total_messages, 2);
    // Per-provider totals reconcile to the grand `UsageSummary.total_messages`.
    let summed: u64 = pstats.iter().map(|p| p.total_messages).sum();
    assert_eq!(summed, all.total_messages);

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
        pricing_source: None,
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
        pricing_source: None,
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
        pricing_source: None,
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
        pricing_source: None,
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
        pricing_source: None,
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
        pricing_source: None,
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
        pricing_source: None,
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
fn health_all_na_is_insufficient_data() {
    // Fresh session with only 3 messages — none of the four vitals can be
    // scored yet. Per #441 this must render as `insufficient_data`, not a
    // trust-eroding `green` / "Session healthy".
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
    assert!(h.vitals.thrashing.is_none());
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(h.state, "insufficient_data");
    assert_eq!(h.tip, "Not enough session data yet to assess");
}

#[test]
fn health_three_na_stays_insufficient_even_with_one_green_vital() {
    // Contrived case: four messages scores exactly one vital
    // (cache_efficiency) green, leaving three N/A. Per #441 the session
    // should stay `insufficient_data` — one green reading isn't enough
    // evidence to flip a verdict and never should paint green over N/A rows.
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..4)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 1.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    let scored = [
        h.vitals.context_drag.is_some(),
        h.vitals.cache_efficiency.is_some(),
        h.vitals.thrashing.is_some(),
        h.vitals.cost_acceleration.is_some(),
    ];
    let na_count = scored.iter().filter(|s| !**s).count();
    assert!(
        na_count >= 3,
        "expected at least 3 N/A with 4 messages, got na_count={na_count} scored={scored:?}"
    );
    assert_eq!(h.state, "insufficient_data");
}

#[test]
fn health_green_stays_plain_with_single_na() {
    // Stable 8-message session scores 3 vitals green (context_drag,
    // cache_efficiency, cost_acceleration); thrashing stays None because the
    // v22+ schema drops hook_events. 1 N/A is the normal steady state for
    // every session today — per #441 we resolve in favour of rule 3 ("at
    // least 3 of 4 numeric = plain green"), so this renders plain healthy
    // rather than flagging partial on every working session.
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
    let scored_count = [
        h.vitals.context_drag.is_some(),
        h.vitals.cache_efficiency.is_some(),
        h.vitals.thrashing.is_some(),
        h.vitals.cost_acceleration.is_some(),
    ]
    .iter()
    .filter(|s| **s)
    .count();
    assert_eq!(
        scored_count, 3,
        "this fixture should score exactly 3 vitals; 1 N/A (thrashing)"
    );
    assert_eq!(h.state, "green");
    assert_eq!(h.tip, "Session healthy");
}

#[test]
fn health_green_partial_two_na() {
    // 5 messages, stable, same model. context_drag (needs 5) and
    // cache_efficiency (needs 4) score green; thrashing stays None and
    // cost_acceleration stays None (reply-fallback needs 6 messages). That
    // is 2 N/A — still green (all scored signals are healthy) but the tip
    // must mention both gaps so the verdict is honest.
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    let msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 100, 900, 5.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.context_drag.is_some());
    assert!(h.vitals.cache_efficiency.is_some());
    assert!(h.vitals.thrashing.is_none());
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(h.state, "green");
    assert_eq!(
        h.tip,
        "Session healthy (partial — 2 metrics need more session data)"
    );
}

#[test]
fn health_red_with_many_na_stays_red() {
    // Build a session where cache_efficiency goes red and the other vitals
    // stay N/A — a real issue signal must trump the N/A count. One red
    // vital with three N/A is still `red` so the user doesn't miss the
    // failure indicator.
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();

    // Large input, tiny cache reads → below CACHE_REUSE_RED threshold once
    // enough messages accumulate. Only 5 messages so context_drag (needs 5)
    // barely qualifies, but other metrics remain None.
    let msgs: Vec<ParsedMessage> = (0..5)
        .map(|i| health_msg(&format!("m{i}"), "s1", i, 1000, 100, 1.0))
        .collect();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let h = session_health(&conn, Some("s1")).unwrap();
    assert_eq!(h.vitals.cache_efficiency.as_ref().unwrap().state, "red");
    // Even with thrashing + cost_acceleration N/A, the red signal wins.
    assert_eq!(h.state, "red");
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
    // A session with no messages can't be called healthy (#441); batch mirrors
    // the detail path and reports `insufficient_data` so the list view falls
    // through to a neutral open circle instead of a trust-killer green dot.
    assert_eq!(batch["nonexistent"], "insufficient_data");
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

/// #497 (D-1): the sessions LIST health dot (`session_health_batch`) and
/// the sessions DETAIL health verdict (`session_health`) must agree on
/// the same session's overall state, regardless of which shape the
/// vitals land in. Pre-ticket the list used a separate heuristic that
/// could paint red while detail computed `insufficient_data` for the
/// same session (≥3 of 4 vitals N/A).
///
/// This test parametrizes the two paths across four fixture shapes,
/// including the two specific cases called out in the ticket's
/// acceptance list.
#[test]
fn health_list_detail_parity_across_fixture_shapes() {
    // Case A: ≥ 3 vitals N/A — too few messages / events for any
    // per-vital calculation to score. Expected: insufficient_data.
    {
        let mut conn = test_db();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at) VALUES ('sA', 'claude_code', '2026-03-14')",
            [],
        ).unwrap();
        let msgs: Vec<ParsedMessage> = (0..2)
            .map(|i| health_msg(&format!("a{i}"), "sA", i, 500, 300, 1.0))
            .collect();
        ingest_messages(&mut conn, &msgs, None).unwrap();

        let detail = session_health(&conn, Some("sA")).unwrap();
        let batch = session_health_batch(&conn, &["sA"]).unwrap();
        assert_eq!(
            batch["sA"], detail.state,
            "list ↔ detail disagree for ≥3-N/A fixture (list={}, detail={})",
            batch["sA"], detail.state
        );
        // Pin the verdict itself so a future threshold change that
        // accidentally promotes a two-message session from
        // `insufficient_data` to `green` fails here too.
        assert_eq!(batch["sA"], "insufficient_data");
    }

    // Case B: empty session (zero messages). Same verdict from both
    // paths. Pre-#441 batch returned `green` here; the #441 batch
    // test above covers the non-existent-session case, and this one
    // covers the "session row exists but no messages" case.
    {
        let conn = test_db();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at) VALUES ('sB', 'claude_code', '2026-03-14')",
            [],
        ).unwrap();
        // No ingest_messages call — the session is known but has zero
        // assistant rows.
        let detail = session_health(&conn, Some("sB")).unwrap();
        let batch = session_health_batch(&conn, &["sB"]).unwrap();
        assert_eq!(batch["sB"], detail.state);
        assert_eq!(batch["sB"], "insufficient_data");
    }

    // Case C: enough messages to score at least one vital green.
    // Both paths should agree on the computed state regardless of
    // what that state is.
    {
        let mut conn = test_db();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at) VALUES ('sC', 'claude_code', '2026-03-14')",
            [],
        ).unwrap();
        // 8 messages with stable input sizes — cache-efficiency can
        // score once, context-drag once. Thrashing + cost-accel stay
        // N/A without tool events / cost spikes.
        let msgs: Vec<ParsedMessage> = (0..8)
            .map(|i| health_msg(&format!("c{i}"), "sC", i, 1000, 500, 1.0))
            .collect();
        ingest_messages(&mut conn, &msgs, None).unwrap();
        let detail = session_health(&conn, Some("sC")).unwrap();
        let batch = session_health_batch(&conn, &["sC"]).unwrap();
        assert_eq!(
            batch["sC"], detail.state,
            "list ↔ detail disagree for scored-vitals fixture"
        );
    }
}

// --- Coverage: insufficient_data when no vitals can be scored (v22) ---

#[test]
fn health_insufficient_data_when_all_vitals_unscored() {
    // 2 messages is below every vital's minimum sample requirement, and
    // hook_events were dropped in v22 so thrashing is also absent. Pre-#441
    // this returned plain `green` / "New session" — a trust-killer because
    // the CLI then rendered "GREEN / Session healthy" over four N/A rows.
    // Post-#441 the verdict is `insufficient_data` and the tip says so.
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
    assert_eq!(h.state, "insufficient_data");
    assert_eq!(h.tip, "Not enough session data yet to assess");
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

// --- #382: ingest_messages_with_sync `tail_file` atomic offset upsert ---

fn tailer_assistant_msg(uuid: &str, session: &str, ts: &str) -> ParsedMessage {
    ParsedMessage {
        uuid: uuid.to_string(),
        session_id: Some(session.to_string()),
        timestamp: ts.parse().unwrap(),
        role: "assistant".to_string(),
        provider: "stub".to_string(),
        cost_confidence: "estimated".to_string(),
        pricing_source: None,
        ..Default::default()
    }
}

#[test]
fn ingest_with_tail_file_upserts_offset_in_same_transaction() {
    let mut conn = test_db();
    let path = "/tmp/stub/session-382.jsonl";
    let provider = "stub";

    // Pre-seed the row at 100 so we can prove the inline upsert advances
    // it to the post-batch offset, not just inserts a new row.
    set_tail_offset(&conn, provider, path, 100).unwrap();
    assert_eq!(
        get_tail_offset(&conn, provider, path).unwrap(),
        Some(100),
        "precondition: pre-seeded tail offset"
    );

    let msgs = vec![tailer_assistant_msg(
        "382-a",
        "session-382",
        "2026-04-19T10:00:00Z",
    )];
    let ingested =
        ingest_messages_with_sync(&mut conn, &msgs, None, None, Some((provider, path, 250)))
            .unwrap();
    assert_eq!(ingested, 1, "exactly one new message ingested");

    let stored: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE id = ?1",
            ["382-a"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "message row committed");

    assert_eq!(
        get_tail_offset(&conn, provider, path).unwrap(),
        Some(250),
        "tail_offsets row advanced to the post-batch offset inline with the ingest commit",
    );
}

#[test]
fn ingest_without_tail_file_leaves_tail_offsets_untouched() {
    let mut conn = test_db();
    let path = "/tmp/stub/session-382-noop.jsonl";
    let provider = "stub";

    // Sentinel offset proves we are observing a no-touch, not a coincident
    // re-insert at the same value.
    set_tail_offset(&conn, provider, path, 17).unwrap();

    let msgs = vec![tailer_assistant_msg(
        "382-b",
        "session-382-b",
        "2026-04-19T11:00:00Z",
    )];
    let ingested = ingest_messages_with_sync(&mut conn, &msgs, None, None, None).unwrap();
    assert_eq!(ingested, 1);

    assert_eq!(
        get_tail_offset(&conn, provider, path).unwrap(),
        Some(17),
        "tail_offsets row must not be touched when tail_file is None",
    );
}

#[test]
fn ingest_with_tail_file_writes_offset_atomically_for_empty_message_batch() {
    // Empty message batch + Some(tail_file) is the parser-skip case
    // from process_path: no rows to ingest, but the tailer has still
    // advanced past a parseable region and wants the offset persisted.
    // The single-tx contract should still hold so the offset write
    // rides on the same commit as the (empty) ingest pass.
    let mut conn = test_db();
    let path = "/tmp/stub/session-382-empty.jsonl";
    let provider = "stub";

    let ingested =
        ingest_messages_with_sync(&mut conn, &[], None, None, Some((provider, path, 64))).unwrap();
    assert_eq!(ingested, 0);

    assert_eq!(
        get_tail_offset(&conn, provider, path).unwrap(),
        Some(64),
        "empty-batch ingest with tail_file must still upsert the offset",
    );
}

// ---------------------------------------------------------------------------
// Breakdown reconciliation (#448)
//
// Regression guard for the release-blocker where every breakdown silently
// capped at 30 rows with no grand total, underreporting by ~9% on a real
// `--files 30d` sample. The contract these tests pin down:
//
//   sum(rows) + other.cost_cents == total_cost_cents, to the cent,
//
// for every breakdown view (`--projects/--branches/--tickets/--activities/
// --files/--models/--tag`) across every period (`today/7d/30d`). Plus:
//   * `total_cost_cents` equals the grand total of assistant cost in the
//     window (reconciles with `usage_summary`), because every ranked row
//     spans the full tagged-or-untagged partition.
//   * `paginate_breakdown(rows, 0)` never truncates (0 = unlimited).
// ---------------------------------------------------------------------------

/// Assert that `sum(rows.cost_cents) + other.cost_cents == total_cost_cents`
/// to within 0.01 cent for rounding slack. Used as the reconciliation
/// oracle for every breakdown view.
fn assert_breakdown_reconciles<T: BreakdownRowCost>(
    page: &BreakdownPage<T>,
    expected_total_cents: f64,
    label: &str,
) {
    let rows_cost: f64 = page.rows.iter().map(BreakdownRowCost::cost_cents).sum();
    let other_cost = page.other.as_ref().map(|o| o.cost_cents).unwrap_or(0.0);
    assert!(
        (rows_cost + other_cost - page.total_cost_cents).abs() < 0.01,
        "{label}: rows + other != total_cost_cents ({rows_cost} + {other_cost} vs {})",
        page.total_cost_cents,
    );
    assert!(
        (page.total_cost_cents - expected_total_cents).abs() < 0.01,
        "{label}: total_cost_cents {} diverges from expected {expected_total_cents}",
        page.total_cost_cents,
    );
    // total_rows always equals shown_rows + other.row_count, and
    // paginate_breakdown never produces an `other` row with zero rows in it.
    if let Some(other) = page.other.as_ref() {
        assert!(other.row_count > 0, "{label}: other row must be non-empty");
        assert_eq!(
            page.shown_rows + other.row_count,
            page.total_rows,
            "{label}: shown + other != total_rows",
        );
    } else {
        assert_eq!(
            page.shown_rows, page.total_rows,
            "{label}: other=None implies shown_rows == total_rows",
        );
    }
}

/// Seed 42 tickets of varying cost in a single window so the default
/// limit of 30 necessarily truncates. Each cost is picked to avoid
/// collisions with the untagged bucket's cost.
fn seed_tickets_for_reconciliation(conn: &mut Connection) -> f64 {
    let mut msgs = Vec::new();
    let mut tags = Vec::new();
    let mut total = 0.0;
    for i in 0..42 {
        let cost = 1.0 + i as f64 * 0.37;
        total += cost;
        let uuid = format!("tk-rec-{i}");
        let sid = format!("s-tk-{i}");
        let ticket = format!("RECON-{i}");
        let branch = format!("{ticket}-work");
        let m = ticket_msg(&uuid, &sid, &branch, "repo-rec", cost);
        msgs.push(m);
        tags.push(ticket_tags(&[&ticket]));
    }
    // An untagged assistant message so the `(untagged)` bucket is part
    // of the truncation / reconciliation math.
    let untagged = assistant_msg("tk-rec-untagged", "s-tk-u", 0.73);
    total += 0.73;
    msgs.push(untagged);
    tags.push(Vec::new());

    ingest_messages(conn, &msgs, Some(&tags)).unwrap();
    total
}

#[test]
fn breakdown_tickets_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let expected_total = seed_tickets_for_reconciliation(&mut conn);

    let all = ticket_cost_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();

    // Capped at 30 → `(other)` aggregates the remaining 13 rows.
    let page = paginate_breakdown(all.clone(), 30);
    assert_eq!(page.shown_rows, 30);
    assert_eq!(page.rows.len(), 30);
    assert!(page.other.is_some(), "truncation must surface `(other)`");
    let other = page.other.as_ref().unwrap();
    assert_eq!(
        other.row_count,
        page.total_rows - 30,
        "other.row_count must cover every truncated row",
    );
    assert_breakdown_reconciles(&page, expected_total, "--tickets cap=30");

    // Unlimited → everything in `rows`, nothing in `other`.
    let unlimited = paginate_breakdown(all.clone(), 0);
    assert!(unlimited.other.is_none(), "limit=0 must not truncate");
    assert_eq!(unlimited.shown_rows, unlimited.total_rows);
    assert_breakdown_reconciles(&unlimited, expected_total, "--tickets cap=0");
}

#[test]
fn breakdown_files_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut tags = Vec::new();
    let mut expected = 0.0;
    for i in 0..40 {
        let cost = 0.75 + i as f64 * 0.31;
        expected += cost;
        let uuid = format!("fc-rec-{i}");
        let sid = format!("s-fc-{i}");
        let path = format!("src/generated/file_{i:03}.rs");
        let m = file_msg(&uuid, &sid, "main", "repo-files", cost);
        msgs.push(m);
        tags.push(file_tags(&[&path]));
    }
    // Untagged bucket.
    msgs.push(assistant_msg("fc-rec-untagged", "s-fc-u", 0.41));
    tags.push(Vec::new());
    expected += 0.41;
    ingest_messages(&mut conn, &msgs, Some(&tags)).unwrap();

    let all = file_cost_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--files must emit (other) when truncated"
    );
    assert_breakdown_reconciles(&page, expected, "--files cap=30");
}

#[test]
fn breakdown_activities_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut tags = Vec::new();
    let mut expected = 0.0;
    for i in 0..35 {
        let cost = 0.5 + i as f64 * 0.19;
        expected += cost;
        let uuid = format!("ac-rec-{i}");
        let sid = format!("s-ac-{i}");
        let activity = format!("activity_{i:02}");
        let m = activity_msg(&uuid, &sid, "main", "repo-acts", cost);
        msgs.push(m);
        tags.push(activity_tags(&[&activity]));
    }
    ingest_messages(&mut conn, &msgs, Some(&tags)).unwrap();

    let all = activity_cost_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--activities must emit (other) when truncated",
    );
    assert_breakdown_reconciles(&page, expected, "--activities cap=30");
}

#[test]
fn breakdown_projects_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut expected = 0.0;
    for i in 0..35 {
        let cost = 0.9 + i as f64 * 0.22;
        expected += cost;
        let mut m = assistant_msg(&format!("pr-rec-{i}"), &format!("s-pr-{i}"), cost);
        m.repo_id = Some(format!("github.com/acme/repo-{i:03}"));
        msgs.push(m);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let all = repo_usage_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--projects must emit (other) when truncated",
    );
    assert_breakdown_reconciles(&page, expected, "--projects cap=30");
}

#[test]
fn breakdown_branches_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut expected = 0.0;
    for i in 0..33 {
        let cost = 1.2 + i as f64 * 0.41;
        expected += cost;
        let mut m = assistant_msg(&format!("br-rec-{i}"), &format!("s-br-{i}"), cost);
        m.git_branch = Some(format!("feature/branch-{i:03}"));
        m.repo_id = Some("repo-branches".to_string());
        msgs.push(m);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let all = branch_cost_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--branches must emit (other) when truncated",
    );
    assert_breakdown_reconciles(&page, expected, "--branches cap=30");
}

#[test]
fn breakdown_models_reconcile_with_other_row_when_truncated() {
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut expected = 0.0;
    for i in 0..33 {
        let cost = 0.65 + i as f64 * 0.27;
        expected += cost;
        let mut m = assistant_msg(&format!("md-rec-{i}"), &format!("s-md-{i}"), cost);
        m.model = Some(format!("model-family-{i:03}"));
        msgs.push(m);
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let all = model_usage_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--models must emit (other) when truncated",
    );
    assert_breakdown_reconciles(&page, expected, "--models cap=30");
}

#[test]
fn breakdown_tags_reconcile_with_other_row_when_truncated() {
    // `--tag ticket_id` mirrors `--tickets` but flows through a different
    // code path (`tag_stats_with_filters`), so it gets its own guard.
    let mut conn = test_db();
    let expected = seed_tickets_for_reconciliation(&mut conn);

    let all = tag_stats_with_filters(
        &conn,
        Some("ticket_id"),
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert_eq!(page.shown_rows, 30);
    assert!(
        page.other.is_some(),
        "--tag ticket_id must emit (other) when truncated",
    );
    assert_breakdown_reconciles(&page, expected, "--tag ticket_id cap=30");
}

#[test]
fn paginate_breakdown_no_truncation_when_rows_fit() {
    // <= limit → `other` stays `None`, shown == total, cost reconciles.
    let mut conn = test_db();
    let mut msgs = Vec::new();
    let mut expected = 0.0;
    for i in 0..5 {
        let cost = 1.0 + i as f64;
        expected += cost;
        msgs.push(assistant_msg(
            &format!("fit-{i}"),
            &format!("s-fit-{i}"),
            cost,
        ));
    }
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let all = model_usage_with_filters(
        &conn,
        None,
        None,
        &DimensionFilters::default(),
        BREAKDOWN_FETCH_ALL_LIMIT,
    )
    .unwrap();
    let page = paginate_breakdown(all, 30);
    assert!(page.other.is_none());
    assert_eq!(page.shown_rows, page.total_rows);
    assert_breakdown_reconciles(&page, expected, "fits-under-limit");
}

#[test]
fn breakdown_tickets_reconcile_across_today_7d_and_30d() {
    // Replicates the #448 acceptance: reconciliation to the cent across
    // `today/7d/30d`. We plant tickets at three different anchor dates
    // and query with the corresponding `since` bound so each window's
    // total is a known strict subset of the universe.
    //
    // Fix (#502 / D-4): anchor `now` to noon UTC of today's UTC date
    // rather than the wall clock. Pre-fix the test used `Utc::now()`
    // directly, so a CI run at 00:07 UTC on 2026-04-23 put the today
    // cohort at `now - 1h = 23:07 UTC on 2026-04-22` — the previous
    // UTC day — and the `today_since = midnight UTC of today_date`
    // window filtered every today row out, dropping `shown_rows` from
    // the expected 30 to 0. Noon UTC is > 12h away from midnight on
    // both sides so the `- 1h` anchor stays inside today's UTC day
    // regardless of when the test runs.
    use chrono::{Duration, Utc};

    let mut conn = test_db();
    let now = Utc::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .expect("12:00:00 is a valid time")
        .and_utc();
    // Anchor ticket cohorts in each of the three windows. Each cohort
    // has 40 distinct tickets so the default cap of 30 forces
    // truncation, and each cost is unique (0.5 cent steps) so every row
    // sorts deterministically.
    let mut msgs: Vec<ParsedMessage> = Vec::new();
    let mut tags: Vec<Vec<Tag>> = Vec::new();
    let anchors = [
        (now - Duration::hours(1), "today-"),    // inside today
        (now - Duration::days(3), "seven-d-"),   // inside 7d
        (now - Duration::days(20), "thirty-d-"), // inside 30d
    ];

    // Running totals per window (today ⊂ 7d ⊂ 30d).
    let mut total_today = 0.0;
    let mut total_7d = 0.0;
    let mut total_30d = 0.0;

    for (cohort_idx, (ts, prefix)) in anchors.iter().enumerate() {
        for i in 0..40 {
            let cost = 0.5 + cohort_idx as f64 * 5.0 + i as f64 * 0.11;
            let uuid = format!("recon-{prefix}{i}");
            let sid = format!("s-{prefix}{i}");
            let ticket = format!("RECON-{prefix}{i}");
            let branch = format!("{ticket}-wip");
            let mut m = ticket_msg(&uuid, &sid, &branch, "repo-recon", cost);
            m.timestamp = *ts;
            msgs.push(m);
            tags.push(ticket_tags(&[&ticket]));
            total_30d += cost;
            if cohort_idx <= 1 {
                total_7d += cost;
            }
            if cohort_idx == 0 {
                total_today += cost;
            }
        }
    }
    ingest_messages(&mut conn, &msgs, Some(&tags)).unwrap();

    let today_since = (now - Duration::days(0))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339();
    let since_7d = (now - Duration::days(7)).to_rfc3339();
    let since_30d = (now - Duration::days(30)).to_rfc3339();

    for (since, expected_total, label) in [
        (Some(today_since.as_str()), total_today, "today"),
        (Some(since_7d.as_str()), total_7d, "7d"),
        (Some(since_30d.as_str()), total_30d, "30d"),
    ] {
        let all = ticket_cost_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all, 30);
        assert_eq!(
            page.shown_rows, 30,
            "{label}: expected 30 rows rendered, got {}",
            page.shown_rows,
        );
        assert!(
            page.other.is_some(),
            "{label}: expected `(other)` since cohort has 40 tickets",
        );
        assert_breakdown_reconciles(&page, expected_total, label);
    }
}

#[test]
fn breakdown_other_label_is_stable_wire_value() {
    // Scripts keying off `(other)` must stay stable — guarding the
    // constant here prevents an accidental rename from breaking
    // downstream reconciliation tooling.
    assert_eq!(BREAKDOWN_OTHER_LABEL, "(other)");
}

// ─── #484 RC-2 property test: reconciliation under fractional cents ───────
//
// The 2026-04-22 audit found `sum(rows) + other - total_cost_cents`
// diverged by 1-4¢ in most views and by 22¢ on `--models -p 30d`. The
// reconciliation tests above (`breakdown_*_reconcile_with_other_row_*`)
// seed integer-cent fixtures (`cost = 0.5 + idx * 0.37`, etc.), which
// produce exact f64 sums by accident — those tests couldn't catch the
// float-rounding-in-aggregation path that live data actually exercises.
//
// This property test seeds ≥ 100 rows whose per-row `cost_cents` is
// derived from the same token-count × rate-per-million arithmetic that
// `CostEnricher` runs at ingest (plus a tiny jitter to guarantee
// non-integer values). It sweeps every breakdown view × `today / 7d /
// 30d` and asserts the reconciliation contract holds TO THE CENT. The
// contract is load-bearing for the `#448` "sum reconciles to grand
// total" shape every downstream consumer depends on.

/// Row shape for `seed_fractional_cents_for_reconciliation`. Each row
/// contributes a unique dimension value on every axis so the breakdown
/// can't collapse rows by accident. The arg count mirrors the
/// breakdown axes this property test sweeps (repo, branch, ticket,
/// activity, file, model); the clippy allow is intentional.
#[allow(clippy::too_many_arguments)]
fn fractional_cost_msg(
    idx: usize,
    ts: chrono::DateTime<chrono::Utc>,
    ticket: &str,
    branch: &str,
    repo: &str,
    model: &str,
    cwd: &str,
    cost_cents: f64,
) -> ParsedMessage {
    let mut m = assistant_msg(
        &format!("recon-frac-{idx}"),
        &format!("s-recon-frac-{idx}"),
        cost_cents,
    );
    m.timestamp = ts;
    m.git_branch = Some(branch.to_string());
    m.repo_id = Some(repo.to_string());
    m.model = Some(model.to_string());
    m.cwd = Some(cwd.to_string());
    m.cost_confidence = "estimated".to_string();
    let _ = ticket; // ticket is attached via Tag; kept as param for symmetry.
    m
}

/// Seed `count` assistant messages whose per-row cost_cents is
/// deliberately non-integer (derived from `input_tokens * 15 /
/// 1_000_000 × jitter`, the same fractional-cents arithmetic that
/// `CostEnricher` runs at ingest). Every dimension axis gets a unique
/// value per row so `--projects/--branches/--tickets/--activities/
/// --files/--models` all rank 100-way. Returns total cost in cents.
fn seed_fractional_cents_for_reconciliation(
    conn: &mut Connection,
    now: chrono::DateTime<chrono::Utc>,
    count: usize,
) -> f64 {
    use chrono::Duration;
    let mut msgs = Vec::new();
    let mut tags = Vec::new();
    let mut total = 0.0;
    for i in 0..count {
        // Spread rows across today / 7d / 30d windows. Anchors mirror
        // the `breakdown_tickets_reconcile_across_today_7d_and_30d`
        // fixture: today-cohort inside today, 7d-cohort at -3d, 30d-
        // cohort at -20d.
        let ts = match i % 3 {
            0 => now - Duration::hours(1),
            1 => now - Duration::days(3),
            _ => now - Duration::days(20),
        };
        // `input_tokens × rate-per-million` is where CostEnricher
        // introduces f64 sub-cent remainders. Use $15 / M (Claude
        // Sonnet-ish output rate) for input-tokens proxy; multiply by
        // a non-integer jitter per row so no two rows share a cost.
        let input_tokens = 10_000u64 + (i as u64 * 37);
        let jitter = 1.0 + (i as f64 * 0.00073);
        let cost = (input_tokens as f64) * 15.0 / 1_000_000.0 * jitter * 100.0; // dollars → cents
        total += cost;
        let ticket = format!("RECON-FRAC-{i:03}");
        let branch = format!("feat/recon-frac-{i:03}");
        let repo = format!("repo-recon-{:02}", i % 10);
        let model = format!(
            "claude-recon-{}-sonnet",
            match i % 4 {
                0 => "a",
                1 => "b",
                2 => "c",
                _ => "d",
            }
        );
        let cwd = format!("/tmp/recon-{repo}/src/module-{i:03}.rs");
        let m = fractional_cost_msg(i, ts, &ticket, &branch, &repo, &model, &cwd, cost);
        msgs.push(m);
        tags.push(vec![
            Tag {
                key: "ticket_id".to_string(),
                value: ticket.clone(),
            },
            Tag {
                key: "activity".to_string(),
                value: format!("bucket-{}", i % 7),
            },
            Tag {
                key: "file".to_string(),
                value: cwd.clone(),
            },
        ]);
    }
    ingest_messages(conn, &msgs, Some(&tags)).unwrap();
    total
}

/// RC-2 acceptance: `sum(rows.cost_cents) + other.cost_cents ==
/// total_cost_cents` for every breakdown view × period, under
/// fractional per-row costs. Pins the contract at 1/1000 cent tolerance
/// (tighter than the 1/100 cent tolerance the existing reconciliation
/// tests use).
#[test]
fn breakdown_reconciles_under_fractional_per_row_costs() {
    use chrono::Utc;
    let mut conn = test_db();
    // Anchor `now` at noon UTC of the current UTC date so the today
    // cohort at `now - 1h` stays inside today's UTC window regardless
    // of when the test runs (see #502 fix for the same class of flake).
    let now = Utc::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let _total_30d = seed_fractional_cents_for_reconciliation(&mut conn, now, 120);

    let today_since = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339();
    let since_7d = (now - chrono::Duration::days(7)).to_rfc3339();
    let since_30d = (now - chrono::Duration::days(30)).to_rfc3339();

    let windows: [(&str, Option<&str>); 3] = [
        ("today", Some(today_since.as_str())),
        ("7d", Some(since_7d.as_str())),
        ("30d", Some(since_30d.as_str())),
    ];

    for (window_label, since) in windows {
        // --projects
        let all_projects = repo_usage_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_projects, 30);
        assert_breakdown_reconciles_tight(&page, &format!("projects/{window_label}"));

        // --branches
        let all_branches = branch_cost_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_branches, 30);
        assert_breakdown_reconciles_tight(&page, &format!("branches/{window_label}"));

        // --tickets
        let all_tickets = ticket_cost_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_tickets, 30);
        assert_breakdown_reconciles_tight(&page, &format!("tickets/{window_label}"));

        // --activities
        let all_activities = activity_cost_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_activities, 30);
        assert_breakdown_reconciles_tight(&page, &format!("activities/{window_label}"));

        // --files
        let all_files = file_cost_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_files, 30);
        assert_breakdown_reconciles_tight(&page, &format!("files/{window_label}"));

        // --models
        let all_models = model_usage_with_filters(
            &conn,
            since,
            None,
            &DimensionFilters::default(),
            BREAKDOWN_FETCH_ALL_LIMIT,
        )
        .unwrap();
        let page = paginate_breakdown(all_models, 30);
        assert_breakdown_reconciles_tight(&page, &format!("models/{window_label}"));
    }
}

/// Tight reconciliation: `sum(rows.cost_cents) + other.cost_cents ==
/// total_cost_cents` to within 0.0005 cents (effectively exact). This
/// is the contract the #484 audit expected; the pre-8.3.1
/// `paginate_breakdown` shape drifted by up to a few cents due to f64
/// associativity between `sum(all_rows)` and `sum(kept) + sum(rest)`.
fn assert_breakdown_reconciles_tight<T: BreakdownRowCost>(page: &BreakdownPage<T>, label: &str) {
    let rows_cost: f64 = page.rows.iter().map(BreakdownRowCost::cost_cents).sum();
    let other_cost = page.other.as_ref().map(|o| o.cost_cents).unwrap_or(0.0);
    let delta = (rows_cost + other_cost - page.total_cost_cents).abs();
    assert!(
        delta < 0.0005,
        "{label}: rows + other != total_cost_cents to the cent ({rows_cost} + {other_cost} = {} vs {}, delta = {})",
        rows_cost + other_cost,
        page.total_cost_cents,
        delta,
    );
    if let Some(other) = page.other.as_ref() {
        assert!(
            other.row_count > 0,
            "{label}: other row must be non-empty when Some(other)"
        );
        assert_eq!(
            page.shown_rows + other.row_count,
            page.total_rows,
            "{label}: shown + other != total_rows",
        );
    } else {
        assert_eq!(
            page.shown_rows, page.total_rows,
            "{label}: other=None implies shown_rows == total_rows",
        );
    }
}

// ─── #452 text-vs-JSON parity property tests ─────────────────────────────────
//
// The audit reported a 74-message gap between `budi stats` text output and
// `budi stats --format json` for the same query, plus a `--provider cursor`
// count that exceeded the unfiltered count. We could not reproduce either on
// a second machine. Code-read findings:
//
//   1. The text and JSON CLI paths both call the *same* daemon endpoint
//      (`GET /analytics/summary`) backed by `usage_summary_with_filters`.
//      Within a single daemon invocation, both paths see identical data —
//      they cannot disagree by construction. The reported 74-message gap
//      must come from two separate CLI invocations taking separate
//      snapshots while a live tailer ingests in between.
//
//   2. `usage_summary_with_filters` and `estimate_cost_with_filters` apply
//      the same provider predicate (`COALESCE(provider, 'claude_code') = ?`)
//      to the same WHERE clause that drives both the row count and the cost
//      sum. There is no path that filters cost without filtering count, or
//      vice versa, on the message-table query.
//
//   3. The rollup-path (`usage_summary_from_rollups`) uses `provider = ?`
//      without the COALESCE wrapper. Rows with NULL provider in the rollup
//      tables would be excluded from any provider-filtered query — but this
//      can only make `filtered <= unfiltered`, never the other direction
//      the audit reports.
//
// The property tests below pin contract (2) and (3) at the SQL layer. If the
// audit's anomaly ever reproduces, we file a follow-up bug rather than chase
// the 8.2.1 phantom.

/// Build an assistant message attributed to a specific provider. The
/// `--provider` filter scope tests need to seed cohorts from cursor /
/// codex / claude_code in the same window so the property
/// `filtered <= unfiltered` is meaningful.
fn provider_msg(uuid: &str, session: &str, provider: &str, cost_cents: f64) -> ParsedMessage {
    ParsedMessage {
        uuid: uuid.to_string(),
        session_id: Some(session.to_string()),
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
        provider: provider.to_string(),
        cost_cents: Some(cost_cents),
        session_title: None,
        parent_uuid: None,
        user_name: None,
        machine_name: None,
        cost_confidence: "exact".to_string(),
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
    }
}

#[test]
fn provider_filtered_summary_count_is_at_most_unfiltered_count() {
    // #452 acceptance: for any provider P, the message count under
    // `--provider P` must be <= the unfiltered count. Pre-#452 the
    // audit reported `--provider cursor` returning 184 msgs vs 115
    // unfiltered, which is mathematically impossible if the same
    // predicate is applied to both queries. Pin the math here.
    let mut conn = test_db();
    let msgs = vec![
        provider_msg("p-cursor-1", "s-cursor", "cursor", 10.0),
        provider_msg("p-cursor-2", "s-cursor", "cursor", 20.0),
        provider_msg("p-codex-1", "s-codex", "codex", 30.0),
        provider_msg("p-codex-2", "s-codex", "codex", 40.0),
        provider_msg("p-codex-3", "s-codex", "codex", 50.0),
        provider_msg("p-claude-1", "s-claude", "claude_code", 60.0),
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let unfiltered =
        usage_summary_with_filters(&conn, None, None, None, &DimensionFilters::default()).unwrap();
    assert_eq!(unfiltered.total_messages, 6);

    for provider in ["cursor", "codex", "claude_code", "copilot_cli", "openai"] {
        let filtered = usage_summary_with_filters(
            &conn,
            None,
            None,
            Some(provider),
            &DimensionFilters::default(),
        )
        .unwrap();
        assert!(
            filtered.total_messages <= unfiltered.total_messages,
            "--provider {provider} returned {} messages, more than unfiltered {} (audit hypothesis B)",
            filtered.total_messages,
            unfiltered.total_messages,
        );
    }
}

#[test]
fn provider_filtered_summary_partitions_by_provider_to_the_message() {
    // #452 acceptance: summing `summary(--provider P).total_messages`
    // across every provider in the window must equal the unfiltered
    // count exactly. This proves the WHERE clause partitions cleanly
    // — no rows are double-counted or dropped under filtering.
    let mut conn = test_db();
    let msgs = vec![
        provider_msg("part-cursor-1", "s1", "cursor", 11.0),
        provider_msg("part-cursor-2", "s1", "cursor", 12.0),
        provider_msg("part-cursor-3", "s2", "cursor", 13.0),
        provider_msg("part-codex-1", "s3", "codex", 21.0),
        provider_msg("part-codex-2", "s3", "codex", 22.0),
        provider_msg("part-claude-1", "s4", "claude_code", 31.0),
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let unfiltered =
        usage_summary_with_filters(&conn, None, None, None, &DimensionFilters::default()).unwrap();

    let providers = ["cursor", "codex", "claude_code"];
    let summed: u64 = providers
        .iter()
        .map(|p| {
            usage_summary_with_filters(&conn, None, None, Some(p), &DimensionFilters::default())
                .unwrap()
                .total_messages
        })
        .sum();
    assert_eq!(
        summed, unfiltered.total_messages,
        "sum of per-provider message counts must equal the unfiltered total — partitioning bug if not"
    );
}

#[test]
fn provider_filtered_cost_partitions_by_provider_to_the_cent() {
    // #452 acceptance companion: the same predicate applied to the
    // cost query must partition the cost sum exactly. Pre-#452 the
    // audit reported `--provider cursor` returning the full
    // unfiltered cost ($113.87) — that's only possible if the cost
    // predicate is a no-op while the count predicate works. Pin the
    // math.
    let mut conn = test_db();
    let msgs = vec![
        provider_msg("cost-cursor-1", "s1", "cursor", 11.5),
        provider_msg("cost-cursor-2", "s1", "cursor", 12.5),
        provider_msg("cost-codex-1", "s2", "codex", 21.0),
        provider_msg("cost-codex-2", "s2", "codex", 22.0),
        provider_msg("cost-claude-1", "s3", "claude_code", 31.25),
    ];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let unfiltered = crate::cost::estimate_cost_with_filters(
        &conn,
        None,
        None,
        None,
        &DimensionFilters::default(),
    )
    .unwrap();

    let providers = ["cursor", "codex", "claude_code"];
    let summed_cents: f64 = providers
        .iter()
        .map(|p| {
            crate::cost::estimate_cost_with_filters(
                &conn,
                None,
                None,
                Some(p),
                &DimensionFilters::default(),
            )
            .unwrap()
            .total_cost
        })
        .sum::<f64>();

    // total_cost is in dollars (cents/100). Compare to one cent of
    // tolerance so floating-point rounding doesn't flake the test.
    assert!(
        (summed_cents - unfiltered.total_cost).abs() < 0.01,
        "sum of per-provider cost must equal unfiltered total ({summed_cents} vs {})",
        unfiltered.total_cost,
    );

    // And every per-provider sum must be <= unfiltered (the dual of
    // hypothesis B from the audit, applied to cost).
    for p in &providers {
        let filtered = crate::cost::estimate_cost_with_filters(
            &conn,
            None,
            None,
            Some(p),
            &DimensionFilters::default(),
        )
        .unwrap();
        assert!(
            filtered.total_cost <= unfiltered.total_cost + 0.01,
            "--provider {p} cost ({}) exceeded unfiltered cost ({})",
            filtered.total_cost,
            unfiltered.total_cost,
        );
    }
}

#[test]
fn unknown_provider_filter_yields_zero_messages_and_zero_cost() {
    // Defensive: a provider value that doesn't match any row in the
    // window must produce zero messages and zero cost. The CLI
    // `normalize_provider` rejects unknown providers up-front, but
    // the SQL layer should still degrade gracefully if a stale alias
    // ever sneaks through.
    let mut conn = test_db();
    let msgs = vec![provider_msg("u-cursor-1", "s1", "cursor", 5.0)];
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let summary = usage_summary_with_filters(
        &conn,
        None,
        None,
        Some("ghost_provider_that_does_not_exist"),
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(summary.total_messages, 0);
    assert_eq!(summary.total_cost_cents, 0.0);

    let cost = crate::cost::estimate_cost_with_filters(
        &conn,
        None,
        None,
        Some("ghost_provider_that_does_not_exist"),
        &DimensionFilters::default(),
    )
    .unwrap();
    assert_eq!(cost.total_cost, 0.0);
}

// ─── #496 D-3: short-UUID prefix resolver contract ──────────────────────────

#[test]
fn resolve_session_id_covers_full_prefix_empty_and_ambiguous() {
    // Ticket acceptance: full-uuid hit, short-prefix unique hit,
    // short-prefix multi-hit (ambiguous), short-prefix no-hit. Every
    // future `--session <ID>` surface inherits these four paths via
    // `budi_core::analytics::resolve_session_id`, which is the shared
    // resolver the daemon's `resolve_sid` helper wraps.
    let mut conn = test_db();
    let session_full = "670b9539-aaaa-bbbb-cccc-111122223333";
    // One other session sharing a 4-char prefix with the full id so
    // `670b9539` resolves uniquely, `670b` is ambiguous, and
    // `deadbeef` matches nothing.
    let session_alt = "670bdead-1111-2222-3333-444455556677";
    // Seed two assistant rows — one per session. ingest_messages also
    // seeds the `sessions` table via the pipeline-independent path we
    // rely on for resolve_session_id's subquery.
    let mut m1 = assistant_msg("s-d3-1", session_full, 1.0);
    m1.cwd = Some("/tmp/d3-full".to_string());
    let mut m2 = assistant_msg("s-d3-2", session_alt, 1.0);
    m2.cwd = Some("/tmp/d3-alt".to_string());
    ingest_messages(&mut conn, &[m1, m2], None).unwrap();

    // Full UUID hit.
    let full_hit = resolve_session_id(&conn, session_full).unwrap();
    assert_eq!(full_hit.as_deref(), Some(session_full));

    // Short-prefix unique hit — the 8-char prefix `670b9539` only
    // matches `session_full`.
    let short_hit = resolve_session_id(&conn, "670b9539").unwrap();
    assert_eq!(short_hit.as_deref(), Some(session_full));

    // Short-prefix multi-hit — `670b` matches both seeded sessions.
    let ambig = resolve_session_id(&conn, "670b").unwrap_err();
    let msg = format!("{ambig:#}");
    assert!(
        msg.contains("ambiguous session prefix"),
        "ambiguous prefix should surface as an error, got {msg:?}",
    );

    // Short-prefix no-hit.
    let miss = resolve_session_id(&conn, "deadbeef").unwrap();
    assert!(
        miss.is_none(),
        "no-match prefix should return Ok(None), got {miss:?}",
    );

    // Empty prefix: `LIKE '' || '%'` matches every row, so both
    // seeded sessions surface and the resolver correctly flags it as
    // ambiguous. Worth pinning since the daemon route now passes the
    // raw query-string value through without trimming.
    let empty = resolve_session_id(&conn, "").unwrap_err();
    let msg = format!("{empty:#}");
    assert!(
        msg.contains("ambiguous session prefix"),
        "empty prefix must not silently return a random session, got {msg:?}",
    );
}

/// #569: a fresh ingest path for non-cursor providers (claude_code, codex)
/// must populate `sessions.started_at` / `sessions.ended_at` so that
/// `cloud_sync::fetch_session_summaries` can pick up the row. Pre-fix, the
/// stub session row was inserted with only `(id, provider)` and the cloud
/// sync predicate `started_at IS NOT NULL` filtered it out forever.
#[test]
fn ingest_populates_session_timestamps_for_claude_code() {
    let mut conn = test_db();
    let mut early = assistant_msg("cc-1", "s-cc-569", 1.0);
    early.timestamp = "2026-04-28T09:00:00Z".parse().unwrap();
    let mut late = assistant_msg("cc-2", "s-cc-569", 2.0);
    late.timestamp = "2026-04-28T10:30:00Z".parse().unwrap();

    ingest_messages(&mut conn, &[early, late], None).unwrap();

    let (started, ended): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-cc-569'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(started.as_deref(), Some("2026-04-28T09:00:00+00:00"));
    assert_eq!(ended.as_deref(), Some("2026-04-28T10:30:00+00:00"));
}

/// #569: a session whose row was inserted with NULL timestamps by older
/// code must get healed when fresh messages for the same session arrive.
/// COALESCE means we fill the holes without overwriting a value that some
/// other source (e.g. cursor's composer-header repair) already set.
#[test]
fn ingest_heals_stranded_session_when_new_message_arrives() {
    let mut conn = test_db();
    // Simulate the legacy stub-only row: just (id, provider).
    conn.execute(
        "INSERT INTO sessions (id, provider) VALUES ('s-stranded', 'claude_code')",
        [],
    )
    .unwrap();

    let mut msg = assistant_msg("strand-1", "s-stranded", 1.0);
    msg.timestamp = "2026-04-28T09:00:00Z".parse().unwrap();
    ingest_messages(&mut conn, &[msg], None).unwrap();

    let (started, ended): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-stranded'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(started.as_deref(), Some("2026-04-28T09:00:00+00:00"));
    assert_eq!(ended.as_deref(), Some("2026-04-28T09:00:00+00:00"));
}

/// #569: the standalone repair pass heals legacy stranded sessions even
/// when no new messages for them arrive. This is the workhorse for user
/// DBs that already accumulated thousands of NULL-timestamp rows.
#[test]
fn migration_backfills_session_timestamps_from_messages() {
    let conn = test_db();
    // Two stranded sessions with messages already in place but no
    // timestamps on the session rows (older bug).
    conn.execute(
        "INSERT INTO sessions (id, provider) VALUES ('s-cc', 'claude_code')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider) VALUES ('s-codex', 'codex')",
        [],
    )
    .unwrap();
    // Insert messages directly so the per-batch ingest backfill can't
    // mask the test — we want to prove the repair pass alone heals these.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, provider)
         VALUES ('m-cc-1', 's-cc', 'assistant', '2026-04-28T09:00:00+00:00', 'claude_code'),
                ('m-cc-2', 's-cc', 'assistant', '2026-04-28T11:00:00+00:00', 'claude_code'),
                ('m-cx-1', 's-codex', 'user', '2026-04-29T12:00:00+00:00', 'codex')",
        [],
    )
    .unwrap();

    let healed = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(healed, 2);

    let (cc_start, cc_end): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-cc'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(cc_start.as_deref(), Some("2026-04-28T09:00:00+00:00"));
    assert_eq!(cc_end.as_deref(), Some("2026-04-28T11:00:00+00:00"));

    let (cx_start, cx_end): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-codex'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(cx_start.as_deref(), Some("2026-04-29T12:00:00+00:00"));
    assert_eq!(cx_end.as_deref(), Some("2026-04-29T12:00:00+00:00"));

    // Idempotent: subsequent run touches nothing.
    let again = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(again, 0);
}

/// #569 / #578: `started_at` is immutable so the repair pass must not
/// clobber an already-populated value (e.g. cursor's composer-header
/// repair). `ended_at` *does* advance to MAX(messages.timestamp) — pre-#578
/// the heal froze it at the first tick's MAX so every active session was
/// rendered as `<1m` on the cloud.
#[test]
fn backfill_preserves_started_at_but_advances_ended_at() {
    let conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at)
         VALUES ('s-cursor', 'cursor', '2026-04-26T08:00:00+00:00', '2026-04-26T09:00:00+00:00')",
        [],
    )
    .unwrap();
    // Messages span a wider window. `started_at` must stay at '08' (cursor
    // chose this from composer headers and it's older than MIN(timestamp));
    // `ended_at` must advance to '10' so cloud Sessions doesn't freeze the
    // duration at first-tick MAX.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, provider)
         VALUES ('m-1', 's-cursor', 'user', '2026-04-26T07:00:00+00:00', 'cursor'),
                ('m-2', 's-cursor', 'assistant', '2026-04-26T10:00:00+00:00', 'cursor')",
        [],
    )
    .unwrap();

    let healed = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(healed, 1);

    let (start, end): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-cursor'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(start.as_deref(), Some("2026-04-26T08:00:00+00:00"));
    assert_eq!(end.as_deref(), Some("2026-04-26T10:00:00+00:00"));

    // Idempotent: a second run does not re-touch the row now that
    // ended_at == MAX(messages.timestamp).
    let again = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(again, 0);
}

/// #578: regression guard. Pre-fix `COALESCE(ended_at, MAX)` froze ended_at
/// at the first tick's MAX, so every active session showed `<1m` on the
/// cloud. Post-fix, fresh messages arriving for a session whose `ended_at`
/// was already populated must extend it to the new MAX(timestamp).
#[test]
fn backfill_advances_ended_at_for_active_session() {
    let conn = test_db();
    // Simulate the state right after the first ingest tick: started_at and
    // ended_at both populated with the first observed message timestamp.
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at)
         VALUES ('s-active', 'claude_code',
                 '2026-04-30T02:37:07+00:00',
                 '2026-04-30T02:37:08+00:00')",
        [],
    )
    .unwrap();
    // 30 minutes later, more messages have streamed in.
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, provider)
         VALUES ('m-1', 's-active', 'user', '2026-04-30T02:37:07+00:00', 'claude_code'),
                ('m-2', 's-active', 'assistant', '2026-04-30T02:37:08+00:00', 'claude_code'),
                ('m-3', 's-active', 'assistant', '2026-04-30T03:11:44+00:00', 'claude_code')",
        [],
    )
    .unwrap();

    let healed = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(healed, 1);

    let (start, end): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT started_at, ended_at FROM sessions WHERE id = 's-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    // started_at preserved (immutable).
    assert_eq!(start.as_deref(), Some("2026-04-30T02:37:07+00:00"));
    // ended_at advanced to the new MAX.
    assert_eq!(end.as_deref(), Some("2026-04-30T03:11:44+00:00"));

    // Idempotent now that ended_at == MAX.
    let again = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(again, 0);
}

/// #577: the repair pass backfills `repo_id` / `git_branch` from the
/// linked messages so the cloud Sessions table can render a real repo /
/// branch instead of `(unknown)` / `-`. Pre-#577 the heal pass only
/// touched `started_at` / `ended_at`, leaving every claude_code / codex
/// session row's repo and branch NULL.
#[test]
fn backfill_repo_and_branch_from_messages() {
    let conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider) VALUES ('s-cc', 'claude_code'),
                                                    ('s-cx', 'codex')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, provider, repo_id, git_branch)
         VALUES ('m-cc-1', 's-cc', 'user', '2026-04-30T09:00:00+00:00', 'claude_code',
                  'github.com/siropkin/budi', 'main'),
                ('m-cc-2', 's-cc', 'assistant', '2026-04-30T09:30:00+00:00', 'claude_code',
                  'github.com/siropkin/budi', 'fix-577'),
                ('m-cx-1', 's-cx', 'user', '2026-04-30T10:00:00+00:00', 'codex',
                  'github.com/siropkin/codex-experiments', 'master')",
        [],
    )
    .unwrap();

    let healed = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(healed, 2);

    // claude_code session: most-recent message wins for branch (so a
    // mid-session branch switch is reflected).
    let (cc_repo, cc_branch): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, git_branch FROM sessions WHERE id = 's-cc'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(cc_repo.as_deref(), Some("github.com/siropkin/budi"));
    assert_eq!(cc_branch.as_deref(), Some("fix-577"));

    let (cx_repo, cx_branch): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, git_branch FROM sessions WHERE id = 's-cx'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        cx_repo.as_deref(),
        Some("github.com/siropkin/codex-experiments")
    );
    assert_eq!(cx_branch.as_deref(), Some("master"));

    // Idempotent.
    let again = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(again, 0);
}

/// #577: a session row with `repo_id` / `git_branch` already populated
/// (e.g. by a future provider-authoritative writer) is preserved by the
/// heal — COALESCE / NULLIF only fills holes.
#[test]
fn backfill_preserves_already_populated_repo_and_branch() {
    let conn = test_db();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, repo_id, git_branch)
         VALUES ('s-set', 'claude_code',
                 '2026-04-30T09:00:00+00:00',
                 '2026-04-30T09:30:00+00:00',
                 'github.com/example/authoritative-repo',
                 'authoritative-branch')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, provider, repo_id, git_branch)
         VALUES ('m-1', 's-set', 'user', '2026-04-30T09:00:00+00:00', 'claude_code',
                  'github.com/example/some-other-repo', 'some-other-branch'),
                ('m-2', 's-set', 'assistant', '2026-04-30T09:30:00+00:00', 'claude_code',
                  'github.com/example/some-other-repo', 'some-other-branch')",
        [],
    )
    .unwrap();

    let healed = crate::migration::backfill_session_timestamps_from_messages(&conn).unwrap();
    assert_eq!(healed, 0);

    let (repo, branch): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT repo_id, git_branch FROM sessions WHERE id = 's-set'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        repo.as_deref(),
        Some("github.com/example/authoritative-repo")
    );
    assert_eq!(branch.as_deref(), Some("authoritative-branch"));
}
