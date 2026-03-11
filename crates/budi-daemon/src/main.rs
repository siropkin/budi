use std::collections::{HashMap, HashSet};
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
use budi_core::hooks::{UserPromptSubmitInput, UserPromptSubmitOutput};
use budi_core::index;
use budi_core::prompt_controls::{
    build_runtime_guard_context, evaluate_context_skip, parse_prompt_directives,
    sanitize_prompt_for_query,
};
use budi_core::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, PrefetchRequest,
    PrefetchResponse, QueryRequest, QueryResponse, StatusRequest, StatusResponse, UpdateRequest,
};
use clap::{Parser, Subcommand};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{Mutex, mpsc};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

const WATCH_DEBOUNCE_MS: u64 = 650;
const WATCH_FLUSH_TICK_MS: u64 = 200;
const RECONCILE_INTERVAL_SECS: u64 = 90;
const WATCH_RESTART_BASE_MS: u64 = 1_000;
const WATCH_RESTART_MAX_MS: u64 = 30_000;

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
    watcher_restarts: Arc<Mutex<HashMap<String, u64>>>,
    watcher_events: Arc<Mutex<HashMap<String, WatchEventMetrics>>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct WatchEventMetrics {
    seen: u64,
    accepted: u64,
    dropped: u64,
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
        self.record_watcher_restart(repo_root, 0).await;
        self.watch_event_metrics_for_repo(repo_root).await;

        let repo_key = repo_root.to_string();
        let autosync = self.clone();
        tokio::spawn(async move {
            run_repo_autosync(repo_key, daemon_state, config, autosync).await;
        });
    }

    async fn record_watcher_restart(&self, repo_root: &str, restart_count: u64) {
        let mut counts = self.watcher_restarts.lock().await;
        counts.insert(repo_root.to_string(), restart_count);
    }

    async fn watcher_restart_snapshot(&self) -> HashMap<String, u64> {
        self.watcher_restarts.lock().await.clone()
    }

    async fn record_watch_event(&self, repo_root: &str, accepted: bool) {
        let mut metrics = self.watcher_events.lock().await;
        let entry = metrics.entry(repo_root.to_string()).or_default();
        entry.seen = entry.seen.saturating_add(1);
        if accepted {
            entry.accepted = entry.accepted.saturating_add(1);
        } else {
            entry.dropped = entry.dropped.saturating_add(1);
        }
    }

    async fn watch_event_metrics_for_repo(&self, repo_root: &str) -> WatchEventMetrics {
        let mut metrics = self.watcher_events.lock().await;
        let entry = metrics.entry(repo_root.to_string()).or_default();
        *entry
    }

    async fn watch_event_metrics_snapshot(&self) -> HashMap<String, WatchEventMetrics> {
        self.watcher_events.lock().await.clone()
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
        .route("/prefetch-neighbors", post(prefetch_neighbors))
        .route("/stats", get(stats))
        .route("/session-stats", post(session_stats))
        .route("/hook/prompt-submit", post(hook_prompt_submit))
        .route("/hook/tool-use", post(hook_tool_use))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("budi-daemon listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let watcher_restarts = state.autosync.watcher_restart_snapshot().await;
    let watcher_restarts_total = watcher_restarts.values().copied().sum::<u64>();
    let watcher_events = state.autosync.watch_event_metrics_snapshot().await;
    let watch_events_seen = watcher_events.values().map(|item| item.seen).sum::<u64>();
    let watch_events_accepted = watcher_events
        .values()
        .map(|item| item.accepted)
        .sum::<u64>();
    let watch_events_dropped = watcher_events
        .values()
        .map(|item| item.dropped)
        .sum::<u64>();
    Json(json!({
        "ok": true,
        "autosync_repos": watcher_restarts.len(),
        "watcher_restarts_total": watcher_restarts_total,
        "watcher_restarts_by_repo": watcher_restarts,
        "watch_events_seen": watch_events_seen,
        "watch_events_accepted": watch_events_accepted,
        "watch_events_dropped": watch_events_dropped,
        "watch_events_by_repo": watcher_events,
    }))
}

async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (queries, injections, skips, chars_injected, prefetches, confirmed_reads, total_reads) =
        state.daemon_state.query_stats_snapshot();
    let mut result = json!({
        "queries": queries,
        "injections": injections,
        "skips": skips,
        "chars_injected": chars_injected,
        "prefetches": prefetches,
        "injection_rate": if queries > 0 { format!("{:.0}%", injections as f64 / queries as f64 * 100.0) } else { "n/a".to_string() },
        "confirmed_reads": confirmed_reads,
        "total_reads": total_reads,
        "read_hit_rate": if total_reads > 0 { format!("{:.0}%", confirmed_reads as f64 / total_reads as f64 * 100.0) } else { "n/a".to_string() },
    });
    if let Some(indexing) = state.daemon_state.indexing_summary() {
        result["indexing"] = indexing;
    }
    Json(result)
}

async fn session_stats(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let session_id = body
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if let Some(snap) = state.daemon_state.session_stats(session_id) {
        Json(serde_json::to_value(snap).unwrap_or_default())
    } else {
        Json(serde_json::json!({}))
    }
}

/// HTTP hook endpoint for UserPromptSubmit. Receives Claude Code's hook JSON
/// directly and returns the UserPromptSubmitOutput, eliminating CLI subprocess overhead.
async fn hook_prompt_submit(
    State(state): State<AppState>,
    Json(input): Json<UserPromptSubmitInput>,
) -> Json<UserPromptSubmitOutput> {
    let cwd = PathBuf::from(&input.common.cwd);
    let session_id = input.common.session_id.clone();

    let repo_root = match config::find_repo_root(&cwd) {
        Ok(path) => path,
        Err(_) => return Json(UserPromptSubmitOutput::allow_with_context(String::new())),
    };
    let config = match config::load_or_default(&repo_root) {
        Ok(c) => c,
        Err(_) => return Json(UserPromptSubmitOutput::allow_with_context(String::new())),
    };

    let directives = parse_prompt_directives(&input.prompt);
    if directives.force_skip {
        return Json(UserPromptSubmitOutput::allow_with_context(String::new()));
    }

    let sanitized_prompt = sanitize_prompt_for_query(&input.prompt);

    // Ensure autosync is running for this repo.
    let repo_root_str = repo_root.display().to_string();
    state
        .autosync
        .ensure_repo_started(&repo_root_str, state.daemon_state.clone(), config.clone())
        .await;

    let request = QueryRequest {
        repo_root: repo_root_str,
        prompt: sanitized_prompt,
        cwd: Some(cwd.display().to_string()),
        retrieval_mode: None,
        session_id: Some(session_id),
    };

    let response = match state.daemon_state.query(request, &config).await {
        Ok(r) => r,
        Err(_) => return Json(UserPromptSubmitOutput::allow_with_context(String::new())),
    };

    let skip_reason = evaluate_context_skip(&config, &directives, &response.diagnostics);
    let context = if let Some(_skip) = skip_reason {
        if response.diagnostics.intent == "runtime-config" {
            build_runtime_guard_context(&response.snippets)
        } else {
            String::new()
        }
    } else {
        response.context
    };

    Json(UserPromptSubmitOutput::allow_with_context(context))
}

/// HTTP hook endpoint for PostToolUse. Handles Write/Edit (incremental reindex)
/// and Read/Glob (prefetch neighbors + feedback tracking).
async fn hook_tool_use(
    State(state): State<AppState>,
    Json(input): Json<budi_core::hooks::PostToolUseInput>,
) -> Json<serde_json::Value> {
    let tool_name = &input.tool_name;
    let is_write_edit = tool_name == "Write" || tool_name == "Edit";
    let is_read = tool_name == "Read" || tool_name == "Glob";

    if !is_write_edit && !is_read {
        return Json(json!({}));
    }

    let file_path = input
        .tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if file_path.is_empty() {
        return Json(json!({}));
    }

    let cwd = PathBuf::from(&input.common.cwd);
    let session_id = input.common.session_id.clone();
    let repo_root = match config::find_repo_root(&cwd) {
        Ok(path) => path,
        Err(_) => return Json(json!({})),
    };
    let config = match config::load_or_default(&repo_root) {
        Ok(c) => c,
        Err(_) => return Json(json!({})),
    };
    let repo_root_str = repo_root.display().to_string();
    state
        .autosync
        .ensure_repo_started(&repo_root_str, state.daemon_state.clone(), config.clone())
        .await;

    if is_read {
        let request = PrefetchRequest {
            repo_root: repo_root_str,
            file_path,
            session_id,
            limit: Some(5),
        };
        if let Ok(prefetch) = state
            .daemon_state
            .prefetch_neighbors(request, &config)
            .await
            && !prefetch.context.is_empty()
        {
            return Json(json!({ "systemMessage": prefetch.context }));
        }
    } else {
        // Write/Edit: trigger incremental index update.
        let request = UpdateRequest {
            repo_root: repo_root_str,
            changed_files: vec![file_path],
        };
        if let Ok(resp) = state.daemon_state.update(request, &config).await {
            let msg = format!(
                "budi indexed {} changed file(s), total chunks={}",
                resp.changed_files, resp.indexed_chunks
            );
            return Json(json!({ "systemMessage": msg }));
        }
    }

    Json(json!({}))
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
    let repo_root = request.repo_root.clone();
    let config = request_config(&request.repo_root).map_err(internal_error)?;
    state
        .autosync
        .ensure_repo_started(
            &request.repo_root,
            state.daemon_state.clone(),
            config.clone(),
        )
        .await;
    let mut response = state
        .daemon_state
        .status(request, &config)
        .await
        .map_err(internal_error)?;
    let watch_metrics = state
        .autosync
        .watch_event_metrics_for_repo(&repo_root)
        .await;
    response.watch_events_seen = watch_metrics.seen;
    response.watch_events_accepted = watch_metrics.accepted;
    response.watch_events_dropped = watch_metrics.dropped;
    Ok(Json(response))
}

async fn prefetch_neighbors(
    State(state): State<AppState>,
    Json(request): Json<PrefetchRequest>,
) -> Result<Json<PrefetchResponse>, (StatusCode, String)> {
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
        .prefetch_neighbors(request, &config)
        .await
        .map_err(internal_error)?;
    Ok(Json(response))
}

fn request_config(repo_root: &str) -> Result<BudiConfig> {
    let root = Path::new(repo_root);
    config::load_or_default(root)
}

async fn run_repo_autosync(
    repo_root: String,
    daemon_state: DaemonState,
    config: BudiConfig,
    autosync: AutoSyncRegistry,
) {
    tracing::info!("auto-sync supervisor started for {}", repo_root);
    let mut consecutive_failures = 0u32;
    let mut restart_count = 0u64;

    loop {
        let watch_scope = match index::compile_index_scope(Path::new(&repo_root), &config, None) {
            Ok(scope) => Some(scope),
            Err(err) => {
                tracing::warn!(
                    "failed compiling watcher index scope for {}: {:#}; falling back to permissive filtering",
                    repo_root,
                    err
                );
                None
            }
        };
        let (path_tx, mut path_rx) = mpsc::unbounded_channel::<String>();
        let watch_root = repo_root.clone();
        let watcher_name = format!("budi-watch-{}", short_repo_tag(&watch_root));
        let watcher_started = std::thread::Builder::new()
            .name(watcher_name)
            .spawn(move || watch_repo_events(&watch_root, path_tx));
        if let Err(err) = watcher_started {
            consecutive_failures = consecutive_failures.saturating_add(1);
            restart_count = restart_count.saturating_add(1);
            autosync
                .record_watcher_restart(&repo_root, restart_count)
                .await;
            let delay = watcher_restart_backoff(consecutive_failures);
            tracing::warn!(
                "failed starting file watcher for {} (restart #{}, consecutive failures {}): {}. retrying in {}ms",
                repo_root,
                restart_count,
                consecutive_failures,
                err,
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        if consecutive_failures > 0 {
            tracing::info!(
                "auto-sync watcher recovered for {} after {} consecutive failures",
                repo_root,
                consecutive_failures
            );
            consecutive_failures = 0;
        }

        let mut pending_paths: HashSet<String> = HashSet::new();
        let mut last_event_at: Option<Instant> = None;
        let mut flush_tick = tokio::time::interval(Duration::from_millis(WATCH_FLUSH_TICK_MS));
        flush_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut reconcile_tick =
            tokio::time::interval(Duration::from_secs(RECONCILE_INTERVAL_SECS));
        reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Consume the immediate first tick so we reconcile on cadence, not instantly again.
        reconcile_tick.tick().await;

        loop {
            tokio::select! {
                maybe_path = path_rx.recv() => {
                    match maybe_path {
                        Some(path) => {
                            let accepted = if let Some(scope) = &watch_scope {
                                scope.allows_relative_file_path(&path)
                            } else {
                                true
                            };
                            autosync.record_watch_event(&repo_root, accepted).await;
                            if accepted {
                                pending_paths.insert(path);
                                last_event_at = Some(Instant::now());
                            }
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

        if !pending_paths.is_empty() {
            let mut changed_files = pending_paths.drain().collect::<Vec<_>>();
            changed_files.sort();
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
                tracing::warn!(
                    "auto-sync update failed while draining pending paths for {}: {:#}",
                    repo_root,
                    err
                );
            }
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        restart_count = restart_count.saturating_add(1);
        autosync
            .record_watcher_restart(&repo_root, restart_count)
            .await;
        let delay = watcher_restart_backoff(consecutive_failures);
        tracing::warn!(
            "restarting auto-sync watcher for {} (restart #{}, consecutive failures {}, backoff {}ms)",
            repo_root,
            restart_count,
            consecutive_failures,
            delay.as_millis()
        );
        tokio::time::sleep(delay).await;
    }
}

fn watcher_restart_backoff(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(6);
    let multiplier = 1u64 << exponent;
    let millis = WATCH_RESTART_BASE_MS
        .saturating_mul(multiplier)
        .min(WATCH_RESTART_MAX_MS);
    Duration::from_millis(millis)
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

    #[test]
    fn watcher_backoff_caps_at_maximum_delay() {
        assert_eq!(watcher_restart_backoff(1), Duration::from_millis(1_000));
        assert_eq!(watcher_restart_backoff(2), Duration::from_millis(2_000));
        assert_eq!(watcher_restart_backoff(7), Duration::from_millis(30_000));
        assert_eq!(watcher_restart_backoff(20), Duration::from_millis(30_000));
    }
}
