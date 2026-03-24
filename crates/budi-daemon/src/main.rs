use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::Router;
use axum::routing::{get, post};
use budi_core::analytics;
use budi_core::config::{DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use budi_core::daemon::DaemonState;
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
    pub daemon_state: DaemonState,
    pub syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub hook_sync_tx: tokio::sync::mpsc::Sender<PathBuf>,
}

fn build_router(app_state: AppState) -> Router {
    use routes::{analytics as a, dashboard as d, hooks as h};

    Router::new()
        .route("/health", get(h::health))
        .route("/status", post(h::status_repo))
        .route("/stats", get(h::hook_stats))
        .route("/session-stats", post(h::hook_session_stats))
        .route("/hook/prompt-submit", post(h::hook_prompt_submit))
        .route("/hook/tool-use", post(h::hook_tool_use))
        .route("/sync", post(h::analytics_sync))
        .route("/analytics/summary", get(a::analytics_summary))
        .route("/analytics/sessions", get(a::analytics_sessions))
        .route("/analytics/session/{id}", get(a::analytics_session_detail))
        .route("/analytics/projects", get(a::analytics_projects))
        .route("/analytics/cost", get(a::analytics_cost))
        .route("/analytics/models", get(a::analytics_models))
        .route("/analytics/activity", get(a::analytics_activity))
        .route("/analytics/top-tools", get(a::analytics_top_tools))
        .route("/analytics/mcp-tools", get(a::analytics_mcp_tools))
        .route("/analytics/branches", get(a::analytics_branches))
        .route("/analytics/tags", get(a::analytics_tags))
        .route("/analytics/git-summary", get(a::analytics_git_summary))
        .route("/analytics/providers", get(a::analytics_providers))
        .route(
            "/analytics/registered-providers",
            get(a::analytics_registered_providers),
        )
        .route("/analytics/statusline", get(a::analytics_statusline))
        .route("/analytics/context-usage", get(a::analytics_context_usage))
        .route(
            "/analytics/interaction-modes",
            get(a::analytics_interaction_modes),
        )
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

    let (hook_sync_tx, mut hook_sync_rx) = tokio::sync::mpsc::channel::<PathBuf>(64);

    let app_state = AppState {
        daemon_state: DaemonState::new(),
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        hook_sync_tx,
    };

    let sync_flag = app_state.syncing.clone();

    // Debounced hook-triggered sync: when hooks fire, sync just the active transcript
    // file after a 500ms debounce to collapse rapid hook bursts.
    tokio::spawn(async move {
        let mut pending_path: Option<PathBuf> = None;
        loop {
            if pending_path.is_some() {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    hook_sync_rx.recv(),
                )
                .await
                {
                    Ok(Some(path)) => {
                        pending_path = Some(path);
                        continue; // reset debounce timer
                    }
                    Ok(None) => break,
                    Err(_) => {
                        // Debounce expired — sync the file
                        let path = pending_path.take().unwrap();
                        let _ = tokio::task::spawn_blocking(move || {
                            let db_path = analytics::db_path().ok()?;
                            let mut conn = analytics::open_db(&db_path).ok()?;
                            if budi_core::migration::needs_migration(&conn) {
                                tracing::warn!(
                                    "Database needs migration. Run `budi sync` or `budi update`."
                                );
                                return None;
                            }
                            analytics::sync_one_file(&mut conn, &path).ok()
                        })
                        .await;
                    }
                }
            } else {
                match hook_sync_rx.recv().await {
                    Some(path) => {
                        pending_path = Some(path);
                    }
                    None => break,
                }
            }
        }
    });

    let app = build_router(app_state);

    // Auto-sync JSONL transcripts every 30 seconds to keep analytics fresh.
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
                    let sync_result = analytics::sync_all(&mut conn).ok();

                    // Post-sync: enrich a batch of sessions with git data.
                    // Processes up to 50 sessions per cycle for progressive backfill.
                    match budi_core::git::enrich_git_batch(&mut conn, 50) {
                        Ok(r) if r.commits_found > 0 => {
                            tracing::info!(
                                "Git enrichment: {} commits from {} sessions ({} remaining)",
                                r.commits_found,
                                r.sessions_processed,
                                r.sessions_remaining
                            );
                        }
                        Ok(r) if r.sessions_remaining > 0 => {
                            tracing::debug!(
                                "Git enrichment: {} sessions checked, {} remaining",
                                r.sessions_processed,
                                r.sessions_remaining
                            );
                        }
                        Err(e) => tracing::warn!("Git enrichment failed: {e}"),
                        _ => {}
                    }

                    sync_result
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> Router {
        let (hook_sync_tx, _hook_sync_rx) = tokio::sync::mpsc::channel(64);
        build_router(AppState {
            daemon_state: DaemonState::new(),
            syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_sync_tx,
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
    async fn stats_returns_json() {
        let app = test_app();
        let resp = app
            .oneshot(Request::get("/stats").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("queries").is_some());
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
