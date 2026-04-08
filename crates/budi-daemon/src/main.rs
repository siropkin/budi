use std::net::SocketAddr;

use anyhow::Result;
use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use budi_core::analytics;
use budi_core::config::{DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use clap::{Parser, Subcommand};
use serde_json::json;
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
    pub admin_token: Option<String>,
}

const ADMIN_TOKEN_ENV_VAR: &str = "BUDI_DAEMON_ADMIN_TOKEN";
const ADMIN_TOKEN_HEADER: &str = "x-budi-admin-token";

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

    let protected_mutation_routes = Router::new()
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
        .route("/sync/reset", post(h::analytics_sync_reset))
        .route("/admin/migrate", post(a::analytics_migrate))
        .route("/admin/repair", post(a::analytics_repair))
        .route(
            "/admin/integrations/install",
            post(h::admin_install_integrations),
        )
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            require_local_or_admin_token,
        ));

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
        .route("/sync/status", get(h::sync_status))
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
        .route("/analytics/providers", get(a::analytics_providers))
        .route("/analytics/statusline", get(a::analytics_statusline))
        .route("/admin/providers", get(a::analytics_registered_providers))
        .route("/admin/schema", get(a::analytics_schema_version))
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
            "/analytics/sessions/{session_id}/hook-events",
            get(a::analytics_session_hook_events),
        )
        .route(
            "/analytics/sessions/{session_id}/otel-events",
            get(a::analytics_session_otel_events),
        )
        .route(
            "/analytics/sessions/{session_id}/tags",
            get(a::analytics_session_tags),
        )
        .route(
            "/analytics/messages/{message_uuid}/detail",
            get(a::analytics_message_detail),
        )
        .route("/hooks/ingest", post(h::hooks_ingest))
        // Dashboard SPA shell + hashed static assets.
        .route("/dashboard", get(d::dashboard))
        .route("/dashboard/{*rest}", get(d::dashboard))
        .route("/static/dashboard/{*path}", get(d::dashboard_asset))
        .merge(protected_mutation_routes)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(app_state)
}

fn host_is_loopback(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| host.eq_ignore_ascii_case("localhost"))
}

fn load_admin_token_from_env() -> Option<String> {
    std::env::var(ADMIN_TOKEN_ENV_VAR)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn request_has_valid_admin_token(headers: &axum::http::HeaderMap, configured_token: &str) -> bool {
    let bearer_matches = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == configured_token);
    if bearer_matches {
        return true;
    }
    headers
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|token| token == configured_token)
}

async fn require_local_or_admin_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> impl IntoResponse {
    let is_loopback_caller = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|connect| connect.0.ip().is_loopback());
    if is_loopback_caller {
        return next.run(request).await;
    }

    if let Some(configured_token) = state.admin_token.as_deref()
        && request_has_valid_admin_token(request.headers(), configured_token)
    {
        return next.run(request).await;
    }

    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "error": format!(
                "remote mutation endpoints are blocked; use loopback requests or provide a valid admin token via Authorization: Bearer <token> or {ADMIN_TOKEN_HEADER}"
            )
        })),
    )
        .into_response()
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

    let admin_token = load_admin_token_from_env();
    if !host_is_loopback(&host) && admin_token.is_none() {
        tracing::warn!(
            "Daemon bound to non-loopback host ({host}) without {ADMIN_TOKEN_ENV_VAR}; mutation endpoints are loopback-only."
        );
    }

    let app_state = AppState {
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        admin_token,
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

    fn test_state(admin_token: Option<&str>) -> AppState {
        AppState {
            syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admin_token: admin_token.map(|token| token.to_string()),
        }
    }

    fn test_app() -> Router {
        build_router(test_state(None))
    }

    fn protected_test_app(admin_token: Option<&str>) -> Router {
        let state = test_state(admin_token);
        Router::new()
            .route("/protected", post(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_local_or_admin_token,
            ))
            .with_state(state)
    }

    fn request_with_caller_ip(path: &str, ip: [u8; 4]) -> Request<Body> {
        Request::post(path)
            .extension(ConnectInfo(SocketAddr::from((ip, 62000))))
            .body(Body::empty())
            .unwrap()
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

    #[tokio::test]
    async fn protected_mutation_blocks_remote_without_token() {
        let app = protected_test_app(None);
        let resp = app
            .oneshot(request_with_caller_ip("/protected", [10, 1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn protected_mutation_allows_loopback_without_token() {
        let app = protected_test_app(None);
        let resp = app
            .oneshot(request_with_caller_ip("/protected", [127, 0, 0, 1]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_mutation_allows_remote_with_valid_bearer_token() {
        let app = protected_test_app(Some("top-secret"));
        let req = Request::post("/protected")
            .header("authorization", "Bearer top-secret")
            .extension(ConnectInfo(SocketAddr::from(([10, 0, 0, 15], 62000))))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
