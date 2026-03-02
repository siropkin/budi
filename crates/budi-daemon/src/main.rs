use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use budi_core::config::{self, BudiConfig, DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT};
use budi_core::daemon::DaemonState;
use budi_core::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, QueryRequest,
    QueryResponse, StatusRequest, StatusResponse, UpdateRequest,
};
use clap::{Parser, Subcommand};
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "budi-daemon")]
#[command(about = "budi local retrieval daemon")]
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
struct AppState {
    daemon_state: DaemonState,
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

    let app_state = AppState {
        daemon_state: DaemonState::new(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/query", post(query))
        .route("/index", post(index_repo))
        .route("/progress", post(progress_repo))
        .route("/update", post(update_repo))
        .route("/status", post(status_repo))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("budi-daemon listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn query(
    State(state): State<AppState>,
    Json(mut request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    if request.cwd.is_none() {
        request.cwd = Some(request.repo_root.clone());
    }
    let response = state
        .daemon_state
        .query(request, &config)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

async fn index_repo(
    State(state): State<AppState>,
    Json(request): Json<IndexRequest>,
) -> Result<Json<IndexResponse>, (StatusCode, String)> {
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    let response = state
        .daemon_state
        .index(request, &config)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

async fn update_repo(
    State(state): State<AppState>,
    Json(request): Json<UpdateRequest>,
) -> Result<Json<IndexResponse>, (StatusCode, String)> {
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    let response = state
        .daemon_state
        .update(request, &config)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

async fn progress_repo(
    State(state): State<AppState>,
    Json(request): Json<IndexProgressRequest>,
) -> Result<Json<IndexProgressResponse>, (StatusCode, String)> {
    let response = state
        .daemon_state
        .index_progress(request)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

async fn status_repo(
    State(state): State<AppState>,
    Json(request): Json<StatusRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    let response = state
        .daemon_state
        .status(request, &config)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

fn request_config(repo_root: &str) -> Result<BudiConfig> {
    let root = Path::new(repo_root);
    config::load_or_default(root)
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
}
