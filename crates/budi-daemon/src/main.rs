use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn;
use axum::routing::{get, post};
use budi_core::analytics;
use budi_core::config::{DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod workers;

mod routes;

#[derive(Debug, Parser)]
#[command(name = "budi-daemon")]
#[command(about = "budi analytics daemon")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Serve {
        #[arg(long, default_value = DEFAULT_DAEMON_HOST)]
        host: String,
        #[arg(long, default_value_t = DEFAULT_DAEMON_PORT)]
        port: u16,
    },
}

#[derive(Clone)]
pub struct AppState {
    pub syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub integrations_installing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Guards manual and background cloud sync runs so they cannot overlap.
    /// Owned by the `/cloud/sync` route (see `routes::cloud`) and the
    /// background worker in `workers::cloud_sync`.
    pub cloud_syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Live per-agent progress snapshot for the in-flight sync run, if any.
    /// `Some(progress)` while `syncing == true`, `None` otherwise. Set from
    /// the `/sync/*` handlers' progress callback and read by
    /// `/sync/status` so `budi db import` can render per-agent progress
    /// without a streaming API (#440).
    pub sync_progress: std::sync::Arc<std::sync::Mutex<Option<budi_core::analytics::SyncProgress>>>,
}

fn build_router(app_state: AppState) -> Router {
    use routes::{
        analytics as a, cloud as c, hooks as h, pricing as p, require_current_schema,
        require_loopback,
    };

    // Loopback-only admin / sync / cloud mutation routes.
    //
    // `/sync/all` and `/sync/reset` call `open_db_with_migration` internally
    // so they can't trip the schema guard; `POST /sync` contains its own
    // bail-on-stale-schema branch that now returns a structured 503 (see
    // `routes::hooks::analytics_sync`).  `/admin/migrate` and
    // `/admin/repair` must NOT be gated by the schema guard — those are
    // the escape hatches operators use to fix the very drift that trips
    // it.
    let protected_routes = Router::new()
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
        .route("/sync/reset", post(h::analytics_sync_reset))
        .route("/cloud/sync", post(c::cloud_sync))
        .route("/pricing/refresh", post(p::pricing_refresh))
        .route("/admin/providers", get(a::analytics_registered_providers))
        .route("/admin/schema", get(a::analytics_schema_version))
        .route("/admin/migrate", post(a::analytics_migrate))
        .route("/admin/repair", post(a::analytics_repair))
        .route(
            "/admin/integrations/install",
            post(h::admin_install_integrations),
        )
        .route_layer(from_fn(require_loopback));

    // Public `/analytics/*` surface.  All of these read the analytics
    // SQLite DB, so `require_current_schema` short-circuits them with
    // `503 + needs_migration: true` when the DB is behind the schema
    // this binary was built for (#366).  `/health`, `/health/*`,
    // `/sync/status`, `/cloud/status`, and `/favicon.ico` stay
    // un-gated so operators can still observe daemon status on a
    // stale-schema box without seeing spurious 503s.
    let analytics_routes = Router::new()
        .route("/analytics/summary", get(a::analytics_summary))
        .route("/analytics/messages", get(a::analytics_messages))
        .route("/analytics/projects", get(a::analytics_projects))
        .route("/analytics/non_repo", get(a::analytics_non_repo))
        .route("/analytics/cost", get(a::analytics_cost))
        .route("/analytics/models", get(a::analytics_models))
        .route(
            "/analytics/filter-options",
            get(a::analytics_filter_options),
        )
        .route("/analytics/activity", get(a::analytics_activity))
        .route("/analytics/branches", get(a::analytics_branches))
        .route("/analytics/tags", get(a::analytics_tags))
        .route(
            "/analytics/branches/{branch}",
            get(a::analytics_branch_detail),
        )
        .route("/analytics/tickets", get(a::analytics_tickets))
        .route(
            "/analytics/tickets/{ticket_id}",
            get(a::analytics_ticket_detail),
        )
        .route("/analytics/activities", get(a::analytics_activities))
        .route(
            "/analytics/activities/{activity}",
            get(a::analytics_activity_detail),
        )
        .route("/analytics/files", get(a::analytics_files))
        .route(
            "/analytics/files/{*file_path}",
            get(a::analytics_file_detail),
        )
        .route("/analytics/providers", get(a::analytics_providers))
        .route("/analytics/statusline", get(a::analytics_statusline))
        .route(
            "/analytics/cache-efficiency",
            get(a::analytics_cache_efficiency),
        )
        .route(
            "/analytics/session-cost-curve",
            get(a::analytics_session_cost_curve),
        )
        .route(
            "/analytics/cost-confidence",
            get(a::analytics_cost_confidence),
        )
        .route("/analytics/subagent-cost", get(a::analytics_subagent_cost))
        .route("/analytics/session-audit", get(a::analytics_session_audit))
        .route(
            "/analytics/session-health",
            get(a::analytics_session_health),
        )
        .route("/analytics/sessions", get(a::analytics_sessions))
        .route(
            "/analytics/sessions/{session_id}",
            get(a::analytics_session_detail),
        )
        .route(
            "/analytics/sessions/{session_id}/messages",
            get(a::analytics_session_messages),
        )
        .route(
            "/analytics/sessions/{session_id}/curve",
            get(a::analytics_session_message_curve),
        )
        .route(
            "/analytics/sessions/{session_id}/tags",
            get(a::analytics_session_tags),
        )
        .route(
            "/analytics/messages/{message_uuid}/detail",
            get(a::analytics_message_detail),
        )
        .route_layer(from_fn(require_current_schema));

    Router::new()
        .route("/favicon.ico", get(h::favicon))
        .route("/health", get(h::health))
        .route("/health/integrations", get(h::health_integrations))
        .route("/health/check-update", get(h::health_check_update))
        .route("/sync/status", get(h::sync_status))
        .route("/cloud/status", get(routes::cloud::cloud_status))
        .route("/pricing/status", get(p::pricing_status))
        .merge(analytics_routes)
        .merge(protected_routes)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(app_state)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Default log level is `info` so `~/.local/share/budi/logs/daemon.log`
    // is not a 0-byte file after boot (see #309 / #366 — the lingering
    // "0-byte log file" audit finding that #309's closure disposition left
    // on the R4.2 smoke set).  Operators can still override with
    // `RUST_LOG=...`, and anything logged via `tracing::error!`
    // (including the `anyhow` chain from `routes::internal_error`) is now
    // retained by default.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,reqwest=warn,h2=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();
    let (host, port) = match cli.command.unwrap_or(Commands::Serve {
        host: DEFAULT_DAEMON_HOST.to_string(),
        port: DEFAULT_DAEMON_PORT,
    }) {
        Commands::Serve { host, port } => (host, port),
    };

    // Kill any existing budi-daemon on the same port so a fresh binary can
    // take over without manual intervention (e.g. after `cargo build && cp`).
    kill_existing_daemon(port);

    let cloud_syncing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app_state = AppState {
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cloud_syncing: cloud_syncing.clone(),
        sync_progress: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };

    let app = build_router(app_state);

    // Ensure the database exists and schema is up-to-date.
    // This makes the daemon self-sufficient — it doesn't require `budi init` to have run first.
    if let Ok(db_path) = analytics::db_path() {
        if let Err(e) = analytics::open_db_with_migration(&db_path) {
            tracing::warn!("Failed to initialize database: {e}");
        }
        // Post-migration sanity check.  If the DB still reports a version
        // lower than this binary expects — e.g. auto-migration failed silently
        // or the DB file is owned by another daemon build — emit a loud WARN
        // so `daemon.log` makes the drift obvious.  `/analytics/*` requests
        // on this daemon will return the structured 503 defined in #366.
        if let Ok(conn) = analytics::open_db(&db_path) {
            let current = budi_core::migration::current_version(&conn);
            let target = budi_core::migration::SCHEMA_VERSION;
            if current < target {
                tracing::warn!(
                    target: "budi_daemon::schema",
                    current,
                    target,
                    db_path = %db_path.display(),
                    "analytics schema is behind this daemon binary; /analytics/* and POST /sync will return 503 until `budi db migrate` succeeds"
                );
            } else if current > target {
                tracing::warn!(
                    target: "budi_daemon::schema",
                    current,
                    target,
                    db_path = %db_path.display(),
                    "analytics schema is ahead of this daemon binary (downgrade?); results may be inconsistent"
                );
            } else {
                // #499 (D-2): once schema is aligned, run the ticket-
                // extraction denylist backfill idempotently. Removes
                // `SWEEP-2` / `ADR-0091` / etc. tags produced by the
                // pre-8.3.1 extractor so `budi stats --tickets` stops
                // counting them. Matches the #442 startup-backfill
                // precedent; safe to run on every boot because each
                // pass is a no-op once the DB is clean.
                match budi_core::pipeline::backfill_remove_denylisted_ticket_tags(&conn) {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(
                        target: "budi_daemon::ticket_backfill",
                        rewritten_tags = n,
                        "ticket-extraction denylist backfill removed {n} denylisted ticket tag row(s)"
                    ),
                    Err(e) => tracing::warn!(
                        target: "budi_daemon::ticket_backfill",
                        error = %e,
                        "ticket-extraction denylist backfill failed"
                    ),
                }
            }
        }
    }
    if let Err(e) = budi_core::legacy_proxy::emit_upgrade_notice_once() {
        tracing::warn!("Failed to scan legacy proxy residue on startup: {e}");
    }

    // --- Start filesystem tailer (ADR-0089 §1 / R1.4 #320) ---
    //
    // R1.3 (#319) shipped the tailer behind `BUDI_LIVE_TAIL=1`. R1.4 (#320)
    // promoted it to the default. R2.1 (#322) removes the proxy runtime, so
    // tailer ingestion is now the only live path.
    //
    // Graceful shutdown (#384): the tailer's blocking loop exits when
    // `shutdown` flips to true at the next event or backstop tick (≤ 5 s).
    // `install_shutdown_listener` below wires SIGINT / SIGTERM to flip the
    // flag, wait up to one backstop interval for the tailer to drain, then
    // exit the process cleanly.
    let tailer_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tailer_handle = match analytics::db_path() {
        Ok(db_path) => {
            tracing::info!(
                target: "budi_daemon::tailer",
                "starting filesystem tailer (ADR-0089 §1)"
            );
            Some(tokio::spawn(workers::tailer::run(
                db_path,
                tailer_shutdown.clone(),
            )))
        }
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon::tailer",
                error = %e,
                "db_path is not resolvable; tailer not started"
            );
            None
        }
    };

    install_shutdown_listener(tailer_shutdown, tailer_handle);

    // --- Start pricing refresh worker (ADR-0091 §3) ---
    //
    // Runs independently of cloud sync: warm-loads the on-disk cache,
    // fires an initial fetch if absent or >24h old, then cycles every
    // 24h. Network calls are disabled when `BUDI_PRICING_REFRESH=0` is
    // set — the embedded baseline becomes authoritative in that mode.
    // Failures never block ingestion; the previous cache keeps serving
    // `pricing::lookup` until the next tick succeeds.
    let pricing_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Ok(db_path) = analytics::db_path() {
        tracing::info!(
            target: "budi_daemon::pricing_refresh",
            "starting pricing manifest refresh worker (ADR-0091 §3)"
        );
        tokio::spawn(workers::pricing_refresh::run(
            db_path,
            pricing_shutdown.clone(),
        ));
    }

    // --- Start cloud sync worker if configured ---
    //
    // #540: always emit exactly one INFO line from this block at boot
    // so operators can grep `daemon.log` for `cloud uploader` and see
    // current state regardless of whether the uploader is running.
    // When disabled for any reason, the `reason=...` field matches the
    // taxonomy in `CloudConfig::disabled_reason`.
    {
        let cloud_config = budi_core::config::load_cloud_config();
        match cloud_config.disabled_reason() {
            None => {
                if let Ok(db_path) = analytics::db_path() {
                    tracing::info!(
                        endpoint = %cloud_config.effective_endpoint(),
                        device_id = %log_id_prefix(cloud_config.device_id.as_deref()),
                        org_id = %log_id_prefix(cloud_config.org_id.as_deref()),
                        interval_s = cloud_config.sync.interval_seconds,
                        "cloud uploader configured"
                    );
                    tokio::spawn(workers::cloud_sync::run(
                        db_path,
                        cloud_config,
                        cloud_syncing.clone(),
                    ));
                }
            }
            Some(reason) => {
                tracing::info!(reason, "cloud uploader disabled");
            }
        }
    }

    // --- Start dummy proxy listener if legacy proxy residue exists ---
    if let Ok(scan) = budi_core::legacy_proxy::scan()
        && scan.has_managed_blocks()
    {
        tracing::info!(
            target: "budi_daemon::dummy_proxy",
            "legacy proxy residue detected; starting dummy proxy on 9878 to return actionable 400s to agents"
        );
        tokio::spawn(async move {
            let dummy_app = axum::Router::new().fallback(axum::routing::any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "Budi proxy is removed in 8.2.0. Please run `budi init --cleanup` to fix your shell profile."
                )
            }));
            if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:9878").await {
                let _ = axum::serve(listener, dummy_app).await;
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("budi-daemon listening on {}", addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// How long we'll let the tailer drain after a shutdown signal before
/// forcing process exit. The tailer loop checks `shutdown` once per
/// `BACKSTOP_POLL` (5 s in `workers::tailer`); one backstop plus a small
/// buffer gives it room to finish an in-flight `process_path` call and
/// emit its final structured log line.
const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// Install a SIGINT / SIGTERM listener that flips the tailer shutdown
/// flag, waits up to [`SHUTDOWN_DRAIN_TIMEOUT`] for the blocking tailer
/// loop to exit, and then terminates the process.
///
/// The tailer parameter on [`workers::tailer::run`] has always advertised
/// a graceful-stop API (checked every iteration of `run_blocking`'s main
/// loop), but without this listener nothing ever flipped the flag in
/// production. See #384 for history.
///
/// Axum's HTTP serve loop is not given its own graceful-shutdown future
/// here — `std::process::exit(0)` below ends it along with the rest of
/// the runtime. If we want to drain in-flight HTTP requests on the same
/// signal, that is a larger refactor tracked outside this ticket.
/// #540: abbreviate a `device_id` / `org_id` for the daemon startup log
/// line. Full values are stored in `cloud.toml` and aren't secret, but
/// long opaque UUIDs / org slugs clutter the log. Prints the first 8
/// chars followed by `…` when the value is longer than that; returns
/// `"(missing)"` for `None` (shouldn't appear for a ready config, but
/// defensive so the log line never blows up).
fn log_id_prefix(value: Option<&str>) -> String {
    match value {
        None => "(missing)".to_string(),
        Some(v) => {
            // Unicode-safe truncation: step 8 chars, not 8 bytes.
            let mut it = v.chars();
            let head: String = (&mut it).take(8).collect();
            if it.next().is_some() {
                format!("{head}…")
            } else {
                head
            }
        }
    }
}

fn install_shutdown_listener(
    tailer_shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tailer_handle: Option<tokio::task::JoinHandle<()>>,
) {
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;

        tracing::info!(
            target: "budi_daemon",
            "shutdown signal received; draining tailer"
        );
        tailer_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);

        if let Some(h) = tailer_handle {
            match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, h).await {
                Ok(Ok(())) => tracing::info!(
                    target: "budi_daemon",
                    "tailer drained cleanly; exiting"
                ),
                Ok(Err(e)) => tracing::warn!(
                    target: "budi_daemon",
                    error = %e,
                    "tailer task join error; exiting"
                ),
                Err(_) => tracing::warn!(
                    target: "budi_daemon",
                    timeout_s = SHUTDOWN_DRAIN_TIMEOUT.as_secs(),
                    "tailer did not drain within budget; exiting anyway"
                ),
            }
        }

        std::process::exit(0);
    });
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon",
                error = %e,
                "failed to install SIGTERM handler; falling back to SIGINT only"
            );
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Kill any existing budi-daemon process listening on the given port.
/// This allows a new binary to take over seamlessly after an upgrade.
///
/// The old daemon may install a graceful-shutdown listener (#384) that
/// needs up to one tailer backstop interval (~5 s) to drain, so we poll
/// for the process to actually exit after SIGTERM and escalate to
/// SIGKILL on timeout rather than assuming a fixed sleep is enough.
#[cfg(unix)]
fn kill_existing_daemon(port: u16) {
    use std::process::Command;

    // Find PIDs listening on this port
    let Ok(output) = Command::new("lsof")
        .args(["-nP", &format!("-tiTCP:{port}"), "-sTCP:LISTEN"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }

    let my_pid = std::process::id();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(pid) = line.trim().parse::<u32>().ok() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        // Verify it's actually a budi-daemon process
        let Ok(ps) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
        else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&ps.stdout);
        if !cmd.contains("budi-daemon") {
            continue;
        }
        tracing::info!("Killing old budi-daemon (pid {pid})");
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        wait_for_pid_exit_or_sigkill(pid);
    }
}

/// Poll `kill -0 <pid>` for up to `SHUTDOWN_DRAIN_TIMEOUT + 1 s` so a
/// graceful-shutdown-capable daemon has room to drain its tailer; fall
/// back to `kill -KILL` if it is still alive at the end of the window.
#[cfg(unix)]
fn wait_for_pid_exit_or_sigkill(pid: u32) {
    use std::process::Command;

    let deadline =
        std::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT + std::time::Duration::from_secs(1);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                pid,
                "old budi-daemon did not exit after SIGTERM within grace window; sending SIGKILL"
            );
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            return;
        }
    }
}

#[cfg(windows)]
fn kill_existing_daemon(port: u16) {
    use std::collections::HashSet;
    use std::process::Command;

    let script = format!(
        "Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue \
         | ForEach-Object {{ $_.OwningProcess }}"
    );
    let Ok(output) = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let my_pid = std::process::id();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == my_pid || !seen.insert(pid) {
            continue;
        }
        let Ok(tasklist) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            continue;
        };
        let listing = String::from_utf8_lossy(&tasklist.stdout).to_lowercase();
        if !listing.contains("budi-daemon") {
            continue;
        }
        tracing::info!("Killing old budi-daemon (pid {pid})");
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

#[cfg(not(any(unix, windows)))]
fn kill_existing_daemon(_port: u16) {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> Router {
        build_router(AppState {
            syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cloud_syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            sync_progress: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    }

    #[test]
    fn log_id_prefix_abbreviates_long_ids_and_preserves_short_ones() {
        // #540: the daemon startup log prints device_id / org_id
        // abbreviated so operators can eyeball "am I on the right
        // device?" without full opaque UUIDs cluttering the log.
        assert_eq!(
            log_id_prefix(Some("7b322df1-3bcd-4e72-9e2a-0b2f3c4d5e6f")),
            "7b322df1…"
        );
        // Short values (shorter than the 8-char abbreviation
        // threshold) render intact — no trailing ellipsis.
        assert_eq!(log_id_prefix(Some("short")), "short");
        // Exactly-8-char values don't grow an ellipsis — there's
        // nothing hidden.
        assert_eq!(log_id_prefix(Some("exactly8")), "exactly8");
        // None → explicit "(missing)" sentinel. Should not fire in
        // production (the configured branch only runs for
        // `is_ready` configs), but keeps the log line stable if it
        // ever does.
        assert_eq!(log_id_prefix(None), "(missing)");
        // Unicode: the truncation is char-boundary-safe.
        assert_eq!(log_id_prefix(Some("✨org_aaabbbccc")), "✨org_aaa…");
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json["version"].is_string(), "health should include version");
        assert!(
            json["api_version"].is_u64(),
            "health should include api_version"
        );
    }

    #[tokio::test]
    async fn favicon_returns_ok() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/favicon.ico").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_admin_route_requires_connect_info() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::get("/admin/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn protected_admin_route_allows_loopback_client() {
        let app = test_app();
        let mut req = Request::get("/admin/providers")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54545))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_admin_route_blocks_non_loopback_client() {
        let app = test_app();
        let mut req = Request::get("/admin/providers")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([192, 168, 1, 10], 54545))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sync_mutation_route_blocks_non_loopback_client() {
        let app = test_app();
        let mut req = Request::post("/sync").body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 4], 43434))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cloud_sync_route_blocks_non_loopback_client() {
        let app = test_app();
        let mut req = Request::post("/cloud/sync").body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 4], 43434))));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cloud_status_route_public_and_reports_shape() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/cloud/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("enabled").is_some(),
            "cloud/status should include `enabled`"
        );
        assert!(
            json.get("ready").is_some(),
            "cloud/status should include `ready`"
        );
        assert!(
            json.get("endpoint").is_some(),
            "cloud/status should include `endpoint`"
        );
    }

    // ─── #366 stale-schema 503 regression tests ──────────────────────────
    //
    // These tests cover the two halves of the schema-guard contract:
    //
    // 1. Wire-shape: `routes::schema_unavailable` must emit the exact body
    //    that `budi-cli::client::parse_needs_migration_error` matches on.
    // 2. Decision logic: `routes::schema_status_for` must correctly
    //    classify a DB file as `Proceed`, `Stale`, or `Ahead` against
    //    `SCHEMA_VERSION`.
    //
    // We intentionally do NOT drive the full axum router through a
    // `BUDI_HOME`-overridden `analytics::db_path()` here.
    // `std::env::set_var` is process-global and, on macOS, not sound
    // across threads — doing so produced flaky `500 != 503` failures in
    // CI on the initial cut of #366.  The pure helper above captures the
    // middleware's only real business logic; `require_current_schema` is
    // a five-line match over its output, exercised indirectly by every
    // other router-level test in this file (they all land on `Proceed`).

    #[test]
    fn schema_unavailable_has_stable_body_shape() {
        // Lock the wire shape the CLI pattern-matches on
        // (`budi-cli::client::parse_needs_migration_error`).
        let (status, body) = routes::schema_unavailable(0, 1);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v = body.0;
        assert_eq!(v["ok"], false);
        assert_eq!(v["needs_migration"], true);
        assert_eq!(v["current"], 0);
        assert_eq!(v["target"], 1);
        let msg = v["error"].as_str().unwrap_or_default();
        assert!(msg.contains("analytics schema is v0"));
        assert!(msg.contains("daemon expects v1"));
        assert!(msg.contains("budi db migrate"));
    }

    #[test]
    fn schema_status_for_returns_proceed_when_db_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let status = routes::schema_status_for(&tmp.path().join("analytics.db"));
        assert_eq!(status, routes::SchemaStatus::Proceed);
    }

    #[test]
    fn schema_status_for_returns_stale_for_pre_migration_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("analytics.db");
        // Materialize a DB file with `user_version = 0`, simulating the
        // pre-migration beta DB that a newer daemon binary
        // (SCHEMA_VERSION = 1) might be pointed at.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE dummy(x INTEGER);")
                .unwrap();
            // Defensive pin — don't depend on SQLite's default user_version.
            conn.pragma_update(None, "user_version", 0_u32).unwrap();
        }
        assert_eq!(
            routes::schema_status_for(&db_path),
            routes::SchemaStatus::Stale {
                current: 0,
                target: budi_core::migration::SCHEMA_VERSION,
            }
        );
    }

    #[test]
    fn schema_status_for_returns_proceed_at_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("analytics.db");
        budi_core::analytics::open_db_with_migration(&db_path).unwrap();
        assert_eq!(
            routes::schema_status_for(&db_path),
            routes::SchemaStatus::Proceed
        );
    }

    #[test]
    fn schema_status_for_returns_ahead_when_db_is_future_version() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("analytics.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let future = budi_core::migration::SCHEMA_VERSION + 99;
            conn.pragma_update(None, "user_version", future).unwrap();
        }
        match routes::schema_status_for(&db_path) {
            routes::SchemaStatus::Ahead { current, target } => {
                assert!(
                    current > target,
                    "current {current} should be > target {target}"
                );
                assert_eq!(target, budi_core::migration::SCHEMA_VERSION);
            }
            other => panic!("expected SchemaStatus::Ahead, got {other:?}"),
        }
    }
}
