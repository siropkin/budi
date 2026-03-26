use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
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
}

fn build_router(app_state: AppState) -> Router {
    use routes::{analytics as a, dashboard as d, hooks as h};

    Router::new()
        .route("/health", get(h::health))
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
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
        .route(
            "/analytics/registered-providers",
            get(a::analytics_registered_providers),
        )
        .route("/analytics/statusline", get(a::analytics_statusline))
        .route("/analytics/schema-version", get(a::analytics_schema_version))
        .route("/migrate", post(a::analytics_migrate))
        .route("/analytics/sessions", get(h::analytics_sessions))
        .route("/analytics/tools", get(h::analytics_tools))
        .route("/analytics/mcp", get(h::analytics_mcp))
        .route("/hooks/ingest", post(h::hooks_ingest))
        .route("/dashboard", get(d::dashboard))
        .route("/static/dashboard.css", get(d::dashboard_css))
        .route("/static/dashboard.js", get(d::dashboard_js))
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
    };

    let sync_flag = app_state.syncing.clone();
    let app = build_router(app_state);

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
                let result = (|| {
                    let db_path = analytics::db_path().ok()?;
                    let mut conn = analytics::open_db(&db_path).ok()?;
                    if budi_core::migration::needs_migration(&conn) {
                        tracing::warn!("Database needs migration. Skipping auto-sync.");
                        return None;
                    }
                    analytics::sync_all(&mut conn).ok()
                })();
                flag.store(false, std::sync::atomic::Ordering::SeqCst);
                result
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
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            // Brief wait for graceful shutdown
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

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
        assert_eq!(json, serde_json::json!({ "ok": true }));
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
}
