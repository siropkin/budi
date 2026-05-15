use super::*;

/// #756: schema_version mismatch where the client is below the cloud's
/// accepted set should classify as ClientTooOld — the only path where
/// "update budi" advice is correct.
#[test]
fn classify_schema_mismatch_client_too_old() {
    let body = "Unsupported schema_version: 1. Expected one of: [2, 3]. Update your client.";
    let kind = classify_schema_mismatch(body, 1);
    assert_eq!(
        kind,
        SchemaMismatchKind::ClientTooOld {
            client: 1,
            expected_min: 2,
        }
    );
}

/// #756: when the client is *above* the cloud's accepted set (the
/// failure mode flagged in #749's body — a fresh release shipped a
/// schema bump faster than the cloud deployed), the daemon must call
/// out the cloud as the lagging side instead of telling the user to
/// update budi.
#[test]
fn classify_schema_mismatch_cloud_too_old() {
    let body = "Unsupported schema_version: 3. Expected one of: [1, 2]";
    let kind = classify_schema_mismatch(body, 3);
    assert_eq!(
        kind,
        SchemaMismatchKind::CloudTooOld {
            client: 3,
            expected_max: 2,
        }
    );
}

/// #756: a 422 body that doesn't mention `schema_version` is per-field
/// validation (the exact failure mode from the v8.4.4 smoke test —
/// `daily_rollups[0].cost_cents must be a finite, non-negative
/// number`). Classifier returns NotSchemaRelated; the daemon
/// surfaces the body verbatim and skips the "update budi" advice.
#[test]
fn classify_schema_mismatch_non_schema_body() {
    let body = "daily_rollups[0].cost_cents must be a finite, non-negative number; got null";
    let kind = classify_schema_mismatch(body, 2);
    assert_eq!(kind, SchemaMismatchKind::NotSchemaRelated);
}

/// #756: a malformed schema_version error (missing `Expected one of`
/// list, garbled integer, etc.) falls back to NotSchemaRelated so the
/// daemon doesn't crash on cloud format drift — the body is still
/// surfaced verbatim.
#[test]
fn classify_schema_mismatch_malformed_falls_back() {
    let kind = classify_schema_mismatch("Unsupported schema_version: oops.", 2);
    assert_eq!(kind, SchemaMismatchKind::NotSchemaRelated);
    let kind = classify_schema_mismatch("Unsupported schema_version: 2.", 2);
    assert_eq!(kind, SchemaMismatchKind::NotSchemaRelated);
}

#[test]
fn extract_ticket_basic() {
    // After #333, cloud_sync delegates to `pipeline::extract_ticket_from_branch`;
    // keep the spot-checks in place to confirm the thin wrapper preserves
    // alpha-pattern, integration-branch, and non-branch-like behavior.
    assert_eq!(
        extract_ticket("feature/PROJ-1234-add-auth").map(|(id, _)| id),
        Some("PROJ-1234".to_string())
    );
    assert_eq!(
        extract_ticket("PROJ-1234").map(|(id, _)| id),
        Some("PROJ-1234".to_string())
    );
    assert_eq!(
        extract_ticket("fix/ABC-42-hotfix").map(|(id, _)| id),
        Some("ABC-42".to_string())
    );
    assert_eq!(extract_ticket("main"), None);
    assert_eq!(extract_ticket("(untagged)"), None);
}

#[test]
fn https_enforcement() {
    assert!(validate_https_endpoint("https://app.getbudi.dev").is_ok());
    assert!(validate_https_endpoint("http://app.getbudi.dev").is_err());
    assert!(validate_https_endpoint("ftp://example.com").is_err());
}

#[test]
fn backoff_delay_escalation() {
    assert_eq!(backoff_delay(0, 300), Duration::from_secs(1));
    assert_eq!(backoff_delay(1, 300), Duration::from_secs(2));
    assert_eq!(backoff_delay(2, 300), Duration::from_secs(4));
    assert_eq!(backoff_delay(3, 300), Duration::from_secs(8));
    assert_eq!(backoff_delay(10, 300), Duration::from_secs(300)); // Capped
    assert_eq!(backoff_delay(20, 300), Duration::from_secs(300)); // Capped
}

#[test]
fn empty_payload_detected() {
    let result = send_sync_envelope(
        "https://app.getbudi.dev",
        "budi_test",
        &SyncEnvelope {
            schema_version: 1,
            device_id: "dev_test".into(),
            workspace_id: "org_test".into(),
            label: "test-host".into(),
            synced_at: "2026-04-12T00:00:00Z".into(),
            payload: SyncPayload {
                daily_rollups: vec![],
                session_summaries: vec![],
            },
        },
    );
    assert!(matches!(result, SyncResult::EmptyPayload));
}

#[test]
fn watermark_round_trip() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-wm");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Initially no watermark
    assert!(get_cloud_watermark_value(&conn).unwrap().is_none());
    assert!(get_session_watermark(&conn).unwrap().is_none());

    // Set and read back
    set_cloud_watermark(&conn, "2026-04-10").unwrap();
    assert_eq!(
        get_cloud_watermark_value(&conn).unwrap().as_deref(),
        Some("2026-04-10")
    );

    set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();
    assert_eq!(
        get_session_watermark(&conn).unwrap().as_deref(),
        Some("2026-04-10T10:00:00Z")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reset_cloud_watermarks_drops_sentinel_rows() {
    // #564: dropping the three sentinel rows must move the daemon
    // back to the no-watermark path so the next sync re-sends every
    // local rollup + session summary. After reset, getters return
    // None — the same shape a fresh install reports.
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-reset");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    set_cloud_watermark(&conn, "2026-04-10").unwrap();
    set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();

    let removed = reset_cloud_watermarks(&conn).unwrap();
    assert_eq!(
        removed, 3,
        "all three sentinels (rollup-completed, rollup-value, session) must be removed",
    );
    assert!(get_cloud_watermark_value(&conn).unwrap().is_none());
    assert!(get_session_watermark(&conn).unwrap().is_none());

    // Idempotent: a second reset is a no-op (returns 0 rows
    // removed). Lets the CLI render the right "nothing to reset"
    // line without an extra existence check.
    let removed_again = reset_cloud_watermarks(&conn).unwrap();
    assert_eq!(removed_again, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reset_cloud_watermarks_leaves_unrelated_rows_alone() {
    // The DELETE must be scoped to the cloud sentinels — never
    // touch ingestion offsets / tail offsets / completion markers
    // that share `sync_state`. A regression here would silently
    // re-import every JSONL transcript on the next tick.
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-reset-scope");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    crate::analytics::set_sync_offset(&conn, "/tmp/transcript.jsonl", 4096).unwrap();
    crate::analytics::mark_sync_completed(&conn).unwrap();
    set_cloud_watermark(&conn, "2026-04-10").unwrap();

    reset_cloud_watermarks(&conn).unwrap();

    // Ingestion offset survives.
    assert_eq!(
        crate::analytics::get_sync_offset(&conn, "/tmp/transcript.jsonl").unwrap(),
        4096,
    );
    // Sync-completion marker survives.
    assert!(
        crate::analytics::last_sync_completed_at(&conn)
            .unwrap()
            .is_some(),
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #767: simulate the upgrade boundary the v8.4.5 smoke test exposed —
/// local DB carries history that landed under wire shape v1 (rows
/// without the `surface` field populated correctly on the cloud
/// because the daemon never re-uploaded them). On boot, the binary
/// expects v2; the reset must drop the matching watermark so the
/// next sync re-emits every affected row, and must bump each row's
/// `wire_shape_version` to the binary's value so the next boot is a
/// no-op.
#[test]
fn reset_stale_shape_watermarks_drops_watermark_on_version_drift() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-wire-shape-drift");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Two sessions + one rollup row, simulating the pre-8.4.6 state.
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms,
                               surface, wire_shape_version)
         VALUES ('s1', 'copilot_chat', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z',
                 3600000, 'jetbrains', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms,
                               surface, wire_shape_version)
         VALUES ('s2', 'copilot_chat', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z',
                 3600000, 'jetbrains', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message_rollups_daily (bucket_day, role, provider, model,
                                             repo_id, git_branch, surface,
                                             message_count, wire_shape_version)
         VALUES ('2026-04-10', 'assistant', 'copilot_chat', 'gpt-5',
                 'sha256:abc', 'main', 'jetbrains', 3, 1)",
        [],
    )
    .unwrap();

    // Plant the watermarks that the pre-upgrade daemon would have
    // advanced past — the entire bug is that history landed *under*
    // these watermarks under shape v1.
    set_cloud_watermark(&conn, "2026-04-10").unwrap();
    set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();

    let report = reset_stale_shape_watermarks(&conn).unwrap();
    assert!(report.sessions_reset, "session watermark must be dropped");
    assert!(report.rollups_reset, "rollup watermark must be dropped");
    assert_eq!(report.session_rows_updated, 2);
    assert_eq!(report.rollup_rows_updated, 1);
    assert_eq!(report.sessions_local_max, Some(1));
    assert_eq!(report.rollup_local_max, Some(1));

    // Watermarks gone → next sync re-emits everything.
    assert!(get_session_watermark(&conn).unwrap().is_none());
    assert!(get_cloud_watermark_value(&conn).unwrap().is_none());

    // Per-row versions bumped so the next boot is a no-op.
    let session_max: i64 = conn
        .query_row("SELECT MAX(wire_shape_version) FROM sessions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(session_max as u32, WIRE_SHAPE_VERSION_SESSIONS);
    let rollup_max: i64 = conn
        .query_row(
            "SELECT MAX(wire_shape_version) FROM message_rollups_daily",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rollup_max as u32, WIRE_SHAPE_VERSION_ROLLUPS);

    // Second invocation is a no-op (rows already at expected version).
    let second = reset_stale_shape_watermarks(&conn).unwrap();
    assert!(!second.any_reset());
    assert_eq!(second.session_rows_updated, 0);
    assert_eq!(second.rollup_rows_updated, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// #767: a fresh install has no rows. The boot check must not drop
/// the watermarks just because the tables are empty (the next sync
/// would otherwise be a no-op anyway, but we still want the boot
/// check to be quiet on a healthy first run).
#[test]
fn reset_stale_shape_watermarks_noop_on_empty_db() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-wire-shape-empty");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Plant watermarks that a healthy daemon would have advanced.
    set_cloud_watermark(&conn, "2026-04-10").unwrap();
    set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();

    let report = reset_stale_shape_watermarks(&conn).unwrap();
    assert!(!report.any_reset());
    assert_eq!(report.sessions_local_max, None);
    assert_eq!(report.rollup_local_max, None);

    // Watermarks survive — empty tables don't drift.
    assert_eq!(
        get_session_watermark(&conn).unwrap().as_deref(),
        Some("2026-04-10T10:00:00Z")
    );
    assert_eq!(
        get_cloud_watermark_value(&conn).unwrap().as_deref(),
        Some("2026-04-10")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #767: only the affected table's watermark gets dropped. If rollups
/// are up-to-date but sessions lag, the session watermark resets and
/// the rollup watermark survives (otherwise we'd re-emit hundreds of
/// rollup days for no reason — the briefing's "Option A is surgical"
/// promise).
#[test]
fn reset_stale_shape_watermarks_scoped_to_affected_table() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-wire-shape-scoped");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Rollups already at the binary's version; sessions still v1.
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms,
                               surface, wire_shape_version)
         VALUES ('s1', 'copilot_chat', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z',
                 3600000, 'jetbrains', 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message_rollups_daily (bucket_day, role, provider, model,
                                             repo_id, git_branch, surface,
                                             message_count, wire_shape_version)
         VALUES ('2026-04-10', 'assistant', 'copilot_chat', 'gpt-5',
                 'sha256:abc', 'main', 'jetbrains', 3, ?1)",
        params![WIRE_SHAPE_VERSION_ROLLUPS],
    )
    .unwrap();

    set_cloud_watermark(&conn, "2026-04-10").unwrap();
    set_session_watermark(&conn, "2026-04-10T10:00:00Z").unwrap();

    let report = reset_stale_shape_watermarks(&conn).unwrap();
    assert!(report.sessions_reset);
    assert!(!report.rollups_reset);

    // Session watermark gone, rollup watermark survives.
    assert!(get_session_watermark(&conn).unwrap().is_none());
    assert_eq!(
        get_cloud_watermark_value(&conn).unwrap().as_deref(),
        Some("2026-04-10")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_rollups_empty_db() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollups");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    assert!(rollups.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_rollups_with_data() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollups-data");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Insert a message to trigger the rollup trigger
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                               input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                               cost_cents_ingested, cost_cents_effective)
         VALUES ('msg-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                 'sha256:abc123', 'feature/PROJ-42-auth', 100, 200, 10, 50, 1.5, 1.5)",
        [],
    ).unwrap();

    // Fetch all rollups (no watermark)
    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    assert_eq!(rollups.len(), 1);
    assert_eq!(rollups[0].bucket_day, "2026-04-10");
    assert_eq!(rollups[0].model, "claude-sonnet-4-6");
    assert_eq!(rollups[0].input_tokens, 100);
    assert_eq!(rollups[0].output_tokens, 200);
    assert_eq!(rollups[0].ticket.as_deref(), Some("PROJ-42"));
    assert_eq!(
        rollups[0].ticket_source.as_deref(),
        Some(crate::pipeline::TICKET_SOURCE_BRANCH)
    );

    // Fetch with watermark that excludes the data
    let rollups = fetch_daily_rollups(&conn, Some("2026-04-10")).unwrap();
    // The watermark is "2026-04-10" and today is after it, so we only get
    // records where bucket_day > watermark OR bucket_day == today.
    // Since the record is from 2026-04-10 and today != 2026-04-10,
    // we should get 0 (bucket_day is not > watermark, and it's not today).
    assert!(rollups.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_session_summaries_empty_db() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-sessions");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    let summaries = fetch_session_summaries(&conn, None).unwrap();
    assert!(summaries.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// #638: helper for the primary_model tests below — seeds a session
// and a configurable batch of assistant messages.
fn seed_session_with_messages(
    conn: &Connection,
    session_id: &str,
    rows: &[(&str, Option<&str>, &str, i64, i64)],
) {
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
         VALUES (?1, 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z', 3600000,
                 'sha256:pm', 'main')",
        params![session_id],
    )
    .unwrap();
    for (msg_id, model, ts, input, output) in rows {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, timestamp, model, provider, repo_id, git_branch,
                                   input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES (?1, ?2, 'assistant', ?3, ?4, 'anthropic', 'sha256:pm', 'main', ?5, ?6, 0, 0, 0.1, 0.1)",
            params![msg_id, session_id, ts, model, input, output],
        )
        .unwrap();
    }
}

/// #638: argmax over `input + output` tokens picks the high-token
/// model even when it has fewer messages.
#[test]
fn primary_model_picks_argmax_by_tokens() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-pm-argmax");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    // One Opus message (10k tokens) outweighs ten Haiku messages (100 tokens each).
    let mut rows: Vec<(&str, Option<&str>, &str, i64, i64)> = vec![(
        "opus-1",
        Some("claude-opus-4-7"),
        "2026-04-10T09:30:00Z",
        5_000,
        5_000,
    )];
    let haiku_ids: Vec<String> = (0..10).map(|i| format!("haiku-{i}")).collect();
    for id in &haiku_ids {
        rows.push((
            id.as_str(),
            Some("claude-haiku-4-5"),
            "2026-04-10T09:45:00Z",
            50,
            50,
        ));
    }
    seed_session_with_messages(&conn, "sess-pm-argmax", &rows);

    let summaries = fetch_session_summaries(&conn, None).unwrap();
    let s = summaries
        .iter()
        .find(|s| s.session_id == "sess-pm-argmax")
        .expect("session present");
    assert_eq!(s.primary_model.as_deref(), Some("claude-opus-4-7"));
}

/// #638: when two models tie on token count, the model with the
/// latest message timestamp wins.
#[test]
fn primary_model_tie_broken_by_latest_used() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-pm-tie");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    // Opus and Sonnet each consume exactly 1000 tokens; Sonnet's
    // latest message lands later, so Sonnet must win.
    seed_session_with_messages(
        &conn,
        "sess-pm-tie",
        &[
            (
                "opus-1",
                Some("claude-opus-4-7"),
                "2026-04-10T09:10:00Z",
                500,
                500,
            ),
            (
                "sonnet-1",
                Some("claude-sonnet-4-6"),
                "2026-04-10T09:50:00Z",
                500,
                500,
            ),
        ],
    );

    let summaries = fetch_session_summaries(&conn, None).unwrap();
    let s = summaries
        .iter()
        .find(|s| s.session_id == "sess-pm-tie")
        .expect("session present");
    assert_eq!(s.primary_model.as_deref(), Some("claude-sonnet-4-6"));
}

/// #638: a session with zero scored messages must omit `primary_model`
/// entirely — the cloud column is nullable for exactly this case, and
/// the daemon must not guess.
#[test]
fn primary_model_omitted_for_session_without_scored_messages() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-pm-empty");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
         VALUES ('sess-pm-empty', 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z', 3600000,
                 'sha256:pm', 'main')",
        [],
    )
    .unwrap();

    let summaries = fetch_session_summaries(&conn, None).unwrap();
    let s = summaries
        .iter()
        .find(|s| s.session_id == "sess-pm-empty")
        .expect("session present");
    assert!(s.primary_model.is_none());

    // Serialization must drop the field entirely so the cloud row stays NULL.
    let json = serde_json::to_value(s).unwrap();
    assert!(json.get("primary_model").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_envelope_requires_config() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-envelope");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    let config = CloudConfig::default();

    // Should fail without device_id
    let result = build_sync_envelope(&conn, &config);
    assert!(result.is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_envelope_success() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-envelope-ok");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    let config = CloudConfig {
        enabled: true,
        api_key: Some("budi_test".into()),
        device_id: Some("dev_test".into()),
        workspace_id: Some("org_test".into()),
        ..CloudConfig::default()
    };

    let envelope = build_sync_envelope(&conn, &config).unwrap();
    assert_eq!(envelope.schema_version, 2);
    assert_eq!(envelope.device_id, "dev_test");
    assert_eq!(envelope.workspace_id, "org_test");
    assert!(envelope.payload.daily_rollups.is_empty());
    assert!(envelope.payload.session_summaries.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_cloud_status_reports_disabled_when_config_default() {
    let dir = std::env::temp_dir().join("budi-cloud-status-disabled");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);
    let _ = crate::analytics::open_db_with_migration(&db_path).unwrap();

    let status = current_cloud_status(&db_path, &CloudConfig::default());
    assert!(!status.enabled);
    assert!(!status.ready);
    assert_eq!(status.pending_rollups, 0);
    assert_eq!(status.pending_sessions, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_cloud_status_reports_api_key_stub_when_placeholder() {
    let dir = std::env::temp_dir().join("budi-cloud-status-stub");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);
    let _ = crate::analytics::open_db_with_migration(&db_path).unwrap();

    let config = CloudConfig {
        api_key: Some(crate::config::CLOUD_API_KEY_STUB.to_string()),
        ..CloudConfig::default()
    };
    let status = current_cloud_status(&db_path, &config);
    assert!(
        status.api_key_stub,
        "placeholder api_key must surface as api_key_stub=true"
    );
    assert!(
        !status.ready,
        "stub key must never look ready even if enabled is true elsewhere"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_cloud_status_reports_pending_counts_when_ready() {
    let dir = std::env::temp_dir().join("budi-cloud-status-ready");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);
    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                               input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                               cost_cents_ingested, cost_cents_effective)
         VALUES ('msg-status-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                 'sha256:abc', 'main', 100, 200, 10, 50, 1.5, 1.5)",
        [],
    )
    .unwrap();

    let config = CloudConfig {
        enabled: true,
        api_key: Some("budi_test".into()),
        device_id: Some("dev_test".into()),
        workspace_id: Some("org_test".into()),
        ..CloudConfig::default()
    };
    let status = current_cloud_status(&db_path, &config);
    assert!(status.enabled);
    assert!(status.ready);
    assert!(status.pending_rollups >= 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn envelope_serializes_to_expected_shape() {
    let envelope = SyncEnvelope {
        schema_version: 2,
        device_id: "dev_test".into(),
        workspace_id: "org_test".into(),
        label: "ivan-mbp".into(),
        synced_at: "2026-04-12T00:00:00Z".into(),
        payload: SyncPayload {
            daily_rollups: vec![DailyRollupRecord {
                bucket_day: "2026-04-10".into(),
                role: "assistant".into(),
                provider: "claude_code".into(),
                model: "claude-sonnet-4-6".into(),
                repo_id: "sha256:abc".into(),
                git_branch: "main".into(),
                surface: "cursor".into(),
                ticket: None,
                ticket_source: None,
                message_count: 5,
                input_tokens: 1000,
                output_tokens: 500,
                cache_creation_tokens: 100,
                cache_read_tokens: 200,
                cost_cents_effective: 2.5,
                cost_cents_ingested: 2.5,
            }],
            session_summaries: vec![],
        },
    };

    let json = serde_json::to_value(&envelope).unwrap();
    // #723: bumped to 2 alongside the `surface` field landing on both
    // wire structs.
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["device_id"], "dev_test");
    // #552: label travels alongside device_id / workspace_id / synced_at
    // on the envelope root.
    assert_eq!(json["label"], "ivan-mbp");
    assert_eq!(
        json["payload"]["daily_rollups"][0]["bucket_day"],
        "2026-04-10"
    );
    // ticket should be absent (None → skipped)
    assert!(json["payload"]["daily_rollups"][0].get("ticket").is_none());
    // ticket_source should also be absent when ticket is None
    assert!(
        json["payload"]["daily_rollups"][0]
            .get("ticket_source")
            .is_none()
    );
    // ADR-0094 §1: envelope carries both `cost_cents_effective` (read
    // surface, may be overridden by team pricing) and `cost_cents_ingested`
    // (LiteLLM-priced ingest cost, immutable per ADR-0091 §5 Rule D).
    // Cloud uses `_ingested` to populate its own ingested column on insert.
    assert_eq!(
        json["payload"]["daily_rollups"][0]["cost_cents_effective"],
        2.5
    );
    assert_eq!(
        json["payload"]["daily_rollups"][0]["cost_cents_ingested"],
        2.5
    );
    // #723: surface always emitted (NOT NULL on the local column).
    assert_eq!(json["payload"]["daily_rollups"][0]["surface"], "cursor");
}

/// #552: when `cloud.toml` omits `label`, `effective_label()` falls
/// back to the local OS hostname. We don't pin the exact value —
/// the test host's hostname is whatever the CI image decided — but
/// it must match `get_hostname()` (same source of truth) so a
/// hostname change propagates consistently across callers.
#[test]
fn effective_label_defaults_to_hostname_when_unset() {
    let config = CloudConfig::default();
    assert!(config.label.is_none());
    assert_eq!(
        config.effective_label(),
        crate::pipeline::enrichers::get_hostname(),
    );
}

/// #552: explicit TOML value is sent verbatim, including an empty
/// string (documented as the opt-out contract on `CloudConfig::label`).
#[test]
fn effective_label_sends_explicit_value_verbatim() {
    let explicit = CloudConfig {
        label: Some("ivan-mbp".into()),
        ..CloudConfig::default()
    };
    assert_eq!(explicit.effective_label(), "ivan-mbp");

    let opt_out = CloudConfig {
        label: Some(String::new()),
        ..CloudConfig::default()
    };
    assert_eq!(
        opt_out.effective_label(),
        "",
        "opt-out must send empty label rather than silently \
         falling back to hostname — otherwise the user can't \
         actually hide their hostname",
    );
}

/// Round-trip through `build_sync_envelope` to confirm the label
/// lands on the envelope with the same precedence.
#[test]
fn build_envelope_populates_label_from_config() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-label-envelope");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    let config = CloudConfig {
        enabled: true,
        api_key: Some("budi_test".into()),
        device_id: Some("dev_test".into()),
        workspace_id: Some("org_test".into()),
        label: Some("ivan-mbp".into()),
        ..CloudConfig::default()
    };
    let envelope = build_sync_envelope(&conn, &config).unwrap();
    assert_eq!(envelope.label, "ivan-mbp");

    let _ = std::fs::remove_dir_all(&dir);
}

// Regression for #333: cloud_sync must produce the same ticket_id as the
// canonical pipeline extractor on the divergent cases that motivated the
// ticket — the numeric fallback and the nested alphanumeric form — and
// integration branches must not leak a ticket to the cloud.
#[test]
fn rollup_extraction_matches_pipeline_extractor() {
    let cases = [
        "feature/1234",
        "bugfix/ENG-99/refactor",
        "feature/PROJ-42-auth",
        "42-stabilize-auth",
        "main",
        "master",
        "develop",
        "HEAD",
        "kiyoshi/pava-searchbars", // no ticket at all
    ];
    for branch in cases {
        let pipeline = crate::pipeline::extract_ticket_from_branch(branch);
        let local = extract_ticket(branch);
        assert_eq!(
            pipeline, local,
            "cloud_sync extractor diverged from pipeline for {branch:?}"
        );
    }
}

#[test]
fn rollup_numeric_branch_preserves_source_marker() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-ticket-source");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // A numeric-only branch — previously the local helper returned
    // None here, so cloud ticket buckets disagreed with local CLI.
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                               input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                               cost_cents_ingested, cost_cents_effective)
         VALUES ('msg-num-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                 'sha256:num', 'feature/1234', 10, 20, 0, 0, 0.1, 0.1)",
        [],
    )
    .unwrap();

    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    let numeric = rollups
        .iter()
        .find(|r| r.git_branch == "feature/1234")
        .expect("numeric rollup present");
    assert_eq!(numeric.ticket.as_deref(), Some("1234"));
    assert_eq!(
        numeric.ticket_source.as_deref(),
        Some(crate::pipeline::TICKET_SOURCE_BRANCH_NUMERIC)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// Regression for #344: `count_pending_*` must return the same row
// counts as `build_sync_envelope` so `/cloud/status` pollers and the
// actual sync tick never disagree about what is pending.
#[test]
fn count_pending_matches_envelope() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-counts");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    // Seed a rollup via the message trigger, plus an explicit session row.
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                               input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                               cost_cents_ingested, cost_cents_effective)
         VALUES ('msg-count-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                 'sha256:count', 'feature/PROJ-77-counts', 10, 20, 0, 0, 0.1, 0.1)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms, repo_id, git_branch)
         VALUES ('sess-count-1', 'claude_code', '2026-04-10T14:00:00Z', '2026-04-10T14:30:00Z', 1800000,
                 'sha256:count', 'feature/PROJ-77-counts')",
        [],
    ).unwrap();

    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    let sessions = fetch_session_summaries(&conn, None).unwrap();
    assert_eq!(count_pending_rollups(&conn, None).unwrap(), rollups.len());
    assert_eq!(count_pending_sessions(&conn, None).unwrap(), sessions.len());

    // Same contract holds once watermarks are in place.
    let wm_rollup = "2026-04-10";
    let wm_session = "2026-04-10T14:15:00Z";
    let rollups_wm = fetch_daily_rollups(&conn, Some(wm_rollup)).unwrap();
    let sessions_wm = fetch_session_summaries(&conn, Some(wm_session)).unwrap();
    assert_eq!(
        count_pending_rollups(&conn, Some(wm_rollup)).unwrap(),
        rollups_wm.len()
    );
    assert_eq!(
        count_pending_sessions(&conn, Some(wm_session)).unwrap(),
        sessions_wm.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// -------- #723: surface dimension on cloud-sync wire structs --------

/// #723: rows ingested for every canonical surface value must
/// round-trip through the daily-rollup wire struct. Mirrors the
/// parser-output set landed in #701 (`vscode` / `cursor` /
/// `jetbrains` / `terminal` / `unknown`), so a regression that drops
/// the column from the SELECT list trips here rather than silently
/// re-landing 100% `'unknown'` on the cloud.
#[test]
fn rollup_round_trips_surface_for_every_canonical_value() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-rollup-surface");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    let surfaces = ["vscode", "cursor", "jetbrains", "terminal", "unknown"];
    for (i, surface) in surfaces.iter().enumerate() {
        // One message per surface — the rollup trigger keys on
        // (bucket_day, role, provider, model, repo_id, git_branch,
        // surface), so distinct surfaces fan out to distinct rollup
        // rows even with identical provider/model/repo/branch.
        conn.execute(
            "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                                   surface, input_tokens, output_tokens,
                                   cache_creation_tokens, cache_read_tokens,
                                   cost_cents_ingested, cost_cents_effective)
             VALUES (?1, 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                     'sha256:surface', 'main', ?2, 10, 20, 0, 0, 0.1, 0.1)",
            params![format!("msg-surface-{i}"), surface],
        )
        .unwrap();
    }

    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    for surface in surfaces {
        let r = rollups
            .iter()
            .find(|r| r.surface == surface)
            .unwrap_or_else(|| panic!("rollup for surface={surface:?} present"));
        // JSON round-trip — the cloud parses the same shape.
        let json = serde_json::to_value(r).unwrap();
        assert_eq!(json["surface"], surface);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// #723: same coverage on the session wire struct. The `sessions`
/// table stores `surface` directly (no trigger), so the SELECT in
/// `fetch_session_summaries` is the only thing that has to project it.
#[test]
fn session_round_trips_surface_for_every_canonical_value() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-session-surface");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();

    let surfaces = ["vscode", "cursor", "jetbrains", "terminal", "unknown"];
    for (i, surface) in surfaces.iter().enumerate() {
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, ended_at, duration_ms,
                                   repo_id, git_branch, surface)
             VALUES (?1, 'claude_code', '2026-04-10T09:00:00Z', '2026-04-10T10:00:00Z',
                     3600000, 'sha256:surface', 'main', ?2)",
            params![format!("sess-surface-{i}"), surface],
        )
        .unwrap();
    }

    let summaries = fetch_session_summaries(&conn, None).unwrap();
    for surface in surfaces {
        let s = summaries
            .iter()
            .find(|s| s.surface == surface)
            .unwrap_or_else(|| panic!("session for surface={surface:?} present"));
        let json = serde_json::to_value(s).unwrap();
        assert_eq!(json["surface"], surface);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// #723: snapshot the on-wire JSON shape with `surface` populated so
/// the wire payload is reviewable in PRs. The cloud ingest contract
/// is "field is optional but, when present, must be the literal
/// string surface value" — a regression where the daemon emits
/// e.g. `{"surface": null}` or skips the field would land all rows
/// back at `'unknown'` on the cloud (siropkin/budi-cloud#227).
#[test]
fn rollup_wire_snapshot_with_surface() {
    let record = DailyRollupRecord {
        bucket_day: "2026-04-10".into(),
        role: "assistant".into(),
        provider: "claude_code".into(),
        model: "claude-sonnet-4-6".into(),
        repo_id: "sha256:abc".into(),
        git_branch: "main".into(),
        surface: "jetbrains".into(),
        ticket: None,
        ticket_source: None,
        message_count: 5,
        input_tokens: 1000,
        output_tokens: 500,
        cache_creation_tokens: 100,
        cache_read_tokens: 200,
        cost_cents_effective: 2.5,
        cost_cents_ingested: 2.5,
    };
    let json = serde_json::to_string(&record).unwrap();
    let expected = "{\
        \"bucket_day\":\"2026-04-10\",\
        \"role\":\"assistant\",\
        \"provider\":\"claude_code\",\
        \"model\":\"claude-sonnet-4-6\",\
        \"repo_id\":\"sha256:abc\",\
        \"git_branch\":\"main\",\
        \"surface\":\"jetbrains\",\
        \"message_count\":5,\
        \"input_tokens\":1000,\
        \"output_tokens\":500,\
        \"cache_creation_tokens\":100,\
        \"cache_read_tokens\":200,\
        \"cost_cents_effective\":2.5,\
        \"cost_cents_ingested\":2.5\
    }";
    assert_eq!(json, expected);
}

#[test]
fn rollup_integration_branches_do_not_emit_ticket() {
    let dir = std::env::temp_dir().join("budi-cloud-sync-test-integration");
    std::fs::create_dir_all(&dir).ok();
    let db_path = dir.join("test.db");
    let _ = std::fs::remove_file(&db_path);

    let conn = crate::analytics::open_db_with_migration(&db_path).unwrap();
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, repo_id, git_branch,
                               input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
                               cost_cents_ingested, cost_cents_effective)
         VALUES ('msg-int-1', 'assistant', '2026-04-10T14:30:00Z', 'claude-sonnet-4-6', 'anthropic',
                 'sha256:int', 'main', 10, 20, 0, 0, 0.1, 0.1)",
        [],
    )
    .unwrap();

    let rollups = fetch_daily_rollups(&conn, None).unwrap();
    let main_rollup = rollups
        .iter()
        .find(|r| r.git_branch == "main")
        .expect("rollup for main present");
    assert!(main_rollup.ticket.is_none());
    assert!(main_rollup.ticket_source.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

// -------- #572: chunked envelope tests --------

fn make_rollup(day: &str, model: &str) -> DailyRollupRecord {
    DailyRollupRecord {
        bucket_day: day.into(),
        role: "assistant".into(),
        provider: "anthropic".into(),
        model: model.into(),
        repo_id: "sha256:test".into(),
        git_branch: "main".into(),
        surface: "unknown".into(),
        ticket: None,
        ticket_source: None,
        message_count: 1,
        input_tokens: 10,
        output_tokens: 20,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        cost_cents_effective: 0.1,
        cost_cents_ingested: 0.1,
    }
}

fn make_session(id: &str, started_at: &str) -> SessionSummaryRecord {
    SessionSummaryRecord {
        session_id: id.into(),
        provider: "anthropic".into(),
        started_at: Some(started_at.into()),
        ended_at: None,
        duration_ms: None,
        repo_id: None,
        git_branch: None,
        surface: "unknown".into(),
        ticket: None,
        ticket_source: None,
        message_count: 1,
        total_input_tokens: 10,
        total_output_tokens: 20,
        total_cost_cents: 0.1,
        primary_model: None,
    }
}

#[test]
fn chunk_payload_below_threshold_returns_single_chunk() {
    // Steady-state ticks must keep the pre-#572 single-POST shape.
    let payload = SyncPayload {
        daily_rollups: vec![make_rollup("2026-04-10", "claude-sonnet-4-6")],
        session_summaries: vec![make_session("s1", "2026-04-10T10:00:00Z")],
    };
    let chunks = chunk_payload(payload);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].daily_rollups.len(), 1);
    assert_eq!(chunks[0].session_summaries.len(), 1);
}

#[test]
fn chunk_payload_empty_returns_one_empty_chunk() {
    // Callers iterate the returned vec; one empty chunk keeps the
    // call site uniform with the non-empty path.
    let chunks = chunk_payload(SyncPayload {
        daily_rollups: vec![],
        session_summaries: vec![],
    });
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].daily_rollups.is_empty());
    assert!(chunks[0].session_summaries.is_empty());
}

#[test]
fn chunk_payload_splits_large_rollup_set_at_day_boundaries() {
    // 12 days × 50 rollups = 600 records → at least 2 chunks. The
    // contract under test: no bucket_day spans two chunks.
    let mut rollups: Vec<DailyRollupRecord> = Vec::new();
    for d in 1..=12 {
        for i in 0..50 {
            let model = format!("model-{i:02}");
            rollups.push(make_rollup(&format!("2026-04-{d:02}"), &model));
        }
    }
    let total = rollups.len();
    let chunks = chunk_payload(SyncPayload {
        daily_rollups: rollups,
        session_summaries: vec![],
    });

    let chunked_total: usize = chunks.iter().map(|c| c.daily_rollups.len()).sum();
    assert_eq!(chunked_total, total);

    let seen_days_per_chunk: Vec<Vec<String>> = chunks
        .iter()
        .map(|c| {
            let mut days: Vec<String> = c
                .daily_rollups
                .iter()
                .map(|r| r.bucket_day.clone())
                .collect();
            days.sort();
            days.dedup();
            days
        })
        .collect();
    let total_unique = {
        let mut all: Vec<String> = seen_days_per_chunk.iter().flatten().cloned().collect();
        all.sort();
        all.dedup();
        all.len()
    };
    let pair_count: usize = seen_days_per_chunk.iter().map(|d| d.len()).sum();
    assert_eq!(
        pair_count, total_unique,
        "a single bucket_day must not span multiple chunks"
    );

    assert!(chunks.len() >= 2);
}

#[test]
fn chunk_payload_keeps_oversized_single_day_intact() {
    // A pathological single day > MAX records goes out as one
    // oversized chunk to preserve "watermark = day fully synced".
    let mut rollups = Vec::new();
    for i in 0..(MAX_RECORDS_PER_ENVELOPE + 50) {
        rollups.push(make_rollup("2026-04-01", &format!("model-{i}")));
    }
    let chunks = chunk_payload(SyncPayload {
        daily_rollups: rollups,
        session_summaries: vec![],
    });
    // All records for the single day land in a single chunk.
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].daily_rollups.len(), MAX_RECORDS_PER_ENVELOPE + 50);
}

#[test]
fn chunk_payload_chunks_sessions_separately_from_rollups() {
    // Sessions chunk in fixed-size batches, isolated from rollups.
    let mut sessions = Vec::new();
    for i in 0..(MAX_RECORDS_PER_ENVELOPE * 2 + 100) {
        sessions.push(make_session(&format!("s-{i}"), "2026-04-10T10:00:00Z"));
    }
    let chunks = chunk_payload(SyncPayload {
        daily_rollups: vec![],
        session_summaries: sessions,
    });
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].session_summaries.len(), MAX_RECORDS_PER_ENVELOPE);
    assert_eq!(chunks[1].session_summaries.len(), MAX_RECORDS_PER_ENVELOPE);
    assert_eq!(chunks[2].session_summaries.len(), 100);
    for chunk in &chunks {
        assert!(chunk.daily_rollups.is_empty());
    }
}

#[test]
fn chunk_payload_simulates_dogfood_db_shape() {
    // Recreates the issue's failing case (~1920 rollups + 2350
    // sessions, pre-#572 a single 8+ MB POST → 413).
    let mut rollups = Vec::new();
    for d in 0..240 {
        let day = format!("2025-08-{:02}", (d % 28) + 1);
        for i in 0..8 {
            rollups.push(make_rollup(&day, &format!("model-{i}-{d}")));
        }
    }

    let mut sessions = Vec::new();
    for i in 0..2350 {
        sessions.push(make_session(&format!("s-{i}"), "2026-04-10T10:00:00Z"));
    }

    let chunks = chunk_payload(SyncPayload {
        daily_rollups: rollups,
        session_summaries: sessions,
    });

    assert!(
        chunks.len() >= 5,
        "dogfood-sized payload should split into many chunks; got {}",
        chunks.len(),
    );
    // ⌈2350 / 500⌉ = 5 session chunks.
    let session_chunks: usize = chunks
        .iter()
        .filter(|c| !c.session_summaries.is_empty())
        .count();
    assert_eq!(session_chunks, 5);
}
