use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use anyhow::Result;

use crate::config::{self, BudiConfig, CLAUDE_LOCAL_SETTINGS};
use crate::rpc::{StatusRequest, StatusResponse};

const SESSION_TTL_SECS: u64 = 1800;

#[derive(Debug, Default)]
struct SessionState {
    last_activity: Option<Instant>,
    queries: u64,
    skips: u64,
}

/// Per-session stats snapshot returned by the `/session-stats` endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionStatsSnapshot {
    pub queries: u64,
    pub skips: u64,
}

/// Lightweight query counters for visibility into daemon activity.
#[derive(Default)]
pub struct QueryStats {
    pub queries: u64,
    pub skips: u64,
}

#[derive(Clone, Default)]
pub struct DaemonState {
    sessions: std::sync::Arc<StdMutex<HashMap<String, SessionState>>>,
    query_stats: std::sync::Arc<StdMutex<QueryStats>>,
    repo_query_stats: std::sync::Arc<StdMutex<HashMap<String, QueryStats>>>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_prompt(&self, repo_root: &str, session_id: Option<&str>, skipped: bool) {
        {
            let mut stats = self.query_stats.lock().unwrap();
            stats.queries += 1;
            if skipped {
                stats.skips += 1;
            }
        }
        {
            let mut repo_stats = self.repo_query_stats.lock().unwrap();
            let rs = repo_stats.entry(repo_root.to_string()).or_default();
            rs.queries += 1;
            if skipped {
                rs.skips += 1;
            }
        }
        if let Some(sid) = session_id {
            let mut guard = self.sessions_guard();
            let session = guard.entry(sid.to_string()).or_default();
            session.last_activity = Some(Instant::now());
            session.queries += 1;
            if skipped {
                session.skips += 1;
            }
            // Lazy TTL cleanup.
            guard.retain(|_, v| {
                v.last_activity
                    .map(|t| t.elapsed().as_secs() < SESSION_TTL_SECS)
                    .unwrap_or(true)
            });
        }
    }

    pub fn status(&self, request: StatusRequest, _config: &BudiConfig) -> Result<StatusResponse> {
        let repo_root = Path::new(&request.repo_root);
        let hooks_detected = detect_hooks(repo_root);
        Ok(StatusResponse {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_root: request.repo_root,
            hooks_detected,
        })
    }

    /// Returns a snapshot of query activity counters.
    pub fn query_stats_snapshot(&self) -> (u64, u64) {
        let stats = self.query_stats.lock().unwrap();
        (stats.queries, stats.skips)
    }

    /// Returns a snapshot of per-repo query activity counters.
    pub fn repo_stats_snapshot(&self, repo_root: &str) -> Option<(u64, u64)> {
        let guard = self.repo_query_stats.lock().unwrap();
        guard.get(repo_root).map(|s| (s.queries, s.skips))
    }

    /// Returns per-session stats for a given session_id.
    pub fn session_stats(&self, session_id: &str) -> Option<SessionStatsSnapshot> {
        let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.get(session_id).map(|s| SessionStatsSnapshot {
            queries: s.queries,
            skips: s.skips,
        })
    }

    fn sessions_guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionState>> {
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn detect_hooks(repo_root: &Path) -> bool {
    let settings_path = repo_root.join(CLAUDE_LOCAL_SETTINGS);
    let Ok(raw) = std::fs::read_to_string(settings_path) else {
        return false;
    };
    raw.contains("UserPromptSubmit")
        && (raw.contains("/hook/prompt-submit") || raw.contains("budi hook user-prompt-submit"))
}

pub fn resolve_repo_root(input_repo_root: Option<String>, cwd: &Path) -> Result<String> {
    if let Some(root) = input_repo_root {
        return Ok(root);
    }
    Ok(config::find_repo_root(cwd)?.display().to_string())
}
