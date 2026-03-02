use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub repo_root: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResultItem {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub reason: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub branch: String,
    pub head: String,
    pub total_candidates: usize,
    pub context: String,
    pub snippets: Vec<QueryResultItem>,
    #[serde(default)]
    pub diagnostics: QueryDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryDiagnostics {
    pub intent: String,
    pub confidence: f32,
    pub top_score: f32,
    pub margin: f32,
    pub signals: Vec<String>,
    pub recommended_injection: bool,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRequest {
    pub repo_root: String,
    #[serde(default)]
    pub hard: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResponse {
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub changed_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexProgressRequest {
    pub repo_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexProgressResponse {
    pub repo_root: String,
    pub active: bool,
    pub hard: bool,
    #[serde(default)]
    pub phase: String,
    pub total_files: usize,
    pub processed_files: usize,
    pub changed_files: usize,
    pub current_file: Option<String>,
    pub started_at_unix_ms: u128,
    pub last_update_unix_ms: u128,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub repo_root: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequest {
    pub repo_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub daemon_version: String,
    pub repo_root: String,
    pub branch: String,
    pub head: String,
    pub tracked_files: usize,
    pub embedded_chunks: usize,
    pub dirty_files: usize,
    pub hooks_detected: bool,
}
