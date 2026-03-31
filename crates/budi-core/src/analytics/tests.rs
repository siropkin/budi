use super::*;
use rusqlite::Connection;

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
    };
    // CostEnricher is the single source of truth for cost_cents
    CostEnricher.enrich(&mut msg);
    ingest_messages(&mut conn, &[msg], None).unwrap();

    // Verify cost_cents was baked in: 1M input * $5/M + 100K output * $25/M = $5 + $2.50 = $7.50 = 750 cents
    let cost_cents: f64 = conn
        .query_row(
            "SELECT cost_cents FROM messages WHERE uuid = 'cost-test-1'",
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

    let result = branch_cost_single(&conn, "fix/bug-123", None, None).unwrap();
    assert!(result.is_some());
    let bc = result.unwrap();
    assert_eq!(bc.git_branch, "fix/bug-123");
    assert!((bc.cost_cents - 4.0).abs() < 0.01);

    let none = branch_cost_single(&conn, "nonexistent", None, None).unwrap();
    assert!(none.is_none());
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
    let msg1 = assistant_msg("ts-1", "s1", 10.0);
    let msg2 = assistant_msg("ts-2", "s2", 6.0);
    let tags = vec![
        vec![Tag {
            key: "repo".to_string(),
            value: "proj-a".to_string(),
        }],
        vec![Tag {
            key: "repo".to_string(),
            value: "proj-b".to_string(),
        }],
    ];
    ingest_messages(&mut conn, &[msg1, msg2], Some(&tags)).unwrap();

    let stats = tag_stats(&conn, Some("repo"), None, None, 10).unwrap();
    let proj_a = stats.iter().find(|s| s.value == "proj-a").unwrap();
    assert!((proj_a.cost_cents - 10.0).abs() < 0.01);
    let proj_b = stats.iter().find(|s| s.value == "proj-b").unwrap();
    assert!((proj_b.cost_cents - 6.0).abs() < 0.01);
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
fn session_messages_returns_assistant_only() {
    let mut conn = test_db();
    let msgs = sample_messages();
    ingest_messages(&mut conn, &msgs, None).unwrap();

    let result = session_messages(&conn, "sess-abc").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].uuid, "a1");
    assert_eq!(result[0].role, "assistant");
}

#[test]
fn session_tags_returns_distinct_tags() {
    let mut conn = test_db();
    let msg = assistant_msg("st-1", "sess-tags", 1.0);
    let tags = vec![vec![
        Tag {
            key: "repo".to_string(),
            value: "my-repo".to_string(),
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
    assert!(result.contains(&("repo".to_string(), "my-repo".to_string())));
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    ).unwrap();

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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    ).unwrap();

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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('old', 'claude_code', '2026-03-10')",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('new', 'claude_code', '2026-03-14')",
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
fn health_batch_returns_all_sessions() {
    let mut conn = test_db();
    conn.execute(
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s2', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    ).unwrap();

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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'windsurf', '2026-03-14')",
        [],
    ).unwrap();

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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'claude_code', '2026-03-14')",
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
            &format!("INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', '{provider}', '2026-03-14')"),
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
        "INSERT INTO sessions (session_id, provider, started_at) VALUES ('s1', 'cursor', '2026-03-14')",
        [],
    ).unwrap();

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
