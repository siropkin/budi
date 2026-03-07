use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

use crate::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use crate::index::{self, RuntimeIndex};
use crate::pre_filter;
use crate::project_map;
use crate::reason_codes::SKIP_REASON_NON_CODE_INTENT;
use crate::retrieval;
use crate::rpc::{
    IndexProgressRequest, IndexProgressResponse, IndexRequest, IndexResponse, PrefetchRequest,
    PrefetchResponse, QueryDiagnostics, QueryRequest, QueryResponse, StatusRequest, StatusResponse,
    UpdateRequest,
};

const SESSION_TTL_SECS: u64 = 1800;

const PROGRESS_PERSIST_INTERVAL_MS: u128 = 2_000;
const WRITE_RETRY_ATTEMPTS: usize = 3;
const WRITE_RETRY_BASE_DELAY_MS: u64 = 75;
const WRITE_RETRY_MAX_DELAY_MS: u64 = 600;

#[derive(Debug, Default)]
struct SessionState {
    injected_keys: HashSet<String>, // "path:start_line" of already-injected snippets
    last_activity: Option<Instant>,
}

#[derive(Clone, Default)]
pub struct DaemonState {
    repos: Arc<RwLock<HashMap<String, Arc<Mutex<RuntimeIndex>>>>>,
    load_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    index_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    update_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    queued_updates: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    queued_reconciles: Arc<Mutex<HashSet<String>>>,
    progress: Arc<StdMutex<HashMap<String, IndexProgressSnapshot>>>,
    update_metrics: Arc<StdMutex<HashMap<String, UpdateRetryMetrics>>>,
    job_counter: Arc<StdMutex<u64>>,
    sessions: Arc<StdMutex<HashMap<String, SessionState>>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum IndexJobState {
    #[default]
    Idle,
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

impl IndexJobState {
    fn as_str(self) -> &'static str {
        match self {
            IndexJobState::Idle => "idle",
            IndexJobState::Queued => "queued",
            IndexJobState::Running => "running",
            IndexJobState::Succeeded => "succeeded",
            IndexJobState::Failed => "failed",
            IndexJobState::Interrupted => "interrupted",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "queued" => IndexJobState::Queued,
            "running" => IndexJobState::Running,
            "succeeded" => IndexJobState::Succeeded,
            "failed" => IndexJobState::Failed,
            "interrupted" => IndexJobState::Interrupted,
            _ => IndexJobState::Idle,
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
    job_id: Option<String>,
    job_state: IndexJobState,
    terminal_outcome: Option<String>,
    last_persist_unix_ms: u128,
    last_persist_phase: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct UpdateRetryMetrics {
    retries: u64,
    failures: u64,
    updates_noop: u64,
    updates_applied: u64,
}

#[derive(Debug, Clone, Copy)]
struct BuildRetryRequest<'a> {
    repo_root: &'a Path,
    repo_key: &'a str,
    hard: bool,
    changed_hint: Option<&'a [String]>,
    options: Option<&'a index::IndexBuildOptions>,
    trigger: &'a str,
}

impl DaemonState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn query(&self, request: QueryRequest, config: &BudiConfig) -> Result<QueryResponse> {
        // Step 1: Pre-filter — exit immediately for unambiguous non-code prompts.
        if pre_filter::is_obviously_non_code(&request.prompt) {
            return Ok(QueryResponse {
                total_candidates: 0,
                context: String::new(),
                snippets: Vec::new(),
                call_graph_summary: None,
                detected_intent: None,
                timing_ms: None,
                snippet_refs: Vec::new(),
                diagnostics: QueryDiagnostics {
                    intent: "non-code".to_string(),
                    confidence: 0.0,
                    top_score: 0.0,
                    margin: 0.0,
                    signals: Vec::new(),
                    recommended_injection: false,
                    skip_reason: Some(SKIP_REASON_NON_CODE_INTENT.to_string()),
                },
            });
        }

        // Step 2: Hybrid retrieval with equal channel weights.
        let t_start = Instant::now();
        let repo_root = Path::new(&request.repo_root);
        let runtime = self.ensure_loaded(repo_root, config).await?;
        let t_load_ms = t_start.elapsed().as_millis() as u64;
        let query_embedding = index::embed_query(repo_root, &request.prompt)?;
        let t_embed_ms = t_start.elapsed().as_millis() as u64;
        let runtime_guard = runtime.lock().await;
        let cwd = request.cwd.as_deref().map(Path::new);
        let retrieval_mode = retrieval::parse_retrieval_mode(request.retrieval_mode.as_deref());
        let mut response = retrieval::build_query_response(
            &runtime_guard,
            &request.prompt,
            query_embedding.as_deref(),
            cwd,
            retrieval_mode,
            config,
        )?;
        drop(runtime_guard);
        let t_retrieval_ms = t_start.elapsed().as_millis() as u64;

        // Step 3: Session deduplication — remove snippets Claude already saw this session.
        if let Some(ref sid) = request.session_id {
            self.dedup_session_snippets(sid, &mut response.snippets);
        }
        let t_dedup_ms = t_start.elapsed().as_millis() as u64;

        // Populate snippet_refs for structured analytics (W1).
        response.snippet_refs = response
            .snippets
            .iter()
            .map(|s| crate::rpc::SnippetRef {
                path: s.path.clone(),
                score: s.score,
                start_line: s.start_line,
                end_line: s.end_line,
            })
            .collect();

        // Step 4: Score-based injection decision.
        let should_inject = response
            .snippets
            .iter()
            .any(|s| s.score >= config.min_inject_score);
        response.diagnostics.recommended_injection = should_inject;
        response.diagnostics.skip_reason = if should_inject {
            None
        } else {
            Some("low-score".to_string())
        };

        // Step 5: Call graph summary (structural oracle) — prepended to context.
        // Phase K1: per-intent budget — suppress for breadth intents, boost for flow-trace.
        // Phase L2: FlowTrace budget is gated on top-snippet confidence (≥0.30 → 1200, else 600).
        let call_graph_budget = match response.detected_intent.as_deref() {
            Some("flow-trace") => {
                let top_score = response.snippets.first().map(|s| s.score).unwrap_or(0.0);
                if top_score >= 0.30 { 1200 } else { 600 }
            }
            Some("symbol-definition") | Some("symbol-usage") => 800,
            Some("architecture")
            | Some("test-lookup")
            | Some("runtime-config")
            | Some("path-lookup") => 0,
            _ => 600,
        };
        let runtime_guard = runtime.lock().await;
        let call_graph = if should_inject && call_graph_budget > 0 {
            retrieval::build_call_graph_summary(
                &runtime_guard,
                &response.snippets,
                call_graph_budget,
            )
        } else {
            None
        };
        drop(runtime_guard);
        let t_callgraph_ms = t_start.elapsed().as_millis() as u64;

        // Phase L1: Deduct call graph from snippet budget so total ≤ context_char_budget.
        let base_budget = if call_graph.is_some() {
            config.context_char_budget.saturating_sub(call_graph_budget)
        } else {
            config.context_char_budget
        };
        // Rebuild context after dedup + prepend call graph summary.
        let base_context = if request.session_id.is_some() || call_graph.is_some() {
            retrieval::format_context(&response.snippets, base_budget)
        } else {
            response.context.clone()
        };
        response.context = if let Some(ref cg) = call_graph {
            format!("{}\n{}", cg, base_context)
        } else {
            base_context
        };
        response.call_graph_summary = call_graph;

        // Record injected snippets in session state.
        if should_inject {
            if let Some(ref sid) = request.session_id {
                self.record_session_snippets(sid, &response.snippets);
            }
            // Phase J+M1: Persist session affinity with anchor lines for next-session context.
            let snippets_owned = response.snippets.clone();
            let repo_root_owned = repo_root.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let _ = update_session_affinity(&repo_root_owned, &snippets_owned);
            });
        }

        // Phase I: Per-step timing (populated when debug_io is enabled).
        if config.debug_io {
            let mut timing = HashMap::new();
            timing.insert("load_ms".to_string(), t_load_ms);
            timing.insert("embed_ms".to_string(), t_embed_ms.saturating_sub(t_load_ms));
            timing.insert(
                "retrieval_ms".to_string(),
                t_retrieval_ms.saturating_sub(t_embed_ms),
            );
            timing.insert(
                "dedup_ms".to_string(),
                t_dedup_ms.saturating_sub(t_retrieval_ms),
            );
            timing.insert(
                "callgraph_ms".to_string(),
                t_callgraph_ms.saturating_sub(t_dedup_ms),
            );
            timing.insert("total_ms".to_string(), t_callgraph_ms);
            response.timing_ms = Some(timing);
        }

        Ok(response)
    }

    pub async fn prefetch_neighbors(
        &self,
        request: PrefetchRequest,
        config: &BudiConfig,
    ) -> Result<PrefetchResponse> {
        // Skip if this file was already prefetched this session.
        if self.session_has_path(&request.session_id, &request.file_path) {
            return Ok(PrefetchResponse {
                context: String::new(),
                neighbor_paths: Vec::new(),
                skipped: true,
            });
        }

        let repo_root = Path::new(&request.repo_root);
        let runtime = self.ensure_loaded(repo_root, config).await?;
        let runtime_guard = runtime.lock().await;

        // Strip repo_root prefix from file_path to get index-relative path.
        let file_path_rel = strip_repo_root_prefix(&request.file_path, &request.repo_root);

        let limit = request.limit.unwrap_or(5);
        let (snippets, context) = retrieval::prefetch_neighbors_for_file(
            &runtime_guard,
            &file_path_rel,
            limit,
            config.context_char_budget,
        );
        drop(runtime_guard);

        let neighbor_paths: Vec<String> = snippets.iter().map(|s| s.path.clone()).collect();

        // Record the source file and injected neighbors in the session.
        self.record_session_path(&request.session_id, &request.file_path);
        if !neighbor_paths.is_empty() {
            for path in &neighbor_paths {
                self.record_session_path(&request.session_id, path);
            }
        }

        Ok(PrefetchResponse {
            context,
            neighbor_paths,
            skipped: false,
        })
    }

    pub async fn index(&self, request: IndexRequest, config: &BudiConfig) -> Result<IndexResponse> {
        let repo_key = request.repo_root.clone();
        let index_lock = {
            let mut locks = self.index_locks.lock().await;
            locks
                .entry(repo_key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        if let Ok(index_guard) = index_lock.clone().try_lock_owned() {
            let job_id = self.next_index_job_id();
            self.start_progress(&repo_key, request.hard, &job_id);
            let state = self.clone();
            let request_for_task = request.clone();
            let config_for_task = config.clone();
            let job_id_for_task = job_id.clone();
            tokio::spawn(async move {
                state
                    .run_index_job(
                        request_for_task,
                        config_for_task,
                        job_id_for_task,
                        index_guard,
                    )
                    .await;
            });
            let (
                indexed_files,
                indexed_chunks,
                embedded_chunks,
                missing_embeddings,
                invalid_embeddings,
            ) = self.runtime_counts(&repo_key).await;
            return Ok(IndexResponse {
                indexed_files,
                indexed_chunks,
                embedded_chunks,
                missing_embeddings,
                repaired_embeddings: 0,
                invalid_embeddings,
                changed_files: 0,
                index_status: "scheduled".to_string(),
                job_id: Some(job_id),
                job_state: IndexJobState::Queued.as_str().to_string(),
                terminal_outcome: None,
            });
        }

        let snapshot = self.current_progress_snapshot(&repo_key);
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
            changed_files: 0,
            index_status: "already_running".to_string(),
            job_id: snapshot.job_id,
            job_state: snapshot.job_state.as_str().to_string(),
            terminal_outcome: snapshot.terminal_outcome,
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
        let progress_snapshot = self.current_progress_snapshot(&repo_key);
        Ok(IndexResponse {
            indexed_files,
            indexed_chunks,
            embedded_chunks,
            missing_embeddings,
            repaired_embeddings: 0,
            invalid_embeddings,
            changed_files: changed_count,
            index_status: "scheduled".to_string(),
            job_id: progress_snapshot.job_id,
            job_state: progress_snapshot.job_state.as_str().to_string(),
            terminal_outcome: progress_snapshot.terminal_outcome,
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
        let snapshot = self.current_progress_snapshot(&request.repo_root);
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
            job_id: snapshot.job_id,
            job_state: snapshot.job_state.as_str().to_string(),
            terminal_outcome: snapshot.terminal_outcome,
        })
    }

    pub async fn status(
        &self,
        request: StatusRequest,
        _config: &BudiConfig,
    ) -> Result<StatusResponse> {
        let repo_root = Path::new(&request.repo_root);
        let repo_key = request.repo_root.clone();
        let runtime = if let Some(runtime) = self.repos.read().await.get(&repo_key).cloned() {
            Some(runtime)
        } else if let Some(state) = index::load_state(repo_root)? {
            let runtime = Arc::new(Mutex::new(RuntimeIndex::from_state(repo_root, state)?));
            self.repos
                .write()
                .await
                .insert(repo_key.clone(), runtime.clone());
            Some(runtime)
        } else {
            None
        };
        let hooks_detected = detect_hooks(repo_root);
        let update_metrics = self.update_retry_metrics(&request.repo_root);
        let progress_snapshot = self.current_progress_snapshot(&request.repo_root);
        let (
            tracked_files,
            indexed_chunks,
            embedded_chunks,
            missing_embeddings,
            invalid_embeddings,
        ) = if let Some(runtime) = runtime {
            let runtime_guard = runtime.lock().await;
            let embedded_chunks = runtime_guard
                .state
                .chunks
                .iter()
                .filter(|chunk| !chunk.embedding.is_empty())
                .count();
            let indexed_chunks = runtime_guard.state.chunks.len();
            let missing_embeddings = indexed_chunks.saturating_sub(embedded_chunks);
            let invalid_embeddings = runtime_guard
                .state
                .chunks
                .iter()
                .filter(|chunk| {
                    !chunk.embedding.is_empty()
                        && chunk.embedding.iter().any(|value| !value.is_finite())
                })
                .count();
            (
                runtime_guard.state.files.len(),
                indexed_chunks,
                embedded_chunks,
                missing_embeddings,
                invalid_embeddings,
            )
        } else {
            (0, 0, 0, 0, 0)
        };
        Ok(StatusResponse {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: request.repo_root,
            tracked_files,
            indexed_chunks,
            embedded_chunks,
            missing_embeddings,
            invalid_embeddings,
            hooks_detected,
            update_retries: update_metrics.retries,
            update_failures: update_metrics.failures,
            updates_noop: update_metrics.updates_noop,
            updates_applied: update_metrics.updates_applied,
            index_state: progress_snapshot.state.as_str().to_string(),
            index_job_id: progress_snapshot.job_id,
            index_job_state: progress_snapshot.job_state.as_str().to_string(),
            index_terminal_outcome: progress_snapshot.terminal_outcome,
            watch_events_seen: 0,
            watch_events_accepted: 0,
            watch_events_dropped: 0,
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
            let workspace = self.run_build_or_update_with_retry(
                BuildRetryRequest {
                    repo_root,
                    repo_key: &key,
                    hard: false,
                    changed_hint: None,
                    options: None,
                    trigger: "initial-load",
                },
                config,
                None,
            )?;
            workspace.state
        };
        let runtime = Arc::new(Mutex::new(RuntimeIndex::from_state(repo_root, state)?));
        self.repos.write().await.insert(key, runtime.clone());
        Ok(runtime)
    }

    fn next_index_job_id(&self) -> String {
        let now = now_unix_ms();
        let mut counter = self.job_counter_guard();
        *counter = counter.saturating_add(1);
        format!("idx-{now}-{}", *counter)
    }

    fn current_progress_snapshot(&self, repo_root: &str) -> IndexProgressSnapshot {
        let mut snapshot = {
            let guard = self.progress_guard();
            guard.get(repo_root).cloned().unwrap_or_default()
        };
        if snapshot.started_at_unix_ms == 0
            && let Some(persisted) = self.load_persisted_progress(repo_root)
        {
            snapshot = self.interrupt_stale_active_progress(repo_root, persisted);
        }
        snapshot
    }

    async fn run_index_job(
        &self,
        request: IndexRequest,
        config: BudiConfig,
        job_id: String,
        _index_guard: OwnedMutexGuard<()>,
    ) {
        let repo_key = request.repo_root.clone();
        let hard = request.hard;
        self.mark_progress_running(&repo_key, hard, &job_id);

        let build_options = index::IndexBuildOptions {
            include_extensions: request.include_extensions.clone(),
            ignore_patterns: request.ignore_patterns.clone(),
        };
        let repo_root = PathBuf::from(&repo_key);
        let state_for_build = self.clone();
        let repo_key_for_build = repo_key.clone();
        let job_id_for_build = job_id.clone();
        let config_for_build = config.clone();
        let build_result = tokio::task::spawn_blocking(move || -> Result<index::IndexWorkspace> {
            let state_for_progress = state_for_build.clone();
            let repo_for_progress = repo_key_for_build.clone();
            let job_for_progress = job_id_for_build.clone();
            let mut progress_cb = move |progress: index::IndexBuildProgress| {
                state_for_progress.update_progress(
                    &repo_for_progress,
                    hard,
                    &job_for_progress,
                    progress,
                );
            };
            state_for_build.run_build_or_update_with_retry(
                BuildRetryRequest {
                    repo_root: &repo_root,
                    repo_key: &repo_key_for_build,
                    hard,
                    changed_hint: None,
                    options: Some(&build_options),
                    trigger: "index-job",
                },
                &config_for_build,
                Some(&mut progress_cb),
            )
        })
        .await;

        let workspace = match build_result {
            Ok(Ok(workspace)) => workspace,
            Ok(Err(err)) => {
                self.fail_progress(&repo_key, hard, &job_id, &format!("{err:#}"));
                return;
            }
            Err(err) => {
                self.fail_progress(
                    &repo_key,
                    hard,
                    &job_id,
                    &format!("index worker join error: {err:#}"),
                );
                return;
            }
        };

        let repo_root_path = PathBuf::from(&repo_key);
        let runtime = match RuntimeIndex::from_state(&repo_root_path, workspace.state) {
            Ok(runtime) => runtime,
            Err(err) => {
                self.fail_progress(&repo_key, hard, &job_id, &format!("{err:#}"));
                return;
            }
        };

        // Generate project map while we still own the runtime.
        if let Err(err) = project_map::write_project_map(&runtime, &repo_root_path) {
            tracing::warn!("Failed writing project map for {}: {:#}", repo_key, err);
        }

        self.repos
            .write()
            .await
            .insert(repo_key.clone(), Arc::new(Mutex::new(runtime)));

        let terminal_outcome = if workspace.report.limit_reached {
            "limit_reached"
        } else {
            "completed"
        };
        self.finish_progress(&repo_key, hard, &job_id, terminal_outcome);
    }

    fn run_build_or_update_with_retry(
        &self,
        request: BuildRetryRequest<'_>,
        config: &BudiConfig,
        mut progress_cb: Option<&mut dyn FnMut(index::IndexBuildProgress)>,
    ) -> Result<index::IndexWorkspace> {
        let mut attempt = 0usize;
        loop {
            match index::build_or_update(
                request.repo_root,
                config,
                request.hard,
                request.changed_hint,
                request.options,
                progress_cb.take(),
            ) {
                Ok(workspace) => return Ok(workspace),
                Err(err) if attempt < WRITE_RETRY_ATTEMPTS && is_transient_write_failure(&err) => {
                    let delay = retry_backoff_delay(attempt);
                    attempt += 1;
                    self.bump_update_retries(request.repo_key, 1);
                    tracing::warn!(
                        "Transient write failure for {} (trigger={} attempt={}/{} delay_ms={}): {:#}",
                        request.repo_key,
                        request.trigger,
                        attempt,
                        WRITE_RETRY_ATTEMPTS,
                        delay.as_millis(),
                        err
                    );
                    std::thread::sleep(delay);
                }
                Err(err) => {
                    self.bump_update_failures(request.repo_key, 1);
                    return Err(err);
                }
            }
        }
    }

    fn bump_update_retries(&self, repo_key: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut guard = self.update_metrics_guard();
        let entry = guard.entry(repo_key.to_string()).or_default();
        entry.retries = entry.retries.saturating_add(count);
    }

    fn bump_update_failures(&self, repo_key: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut guard = self.update_metrics_guard();
        let entry = guard.entry(repo_key.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(count);
    }

    fn bump_updates_noop(&self, repo_key: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut guard = self.update_metrics_guard();
        let entry = guard.entry(repo_key.to_string()).or_default();
        entry.updates_noop = entry.updates_noop.saturating_add(count);
    }

    fn bump_updates_applied(&self, repo_key: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut guard = self.update_metrics_guard();
        let entry = guard.entry(repo_key.to_string()).or_default();
        entry.updates_applied = entry.updates_applied.saturating_add(count);
    }

    fn update_retry_metrics(&self, repo_key: &str) -> UpdateRetryMetrics {
        let guard = self.update_metrics_guard();
        guard.get(repo_key).copied().unwrap_or_default()
    }

    fn start_progress(&self, repo_root: &str, hard: bool, job_id: &str) {
        let now = now_unix_ms();
        let snapshot = IndexProgressSnapshot {
            active: true,
            hard,
            state: IndexState::Indexing,
            phase: "queued".to_string(),
            total_files: 0,
            processed_files: 0,
            changed_files: 0,
            current_file: None,
            started_at_unix_ms: now,
            last_update_unix_ms: now,
            last_error: None,
            job_id: Some(job_id.to_string()),
            job_state: IndexJobState::Queued,
            terminal_outcome: None,
            last_persist_unix_ms: now,
            last_persist_phase: "queued".to_string(),
        };
        let mut guard = self.progress_guard();
        guard.insert(repo_root.to_string(), snapshot.clone());
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn mark_progress_running(&self, repo_root: &str, hard: bool, job_id: &str) {
        let now = now_unix_ms();
        let mut guard = self.progress_guard();
        let entry = guard.entry(repo_root.to_string()).or_default();
        if entry.started_at_unix_ms == 0 {
            entry.started_at_unix_ms = now;
        }
        entry.active = true;
        entry.hard = hard;
        entry.state = IndexState::Indexing;
        if entry.phase.is_empty() || entry.phase == "queued" {
            entry.phase = "starting".to_string();
        }
        entry.current_file = None;
        entry.last_update_unix_ms = now;
        entry.last_error = None;
        entry.job_id = Some(job_id.to_string());
        entry.job_state = IndexJobState::Running;
        entry.terminal_outcome = None;
        entry.last_persist_unix_ms = now;
        entry.last_persist_phase = entry.phase.clone();
        let snapshot = entry.clone();
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn update_progress(
        &self,
        repo_root: &str,
        hard: bool,
        job_id: &str,
        progress: index::IndexBuildProgress,
    ) {
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
        entry.job_id = Some(job_id.to_string());
        entry.job_state = IndexJobState::Running;
        entry.terminal_outcome = None;
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
        let should_persist = progress.done
            || entry.phase != entry.last_persist_phase
            || now.saturating_sub(entry.last_persist_unix_ms) >= PROGRESS_PERSIST_INTERVAL_MS;
        let snapshot = if should_persist {
            entry.last_persist_unix_ms = now;
            entry.last_persist_phase = entry.phase.clone();
            Some(entry.clone())
        } else {
            None
        };
        drop(guard);
        if let Some(snapshot) = snapshot {
            self.persist_progress(repo_root, &snapshot);
        }
    }

    fn finish_progress(&self, repo_root: &str, hard: bool, job_id: &str, terminal_outcome: &str) {
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
        entry.job_id = Some(job_id.to_string());
        entry.job_state = IndexJobState::Succeeded;
        entry.terminal_outcome = Some(terminal_outcome.to_string());
        entry.last_persist_unix_ms = now;
        entry.last_persist_phase = entry.phase.clone();
        let snapshot = entry.clone();
        drop(guard);
        self.persist_progress(repo_root, &snapshot);
    }

    fn fail_progress(&self, repo_root: &str, hard: bool, job_id: &str, error: &str) {
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
        entry.job_id = Some(job_id.to_string());
        entry.job_state = IndexJobState::Failed;
        entry.terminal_outcome = Some("failed".to_string());
        entry.last_persist_unix_ms = now;
        entry.last_persist_phase = entry.phase.clone();
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
            job_id: snapshot.job_id.clone(),
            job_state: snapshot.job_state.as_str().to_string(),
            terminal_outcome: snapshot.terminal_outcome.clone(),
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
        let phase = persisted.phase;
        Some(IndexProgressSnapshot {
            active: persisted.active,
            hard: persisted.hard,
            state: IndexState::parse(&persisted.state),
            phase: phase.clone(),
            total_files: persisted.total_files,
            processed_files: persisted.processed_files,
            changed_files: persisted.changed_files,
            current_file: persisted.current_file,
            started_at_unix_ms: persisted.started_at_unix_ms,
            last_update_unix_ms: persisted.last_update_unix_ms,
            last_error: persisted.last_error,
            job_id: persisted.job_id,
            job_state: IndexJobState::parse(&persisted.job_state),
            terminal_outcome: persisted.terminal_outcome,
            last_persist_unix_ms: persisted.last_update_unix_ms,
            last_persist_phase: phase,
        })
    }

    fn interrupt_stale_active_progress(
        &self,
        repo_root: &str,
        mut snapshot: IndexProgressSnapshot,
    ) -> IndexProgressSnapshot {
        if snapshot.active
            || snapshot.state == IndexState::Indexing
            || matches!(
                snapshot.job_state,
                IndexJobState::Queued | IndexJobState::Running
            )
        {
            snapshot.active = false;
            snapshot.state = IndexState::Interrupted;
            snapshot.phase = "interrupted".to_string();
            if snapshot.last_error.is_none() {
                snapshot.last_error = Some("indexing interrupted by daemon restart".to_string());
            }
            snapshot.job_state = IndexJobState::Interrupted;
            snapshot.terminal_outcome = Some("interrupted".to_string());
            snapshot.last_update_unix_ms = now_unix_ms();
            snapshot.last_persist_unix_ms = snapshot.last_update_unix_ms;
            snapshot.last_persist_phase = snapshot.phase.clone();
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

    fn update_metrics_guard(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, UpdateRetryMetrics>> {
        match self.update_metrics.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn job_counter_guard(&self) -> std::sync::MutexGuard<'_, u64> {
        match self.job_counter.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn sessions_guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionState>> {
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn dedup_session_snippets(&self, session_id: &str, snippets: &mut Vec<crate::rpc::QueryResultItem>) {
        let guard = self.sessions_guard();
        if let Some(session) = guard.get(session_id) {
            snippets.retain(|s| {
                let key = format!("{}:{}", s.path, s.start_line);
                !session.injected_keys.contains(&key)
            });
        }
    }

    fn record_session_snippets(&self, session_id: &str, snippets: &[crate::rpc::QueryResultItem]) {
        let mut guard = self.sessions_guard();
        let session = guard.entry(session_id.to_string()).or_default();
        session.last_activity = Some(Instant::now());
        for s in snippets {
            session.injected_keys.insert(format!("{}:{}", s.path, s.start_line));
        }
        // Lazy TTL cleanup: remove inactive sessions.
        guard.retain(|_, v| {
            v.last_activity
                .map(|t| t.elapsed().as_secs() < SESSION_TTL_SECS)
                .unwrap_or(true)
        });
    }

    fn session_has_path(&self, session_id: &str, file_path: &str) -> bool {
        let guard = self.sessions_guard();
        guard
            .get(session_id)
            .map(|s| s.injected_keys.contains(file_path))
            .unwrap_or(false)
    }

    fn record_session_path(&self, session_id: &str, path: &str) {
        let mut guard = self.sessions_guard();
        let session = guard.entry(session_id.to_string()).or_default();
        session.last_activity = Some(Instant::now());
        session.injected_keys.insert(path.to_string());
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
            let scoped_changed_files = if reconcile_requested {
                changed_files.clone()
            } else {
                match index::compile_index_scope(repo_root, &config, None) {
                    Ok(scope) => changed_files
                        .iter()
                        .filter(|path| scope.allows_relative_file_path(path))
                        .cloned()
                        .collect::<Vec<_>>(),
                    Err(err) => {
                        tracing::warn!(
                            "Failed compiling update scope for {}: {:#}; applying updates without watcher-side filtering",
                            repo_key,
                            err
                        );
                        changed_files.clone()
                    }
                }
            };
            if scoped_changed_files.is_empty() && !reconcile_requested {
                self.bump_updates_noop(&repo_key, 1);
                continue;
            }

            let trigger = if reconcile_requested {
                "reconcile"
            } else {
                "watch/hook"
            };
            let workspace = match if reconcile_requested {
                self.run_build_or_update_with_retry(
                    BuildRetryRequest {
                        repo_root,
                        repo_key: &repo_key,
                        hard: false,
                        changed_hint: None,
                        options: None,
                        trigger,
                    },
                    &config,
                    None,
                )
            } else {
                self.run_build_or_update_with_retry(
                    BuildRetryRequest {
                        repo_root,
                        repo_key: &repo_key,
                        hard: false,
                        changed_hint: Some(&scoped_changed_files),
                        options: None,
                        trigger,
                    },
                    &config,
                    None,
                )
            } {
                Ok(workspace) => workspace,
                Err(err) => {
                    tracing::warn!(
                        "Background update failed for {} (trigger={} files={}): {:#}",
                        repo_key,
                        trigger,
                        scoped_changed_files.len(),
                        err
                    );
                    continue;
                }
            };
            if workspace.report.changed_files == 0 {
                self.bump_updates_noop(&repo_key, 1);
                continue;
            }
            if workspace.report.limit_reached {
                tracing::warn!(
                    "Background update hit index budget limits for {} (trigger={} files={})",
                    repo_key,
                    trigger,
                    scoped_changed_files.len()
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
            self.bump_updates_applied(&repo_key, 1);
        }
    }
}

fn is_transient_write_failure(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("database is locked")
        || message.contains("database busy")
        || message.contains("database table is locked")
        || message.contains("resource temporarily unavailable")
        || message.contains("temporarily unavailable")
}

fn retry_backoff_delay(attempt: usize) -> Duration {
    let exponent = u32::try_from(attempt).unwrap_or(u32::MAX).min(10);
    let multiplier = 2u64.saturating_pow(exponent);
    let delay_ms = WRITE_RETRY_BASE_DELAY_MS
        .saturating_mul(multiplier)
        .min(WRITE_RETRY_MAX_DELAY_MS);
    Duration::from_millis(delay_ms)
}

/// Convert an absolute file path to a repo-relative slash path for index lookup.
fn strip_repo_root_prefix<'a>(file_path: &'a str, repo_root: &str) -> String {
    let repo_prefix = repo_root.trim_end_matches('/');
    let stripped = file_path
        .strip_prefix(repo_prefix)
        .unwrap_or(file_path)
        .trim_start_matches('/');
    stripped.replace('\\', "/")
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

/// Phase J+M1: Affinity entry — timestamp plus up to 2 anchor lines per file.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct AffinityEntry {
    ts: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    anchors: Vec<String>,
}

/// Phase J+M1: Persist recently-injected files with anchor lines so the next session can surface them.
/// Reads `session-affinity.json`, migrates old flat format if needed, keeps top 50 by recency.
fn update_session_affinity(repo_root: &Path, snippets: &[crate::rpc::QueryResultItem]) -> Result<()> {
    if snippets.is_empty() {
        return Ok(());
    }
    let affinity_path = config::repo_paths(repo_root)?.data_dir.join("session-affinity.json");
    let mut map: HashMap<String, AffinityEntry> = if affinity_path.exists() {
        let raw = std::fs::read_to_string(&affinity_path)?;
        serde_json::from_str(&raw).or_else(|_| {
            // Migrate old format: HashMap<String, u64>
            let old: HashMap<String, u64> = serde_json::from_str(&raw)?;
            Ok::<_, serde_json::Error>(
                old.into_iter()
                    .map(|(k, ts)| (k, AffinityEntry { ts, anchors: vec![] }))
                    .collect(),
            )
        }).unwrap_or_default()
    } else {
        HashMap::new()
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // Group snippets by path and collect up to 2 anchor lines per file.
    let mut path_anchors: HashMap<String, Vec<String>> = HashMap::new();
    for snippet in snippets {
        let anchors = path_anchors.entry(snippet.path.clone()).or_default();
        if anchors.len() < 2 {
            if let Some(anchor) = affinity_anchor_line(&snippet.text) {
                if !anchors.contains(&anchor) {
                    anchors.push(anchor);
                }
            }
        }
    }
    for (path, anchors) in path_anchors {
        map.insert(path, AffinityEntry { ts: now_ms, anchors });
    }
    if map.len() > 50 {
        let mut entries: Vec<(String, AffinityEntry)> = map.into_iter().collect();
        entries.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
        entries.truncate(50);
        map = entries.into_iter().collect();
    }
    let raw = serde_json::to_string_pretty(&map)?;
    std::fs::write(&affinity_path, raw)?;
    Ok(())
}

/// Extract the first non-empty, non-comment line from a snippet as an anchor.
fn affinity_anchor_line(text: &str) -> Option<String> {
    for raw_line in text.lines() {
        let line: String = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
            || trimmed.starts_with("*/")
        {
            continue;
        }
        let anchor = if trimmed.len() > 80 {
            format!("{}...", &trimmed[..77])
        } else {
            trimmed.to_string()
        };
        return Some(anchor);
    }
    None
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

