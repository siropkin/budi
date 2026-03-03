use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

use crate::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use crate::index::{self, RuntimeIndex};
use crate::retrieval;
use crate::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, QueryRequest,
    QueryResponse, StatusRequest, StatusResponse, UpdateRequest,
};

#[derive(Clone, Default)]
pub struct DaemonState {
    repos: Arc<RwLock<HashMap<String, Arc<Mutex<RuntimeIndex>>>>>,
    load_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    update_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    queued_updates: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    queued_reconciles: Arc<Mutex<HashSet<String>>>,
    progress: Arc<StdMutex<HashMap<String, IndexProgressSnapshot>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum IndexState {
    #[default]
    Ready,
    Indexing,
    Failed,
    Interrupted,
}

impl IndexState {
    fn as_str(self) -> &'static str {
        match self {
            IndexState::Ready => "ready",
            IndexState::Indexing => "indexing",
            IndexState::Failed => "failed",
            IndexState::Interrupted => "interrupted",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "indexing" => IndexState::Indexing,
            "failed" => IndexState::Failed,
            "interrupted" => IndexState::Interrupted,
            _ => IndexState::Ready,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct IndexProgressSnapshot {
    active: bool,
    hard: bool,
    state: IndexState,
    phase: String,
    total_files: usize,
    processed_files: usize,
    changed_files: usize,
    current_file: Option<String>,
    started_at_unix_ms: u128,
    last_update_unix_ms: u128,
    last_error: Option<String>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn query(&self, request: QueryRequest, config: &BudiConfig) -> Result<QueryResponse> {
        let repo_root = Path::new(&request.repo_root);
        let runtime = self.ensure_loaded(repo_root, config).await?;
        let query_embedding = index::embed_query(repo_root, &request.prompt)?;
        let runtime_guard = runtime.lock().await;
        let cwd = request.cwd.as_deref().map(Path::new);
        let retrieval_mode = retrieval::parse_retrieval_mode(request.retrieval_mode.as_deref());
        retrieval::build_query_response(
            &runtime_guard,
            &request.prompt,
            query_embedding.as_deref(),
            cwd,
            retrieval_mode,
            config,
        )
    }

    pub async fn index(&self, request: IndexRequest, config: &BudiConfig) -> Result<IndexResponse> {
        let repo_root = Path::new(&request.repo_root);
        self.start_progress(&request.repo_root, request.hard);
        let state_for_progress = self.clone();
        let repo_for_progress = request.repo_root.clone();
        let hard = request.hard;
        let mut progress_cb = move |progress: index::IndexBuildProgress| {
            state_for_progress.update_progress(&repo_for_progress, hard, progress);
        };
        let workspace = match index::build_or_update(
            repo_root,
            config,
            request.hard,
            None,
            Some(&mut progress_cb),
        ) {
            Ok(workspace) => workspace,
            Err(err) => {
                self.fail_progress(&request.repo_root, request.hard, &format!("{err:#}"));
                return Err(err);
            }
        };
        self.finish_progress(&request.repo_root, request.hard);
        let runtime = RuntimeIndex::from_state(repo_root, workspace.state)?;
        self.repos
            .write()
            .await
            .insert(request.repo_root.clone(), Arc::new(Mutex::new(runtime)));
        Ok(IndexResponse {
            indexed_files: workspace.report.indexed_files,
            indexed_chunks: workspace.report.indexed_chunks,
            embedded_chunks: workspace.report.embedded_chunks,
            missing_embeddings: workspace.report.missing_embeddings,
            repaired_embeddings: workspace.report.repaired_embeddings,
            invalid_embeddings: workspace.report.invalid_embeddings,
            changed_files: workspace.report.changed_files,
            index_status: if workspace.report.limit_reached {
                "limit_reached".to_string()
            } else {
                "completed".to_string()
            },
        })
    }

    pub async fn update(
        &self,
        request: UpdateRequest,
        config: &BudiConfig,
    ) -> Result<IndexResponse> {
        let repo_key = request.repo_root.clone();
        let changed_count = request.changed_files.len();
        self.queue_update_paths(&repo_key, &request.changed_files)
            .await;
        self.kick_update_processor(&repo_key, config).await;

        let (
            indexed_files,
            indexed_chunks,
            embedded_chunks,
            missing_embeddings,
            invalid_embeddings,
        ) = self.runtime_counts(&repo_key).await;
        Ok(IndexResponse {
            indexed_files,
            indexed_chunks,
            embedded_chunks,
            missing_embeddings,
            repaired_embeddings: 0,
            invalid_embeddings,
            changed_files: changed_count,
            index_status: "scheduled".to_string(),
        })
    }

    pub async fn request_reconcile(&self, repo_root: String, config: &BudiConfig) {
        self.queue_reconcile_repo(&repo_root).await;
        self.kick_update_processor(&repo_root, config).await;
    }

    pub async fn index_progress(
        &self,
        request: IndexProgressRequest,
    ) -> Result<IndexProgressResponse> {
        let mut snapshot = {
            let guard = self.progress_guard();
            guard.get(&request.repo_root).cloned().unwrap_or_default()
        };
        if snapshot.started_at_unix_ms == 0
            && let Some(persisted) = self.load_persisted_progress(&request.repo_root)
        {
            snapshot = self.interrupt_stale_active_progress(&request.repo_root, persisted);
        }
        Ok(IndexProgressResponse {
            repo_root: request.repo_root,
            active: snapshot.active,
            hard: snapshot.hard,
            state: snapshot.state.as_str().to_string(),
            phase: snapshot.phase,
            total_files: snapshot.total_files,
            processed_files: snapshot.processed_files,
            changed_files: snapshot.changed_files,
            current_file: snapshot.current_file,
            started_at_unix_ms: snapshot.started_at_unix_ms,
            last_update_unix_ms: snapshot.last_update_unix_ms,
            last_error: snapshot.last_error,
        })
    }

    pub async fn status(
        &self,
        request: StatusRequest,
        config: &BudiConfig,
    ) -> Result<StatusResponse> {
        let repo_root = Path::new(&request.repo_root);
        let runtime = self.ensure_loaded(repo_root, config).await?;
        let runtime_guard = runtime.lock().await;
        let hooks_detected = detect_hooks(repo_root);
        let embedded_chunks = runtime_guard
            .state
            .chunks
            .iter()
            .filter(|chunk| !chunk.embedding.is_empty())
            .count();
        let invalid_embeddings = runtime_guard
            .state
            .chunks
            .iter()
            .filter(|chunk| {
                !chunk.embedding.is_empty()
                    && chunk.embedding.iter().any(|value| !value.is_finite())
            })
            .count();
        Ok(StatusResponse {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: request.repo_root,
            tracked_files: runtime_guard.state.files.len(),
            embedded_chunks,
            invalid_embeddings,
            hooks_detected,
        })
    }

    async fn ensure_loaded(
        &self,
        repo_root: &Path,
        config: &BudiConfig,
    ) -> Result<Arc<Mutex<RuntimeIndex>>> {
        let key = repo_root.display().to_string();
        if let Some(runtime) = self.repos.read().await.get(&key) {
            return Ok(runtime.clone());
        }

        let load_lock = {
            let mut locks = self.load_locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _load_guard = load_lock.lock().await;
        if let Some(runtime) = self.repos.read().await.get(&key) {
            return Ok(runtime.clone());
        }

        let state = if let Some(state) = index::load_state(repo_root)? {
            state
        } else {
            let workspace = index::build_or_update(repo_root, config, false, None, None)?;
            workspace.state
        };
        let runtime = Arc::new(Mutex::new(RuntimeIndex::from_state(repo_root, state)?));
        self.repos.write().await.insert(key, runtime.clone());
        Ok(runtime)
    }

    fn start_progress(&self, repo_root: &str, hard: bool) {
        let now = now_unix_ms();
        let snapshot = IndexProgressSnapshot {
            active: true,
            hard,
            state: IndexState::Indexing,
            phase: "starting".to_string(),
            total_files: 0,
            processed_files: 0,
            changed_files: 0,
            current_file: None,
            started_at_unix_ms: now,
            last_update_unix_ms: now,
            last_error: None,
        };
        let mut guard = self.progress_guard();
        guard.insert(repo_root.to_string(), snapshot.clone());
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn update_progress(&self, repo_root: &str, hard: bool, progress: index::IndexBuildProgress) {
        let now = now_unix_ms();
        let mut guard = self.progress_guard();
        let entry = guard.entry(repo_root.to_string()).or_default();
        if entry.started_at_unix_ms == 0 {
            entry.started_at_unix_ms = now;
        }
        entry.active = !progress.done;
        entry.hard = hard;
        entry.state = if progress.done {
            IndexState::Ready
        } else {
            IndexState::Indexing
        };
        entry.phase = progress.phase;
        entry.total_files = progress.total_files;
        entry.processed_files = progress.processed_files;
        entry.changed_files = progress.changed_files;
        entry.current_file = progress.current_file;
        entry.last_update_unix_ms = now;
        entry.last_error = None;
        if progress.done {
            entry.current_file = None;
        }
        let snapshot = entry.clone();
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn finish_progress(&self, repo_root: &str, hard: bool) {
        let now = now_unix_ms();
        let mut guard = self.progress_guard();
        let entry = guard.entry(repo_root.to_string()).or_default();
        if entry.started_at_unix_ms == 0 {
            entry.started_at_unix_ms = now;
        }
        entry.active = false;
        entry.hard = hard;
        entry.state = IndexState::Ready;
        entry.phase = "ready".to_string();
        entry.current_file = None;
        entry.last_update_unix_ms = now;
        entry.last_error = None;
        let snapshot = entry.clone();
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn fail_progress(&self, repo_root: &str, hard: bool, error: &str) {
        let now = now_unix_ms();
        let mut guard = self.progress_guard();
        let entry = guard.entry(repo_root.to_string()).or_default();
        if entry.started_at_unix_ms == 0 {
            entry.started_at_unix_ms = now;
        }
        entry.active = false;
        entry.hard = hard;
        entry.state = IndexState::Failed;
        entry.phase = "failed".to_string();
        entry.current_file = None;
        entry.last_update_unix_ms = now;
        entry.last_error = Some(error.to_string());
        let snapshot = entry.clone();
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn persist_progress(&self, repo_root: &str, snapshot: &IndexProgressSnapshot) {
        let persisted = index::PersistedIndexProgress {
            active: snapshot.active,
            hard: snapshot.hard,
            state: snapshot.state.as_str().to_string(),
            phase: snapshot.phase.clone(),
            total_files: snapshot.total_files,
            processed_files: snapshot.processed_files,
            changed_files: snapshot.changed_files,
            current_file: snapshot.current_file.clone(),
            started_at_unix_ms: snapshot.started_at_unix_ms,
            last_update_unix_ms: snapshot.last_update_unix_ms,
            last_error: snapshot.last_error.clone(),
        };
        if let Err(err) = index::save_index_progress_snapshot(Path::new(repo_root), &persisted) {
            tracing::warn!(
                "failed persisting progress snapshot for {}: {:#}",
                repo_root,
                err
            );
        }
    }

    fn load_persisted_progress(&self, repo_root: &str) -> Option<IndexProgressSnapshot> {
        let persisted = match index::load_index_progress_snapshot(Path::new(repo_root)) {
            Ok(raw) => raw?,
            Err(err) => {
                tracing::warn!(
                    "failed loading persisted progress snapshot for {}: {:#}",
                    repo_root,
                    err
                );
                return None;
            }
        };
        Some(IndexProgressSnapshot {
            active: persisted.active,
            hard: persisted.hard,
            state: IndexState::parse(&persisted.state),
            phase: persisted.phase,
            total_files: persisted.total_files,
            processed_files: persisted.processed_files,
            changed_files: persisted.changed_files,
            current_file: persisted.current_file,
            started_at_unix_ms: persisted.started_at_unix_ms,
            last_update_unix_ms: persisted.last_update_unix_ms,
            last_error: persisted.last_error,
        })
    }

    fn interrupt_stale_active_progress(
        &self,
        repo_root: &str,
        mut snapshot: IndexProgressSnapshot,
    ) -> IndexProgressSnapshot {
        if snapshot.active || snapshot.state == IndexState::Indexing {
            snapshot.active = false;
            snapshot.state = IndexState::Interrupted;
            snapshot.phase = "interrupted".to_string();
            if snapshot.last_error.is_none() {
                snapshot.last_error = Some("indexing interrupted by daemon restart".to_string());
            }
            snapshot.last_update_unix_ms = now_unix_ms();
            self.persist_progress(repo_root, &snapshot);
        }
        snapshot
    }

    fn progress_guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, IndexProgressSnapshot>> {
        match self.progress.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    async fn queue_update_paths(&self, repo_key: &str, changed_files: &[String]) {
        let mut queue = self.queued_updates.lock().await;
        let entry = queue.entry(repo_key.to_string()).or_default();
        for path in changed_files {
            if !path.trim().is_empty() {
                entry.insert(path.clone());
            }
        }
    }

    async fn queue_reconcile_repo(&self, repo_key: &str) {
        let mut queue = self.queued_reconciles.lock().await;
        queue.insert(repo_key.to_string());
    }

    async fn take_queued_update_paths(&self, repo_key: &str) -> Vec<String> {
        let mut queue = self.queued_updates.lock().await;
        let mut out = queue
            .remove(repo_key)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    async fn take_queued_reconcile(&self, repo_key: &str) -> bool {
        let mut queue = self.queued_reconciles.lock().await;
        queue.remove(repo_key)
    }

    async fn kick_update_processor(&self, repo_key: &str, config: &BudiConfig) {
        let update_lock = {
            let mut locks = self.update_locks.lock().await;
            locks
                .entry(repo_key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        if let Ok(update_guard) = update_lock.clone().try_lock_owned() {
            let state = self.clone();
            let repo_key_for_task = repo_key.to_string();
            let config_for_task = config.clone();
            tokio::spawn(async move {
                state
                    .process_queued_updates(repo_key_for_task, config_for_task, update_guard)
                    .await;
            });
        }
    }

    async fn runtime_counts(&self, repo_key: &str) -> (usize, usize, usize, usize, usize) {
        let runtime = { self.repos.read().await.get(repo_key).cloned() };
        if let Some(runtime) = runtime {
            let guard = runtime.lock().await;
            let embedded_chunks = guard
                .state
                .chunks
                .iter()
                .filter(|chunk| !chunk.embedding.is_empty())
                .count();
            let missing_embeddings = guard.state.chunks.len().saturating_sub(embedded_chunks);
            let invalid_embeddings = guard
                .state
                .chunks
                .iter()
                .filter(|chunk| {
                    !chunk.embedding.is_empty()
                        && chunk.embedding.iter().any(|value| !value.is_finite())
                })
                .count();
            (
                guard.state.files.len(),
                guard.state.chunks.len(),
                embedded_chunks,
                missing_embeddings,
                invalid_embeddings,
            )
        } else {
            (0, 0, 0, 0, 0)
        }
    }

    async fn process_queued_updates(
        &self,
        repo_key: String,
        config: BudiConfig,
        _update_guard: OwnedMutexGuard<()>,
    ) {
        let repo_root = Path::new(&repo_key);
        loop {
            let changed_files = self.take_queued_update_paths(&repo_key).await;
            let reconcile_requested = self.take_queued_reconcile(&repo_key).await;
            if changed_files.is_empty() && !reconcile_requested {
                break;
            }

            let workspace = match if reconcile_requested {
                index::build_or_update(repo_root, &config, false, None, None)
            } else {
                index::build_or_update(repo_root, &config, false, Some(&changed_files), None)
            } {
                Ok(workspace) => workspace,
                Err(err) => {
                    tracing::warn!(
                        "Background update failed for {} (trigger={} files={}): {:#}",
                        repo_key,
                        if reconcile_requested {
                            "reconcile"
                        } else {
                            "watch/hook"
                        },
                        changed_files.len(),
                        err
                    );
                    continue;
                }
            };
            if workspace.report.limit_reached {
                tracing::warn!(
                    "Background update hit index budget limits for {} (trigger={} files={})",
                    repo_key,
                    if reconcile_requested {
                        "reconcile"
                    } else {
                        "watch/hook"
                    },
                    changed_files.len()
                );
            }
            let runtime = match RuntimeIndex::from_state(repo_root, workspace.state) {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::warn!(
                        "Failed to rebuild runtime index after update for {}: {:#}",
                        repo_key,
                        err
                    );
                    continue;
                }
            };
            self.repos
                .write()
                .await
                .insert(repo_key.clone(), Arc::new(Mutex::new(runtime)));
        }
    }
}

fn detect_hooks(repo_root: &Path) -> bool {
    let settings_path = repo_root.join(CLAUDE_LOCAL_SETTINGS);
    let Ok(raw) = std::fs::read_to_string(settings_path) else {
        return false;
    };
    raw.contains("UserPromptSubmit") && raw.contains("budi hook user-prompt-submit")
}

pub fn resolve_repo_root(input_repo_root: Option<String>, cwd: &Path) -> Result<String> {
    if let Some(root) = input_repo_root {
        return Ok(root);
    }
    Ok(config::find_repo_root(cwd)?.display().to_string())
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
