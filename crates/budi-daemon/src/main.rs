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
}

fn build_router(app_state: AppState) -> Router {
    use routes::{analytics as a, cloud as c, hooks as h, require_loopback};

    let protected_routes = Router::new()
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
        .route("/sync/reset", post(h::analytics_sync_reset))
        .route("/cloud/sync", post(c::cloud_sync))
        .route("/admin/providers", get(a::analytics_registered_providers))
        .route("/admin/schema", get(a::analytics_schema_version))
        .route("/admin/migrate", post(a::analytics_migrate))
        .route("/admin/repair", post(a::analytics_repair))
        .route(
            "/admin/integrations/install",
            post(h::admin_install_integrations),
        )
        .route_layer(from_fn(require_loopback));

    Router::new()
        .route("/favicon.ico", get(h::favicon))
        .route("/health", get(h::health))
        .route("/health/integrations", get(h::health_integrations))
        .route("/health/check-update", get(h::health_check_update))
        .route("/sync/status", get(h::sync_status))
        .route("/cloud/status", get(routes::cloud::cloud_status))
        .route("/analytics/summary", get(a::analytics_summary))
        .route("/analytics/messages", get(a::analytics_messages))
        .route("/analytics/projects", get(a::analytics_projects))
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
        .merge(protected_routes)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(app_state)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
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
    };

    let app = build_router(app_state);

    // Ensure the database exists and schema is up-to-date.
    // This makes the daemon self-sufficient — it doesn't require `budi init` to have run first.
    if let Ok(db_path) = analytics::db_path()
        && let Err(e) = analytics::open_db_with_migration(&db_path)
    {
        tracing::warn!("Failed to initialize database: {e}");
    }
    if let Err(e) = budi_core::legacy_proxy::emit_upgrade_notice_once() {
        tracing::warn!("Failed to scan legacy proxy residue on startup: {e}");
    }

    // --- Start filesystem tailer (ADR-0089 §1 / R1.4 #320) ---
    //
    // R1.3 (#319) shipped the tailer behind `BUDI_LIVE_TAIL=1`. R1.4 (#320)
    // promoted it to the default. R2.1 (#322) removes the proxy runtime, so
    // tailer ingestion is now the only live path.
    match analytics::db_path() {
        Ok(db_path) => {
            tracing::info!(
                target: "budi_daemon::tailer",
                "starting filesystem tailer (ADR-0089 §1)"
            );
            let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn(workers::tailer::run(db_path, shutdown));
        }
        Err(e) => tracing::warn!(
            target: "budi_daemon::tailer",
            error = %e,
            "db_path is not resolvable; tailer not started"
        ),
    }

    // --- Start cloud sync worker if configured ---
    {
        let cloud_config = budi_core::config::load_cloud_config();
        if cloud_config.is_ready() {
            if let Ok(db_path) = analytics::db_path() {
                tracing::info!(
                    endpoint = %cloud_config.effective_endpoint(),
                    interval_s = cloud_config.sync.interval_seconds,
                    "Starting cloud sync worker"
                );
                tokio::spawn(workers::cloud_sync::run(
                    db_path,
                    cloud_config,
                    cloud_syncing.clone(),
                ));
            }
        } else if cloud_config.effective_enabled() {
            tracing::warn!(
                "Cloud sync enabled but not fully configured (missing api_key, device_id, or org_id)"
            );
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

/// Kill any existing budi-daemon process listening on the given port.
/// This allows a new binary to take over seamlessly after an upgrade.
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
        if cmd.contains("budi-daemon") {
            tracing::info!("Killing old budi-daemon (pid {pid})");
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
            // Brief wait for graceful shutdown
            std::thread::sleep(std::time::Duration::from_millis(300));
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
        })
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
}
