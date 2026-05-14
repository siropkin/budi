use super::*;

/// #588: `budi doctor --format json` lock-in. The JSON contract is
/// `{checks: [{name, status, detail}], all_pass}` with `status`
/// drawn from a fixed vocabulary of `pass | info | warn | fail`
/// (`info` added in #693 for the pre-boot history hint). Scripted
/// callers branch on this shape — a future rename would silently
/// break them.
#[test]
fn doctor_json_locks_schema_and_status_vocabulary() {
    let checks = [
        CheckResult::pass("daemon health", "responding on http://127.0.0.1:7878"),
        CheckResult::info(
            "pre-boot history detected / Claude Code",
            "3 transcript(s) seeded as history",
            Some("budi db import".into()),
        ),
        CheckResult::warn("tailer providers", "no enabled providers", None),
        CheckResult::fail(
            "schema",
            "missing column `tags`",
            Some("budi db check --fix".into()),
        ),
    ];
    let body = DoctorJson {
        all_pass: false,
        checks: checks.iter().map(CheckResultJson::from).collect(),
    };
    let v = serde_json::to_value(&body).expect("serialise");

    let mut top_keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    top_keys.sort();
    assert_eq!(top_keys, vec!["all_pass", "checks"]);
    assert_eq!(v["all_pass"], serde_json::json!(false));

    let arr = v["checks"].as_array().expect("checks array");
    assert_eq!(arr.len(), 4);
    for entry in arr {
        let mut keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["detail", "name", "status"]);
    }
    assert_eq!(arr[0]["status"], serde_json::json!("pass"));
    assert_eq!(arr[1]["status"], serde_json::json!("info"));
    assert_eq!(arr[2]["status"], serde_json::json!("warn"));
    assert_eq!(arr[3]["status"], serde_json::json!("fail"));
    // `fix` is intentionally not part of the JSON shape — it's a
    // text-mode-only operator hint, not part of the wire contract.
    assert!(arr[3].as_object().unwrap().get("fix").is_none());
}

#[test]
fn doctor_json_all_pass_true_when_all_checks_pass() {
    let checks = [CheckResult::pass("a", "ok"), CheckResult::pass("b", "ok")];
    let body = DoctorJson {
        all_pass: true,
        checks: checks.iter().map(CheckResultJson::from).collect(),
    };
    let v = serde_json::to_value(&body).expect("serialise");
    assert_eq!(v["all_pass"], serde_json::json!(true));
}

fn diag(display_name: &'static str) -> ProviderDoctorData {
    ProviderDoctorData {
        display_name,
        watch_roots: vec![PathBuf::from("/tmp/watch")],
        discovered_files: 1,
        latest_file: Some(PathBuf::from("/tmp/watch/session.jsonl")),
        latest_file_len: Some(120),
        latest_file_mtime: Some(Utc::now()),
        tracked_offsets: Some(1),
        latest_tail_offset: Some(120),
        latest_tail_seen: Some(Utc::now()),
        latest_file_tail_seen: Some(Utc::now()),
        discover_error: None,
        db_error: None,
    }
}

#[test]
fn integrity_check_uses_quick_check_by_default() {
    assert_eq!(integrity_check_pragma(false), "PRAGMA quick_check");
    assert_eq!(integrity_check_mode_label(false), "quick_check");
}

#[test]
fn integrity_check_uses_full_check_in_deep_mode() {
    assert_eq!(integrity_check_pragma(true), "PRAGMA integrity_check");
    assert_eq!(integrity_check_mode_label(true), "integrity_check");
}

#[test]
fn legacy_proxy_history_passes_when_only_retained_messages_remain() {
    let result = summarize_legacy_proxy_history(&LegacyProxyHistoryData {
        retained_assistant_messages: 2,
        oldest_message: Some(Utc::now() - chrono::Duration::days(1)),
        newest_message: Some(Utc::now()),
        proxy_events_table_present: false,
    });

    assert_eq!(result.state, CheckState::Pass);
    assert!(
        result
            .detail
            .contains("retaining 2 proxy-sourced assistant rows")
    );
    assert!(result.detail.contains("transcript tailing"));
}

#[test]
fn legacy_proxy_history_warns_when_proxy_events_table_is_still_present() {
    let result = summarize_legacy_proxy_history(&LegacyProxyHistoryData {
        retained_assistant_messages: 1,
        oldest_message: Some(Utc::now()),
        newest_message: Some(Utc::now()),
        proxy_events_table_present: true,
    });

    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("obsolete `proxy_events` table"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("budi db check --fix")
    );
}

#[test]
fn legacy_proxy_history_loader_reads_proxy_rows_and_table_presence() {
    let conn = Connection::open_in_memory().unwrap();
    budi_core::migration::migrate(&conn).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE proxy_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL
        );
        INSERT INTO messages (
            id, role, timestamp, model, provider, input_tokens, output_tokens,
            cache_creation_tokens, cache_read_tokens,
            cost_cents_ingested, cost_cents_effective, cost_confidence
        ) VALUES (
            'legacy-proxy-row', 'assistant', '2026-04-19T17:00:00Z', 'gpt-4o',
            'openai', 1, 1, 0, 0, 0.5, 0.5, 'proxy_estimated'
        );
        ",
    )
    .unwrap();

    let data = load_legacy_proxy_history(&conn).unwrap();

    assert_eq!(data.retained_assistant_messages, 1);
    assert!(data.proxy_events_table_present);
    assert_eq!(
        data.newest_message
            .expect("newest proxy timestamp should parse")
            .to_rfc3339(),
        "2026-04-19T17:00:00+00:00"
    );
}

#[test]
fn transcript_visibility_fails_when_latest_file_is_untracked() {
    let mut data = diag("Claude Code");
    data.latest_tail_offset = None;

    let result = summarize_transcript_visibility(&data);

    assert_eq!(result.state, CheckState::Fail);
    assert!(result.detail.contains("not tracked by the tailer"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("budi-daemon")
    );
}

#[test]
fn transcript_visibility_passes_on_small_gap_with_recent_tailer_activity() {
    let mut data = diag("Claude Code");
    data.latest_tail_offset = Some(96);

    let result = summarize_transcript_visibility(&data);

    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("24 B behind a live write"));
    assert!(result.detail.contains("tailer last read"));
}

fn path() -> &'static Path {
    Path::new("/tmp/watch/session.jsonl")
}

#[test]
fn visibility_passes_when_caught_up() {
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        0,
        Some(now),
        Some(now),
        now,
    );
    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("caught up"));
    assert!(result.fix.is_none());
}

#[test]
fn visibility_passes_on_live_write_drift() {
    // Precisely the 2026-04-20 fresh-user repro: 2551 B gap, tailer 1s ago.
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        2551,
        Some(now - chrono::Duration::seconds(1)),
        Some(now - chrono::Duration::seconds(1)),
        now,
    );
    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("2.5 KB behind a live write"));
    assert!(result.fix.is_none());
}

#[test]
fn visibility_warns_when_tailer_activity_is_stale_but_gap_is_small() {
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        4096,
        Some(now - chrono::Duration::seconds(120)),
        Some(now - chrono::Duration::seconds(120)),
        now,
    );
    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("4.0 KB behind"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("daemon.log")
    );
}

#[test]
fn visibility_warns_when_gap_exceeds_live_write_threshold() {
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        2 * 1024 * 1024,
        Some(now - chrono::Duration::seconds(2)),
        Some(now - chrono::Duration::seconds(2)),
        now,
    );
    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("2.0 MB behind"));
}

#[test]
fn visibility_fails_when_gap_exceeds_wedge_threshold() {
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        20 * 1024 * 1024,
        Some(now - chrono::Duration::seconds(1)),
        Some(now - chrono::Duration::seconds(1)),
        now,
    );
    assert_eq!(result.state, CheckState::Fail);
    assert!(result.detail.contains("20.0 MB behind"));
    assert!(result.detail.contains("wedge threshold"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("daemon.log")
    );
}

#[test]
fn visibility_fails_when_file_is_actively_written_but_tailer_is_idle() {
    let now = Utc::now();
    let result = classify_transcript_visibility(
        "transcript visibility / Cursor".to_string(),
        path(),
        4096,
        Some(now - chrono::Duration::seconds(600)),
        Some(now - chrono::Duration::seconds(2)),
        now,
    );
    assert_eq!(result.state, CheckState::Fail);
    assert!(result.detail.contains("actively being written"));
    assert!(result.detail.contains("has not read it in"));
}

#[test]
fn visibility_does_not_suggest_restart_with_budi_init() {
    // Regression guard for #438 — the legacy FAIL message told users to
    // restart with `budi init` on harmless live-write drift. Never do that.
    let now = Utc::now();
    for gap in [0u64, 2_551, 4096, 2 * 1024 * 1024, 20 * 1024 * 1024] {
        let result = classify_transcript_visibility(
            "transcript visibility / Cursor".to_string(),
            path(),
            gap,
            Some(now - chrono::Duration::seconds(1)),
            Some(now - chrono::Duration::seconds(1)),
            now,
        );
        let fix = result.fix.as_deref().unwrap_or_default();
        assert!(
            !fix.contains("budi init"),
            "fix copy should not suggest `budi init` (gap={gap}): {fix:?}"
        );
    }
}

#[test]
fn format_bytes_rounds_units() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(2551), "2.5 KB");
    assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MB");
}

#[test]
fn transcript_visibility_passes_when_no_activity_today() {
    let mut data = diag("Claude Code");
    data.latest_file_mtime = Some(Utc::now() - chrono::Duration::days(2));

    let result = summarize_transcript_visibility(&data);

    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("no transcript activity today"));
}

#[test]
fn tailer_health_warns_when_watch_root_is_missing() {
    let mut data = diag("Cursor");
    data.watch_roots.clear();

    let result = summarize_tailer_health(&data);

    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("no transcript watch roots"));
}

#[test]
fn tailer_health_fails_when_offsets_are_missing() {
    let mut data = diag("Claude Code");
    data.tracked_offsets = Some(0);

    let result = summarize_tailer_health(&data);

    assert_eq!(result.state, CheckState::Fail);
    assert!(result.detail.contains("has not seeded any offsets"));
}

// R1.3 (#670): zero-rows-from-tailer AMBER signal.
//
// Acceptance from the ticket:
//   - 50 KB advance + 0 rows in window  → AMBER (parser-regression hint).
//   - same advance + N>0 rows in window → PASS.
//   - zero advance + zero rows          → PASS (idle, not AMBER).

#[test]
fn tailer_rows_ambers_when_bytes_flow_but_zero_rows_land_for_copilot_chat() {
    let now = Utc::now();
    let activity = TailerRowsActivity {
        advanced_bytes: 50 * 1024,
        last_seen: Some(now - chrono::Duration::seconds(30)),
        rows_in_window: 0,
        db_error: None,
    };

    let result = classify_tailer_rows(
        "tailer rows / Copilot Chat".to_string(),
        "copilot_chat",
        &activity,
        now,
    );

    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("50.0 KB"));
    assert!(result.detail.contains("no copilot_chat rows landed"));
    assert!(result.detail.contains("ADR-0092"));
    assert!(result.detail.contains("MIN_API_VERSION"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("daemon.log")
    );
}

#[test]
fn tailer_rows_passes_when_bytes_flow_and_rows_land_in_window() {
    let now = Utc::now();
    let activity = TailerRowsActivity {
        advanced_bytes: 50 * 1024,
        last_seen: Some(now - chrono::Duration::seconds(30)),
        rows_in_window: 7,
        db_error: None,
    };

    let result = classify_tailer_rows(
        "tailer rows / Copilot Chat".to_string(),
        "copilot_chat",
        &activity,
        now,
    );

    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("7 row(s) landed"));
    assert!(result.fix.is_none());
}

#[test]
fn tailer_rows_passes_when_idle_with_zero_advance_and_zero_rows() {
    let now = Utc::now();
    let activity = TailerRowsActivity {
        advanced_bytes: 0,
        last_seen: Some(now - chrono::Duration::seconds(10)),
        rows_in_window: 0,
        db_error: None,
    };

    let result = classify_tailer_rows(
        "tailer rows / Copilot Chat".to_string(),
        "copilot_chat",
        &activity,
        now,
    );

    // Idle workspace: no bytes consumed, no rows expected — must NOT be AMBER.
    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("no recent tailer advance"));
}

#[test]
fn tailer_rows_amber_message_omits_adr_pointer_for_non_copilot_chat_providers() {
    let now = Utc::now();
    let activity = TailerRowsActivity {
        advanced_bytes: 12 * 1024,
        last_seen: Some(now - chrono::Duration::seconds(60)),
        rows_in_window: 0,
        db_error: None,
    };

    let result = classify_tailer_rows("tailer rows / Cursor".to_string(), "cursor", &activity, now);

    assert_eq!(result.state, CheckState::Warn);
    assert!(result.detail.contains("no cursor rows landed"));
    // ADR-0092 hint is copilot_chat-specific only.
    assert!(!result.detail.contains("ADR-0092"));
}

#[test]
fn tailer_rows_passes_when_last_seen_is_outside_window() {
    let now = Utc::now();
    let activity = TailerRowsActivity {
        advanced_bytes: 50 * 1024,
        last_seen: Some(now - chrono::Duration::minutes(ZERO_ROWS_WINDOW_MINUTES + 5)),
        rows_in_window: 0,
        db_error: None,
    };

    let result = classify_tailer_rows(
        "tailer rows / Copilot Chat".to_string(),
        "copilot_chat",
        &activity,
        now,
    );

    // Stale tailer activity: not the same failure mode as a parser regression.
    assert_eq!(result.state, CheckState::Pass);
}

#[test]
fn tailer_rows_loader_reads_offset_advance_and_message_count() {
    let conn = Connection::open_in_memory().unwrap();
    budi_core::migration::migrate(&conn).unwrap();

    let now = Utc::now();
    let last_seen = (now - chrono::Duration::seconds(30)).to_rfc3339();
    let recent_msg = (now - chrono::Duration::seconds(10)).to_rfc3339();
    let stale_msg = (now - chrono::Duration::hours(2)).to_rfc3339();

    // Seed a 50 KB offset advance for copilot_chat.
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, ?3, ?4)",
        params!["copilot_chat", "/tmp/sess.jsonl", 50_i64 * 1024, last_seen],
    )
    .unwrap();
    // One in-window message for copilot_chat, one stale message that must NOT be counted.
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m1', 'assistant', ?1, 'gpt-4o', 'copilot_chat', 1, 2)",
        params![recent_msg],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m2', 'assistant', ?1, 'gpt-4o', 'copilot_chat', 1, 2)",
        params![stale_msg],
    )
    .unwrap();
    // A row for a different provider must not bleed into the count.
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m3', 'assistant', ?1, 'claude-sonnet-4-5', 'claude_code', 1, 2)",
        params![recent_msg],
    )
    .unwrap();

    let activity = load_tailer_rows_activity(&conn, "copilot_chat", now);

    assert!(activity.db_error.is_none());
    assert_eq!(activity.advanced_bytes, 50 * 1024);
    assert_eq!(activity.rows_in_window, 1);
    assert!(activity.last_seen.is_some());
}

#[test]
fn tailer_rows_loader_returns_zero_when_no_offsets_or_messages_exist() {
    let conn = Connection::open_in_memory().unwrap();
    budi_core::migration::migrate(&conn).unwrap();

    let activity = load_tailer_rows_activity(&conn, "copilot_chat", Utc::now());

    assert!(activity.db_error.is_none());
    assert_eq!(activity.advanced_bytes, 0);
    assert_eq!(activity.rows_in_window, 0);
    assert!(activity.last_seen.is_none());
}

// #693: pre-boot history INFO signal.
//
// Acceptance from the ticket:
//   - tail_offsets seeded + lifetime messages = 0  → INFO with backfill hint.
//   - tail_offsets seeded + lifetime messages > 0  → PASS (idempotent silence).
//   - no tail_offsets seeded                       → PASS (nothing to backfill).

#[test]
fn pre_boot_history_info_when_seeded_offsets_have_no_messages() {
    let activity = PreBootHistoryActivity {
        seeded_files: 3,
        advanced_bytes: 100 * 1024,
        lifetime_messages: 0,
        db_error: None,
    };

    let result = classify_pre_boot_history(
        "pre-boot history detected / Claude Code".to_string(),
        &activity,
    );

    assert_eq!(result.state, CheckState::Info);
    assert!(result.detail.contains("3 transcript(s) seeded as history"));
    assert!(result.detail.contains("budi db import"));
    assert!(
        result
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("budi db import")
    );
}

#[test]
fn pre_boot_history_passes_idempotently_once_messages_exist() {
    let activity = PreBootHistoryActivity {
        seeded_files: 3,
        advanced_bytes: 100 * 1024,
        lifetime_messages: 42,
        db_error: None,
    };

    let result = classify_pre_boot_history(
        "pre-boot history detected / Claude Code".to_string(),
        &activity,
    );

    assert_eq!(result.state, CheckState::Pass);
    assert!(result.fix.is_none());
    // Detail mentions both counts so the operator can see backfill /
    // live-ingest already produced rows for this provider.
    assert!(result.detail.contains("3 pre-boot transcript(s)"));
    assert!(result.detail.contains("42 message row(s)"));
}

#[test]
fn pre_boot_history_passes_silently_when_nothing_was_seeded() {
    let activity = PreBootHistoryActivity {
        seeded_files: 0,
        advanced_bytes: 0,
        lifetime_messages: 0,
        db_error: None,
    };

    let result =
        classify_pre_boot_history("pre-boot history detected / Cursor".to_string(), &activity);

    assert_eq!(result.state, CheckState::Pass);
    assert!(result.detail.contains("no pre-boot transcripts"));
    assert!(result.fix.is_none());
}

#[test]
fn pre_boot_history_loader_separates_seeded_from_zero_offset_rows() {
    let conn = Connection::open_in_memory().unwrap();
    budi_core::migration::migrate(&conn).unwrap();

    let now = Utc::now().to_rfc3339();
    // Two seeded transcripts (byte_offset > 0) — these are the
    // pre-existing files seed_offsets jumped past on first boot.
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, ?3, ?4)",
        params!["claude_code", "/tmp/sess-1.jsonl", 32_i64 * 1024, now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, ?3, ?4)",
        params!["claude_code", "/tmp/sess-2.jsonl", 16_i64 * 1024, now],
    )
    .unwrap();
    // A zero-offset row (e.g. a freshly tracked empty file) must not be
    // counted as "history" — the discoverability gap only fires when
    // there's actual byte content the user could backfill.
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, 0, ?3)",
        params!["claude_code", "/tmp/empty.jsonl", now],
    )
    .unwrap();
    // Different provider — must not bleed into the count.
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, ?3, ?4)",
        params!["copilot_chat", "/tmp/other.jsonl", 8_i64 * 1024, now],
    )
    .unwrap();

    let activity = load_pre_boot_history_activity(&conn, "claude_code");

    assert!(activity.db_error.is_none());
    assert_eq!(activity.seeded_files, 2);
    assert_eq!(activity.advanced_bytes, 48 * 1024);
    assert_eq!(activity.lifetime_messages, 0);
}

#[test]
fn pre_boot_history_loader_counts_lifetime_messages_per_provider() {
    let conn = Connection::open_in_memory().unwrap();
    budi_core::migration::migrate(&conn).unwrap();

    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tail_offsets (provider, path, byte_offset, last_seen) VALUES (?1, ?2, ?3, ?4)",
        params!["claude_code", "/tmp/sess.jsonl", 1024_i64, now],
    )
    .unwrap();
    // Two messages for claude_code (must count) and one for copilot_chat
    // (must not bleed in).
    let recent = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let stale = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m1', 'assistant', ?1, 'claude-sonnet-4-5', 'claude_code', 1, 2)",
        params![recent],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m2', 'assistant', ?1, 'claude-sonnet-4-5', 'claude_code', 1, 2)",
        params![stale],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (id, role, timestamp, model, provider, input_tokens, output_tokens) VALUES ('m3', 'assistant', ?1, 'gpt-4o', 'copilot_chat', 1, 2)",
        params![recent],
    )
    .unwrap();

    let activity = load_pre_boot_history_activity(&conn, "claude_code");

    assert!(activity.db_error.is_none());
    assert_eq!(activity.seeded_files, 1);
    assert_eq!(activity.advanced_bytes, 1024);
    // Both stale and recent rows count — this is a *lifetime* check,
    // not a windowed one.
    assert_eq!(activity.lifetime_messages, 2);
}

#[test]
fn daemon_already_running_is_pass_with_no_outage() {
    let check = DaemonCheck {
        result: CheckResult::pass("daemon health", "responding on http://127.0.0.1:7878"),
        started_this_run: false,
        outage: None,
    };
    assert_eq!(check.result.state, CheckState::Pass);
    assert!(!check.started_this_run);
    assert!(check.outage.is_none());
}

#[test]
fn daemon_auto_recovered_is_warn_with_outage() {
    let outage = OutageSummary {
        last_log_entry: Some("2026-04-30T22:42:35+00:00".to_string()),
        gap_seconds: Some(79200),
        supervisor: "launchd LaunchAgent: installed (not running)".to_string(),
    };
    let check = DaemonCheck {
        result: CheckResult::warn(
            "daemon health",
            format!(
                "auto-recovered: was NOT running on first probe{}",
                format_outage_display(&outage),
            ),
            None,
        ),
        started_this_run: true,
        outage: Some(outage),
    };

    assert_eq!(check.result.state, CheckState::Warn);
    assert!(check.started_this_run);
    assert!(check.result.detail.contains("auto-recovered"));
    assert!(check.result.detail.contains("last log entry ~22h ago"));
    assert!(
        check
            .result
            .detail
            .contains("supervisor: launchd LaunchAgent")
    );

    let outage = check.outage.as_ref().unwrap();
    assert_eq!(outage.gap_seconds, Some(79200));
    assert!(outage.supervisor.contains("launchd"));
}

#[test]
fn daemon_json_includes_auto_recovered_and_previous_outage() {
    let outage = OutageSummary {
        last_log_entry: Some("2026-04-30T22:42:35+00:00".to_string()),
        gap_seconds: Some(79200),
        supervisor: "launchd LaunchAgent: installed (not running)".to_string(),
    };
    let mut entry = CheckResultJson::from(&CheckResult::warn(
        "daemon health",
        "auto-recovered: was NOT running",
        None,
    ));
    entry.auto_recovered = Some(true);
    entry.previous_outage = Some(PreviousOutageJson {
        last_log_entry: outage.last_log_entry.clone(),
        gap_seconds: outage.gap_seconds,
        supervisor: outage.supervisor.clone(),
    });

    let v = serde_json::to_value(&entry).expect("serialise");
    let obj = v.as_object().unwrap();

    assert_eq!(obj["auto_recovered"], serde_json::json!(true));

    let po = &obj["previous_outage"];
    assert_eq!(
        po["last_log_entry"],
        serde_json::json!("2026-04-30T22:42:35+00:00")
    );
    assert_eq!(po["gap_seconds"], serde_json::json!(79200));
    assert!(
        po["supervisor"]
            .as_str()
            .unwrap()
            .contains("launchd LaunchAgent")
    );
}

#[test]
fn daemon_json_omits_outage_fields_when_already_running() {
    let entry = CheckResultJson::from(&CheckResult::pass(
        "daemon health",
        "responding on http://127.0.0.1:7878",
    ));
    let v = serde_json::to_value(&entry).expect("serialise");
    let obj = v.as_object().unwrap();

    assert!(obj.get("auto_recovered").is_none());
    assert!(obj.get("previous_outage").is_none());

    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["detail", "name", "status"]);
}

#[test]
fn format_outage_display_includes_gap_and_supervisor() {
    let outage = OutageSummary {
        last_log_entry: Some("2026-04-30T22:42:35+00:00".to_string()),
        gap_seconds: Some(7200),
        supervisor: "launchd LaunchAgent: installed (not running)".to_string(),
    };
    let display = format_outage_display(&outage);
    assert!(display.contains("last log entry ~2h ago"));
    assert!(display.contains("supervisor: launchd LaunchAgent"));
}

#[test]
fn format_outage_display_without_log_shows_only_supervisor() {
    let outage = OutageSummary {
        last_log_entry: None,
        gap_seconds: None,
        supervisor: "systemd user service: not installed".to_string(),
    };
    let display = format_outage_display(&outage);
    assert!(!display.contains("last log entry"));
    assert!(display.contains("supervisor: systemd user service"));
}

// -----------------------------------------------------------------
// Detected providers (#653 / R1.6)
// -----------------------------------------------------------------

#[test]
fn host_hint_picks_specific_vscode_variant_first() {
    let roots = vec![PathBuf::from(
        "/Users/me/Library/Application Support/Code - Insiders/User/workspaceStorage",
    )];
    assert_eq!(
        host_hint_from_paths(&roots),
        Some("VS Code Insiders".to_string())
    );
}

#[test]
fn host_hint_collapses_duplicate_hosts_and_orders_by_appearance() {
    let roots = vec![
        PathBuf::from("/Users/me/Library/Application Support/Code/User/workspaceStorage"),
        PathBuf::from("/Users/me/Library/Application Support/Cursor/User/workspaceStorage"),
        PathBuf::from("/Users/me/Library/Application Support/Code/User/globalStorage"),
    ];
    let hint = host_hint_from_paths(&roots).expect("hosts present");
    assert!(hint.contains("VS Code"));
    assert!(hint.contains("Cursor"));
    // Duplicate "VS Code" must not appear twice.
    assert_eq!(
        hint.matches("VS Code").count(),
        1,
        "host hint should dedupe VS Code: {hint}"
    );
}

#[test]
fn host_hint_returns_none_for_non_host_scoped_paths() {
    let roots = vec![PathBuf::from("/Users/me/.claude/projects")];
    assert_eq!(host_hint_from_paths(&roots), None);
}

#[test]
fn provider_for_extension_id_recognises_copilot_ids_case_insensitively() {
    assert_eq!(
        provider_for_extension_id("GitHub.copilot-chat"),
        Some("copilot_chat")
    );
    assert_eq!(
        provider_for_extension_id("github.copilot"),
        Some("copilot_chat")
    );
    assert_eq!(provider_for_extension_id("ms-python.python"), None);
}

#[test]
fn merge_hint_extensions_accepts_object_shape() {
    let mut hints = HostExtensionHints::default();
    let doc = serde_json::json!({
        "installed_extensions": {
            "copilot_chat": ["github.copilot-chat", "github.copilot"],
            "cursor": []
        }
    });
    merge_hint_extensions(&mut hints, doc);
    let exts = hints.extensions_for("copilot_chat").expect("present");
    assert_eq!(
        exts,
        &vec![
            "github.copilot-chat".to_string(),
            "github.copilot".to_string()
        ]
    );
    // Empty arrays are filtered out by `extensions_for`.
    assert!(hints.extensions_for("cursor").is_none());
}

#[test]
fn merge_hint_extensions_accepts_flat_array_via_known_id_map() {
    let mut hints = HostExtensionHints::default();
    let doc = serde_json::json!({
        "installed_extensions": ["github.copilot-chat", "ms-python.python"]
    });
    merge_hint_extensions(&mut hints, doc);
    // Known id is bucketed by provider.
    assert_eq!(
        hints.extensions_for("copilot_chat").cloned(),
        Some(vec!["github.copilot-chat".to_string()])
    );
    // Unknown ids are ignored — the doctor output stays tight.
    for provider in ["claude_code", "cursor", "codex", "copilot_cli", "ms-python"] {
        assert!(hints.extensions_for(provider).is_none());
    }
}

#[test]
fn merge_hint_extensions_dedupes_repeated_ids() {
    let mut hints = HostExtensionHints::default();
    let doc = serde_json::json!({
        "installed_extensions": {
            "copilot_chat": ["github.copilot-chat", "github.copilot-chat"]
        }
    });
    merge_hint_extensions(&mut hints, doc);
    assert_eq!(
        hints.extensions_for("copilot_chat").cloned(),
        Some(vec!["github.copilot-chat".to_string()])
    );
}

#[test]
fn merge_hint_extensions_ignores_unknown_top_level_fields() {
    // ADR-0086 §3.4 v1 schema: {active_session_id, updated_at}. A v1
    // file with no `installed_extensions` field must be a silent no-op
    // so the doctor output doesn't regress when the user is on an old
    // budi-cursor build.
    let mut hints = HostExtensionHints::default();
    let doc = serde_json::json!({
        "active_session_id": "abc",
        "updated_at": "2026-05-06T20:00:00Z"
    });
    merge_hint_extensions(&mut hints, doc);
    assert!(hints.by_provider.is_empty());
}

#[test]
fn merge_hint_extensions_accepts_v1_1_surface_field_for_vscode_host() {
    // ADR-0086 §3.4 v1.1 schema (#780): the host-aware extension writes
    // the same `cursor-sessions.json` regardless of host and tags the
    // host via an optional `surface` field. The daemon-side loader is
    // permissive — `surface` is purely informational on this path and
    // must not interfere with `installed_extensions` merging.
    let mut hints = HostExtensionHints::default();
    let doc = serde_json::json!({
        "active_session_id": "abc",
        "updated_at": "2026-05-06T20:00:00Z",
        "surface": "vscode",
        "installed_extensions": {
            "copilot_chat": ["github.copilot-chat"]
        }
    });
    merge_hint_extensions(&mut hints, doc);
    assert_eq!(
        hints.extensions_for("copilot_chat"),
        Some(&vec!["github.copilot-chat".to_string()])
    );
}

#[test]
fn read_session_hint_file_returns_none_for_missing_or_invalid() {
    // Missing file.
    assert!(read_session_hint_file(Path::new("/tmp/nonexistent-budi-doctor-hints.json")).is_none());
    // Garbage content.
    let tmp = std::env::temp_dir().join("budi-doctor-invalid-hints.json");
    std::fs::write(&tmp, b"{not-json").unwrap();
    assert!(read_session_hint_file(&tmp).is_none());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn detected_providers_warns_when_nothing_is_available() {
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(StubDetectProvider::new(
            "copilot_chat",
            "Copilot Chat",
            false,
        )),
        Box::new(StubDetectProvider::new("cursor", "Cursor", false)),
    ];
    let hints = HostExtensionHints::default();

    let results = summarize_detected_providers(&providers, &hints);

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].state, CheckState::Warn);
    assert_eq!(results[0].label, "detected providers");
    assert!(results[0].detail.contains("no AI editor data detected"));
    assert!(
        results[0]
            .fix
            .as_deref()
            .unwrap_or_default()
            .contains("Open one of your AI editors")
    );
}

#[test]
fn detected_providers_lists_each_available_provider() {
    let cursor_root =
        PathBuf::from("/Users/me/Library/Application Support/Cursor/User/workspaceStorage");
    let copilot_root =
        PathBuf::from("/Users/me/Library/Application Support/Code/User/workspaceStorage");
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(
            StubDetectProvider::new("copilot_chat", "Copilot Chat", true)
                .with_watch_roots(vec![copilot_root])
                .with_files(vec![PathBuf::from("/tmp/budi-doctor-test/copilot.json")]),
        ),
        Box::new(
            StubDetectProvider::new("cursor", "Cursor", true).with_watch_roots(vec![cursor_root]),
        ),
        Box::new(StubDetectProvider::new("codex", "Codex", false)),
    ];
    let hints = HostExtensionHints::default();

    let results = summarize_detected_providers(&providers, &hints);

    let labels: Vec<&str> = results.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "detected providers / Copilot Chat",
            "detected providers / Cursor",
        ],
        "only available providers should appear",
    );
    for r in &results {
        assert_eq!(r.state, CheckState::Pass, "{r:?}");
    }
    assert!(results[0].detail.contains("VS Code"));
    assert!(results[1].detail.contains("Cursor"));
}

#[test]
fn detected_providers_handles_zero_watch_roots_and_zero_sessions() {
    let providers: Vec<Box<dyn Provider>> = vec![Box::new(StubDetectProvider::new(
        "copilot_chat",
        "Copilot Chat",
        true,
    ))];
    let hints = HostExtensionHints::default();

    let results = summarize_detected_providers(&providers, &hints);

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].state, CheckState::Pass);
    assert!(
        results[0]
            .detail
            .contains("0 watch root(s) detected, no sessions yet"),
        "{}",
        results[0].detail
    );
}

#[test]
fn detected_providers_appends_extension_hint_when_present() {
    let providers: Vec<Box<dyn Provider>> = vec![Box::new(
        StubDetectProvider::new("copilot_chat", "Copilot Chat", true).with_watch_roots(vec![
            PathBuf::from("/Users/me/Library/Application Support/Code/User/workspaceStorage"),
        ]),
    )];
    let mut hints = HostExtensionHints::default();
    hints.by_provider.insert(
        "copilot_chat".to_string(),
        vec!["github.copilot-chat".to_string()],
    );

    let results = summarize_detected_providers(&providers, &hints);

    assert_eq!(results.len(), 1);
    assert!(
        results[0]
            .detail
            .contains("installed extension hints: github.copilot-chat"),
        "{}",
        results[0].detail
    );
}

/// Stubs out a `Provider` with explicit availability and watch-root
/// values so `summarize_detected_providers` can be tested without
/// touching the real filesystem or env vars (which would race against
/// other tests in the same binary).
struct StubDetectProvider {
    name: &'static str,
    display: &'static str,
    available: bool,
    watch_roots: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

impl StubDetectProvider {
    fn new(name: &'static str, display: &'static str, available: bool) -> Self {
        Self {
            name,
            display,
            available,
            watch_roots: Vec::new(),
            files: Vec::new(),
        }
    }
    fn with_watch_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.watch_roots = roots;
        self
    }
    fn with_files(mut self, files: Vec<PathBuf>) -> Self {
        self.files = files;
        self
    }
}

impl Provider for StubDetectProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    fn display_name(&self) -> &'static str {
        self.display
    }
    fn is_available(&self) -> bool {
        self.available
    }
    fn discover_files(&self) -> anyhow::Result<Vec<budi_core::provider::DiscoveredFile>> {
        Ok(self
            .files
            .iter()
            .cloned()
            .map(|path| budi_core::provider::DiscoveredFile { path })
            .collect())
    }
    fn parse_file(
        &self,
        _path: &Path,
        _content: &str,
        _offset: usize,
    ) -> anyhow::Result<(Vec<budi_core::jsonl::ParsedMessage>, usize)> {
        Ok((Vec::new(), 0))
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.watch_roots.clone()
    }
}
