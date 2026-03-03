use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde_json::json;
use tokio::sync::{Mutex, mpsc};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

const WATCH_DEBOUNCE_MS: u64 = 650;
const WATCH_FLUSH_TICK_MS: u64 = 200;
const RECONCILE_INTERVAL_SECS: u64 = 90;

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
    autosync: AutoSyncRegistry,
}

#[derive(Clone, Default)]
struct AutoSyncRegistry {
    started_repos: Arc<Mutex<HashSet<String>>>,
}

impl AutoSyncRegistry {
    async fn ensure_repo_started(
        &self,
        repo_root: &str,
        daemon_state: DaemonState,
        config: BudiConfig,
    ) {
        let mut started = self.started_repos.lock().await;
        if !started.insert(repo_root.to_string()) {
            return;
        }
        drop(started);

        let repo_key = repo_root.to_string();
        tokio::spawn(async move {
            run_repo_autosync(repo_key, daemon_state, config).await;
        });
    }
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
        autosync: AutoSyncRegistry::default(),
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
    state
        .autosync
        .ensure_repo_started(
            &request.repo_root,
            state.daemon_state.clone(),
            config.clone(),
        )
        .await;
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
    state
        .autosync
        .ensure_repo_started(
            &request.repo_root,
            state.daemon_state.clone(),
            config.clone(),
        )
        .await;
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
    state
        .autosync
        .ensure_repo_started(
            &request.repo_root,
            state.daemon_state.clone(),
            config.clone(),
        )
        .await;
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
    state
        .autosync
        .ensure_repo_started(
            &request.repo_root,
            state.daemon_state.clone(),
            config.clone(),
        )
        .await;
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

async fn run_repo_autosync(repo_root: String, daemon_state: DaemonState, config: BudiConfig) {
    tracing::info!("auto-sync started for {}", repo_root);
    let (path_tx, mut path_rx) = mpsc::unbounded_channel::<String>();
    let watch_root = repo_root.clone();
    let watcher_name = format!("budi-watch-{}", short_repo_tag(&watch_root));
    match std::thread::Builder::new()
        .name(watcher_name)
        .spawn(move || watch_repo_events(&watch_root, path_tx))
    {
        Ok(_join_handle) => {}
        Err(err) => {
            tracing::warn!("failed starting file watcher for {}: {}", repo_root, err);
            return;
        }
    }

    let mut pending_paths: HashSet<String> = HashSet::new();
    let mut last_event_at: Option<Instant> = None;
    let mut flush_tick = tokio::time::interval(Duration::from_millis(WATCH_FLUSH_TICK_MS));
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reconcile_tick = tokio::time::interval(Duration::from_secs(RECONCILE_INTERVAL_SECS));
    reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the immediate first tick so we reconcile on cadence, not instantly again.
    reconcile_tick.tick().await;

    loop {
        tokio::select! {
            maybe_path = path_rx.recv() => {
                match maybe_path {
                    Some(path) => {
                        pending_paths.insert(path);
                        last_event_at = Some(Instant::now());
                    }
                    None => {
                        tracing::warn!("watch channel closed for {}", repo_root);
                        break;
                    }
                }
            }
            _ = flush_tick.tick() => {
                if let Some(last_change) = last_event_at
                    && last_change.elapsed() >= Duration::from_millis(WATCH_DEBOUNCE_MS)
                    && !pending_paths.is_empty()
                {
                    let mut changed_files = pending_paths.drain().collect::<Vec<_>>();
                    changed_files.sort();
                    last_event_at = None;
                    if let Err(err) = daemon_state
                        .update(
                            UpdateRequest {
                                repo_root: repo_root.clone(),
                                changed_files,
                            },
                            &config,
                        )
                        .await
                    {
                        tracing::warn!("auto-sync update failed for {}: {:#}", repo_root, err);
                    }
                }
            }
            _ = reconcile_tick.tick() => {
                daemon_state
                    .request_reconcile(repo_root.clone(), &config)
                    .await;
            }
        }
    }
}

fn watch_repo_events(repo_root: &str, path_tx: mpsc::UnboundedSender<String>) {
    let repo_root_path = PathBuf::from(repo_root);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    }) {
        Ok(watcher) => watcher,
        Err(err) => {
            tracing::warn!("failed creating watcher for {}: {}", repo_root, err);
            return;
        }
    };
    if let Err(err) = watcher.watch(&repo_root_path, RecursiveMode::Recursive) {
        tracing::warn!(
            "failed attaching watcher to repo root {}: {}",
            repo_root,
            err
        );
        return;
    }

    loop {
        match event_rx.recv() {
            Ok(Ok(event)) => {
                if !is_relevant_event_kind(&event.kind) {
                    continue;
                }
                for path in event.paths {
                    if let Some(normalized) = normalize_watched_path(&repo_root_path, &path) {
                        let _ = path_tx.send(normalized);
                    }
                }
            }
            Ok(Err(err)) => {
                tracing::warn!("watch event error for {}: {}", repo_root, err);
            }
            Err(_) => {
                break;
            }
        }
    }
}

fn is_relevant_event_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

fn normalize_watched_path(repo_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(repo_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::Normal(name) if name.to_string_lossy() == ".git"
        )
    }) {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => {}
        }
    }

    if normalized.as_os_str().is_empty() {
        return None;
    }
    Some(normalized.to_string_lossy().replace('\\', "/"))
}

fn short_repo_tag(repo_root: &str) -> String {
    let path = Path::new(repo_root);
    let mut out = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>()
        .to_ascii_lowercase();
    if out.is_empty() {
        out = "repo".to_string();
    }
    if out.len() > 24 {
        out.truncate(24);
    }
    out
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind};

    #[test]
    fn normalize_watched_path_skips_git_paths() {
        let repo = Path::new("/tmp/repo");
        let path = Path::new("/tmp/repo/.git/index.lock");
        assert!(normalize_watched_path(repo, path).is_none());
    }

    #[test]
    fn normalize_watched_path_returns_repo_relative_slash_path() {
        let repo = Path::new("/tmp/repo");
        let path = Path::new("/tmp/repo/src/lib.rs");
        assert_eq!(
            normalize_watched_path(repo, path).as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn event_filter_ignores_access_noise() {
        let create_kind = EventKind::Create(CreateKind::File);
        let access_kind = EventKind::Access(AccessKind::Close(AccessMode::Read));
        assert!(is_relevant_event_kind(&create_kind));
        assert!(!is_relevant_event_kind(&access_kind));
    }
}
