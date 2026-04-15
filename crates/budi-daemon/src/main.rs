use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn;
use axum::routing::{get, post};
use budi_core::analytics;
use budi_core::config::{DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT, ProxyConfig};
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
        #[arg(long)]
        proxy_port: Option<u16>,
        #[arg(long)]
        no_proxy: bool,
    },
}

#[derive(Clone)]
pub struct AppState {
    pub syncing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub integrations_installing: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub struct ProxyState {
    pub http_client: reqwest::Client,
    pub anthropic_upstream: String,
    pub openai_upstream: String,
    pub analytics_db_path: PathBuf,
}

fn build_proxy_router(proxy_state: ProxyState) -> Router {
    use routes::proxy as p;

    Router::new()
        .route("/v1/messages", post(p::anthropic_messages))
        .route("/v1/chat/completions", post(p::openai_chat_completions))
        .route("/v1/models", get(p::openai_models))
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(proxy_state)
}

fn build_router(app_state: AppState) -> Router {
    use routes::{analytics as a, hooks as h, require_loopback};

    let protected_routes = Router::new()
        .route("/sync", post(h::analytics_sync))
        .route("/sync/all", post(h::analytics_history))
        .route("/sync/reset", post(h::analytics_sync_reset))
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
    let (host, port, proxy_port_override, no_proxy) = match cli.command.unwrap_or(Commands::Serve {
        host: DEFAULT_DAEMON_HOST.to_string(),
        port: DEFAULT_DAEMON_PORT,
        proxy_port: None,
        no_proxy: false,
    }) {
        Commands::Serve {
            host,
            port,
            proxy_port,
            no_proxy,
        } => (host, port, proxy_port, no_proxy),
    };

    // Kill any existing budi-daemon on the same port so a fresh binary can
    // take over without manual intervention (e.g. after `cargo build && cp`).
    kill_existing_daemon(port);

    let app_state = AppState {
        syncing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        integrations_installing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let app = build_router(app_state);

    // Ensure the database exists and schema is up-to-date.
    // This makes the daemon self-sufficient — it doesn't require `budi init` to have run first.
    if let Ok(db_path) = analytics::db_path()
        && let Err(e) = analytics::open_db_with_migration(&db_path)
    {
        tracing::warn!("Failed to initialize database: {e}");
    }

    // --- Start proxy server if enabled ---
    let proxy_config = ProxyConfig::default();
    let proxy_enabled = !no_proxy && proxy_config.effective_enabled();
    let proxy_port = proxy_port_override.unwrap_or_else(|| proxy_config.effective_port());

    if proxy_enabled {
        let analytics_db_path = analytics::db_path().unwrap_or_default();
        let proxy_state = ProxyState {
            http_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build proxy HTTP client"),
            anthropic_upstream: proxy_config.anthropic_upstream.clone(),
            openai_upstream: proxy_config.openai_upstream.clone(),
            analytics_db_path,
        };

        if let Ok(db_path) = analytics::db_path()
            && let Ok(conn) = analytics::open_db(&db_path)
            && let Err(e) = budi_core::proxy::ensure_proxy_schema(&conn)
        {
            tracing::warn!("Failed to initialize proxy schema: {e}");
        }

        let proxy_app = build_proxy_router(proxy_state);
        let proxy_addr: SocketAddr = format!("127.0.0.1:{proxy_port}").parse()?;

        kill_existing_daemon(proxy_port);

        match tokio::net::TcpListener::bind(proxy_addr).await {
            Ok(proxy_listener) => {
                tracing::info!("budi proxy listening on {}", proxy_addr);
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(proxy_listener, proxy_app).await {
                        tracing::error!("Proxy server error: {e}");
                    }
                });
            }
            Err(e) => {
                let message = format!(
                    "Failed to bind proxy on {proxy_addr}: {e}. \
                     Check if another process is using port {proxy_port}."
                );
                tracing::error!("{message}");
                anyhow::bail!("{message}");
            }
        }
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
                tokio::spawn(workers::cloud_sync::run(db_path, cloud_config));
            }
        } else if cloud_config.effective_enabled() {
            tracing::warn!(
                "Cloud sync enabled but not fully configured (missing api_key, device_id, or org_id)"
            );
        }
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
        })
    }

    fn test_proxy_app() -> Router {
        let tmp = std::env::temp_dir().join("budi-proxy-test-db");
        std::fs::create_dir_all(&tmp).ok();
        let db_path = tmp.join("analytics.db");
        let proxy_state = ProxyState {
            http_client: reqwest::Client::new(),
            anthropic_upstream: "http://127.0.0.1:19999".to_string(),
            openai_upstream: "http://127.0.0.1:19999".to_string(),
            analytics_db_path: db_path,
        };
        build_proxy_router(proxy_state)
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
    async fn proxy_anthropic_returns_bad_gateway_when_upstream_unreachable() {
        let app = test_proxy_app();
        let body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let resp = app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-api-key", "test-key")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "proxy_error");
    }

    #[tokio::test]
    async fn proxy_openai_returns_bad_gateway_when_upstream_unreachable() {
        let app = test_proxy_app();
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let resp = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "proxy_error");
    }

    #[tokio::test]
    async fn proxy_models_returns_bad_gateway_when_upstream_unreachable() {
        let app = test_proxy_app();
        let resp = app
            .oneshot(
                Request::get("/v1/models")
                    .header("authorization", "Bearer test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn proxy_rejects_oversized_body() {
        let app = test_proxy_app();
        let huge_body = vec![0u8; 17 * 1024 * 1024]; // 17 MiB > 16 MiB limit
        let resp = app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(huge_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // --- SSE streaming tests ---

    /// Start a mock HTTP server that returns an SSE response, then closes.
    async fn start_mock_sse_server(sse_body: String) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {sse_body}"
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        });

        addr
    }

    /// Poll the database until `query` returns a row, with 50ms intervals up to 2s.
    /// Returns the mapped result from the row, or panics on timeout.
    async fn poll_proxy_event<T, F>(db_path: &std::path::Path, query: &str, map_row: F) -> T
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T> + Copy,
    {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let conn = rusqlite::Connection::open(db_path).unwrap();
            budi_core::proxy::ensure_proxy_schema(&conn).unwrap();
            match conn.query_row(query, [], map_row) {
                Ok(val) => return val,
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => panic!("unexpected DB error: {e}"),
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for proxy_events row (query: {query})");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    struct ProxyTestHarness {
        app: Router,
        db_path: PathBuf,
    }

    fn proxy_with_upstream(upstream: &str) -> ProxyTestHarness {
        let safe_name = std::thread::current()
            .name()
            .unwrap_or("t")
            .replace("::", "_");
        let tmp = std::env::temp_dir().join(format!("budi-sse-test-{safe_name}"));
        std::fs::create_dir_all(&tmp).ok();
        let db_path = tmp.join("analytics.db");
        let _ = std::fs::remove_file(&db_path);

        let proxy_state = ProxyState {
            http_client: reqwest::Client::new(),
            anthropic_upstream: upstream.to_string(),
            openai_upstream: upstream.to_string(),
            analytics_db_path: db_path.clone(),
        };
        ProxyTestHarness {
            app: build_proxy_router(proxy_state),
            db_path,
        }
    }

    #[tokio::test]
    async fn proxy_streams_anthropic_sse_and_extracts_tokens() {
        let sse_body = [
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ].concat();

        let addr = start_mock_sse_server(sse_body).await;
        let h = proxy_with_upstream(&format!("http://{addr}"));

        let body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "stream": true,
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let resp = h
            .app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-api-key", "test-key")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
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
        assert!(
            ct.contains("text/event-stream"),
            "expected SSE content-type"
        );

        let resp_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&resp_bytes);
        assert!(
            text.contains("message_start"),
            "SSE data should pass through"
        );
        assert!(text.contains("Hello"), "content delta should pass through");

        let (input, output, streaming): (Option<i64>, Option<i64>, i64) = poll_proxy_event(
            &h.db_path,
            "SELECT input_tokens, output_tokens, is_streaming FROM proxy_events ORDER BY id DESC LIMIT 1",
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).await;
        assert_eq!(input, Some(25), "input_tokens from message_start");
        assert_eq!(output, Some(15), "output_tokens from message_delta");
        assert_eq!(streaming, 1);
    }

    #[tokio::test]
    async fn proxy_streams_openai_sse_and_extracts_tokens() {
        let sse_body = [
            "data: {\"id\":\"chatcmpl-t\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-t\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30}}\n\n",
            "data: [DONE]\n\n",
        ].concat();

        let addr = start_mock_sse_server(sse_body).await;
        let h = proxy_with_upstream(&format!("http://{addr}"));

        let body = serde_json::json!({
            "model": "gpt-4o",
            "stream": true,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let resp = h
            .app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer test-key")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let resp_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&resp_bytes);
        assert!(text.contains("Hi"), "content should pass through");
        assert!(
            text.contains("[DONE]"),
            "terminal event should pass through"
        );

        let (input, output): (Option<i64>, Option<i64>) = poll_proxy_event(
            &h.db_path,
            "SELECT input_tokens, output_tokens FROM proxy_events ORDER BY id DESC LIMIT 1",
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).await;
        assert_eq!(input, Some(10), "prompt_tokens from usage");
        assert_eq!(output, Some(20), "completion_tokens from usage");
    }

    #[tokio::test]
    async fn proxy_sse_duration_reflects_stream_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = socket.read(&mut buf).await;
            let headers =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket
                .write_all(b"data: {\"type\":\"message_start\"}\n\n")
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let _ = socket
                .write_all(b"data: {\"type\":\"message_stop\"}\n\n")
                .await;
        });

        let h = proxy_with_upstream(&format!("http://{addr}"));
        let body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let resp = h
            .app
            .oneshot(
                Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = resp.into_body().collect().await.unwrap();

        let duration_ms: i64 = poll_proxy_event(
            &h.db_path,
            "SELECT duration_ms FROM proxy_events ORDER BY id DESC LIMIT 1",
            |row| row.get(0),
        ).await;
        assert!(
            duration_ms >= 200,
            "duration_ms ({duration_ms}) should reflect stream end, not header time"
        );
    }
}
