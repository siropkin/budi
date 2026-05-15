use std::cell::Cell;

use super::*;

#[test]
fn ensure_daemon_ready_checks_running_daemon_too() {
    let config = BudiConfig::default();
    let ensure_calls = Cell::new(0usize);

    let result = ensure_daemon_ready(
        None,
        &config,
        |_| true,
        |_, _| {
            ensure_calls.set(ensure_calls.get() + 1);
            Ok(())
        },
    );

    assert!(result.is_ok());
    assert_eq!(ensure_calls.get(), 1);
}

#[test]
fn ensure_daemon_ready_still_checks_when_daemon_is_down() {
    let config = BudiConfig::default();
    let ensure_calls = Cell::new(0usize);

    let result = ensure_daemon_ready(
        None,
        &config,
        |_| false,
        |_, _| {
            ensure_calls.set(ensure_calls.get() + 1);
            Ok(())
        },
    );

    assert!(result.is_ok());
    assert_eq!(ensure_calls.get(), 1);
}

#[test]
fn ensure_daemon_ready_uses_startup_error_context_when_unhealthy() {
    let config = BudiConfig::default();
    let err = ensure_daemon_ready(None, &config, |_| false, |_, _| anyhow::bail!("boom"))
        .expect_err("should fail");

    assert!(
        err.to_string()
            .contains("Failed to start budi daemon. Run `budi doctor` to diagnose."),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_needs_migration_error_extracts_message() {
    // Body text was renamed `budi db migrate` → `budi db check --fix`
    // in 8.3.14 (#586). The wire contract (`needs_migration: true`)
    // is unchanged; only the human-readable verb in `error` moved.
    let body = r#"{"ok":false,"error":"analytics schema is v0, daemon expects v1; run `budi db check --fix` (or `budi init`) to upgrade","needs_migration":true,"current":0,"target":1}"#;
    let msg = parse_needs_migration_error(body).expect("body matches #366 contract");
    assert!(
        msg.contains("analytics schema is v0, daemon expects v1"),
        "unexpected message: {msg}"
    );
    assert!(
        msg.contains("budi db check --fix"),
        "should mention budi db check --fix"
    );
}

#[test]
fn parse_needs_migration_error_skips_unrelated_503() {
    let body = r#"{"ok":false,"error":"cloud backend unreachable"}"#;
    assert!(parse_needs_migration_error(body).is_none());
}

#[test]
fn parse_needs_migration_error_skips_non_json() {
    assert!(parse_needs_migration_error("").is_none());
    assert!(parse_needs_migration_error("not json").is_none());
}

#[test]
fn ensure_daemon_ready_uses_mismatch_error_context_when_healthy() {
    let config = BudiConfig::default();
    let err = ensure_daemon_ready(None, &config, |_| true, |_, _| anyhow::bail!("boom"))
        .expect_err("should fail");

    assert!(
        err.to_string()
            .contains("Failed to validate or restart budi daemon. Run `budi doctor` to diagnose."),
        "unexpected error: {err}"
    );
}

// ─── #682: breakdown methods forward `--provider` as `?providers=` ───
//
// Each breakdown HTTP method must thread the CLI `--provider` flag into
// a `providers=` query parameter so the daemon's `DimensionParams` (which
// aliases `providers` → `agents`) can scope the SQL filter. Pre-#682 the
// CLI accepted `--provider X` for the summary view only and silently
// dropped it on every breakdown — the bug this ticket fixes.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// Spin up a one-shot HTTP server on 127.0.0.1, capture the first
/// request's path+query, respond with `body`, and return the captured
/// request line. The empty JSON body matches `BreakdownPage<T>` for any
/// `T` that has no required fields beyond the ones below.
fn one_shot_server(body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        // First line is `GET /path?query HTTP/1.1`.
        let request_line = req.lines().next().unwrap_or("").to_string();
        let _ = tx.send(request_line);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

fn assert_providers_forwarded(request_line: &str, expected: &str) {
    assert!(
        request_line.contains(&format!("providers={expected}")),
        "expected `providers={expected}` in request line, got: {request_line}"
    );
}

/// Empty `BreakdownPage` JSON. Works for every `T` because both
/// `rows` and `other` are absent / empty. Produced as a `&'static str`
/// so the spawned thread has no lifetime issues.
const EMPTY_PAGE_BODY: &str =
    r#"{"rows":[],"total_cost_cents":0.0,"total_rows":0,"shown_rows":0,"limit":5}"#;

#[test]
fn projects_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .projects(None, None, Some("copilot_chat"), &[], 5)
        .expect("projects call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn branches_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .branches(None, None, Some("copilot_chat"), &[], 5)
        .expect("branches call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn tickets_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .tickets(None, None, Some("copilot_chat"), &[], 5)
        .expect("tickets call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn activities_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .activities(None, None, Some("copilot_chat"), &[], 5)
        .expect("activities call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn files_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .files(None, None, Some("copilot_chat"), &[], 5)
        .expect("files call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn models_forwards_provider_filter() {
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .models(None, None, Some("copilot_chat"), &[], 5)
        .expect("models call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert_providers_forwarded(&req, "copilot_chat");
}

#[test]
fn breakdown_omits_providers_when_filter_is_none() {
    // `--provider` unset must not synthesize a stray `providers=` —
    // the daemon would treat empty-string as "filter to nothing".
    let (base, rx) = one_shot_server(EMPTY_PAGE_BODY);
    let client = DaemonClient::for_tests(base);
    let _ = client
        .models(None, None, None, &[], 5)
        .expect("models call");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("captured");
    assert!(
        !req.contains("providers="),
        "no provider filter must omit the param entirely, got: {req}"
    );
}

// ─── #822: mock-server coverage for every public client method ───
//
// The block above (added in #682) exercised provider-forwarding for the
// six breakdown methods. Everything below was added in #822 to drive
// `cli/src/client.rs` line coverage above the 65% threshold required
// by the 8.5.2 quality bar. Each test stands up the existing one-shot
// TCP listener with a configurable status + body, calls one method, and
// asserts either:
//   - happy path: the daemon returns a representative body and the call
//     yields `Ok(...)` with the expected request path/query, OR
//   - error path: the daemon returns a non-2xx (or special-cased body)
//     and the call yields an `Err` (or the documented Ok-with-error
//     shape for `pricing_refresh`'s 502 branch).

/// One-shot server with configurable status + body. Mirrors
/// `one_shot_server` but lets the caller drive non-2xx paths through
/// `check_response`. Returns `(base_url, captured_request_line)`.
fn mock_response(status: u16, body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let request_line = req.lines().next().unwrap_or("").to_string();
        let _ = tx.send(request_line);
        let reason = match status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            409 => "Conflict",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "X",
        };
        let resp = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            status,
            reason,
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (format!("http://127.0.0.1:{port}"), rx)
}

const USAGE_SUMMARY_BODY: &str = r#"{"total_messages":3,"total_user_messages":1,"total_assistant_messages":2,"total_input_tokens":100,"total_output_tokens":50,"total_cache_creation_tokens":0,"total_cache_read_tokens":0,"total_cost_cents":1.5}"#;
const COST_BODY: &str = r#"{"total_cost":1.0,"input_cost":0.5,"output_cost":0.3,"cache_write_cost":0.1,"cache_read_cost":0.1,"cache_savings":0.0}"#;
const STATUS_SNAPSHOT_BODY: &str = r#"{"summary":{"total_messages":0,"total_user_messages":0,"total_assistant_messages":0,"total_input_tokens":0,"total_output_tokens":0,"total_cache_creation_tokens":0,"total_cache_read_tokens":0,"total_cost_cents":0.0},"cost":{"total_cost":0.0,"input_cost":0.0,"output_cost":0.0,"cache_write_cost":0.0,"cache_read_cost":0.0,"cache_savings":0.0},"providers":[]}"#;
const SYNC_RESPONSE_BODY: &str =
    r#"{"files_synced":1,"messages_ingested":2,"warnings":[],"per_provider":[]}"#;
const SYNC_STATUS_BODY: &str =
    r#"{"syncing":false,"ingest_backlog":0,"ingest_ready":0,"ingest_failed":0}"#;
const SESSION_HEALTH_BODY: &str = r#"{"state":"ok","message_count":1,"total_cost_cents":0.0,"vitals":{},"tip":"keep going","details":[]}"#;
const SESSION_ENTRY_BODY: &str = r#"{"id":"s1","started_at":null,"ended_at":null,"duration_ms":null,"message_count":0,"cost_cents":0.0,"models":[],"provider":"claude_code","repo_ids":[],"git_branches":[],"input_tokens":0,"output_tokens":0,"cost_confidence":"high"}"#;
const PAGINATED_SESSIONS_BODY: &str = r#"{"sessions":[],"total_count":0}"#;
const RESOLVED_SESSION_BODY: &str = r#"{"session_id":"abc","source":"latest","fallback_reason":"no cwd-encoded match — falling back to newest session"}"#;
const BRANCH_DETAIL_BODY: &str = r#"{"git_branch":"main","repo_id":"r","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0}"#;
const TICKET_DETAIL_BODY: &str = r#"{"ticket_id":"T-1","ticket_prefix":"T","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[]}"#;
const ACTIVITY_DETAIL_BODY: &str = r#"{"activity":"bugfix","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[]}"#;
const FILE_DETAIL_BODY: &str = r#"{"file_path":"src/main.rs","session_count":1,"message_count":1,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"cost_cents":1.0,"repo_id":"r","branches":[],"tickets":[]}"#;

fn run_with<F, T>(status: u16, body: &'static str, call: F) -> (Result<T>, String)
where
    F: FnOnce(&DaemonClient) -> Result<T>,
{
    let (base, rx) = mock_response(status, body);
    let client = DaemonClient::for_tests(base);
    let result = call(&client);
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    (result, req)
}

// ─── check_response branches ────────────────────────────────────────

#[test]
fn check_response_500_includes_body_in_error() {
    let (res, _) =
        run_with::<_, UsageSummary>(500, "boom-details", |c| c.summary(None, None, None, &[]));
    let err = res.expect_err("500 must error");
    let s = err.to_string();
    assert!(s.contains("500"), "missing status: {s}");
    assert!(s.contains("boom-details"), "missing body: {s}");
}

#[test]
fn check_response_500_empty_body_yields_status_only_error() {
    let (res, _) = run_with::<_, UsageSummary>(500, "", |c| c.summary(None, None, None, &[]));
    let err = res.expect_err("500 must error");
    let s = err.to_string();
    assert!(
        s.contains("Daemon returned") && s.contains("500"),
        "unexpected: {s}"
    );
    assert!(!s.contains(":"), "no body suffix expected: {s}");
}

#[test]
fn check_response_503_with_needs_migration_uses_friendly_message() {
    let body = r#"{"ok":false,"error":"schema v0, daemon expects v1; run `budi db check --fix`","needs_migration":true,"current":0,"target":1}"#;
    let (res, _) = run_with::<_, UsageSummary>(503, body, |c| c.summary(None, None, None, &[]));
    let err = res.expect_err("503 needs-migration must error");
    let s = err.to_string();
    assert!(
        s.contains("schema v0, daemon expects v1"),
        "should surface needs_migration error: {s}"
    );
    assert!(
        s.contains("budi db check --fix"),
        "should retain CLI hint: {s}"
    );
}

#[test]
fn check_response_503_unrelated_falls_back_to_raw_body() {
    let body = r#"{"ok":false,"error":"cloud backend unreachable"}"#;
    let (res, _) = run_with::<_, UsageSummary>(503, body, |c| c.summary(None, None, None, &[]));
    let err = res.expect_err("503 must error");
    let s = err.to_string();
    assert!(s.contains("503"), "missing status: {s}");
    assert!(s.contains("cloud backend unreachable"), "raw body: {s}");
}

// ─── describe_send_error ────────────────────────────────────────────

#[test]
fn unreachable_daemon_yields_friendly_connect_error() {
    // 127.0.0.1:1 is reserved; no service listens there.
    let client = DaemonClient::for_tests("http://127.0.0.1:1");
    let err = client
        .summary(None, None, None, &[])
        .expect_err("must fail to connect");
    let s = err.to_string();
    assert!(
        s.contains("daemon is not running") || s.contains("cannot reach daemon"),
        "unexpected error: {s}"
    );
}

// ─── Sync & migration ───────────────────────────────────────────────

#[test]
fn history_happy_path_posts_sync_all() {
    let (res, req) = run_with(200, SYNC_RESPONSE_BODY, |c| c.history());
    let sync = res.expect("history Ok");
    assert_eq!(sync.files_synced, 1);
    assert_eq!(sync.messages_ingested, 2);
    assert!(req.contains("POST /sync/all"), "wrong route: {req}");
}

#[test]
fn history_propagates_error() {
    let (res, _) = run_with::<_, SyncResponse>(500, "", |c| c.history());
    assert!(res.is_err(), "non-200 must surface as Err");
}

#[test]
fn sync_reset_happy_path_posts_sync_reset() {
    let (res, req) = run_with(200, SYNC_RESPONSE_BODY, |c| c.sync_reset());
    let _sync = res.expect("sync_reset Ok");
    assert!(req.contains("POST /sync/reset"), "wrong route: {req}");
}

#[test]
fn sync_status_happy_path() {
    let (res, req) = run_with(200, SYNC_STATUS_BODY, |c| c.sync_status());
    let status = res.expect("sync_status Ok");
    assert!(!status.syncing);
    assert!(req.contains("GET /sync/status"), "wrong route: {req}");
}

// ─── Admin ──────────────────────────────────────────────────────────

#[test]
fn check_happy_path() {
    let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.check());
    let v = res.expect("check Ok");
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
    assert!(req.contains("GET /admin/check"), "wrong route: {req}");
}

#[test]
fn repair_happy_path() {
    let (res, req) = run_with(200, r#"{"repaired":3}"#, |c| c.repair());
    let _v = res.expect("repair Ok");
    assert!(req.contains("POST /admin/repair"), "wrong route: {req}");
}

// ─── Cloud ──────────────────────────────────────────────────────────

#[test]
fn cloud_sync_happy_path() {
    let (res, req) = run_with(200, r#"{"ok":true,"result":"ok"}"#, |c| c.cloud_sync());
    let v = res.expect("cloud_sync Ok");
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
    assert!(req.contains("POST /cloud/sync"), "wrong route: {req}");
}

#[test]
fn cloud_sync_propagates_error() {
    let (res, _) = run_with::<_, Value>(500, "internal", |c| c.cloud_sync());
    assert!(res.is_err(), "5xx must surface as Err");
}

#[test]
fn cloud_reset_happy_path() {
    let (res, req) = run_with(200, r#"{"ok":true,"removed":5}"#, |c| c.cloud_reset());
    let _v = res.expect("cloud_reset Ok");
    assert!(req.contains("POST /cloud/reset"), "wrong route: {req}");
}

#[test]
fn cloud_status_happy_path() {
    let (res, req) = run_with(200, r#"{"enabled":false}"#, |c| c.cloud_status());
    let _v = res.expect("cloud_status Ok");
    assert!(req.contains("GET /cloud/status"), "wrong route: {req}");
}

// ─── Pricing ────────────────────────────────────────────────────────

#[test]
fn pricing_status_happy_path() {
    let (res, req) = run_with(200, r#"{"layer":"shipped"}"#, |c| c.pricing_status());
    let v = res.expect("pricing_status Ok");
    assert_eq!(v.get("layer").and_then(Value::as_str), Some("shipped"));
    assert!(req.contains("GET /pricing/status"), "wrong route: {req}");
}

#[test]
fn pricing_refresh_happy_path() {
    let (res, req) = run_with(200, r#"{"ok":true,"version":"42"}"#, |c| {
        c.pricing_refresh()
    });
    let v = res.expect("pricing_refresh Ok");
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(true));
    assert!(req.contains("POST /pricing/refresh"), "wrong route: {req}");
}

#[test]
fn pricing_refresh_502_validation_body_returns_ok_with_structured_error() {
    // #493: a 502 with `{"ok":false,"error":...}` must be surfaced as
    // an Ok value (the CLI renderer distinguishes ok=false on its own
    // side) rather than swallowed by `check_response`.
    let body = r#"{"ok":false,"error":"manifest validation failed: unknown model 'foo'"}"#;
    let (res, _) = run_with(502, body, |c| c.pricing_refresh());
    let v = res.expect("structured 502 must round-trip as Ok");
    assert_eq!(v.get("ok").and_then(Value::as_bool), Some(false));
    assert!(
        v.get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("manifest validation failed"),
        "error message preserved: {v:?}"
    );
}

#[test]
fn pricing_refresh_502_unstructured_body_errors() {
    // Plain 502 (e.g. proxy in front of the daemon) — must error with
    // a hint pointing at `budi doctor`.
    let (res, _) = run_with::<_, Value>(502, "Bad Gateway", |c| c.pricing_refresh());
    let err = res.expect_err("unstructured 502 must error");
    let s = err.to_string();
    assert!(s.contains("502"), "should mention status: {s}");
    assert!(s.contains("Bad Gateway"), "should include body: {s}");
}

#[test]
fn pricing_refresh_other_status_errors_with_body() {
    let (res, _) = run_with::<_, Value>(500, "kaboom", |c| c.pricing_refresh());
    let err = res.expect_err("500 must error");
    let s = err.to_string();
    assert!(s.contains("500"), "{s}");
    assert!(s.contains("kaboom"), "should include body: {s}");
}

#[test]
fn pricing_refresh_other_status_empty_body_errors() {
    let (res, _) = run_with::<_, Value>(500, "", |c| c.pricing_refresh());
    let err = res.expect_err("500 must error");
    assert!(err.to_string().contains("500"));
}

#[test]
fn pricing_recompute_force_true_sends_true_query() {
    let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.pricing_recompute(true));
    let _ = res.expect("pricing_recompute Ok");
    assert!(req.contains("force=true"), "force=true expected: {req}");
    assert!(
        req.contains("POST /pricing/recompute"),
        "wrong route: {req}"
    );
}

#[test]
fn pricing_recompute_force_false_sends_false_query() {
    let (res, req) = run_with(200, r#"{"ok":true}"#, |c| c.pricing_recompute(false));
    let _ = res.expect("pricing_recompute Ok");
    assert!(req.contains("force=false"), "force=false expected: {req}");
}

// ─── Analytics: summary / cost / status_snapshot ────────────────────

#[test]
fn summary_forwards_all_query_params() {
    let (res, req) = run_with(200, USAGE_SUMMARY_BODY, |c| {
        c.summary(
            Some("2026-01-01"),
            Some("2026-02-01"),
            Some("claude_code"),
            &["vscode".to_string(), "cursor".to_string()],
        )
    });
    let summary = res.expect("summary Ok");
    assert_eq!(summary.total_messages, 3);
    assert!(req.contains("GET /analytics/summary"), "wrong route: {req}");
    assert!(req.contains("since=2026-01-01"), "since: {req}");
    assert!(req.contains("until=2026-02-01"), "until: {req}");
    assert!(req.contains("provider=claude_code"), "provider: {req}");
    // Surfaces are joined on ',' before reqwest's query encoder turns
    // it into `vscode%2Ccursor`.
    assert!(
        req.contains("surfaces=vscode%2Ccursor"),
        "surfaces csv: {req}"
    );
}

#[test]
fn summary_omits_optional_params_when_none() {
    let (res, req) = run_with(200, USAGE_SUMMARY_BODY, |c| {
        c.summary(None, None, None, &[])
    });
    let _ = res.expect("summary Ok");
    assert!(!req.contains("since="), "no since expected: {req}");
    assert!(!req.contains("until="), "no until expected: {req}");
    assert!(!req.contains("provider="), "no provider expected: {req}");
    assert!(!req.contains("surfaces="), "no surfaces expected: {req}");
}

#[test]
fn cost_happy_path_forwards_params() {
    let (res, req) = run_with(200, COST_BODY, |c| {
        c.cost(
            Some("2026-01-01"),
            Some("2026-02-01"),
            Some("copilot_chat"),
            &["jetbrains".to_string()],
        )
    });
    let cost = res.expect("cost Ok");
    assert!((cost.total_cost - 1.0).abs() < f64::EPSILON);
    assert!(req.contains("GET /analytics/cost"), "wrong route: {req}");
    assert!(req.contains("provider=copilot_chat"), "provider: {req}");
    assert!(req.contains("surfaces=jetbrains"), "surfaces: {req}");
}

#[test]
fn status_snapshot_happy_path() {
    let (res, req) = run_with(200, STATUS_SNAPSHOT_BODY, |c| {
        c.status_snapshot(None, None, None, &[])
    });
    let _snap = res.expect("status_snapshot Ok");
    assert!(
        req.contains("GET /analytics/status_snapshot"),
        "wrong route: {req}"
    );
}

// ─── Analytics: list breakdowns ─────────────────────────────────────

#[test]
fn projects_happy_path_forwards_window_and_limit() {
    let (res, req) = run_with(200, EMPTY_PAGE_BODY, |c| {
        c.projects(
            Some("2026-01-01"),
            Some("2026-02-01"),
            None,
            &["vscode".to_string()],
            7,
        )
    });
    let _ = res.expect("projects Ok");
    assert!(
        req.contains("GET /analytics/projects"),
        "wrong route: {req}"
    );
    assert!(req.contains("limit=7"), "limit: {req}");
    assert!(req.contains("surfaces=vscode"), "surfaces: {req}");
}

#[test]
fn non_repo_happy_path() {
    let (res, req) = run_with(200, "[]", |c| c.non_repo(Some("2026-01-01"), None, 3));
    let rows = res.expect("non_repo Ok");
    assert!(rows.is_empty());
    assert!(
        req.contains("GET /analytics/non_repo"),
        "wrong route: {req}"
    );
    assert!(req.contains("limit=3"), "limit: {req}");
}

#[test]
fn tags_happy_path_forwards_key_and_window() {
    let (res, req) = run_with(200, EMPTY_PAGE_BODY, |c| {
        c.tags(Some("env"), Some("2026-01-01"), None, 9)
    });
    let _ = res.expect("tags Ok");
    assert!(req.contains("GET /analytics/tags"), "wrong route: {req}");
    assert!(req.contains("key=env"), "key: {req}");
    assert!(req.contains("limit=9"), "limit: {req}");
}

#[test]
fn providers_happy_path_emits_empty_list() {
    let (res, req) = run_with(200, "[]", |c| {
        c.providers(None, None, &["vscode".to_string()])
    });
    let stats = res.expect("providers Ok");
    assert!(stats.is_empty());
    assert!(
        req.contains("GET /analytics/providers"),
        "wrong route: {req}"
    );
    assert!(req.contains("surfaces=vscode"), "surfaces: {req}");
}

#[test]
fn surfaces_happy_path() {
    let (res, req) = run_with(200, "[]", |c| {
        c.surfaces(
            Some("2026-01-01"),
            Some("2026-02-01"),
            &["jetbrains".to_string()],
        )
    });
    let _ = res.expect("surfaces Ok");
    assert!(
        req.contains("GET /analytics/surfaces"),
        "wrong route: {req}"
    );
    assert!(req.contains("surfaces=jetbrains"), "surfaces: {req}");
}

// ─── Analytics: detail endpoints with 404 → None ───────────────────

#[test]
fn branch_detail_present_returns_some() {
    let (res, req) = run_with(200, BRANCH_DETAIL_BODY, |c| {
        c.branch_detail("main", Some("r1"), Some("2026-01-01"), None)
    });
    let detail = res.expect("branch_detail Ok").expect("Some");
    assert_eq!(detail.git_branch, "main");
    assert!(
        req.contains("GET /analytics/branches/main"),
        "wrong route: {req}"
    );
    assert!(req.contains("repo_id=r1"), "repo_id: {req}");
}

#[test]
fn branch_detail_404_returns_none() {
    let (res, _) = run_with(404, "", |c| c.branch_detail("missing", None, None, None));
    assert!(res.expect("Ok(None) on 404").is_none());
}

#[test]
fn branch_detail_null_body_returns_none() {
    let (res, _) = run_with(200, "null", |c| {
        c.branch_detail("missing", None, None, None)
    });
    assert!(res.expect("Ok(None) on null body").is_none());
}

#[test]
fn branch_detail_encodes_slash_in_branch_name() {
    let (_, req) = run_with::<_, Option<BranchCost>>(404, "", |c| {
        c.branch_detail("feat/x y", None, None, None)
    });
    // path_segments_mut percent-encodes `/` to `%2F` and space to `%20`
    assert!(
        req.contains("GET /analytics/branches/feat%2Fx%20y"),
        "encoded branch: {req}"
    );
}

#[test]
fn ticket_detail_present_returns_some() {
    let (res, req) = run_with(200, TICKET_DETAIL_BODY, |c| {
        c.ticket_detail("T-1", None, None, None)
    });
    let detail = res.expect("ticket_detail Ok").expect("Some");
    assert_eq!(detail.ticket_id, "T-1");
    assert!(
        req.contains("GET /analytics/tickets/T-1"),
        "wrong route: {req}"
    );
}

#[test]
fn ticket_detail_404_returns_none() {
    let (res, _) = run_with(404, "", |c| c.ticket_detail("nope", None, None, None));
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn ticket_detail_null_body_returns_none() {
    let (res, _) = run_with(200, "null", |c| c.ticket_detail("nope", None, None, None));
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn activity_detail_present_returns_some() {
    let (res, req) = run_with(200, ACTIVITY_DETAIL_BODY, |c| {
        c.activity_detail("bugfix", None, None, None)
    });
    let detail = res.expect("activity_detail Ok").expect("Some");
    assert_eq!(detail.activity, "bugfix");
    assert!(
        req.contains("GET /analytics/activities/bugfix"),
        "wrong route: {req}"
    );
}

#[test]
fn activity_detail_404_returns_none() {
    let (res, _) = run_with(404, "", |c| c.activity_detail("nope", None, None, None));
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn activity_detail_null_body_returns_none() {
    let (res, _) = run_with(200, "null", |c| c.activity_detail("nope", None, None, None));
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn file_detail_present_with_subpath_keeps_slashes_structural() {
    let (res, req) = run_with(200, FILE_DETAIL_BODY, |c| {
        c.file_detail("src/main.rs", Some("r1"), None, None)
    });
    let detail = res.expect("file_detail Ok").expect("Some");
    assert_eq!(detail.file_path, "src/main.rs");
    // Each path segment is pushed individually so `/` stays structural.
    assert!(
        req.contains("GET /analytics/files/src/main.rs"),
        "wrong route: {req}"
    );
    assert!(req.contains("repo_id=r1"), "repo_id: {req}");
}

#[test]
fn file_detail_404_returns_none() {
    let (res, _) = run_with(404, "", |c| {
        c.file_detail("src/missing.rs", None, None, None)
    });
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn file_detail_null_body_returns_none() {
    let (res, _) = run_with(200, "null", |c| {
        c.file_detail("src/missing.rs", None, None, None)
    });
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn file_detail_skips_empty_path_segments() {
    // `analytics_file_detail_url` filters out empty segments so a
    // leading or doubled `/` doesn't produce a `//` in the URL.
    let (_, req) = run_with::<_, Option<FileCostDetail>>(404, "", |c| {
        c.file_detail("/a//b", None, None, None)
    });
    assert!(
        req.contains("GET /analytics/files/a/b"),
        "collapsed segments: {req}"
    );
}

// ─── Analytics: sessions ────────────────────────────────────────────

#[test]
fn sessions_forwards_every_filter() {
    let (res, req) = run_with(200, PAGINATED_SESSIONS_BODY, |c| {
        c.sessions(
            Some("2026-01-01"),
            Some("2026-02-01"),
            Some("foo bar"),
            Some("claude_code"),
            &["vscode".to_string()],
            Some("T-1"),
            Some("refactor"),
            10,
            20,
        )
    });
    let page = res.expect("sessions Ok");
    assert_eq!(page.total_count, 0);
    assert!(
        req.contains("GET /analytics/sessions"),
        "wrong route: {req}"
    );
    // Per the comment in `sessions`, --provider rides as `providers=`.
    assert!(req.contains("providers=claude_code"), "providers: {req}");
    assert!(req.contains("ticket=T-1"), "ticket: {req}");
    assert!(req.contains("activity=refactor"), "activity: {req}");
    assert!(req.contains("sort_by=started_at"), "sort_by: {req}");
    assert!(req.contains("limit=10"), "limit: {req}");
    assert!(req.contains("offset=20"), "offset: {req}");
    // `search` is percent-encoded (space → `+` or `%20`).
    assert!(
        req.contains("search=foo+bar") || req.contains("search=foo%20bar"),
        "search: {req}"
    );
}

#[test]
fn session_detail_present_returns_some() {
    let (res, req) = run_with(200, SESSION_ENTRY_BODY, |c| c.session_detail("s1"));
    let entry = res.expect("session_detail Ok").expect("Some");
    assert_eq!(entry.id, "s1");
    assert!(
        req.contains("GET /analytics/sessions/s1"),
        "wrong route: {req}"
    );
}

#[test]
fn session_detail_404_returns_none() {
    let (res, _) = run_with(404, "", |c| c.session_detail("missing"));
    assert!(res.expect("Ok(None)").is_none());
}

#[test]
fn session_tags_happy_path() {
    let (res, req) = run_with(200, "[]", |c| c.session_tags("s1"));
    let tags = res.expect("session_tags Ok");
    assert!(tags.is_empty());
    assert!(
        req.contains("GET /analytics/sessions/s1/tags"),
        "wrong route: {req}"
    );
}

#[test]
fn resolve_session_token_with_cwd_emits_both_params() {
    let (res, req) = run_with(200, RESOLVED_SESSION_BODY, |c| {
        c.resolve_session_token("current", Some("/repo"))
    });
    let resolved = res.expect("resolve Ok");
    assert_eq!(resolved.session_id, "abc");
    assert!(resolved.fallback_reason.is_some(), "fallback_reason set");
    assert!(
        req.contains("GET /analytics/sessions/resolve"),
        "wrong route: {req}"
    );
    assert!(req.contains("token=current"), "token: {req}");
    assert!(req.contains("cwd="), "cwd: {req}");
}

#[test]
fn resolve_session_token_without_cwd_omits_cwd_param() {
    let (res, req) = run_with(200, RESOLVED_SESSION_BODY, |c| {
        c.resolve_session_token("latest", None)
    });
    let _ = res.expect("resolve Ok");
    assert!(req.contains("token=latest"), "token: {req}");
    assert!(!req.contains("cwd="), "no cwd expected: {req}");
}

#[test]
fn session_health_with_id_forwards_param() {
    let (res, req) = run_with(200, SESSION_HEALTH_BODY, |c| c.session_health(Some("s1")));
    let h = res.expect("session_health Ok");
    assert_eq!(h.state, "ok");
    assert!(
        req.contains("GET /analytics/session-health"),
        "wrong route: {req}"
    );
    assert!(req.contains("session_id=s1"), "session_id: {req}");
}

#[test]
fn session_health_without_id_omits_param() {
    let (res, req) = run_with(200, SESSION_HEALTH_BODY, |c| c.session_health(None));
    let _ = res.expect("session_health Ok");
    assert!(!req.contains("session_id="), "no id expected: {req}");
}
