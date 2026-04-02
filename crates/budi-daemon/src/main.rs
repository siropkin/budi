use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use budi_core::analytics;
use budi_core::config::{DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
}

struct BusyFlagGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BusyFlagGuard {
    fn new(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for BusyFlagGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn build_router(app_state: AppState) -> Router {
    use routes::{analytics as a, dashboard as d, hooks as h, otel as o};

    Router::new()
        .route("/favicon.ico", get(d::favicon))
        .route("/health", get(h::health))
        .route("/health/integrations", get(h::health_integrations))
        .route("/health/check-update", get(h::health_check_update))
        .route(
            "/v1/logs",
            post(o::otel_logs_ingest).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route(
            "/v1/metrics",
            post(o::otel_metrics_ingest).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
        .route("/sync/reset", post(h::analytics_sync_reset))
        .route("/sync/status", get(h::sync_status))
        .route("/analytics/summary", get(a::analytics_summary))
        .route("/analytics/messages", get(a::analytics_messages))
        .route("/analytics/projects", get(a::analytics_projects))
        .route("/analytics/cost", get(a::analytics_cost))
        .route("/analytics/models", get(a::analytics_models))
        .route("/analytics/activity", get(a::analytics_activity))
        .route("/analytics/branches", get(a::analytics_branches))
        .route("/analytics/tags", get(a::analytics_tags))
        .route(
            "/analytics/branches/{branch}",
            get(a::analytics_branch_detail),
        )
        .route("/analytics/providers", get(a::analytics_providers))
        .route("/analytics/statusline", get(a::analytics_statusline))
        .route("/admin/providers", get(a::analytics_registered_providers))
        .route("/admin/schema", get(a::analytics_schema_version))
        .route("/admin/migrate", post(a::analytics_migrate))
        .route("/admin/repair", post(a::analytics_repair))
        .route(
            "/admin/integrations/install",
            post(h::admin_install_integrations),
        )
        .route("/analytics/tools", get(a::analytics_tools))
        .route("/analytics/mcp", get(a::analytics_mcp))
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
            "/analytics/sessions/{session_id}/messages",
            get(a::analytics_session_messages),
        )
        .route(
            "/analytics/sessions/{session_id}/tags",
            get(a::analytics_session_tags),
        )
        .route("/hooks/ingest", post(h::hooks_ingest))
        // Dashboard SPA shell + hashed static assets.
        .route("/dashboard", get(d::dashboard))
        .route("/dashboard/{*rest}", get(d::dashboard))
        .route("/static/dashboard/{*path}", get(d::dashboard_asset))
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

    let app_state = AppState {
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let sync_flag = app_state.syncing.clone();
    let app = build_router(app_state);

    // Ensure the database exists and schema is up-to-date.
    // This makes the daemon self-sufficient — it doesn't require `budi init` to have run first.
    if let Ok(db_path) = analytics::db_path()
        && let Err(e) = analytics::open_db_with_migration(&db_path)
    {
        tracing::warn!("Failed to initialize database: {e}");
    }

    // Auto-sync transcripts every 30 seconds to keep analytics fresh.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            if sync_flag
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                continue; // Another sync is running, skip this tick
            }
            let flag = sync_flag.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _busy = BusyFlagGuard::new(flag);
                (|| {
                    let db_path = analytics::db_path().ok()?;
                    let mut conn = analytics::open_db(&db_path).ok()?;
                    if budi_core::migration::needs_migration(&conn) {
                        tracing::warn!("Database needs migration. Skipping auto-sync.");
                        return None;
                    }
                    analytics::sync_all(&mut conn)
                        .ok()
                        .map(|(f, m, _warnings)| (f, m))
                })()
            })
            .await;
        }
    });

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("budi-daemon listening on {}", addr);
    axum::serve(listener, app).await?;
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
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> Router {
        build_router(AppState {
            syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
    async fn dashboard_returns_html() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/dashboard").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"));
    }

    #[tokio::test]
    async fn dashboard_deep_link_returns_html() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::get("/dashboard/sessions/some-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"));
    }

    #[tokio::test]
    async fn dashboard_missing_asset_returns_404() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::get("/static/dashboard/assets/not-found.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
