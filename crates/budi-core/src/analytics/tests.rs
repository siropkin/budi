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
    assert!(tables.contains(&"hook_events".to_string()));
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
            tool_names: Vec::new(),
            tool_use_ids: Vec::new(),
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
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

#[test]
fn session_hook_events_support_filters_and_include_raw() {
    let conn = test_db();
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, raw_json,
            message_id, link_confidence, tool_name, tool_use_id, message_request_id
         ) VALUES (
            'claude_code', 'post_tool_use', 'sess-hooks', '2026-03-25T00:00:01Z', '{\"ok\":true}',
            'msg-1', 'exact_tool_use_id', 'Read', 'toolu_1', 'req-1'
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, raw_json, link_confidence
         ) VALUES (
            'claude_code', 'session_start', 'sess-hooks', '2026-03-25T00:00:00Z', '{\"start\":true}', 'unlinked'
         )",
        [],
    )
    .unwrap();

    let linked = session_hook_events(
        &conn,
        "sess-hooks",
        &SessionHookEventsParams {
            linked_only: true,
            event: Some("post_tool_use"),
            limit: 50,
            offset: 0,
            include_raw: false,
        },
    )
    .unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].message_id.as_deref(), Some("msg-1"));
    assert!(linked[0].raw_json.is_none());

    let with_raw = session_hook_events(
        &conn,
        "sess-hooks",
        &SessionHookEventsParams {
            linked_only: false,
            event: None,
            limit: 50,
            offset: 0,
            include_raw: true,
        },
    )
    .unwrap();
    assert_eq!(with_raw.len(), 2);
    assert!(with_raw[0].raw_json.is_some());
}

#[test]
fn message_detail_returns_linked_hook_and_otel_sets() {
    let conn = test_db();
    conn.execute(
        "INSERT INTO messages (id, session_id, role, timestamp, model, request_id, provider, cost_confidence, cost_cents)
         VALUES ('msg-detail-1', 'sess-detail', 'assistant', '2026-03-25T00:00:01Z', 'claude-opus-4-6', 'req-1', 'claude_code', 'otel_exact', 7.5)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tags (message_id, key, value) VALUES ('msg-detail-1', 'tool', 'Read')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tags (message_id, key, value) VALUES ('msg-detail-1', 'tool_use_id', 'toolu_1')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, raw_json,
            message_id, link_confidence, tool_name, tool_use_id, message_request_id
         ) VALUES (
            'claude_code', 'post_tool_use', 'sess-detail', '2026-03-25T00:00:02Z', '{\"hook\":1}',
            'msg-detail-1', 'exact_request_id', 'Read', 'toolu_1', 'req-1'
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO otel_events (
            event_name, session_id, timestamp, processed, raw_json,
            message_id, timestamp_nano, model, cost_usd_reported, cost_cents_computed
         ) VALUES (
            'claude_code.api_request', 'sess-detail', '2026-03-25T00:00:01.100Z', 1, '{\"otel\":1}',
            'msg-detail-1', '1711324801100000000', 'claude-opus-4-6', 0.075, 7.5
         )",
        [],
    )
    .unwrap();

    let detail = message_detail(&conn, "msg-detail-1").unwrap().unwrap();
    assert_eq!(detail.message.id, "msg-detail-1");
    assert_eq!(detail.tools, vec!["Read".to_string()]);
    assert_eq!(detail.hook_events.len(), 1);
    assert_eq!(detail.otel_events.len(), 1);
    assert_eq!(
        detail.otel_events[0].cost_cents_computed,
        Some(7.5),
        "computed cost should be surfaced"
    );
}

#[test]
fn ingest_messages_relinks_existing_unlinked_hook_and_otel_rows() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, raw_json, message_request_id, link_confidence
         ) VALUES (
            'claude_code', 'post_tool_use', 'sess-relink', '2026-03-25T00:00:01.050Z', '{}', 'msg_req_1', 'unlinked'
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO hook_events (
            provider, event, session_id, timestamp, raw_json, tool_use_id, link_confidence
         ) VALUES (
            'claude_code', 'post_tool_use', 'sess-relink', '2026-03-25T00:00:01.060Z', '{}', 'toolu_link_1', 'unlinked'
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO otel_events (
            event_name, session_id, timestamp, processed, raw_json,
            message_id, timestamp_nano, model, cost_usd_reported, cost_cents_computed
         ) VALUES (
            'claude_code.api_request', 'sess-relink', '2026-03-25T00:00:01.080Z', 1, '{\"otel\":1}',
            NULL, '1711324801080000000', NULL, 0.095, NULL
         )",
        [],
    )
    .unwrap();

    let msg = ParsedMessage {
        uuid: "msg-relink-1".to_string(),
        session_id: Some("sess-relink".to_string()),
        timestamp: "2026-03-25T00:00:01.000Z".parse().unwrap(),
        role: "assistant".to_string(),
        model: Some("claude-opus-4-6".to_string()),
        input_tokens: 10,
        output_tokens: 5,
        cost_cents: Some(9.5),
        cost_confidence: "otel_exact".to_string(),
        request_id: Some("msg_req_1".to_string()),
        ..Default::default()
    };
    let tags = vec![vec![Tag {
        key: "tool_use_id".to_string(),
        value: "toolu_link_1".to_string(),
    }]];
    ingest_messages(&mut conn, &[msg], Some(&tags)).unwrap();

    let (req_link_uuid, req_link_conf): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT message_id, link_confidence
             FROM hook_events
             WHERE session_id = 'sess-relink' AND message_request_id = 'msg_req_1'
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(req_link_uuid.as_deref(), Some("msg-relink-1"));
    assert_eq!(
        req_link_conf.as_deref(),
        Some(crate::hooks::HOOK_LINK_EXACT_REQUEST_ID)
    );

    let (tool_link_uuid, tool_link_conf): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT message_id, link_confidence
             FROM hook_events
             WHERE session_id = 'sess-relink' AND tool_use_id = 'toolu_link_1'
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(tool_link_uuid.as_deref(), Some("msg-relink-1"));
    assert_eq!(
        tool_link_conf.as_deref(),
        Some(crate::hooks::HOOK_LINK_EXACT_TOOL_USE_ID)
    );

    let (otel_link_uuid, otel_model, otel_computed): (Option<String>, Option<String>, Option<f64>) =
        conn.query_row(
            "SELECT message_id, model, cost_cents_computed
             FROM otel_events
             WHERE session_id = 'sess-relink'
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(otel_link_uuid.as_deref(), Some("msg-relink-1"));
    assert_eq!(otel_model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(otel_computed, Some(9.5));
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
        tool_names: Vec::new(),
        tool_use_ids: Vec::new(),
    }
}

fn insert_health_hook_event_at(
    conn: &Connection,
    provider: &str,
    session_id: &str,
    event: &str,
    timestamp: &str,
    tool_name: Option<&str>,
) {
    conn.execute(
        "INSERT INTO hook_events (provider, event, session_id, timestamp, tool_name, raw_json)
         VALUES (?1, ?2, ?3, ?4, ?5, '{}')",
        rusqlite::params![provider, event, session_id, timestamp, tool_name],
    )
    .unwrap();
}

fn insert_health_hook_event(
    conn: &Connection,
    provider: &str,
    session_id: &str,
    event: &str,
    idx: u64,
    tool_name: Option<&str>,
) {
    let ts = chrono::NaiveDateTime::parse_from_str(
        &format!("2026-03-14 10:{:02}:30", idx),
        "%Y-%m-%d %H:%M:%S",
    )
    .unwrap()
    .and_utc()
    .to_rfc3339();
    insert_health_hook_event_at(conn, provider, session_id, event, &ts, tool_name);
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
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(h.state, "green");
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
    assert_eq!(h.vitals.thrashing.as_ref().unwrap().state, "green");
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
    assert_eq!(h.vitals.thrashing.as_ref().unwrap().state, "red");
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

// --- Coverage: green when only one vital is computable (positive default) ---

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
    for idx in 0..3 {
        insert_health_hook_event(
            &conn,
            "claude_code",
            "s1",
            "post_tool_use",
            idx,
            Some("Shell"),
        );
    }

    let h = session_health(&conn, Some("s1")).unwrap();
    assert!(h.vitals.thrashing.is_some());
    assert!(h.vitals.context_drag.is_none());
    assert!(h.vitals.cache_efficiency.is_none());
    assert!(h.vitals.cost_acceleration.is_none());
    assert_eq!(
        h.state, "green",
        "single green vital → green (positive default)"
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
    assert_eq!(h.vitals.cost_acceleration.as_ref().unwrap().state, "red");
    assert!(
        h.vitals
            .cost_acceleration
            .as_ref()
            .unwrap()
            .label
            .contains("turn")
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
    assert_eq!(h.vitals.thrashing.as_ref().unwrap().state, "yellow");
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
    // With only 2 turns and prompt boundaries present, cost_acceleration is suppressed
    assert!(
        h.vitals.cost_acceleration.is_none(),
        "2 prompt turns should suppress cost_acceleration"
    );
    assert_ne!(
        h.state, "red",
        "multi-reply Cursor session should not be false red"
    );
}
