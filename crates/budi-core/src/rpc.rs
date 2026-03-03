use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub repo_root: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub retrieval_mode: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QueryChannelScores {
    #[serde(default)]
    pub lexical: f32,
    #[serde(default)]
    pub vector: f32,
    #[serde(default)]
    pub symbol: f32,
    #[serde(default)]
    pub path: f32,
    #[serde(default)]
    pub graph: f32,
    #[serde(default)]
    pub rerank: f32,
}

impl Default for QueryChannelScores {
    fn default() -> Self {
        Self {
            lexical: 0.0,
            vector: 0.0,
            symbol: 0.0,
            path: 0.0,
            graph: 0.0,
            rerank: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResultItem {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub channel_scores: QueryChannelScores,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
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
    #[serde(default)]
    pub include_extensions: Vec<String>,
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResponse {
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    #[serde(default)]
    pub embedded_chunks: usize,
    #[serde(default)]
    pub missing_embeddings: usize,
    #[serde(default)]
    pub repaired_embeddings: usize,
    #[serde(default)]
    pub invalid_embeddings: usize,
    pub changed_files: usize,
    #[serde(default = "default_index_status")]
    pub index_status: String,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub job_state: String,
    #[serde(default)]
    pub terminal_outcome: Option<String>,
}

fn default_index_status() -> String {
    "completed".to_string()
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
    pub state: String,
    #[serde(default)]
    pub phase: String,
    pub total_files: usize,
    pub processed_files: usize,
    pub changed_files: usize,
    pub current_file: Option<String>,
    pub started_at_unix_ms: u128,
    pub last_update_unix_ms: u128,
    pub last_error: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub job_state: String,
    #[serde(default)]
    pub terminal_outcome: Option<String>,
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
    pub tracked_files: usize,
    #[serde(default)]
    pub indexed_chunks: usize,
    pub embedded_chunks: usize,
    #[serde(default)]
    pub missing_embeddings: usize,
    #[serde(default)]
    pub invalid_embeddings: usize,
    pub hooks_detected: bool,
    #[serde(default)]
    pub update_retries: u64,
    #[serde(default)]
    pub update_failures: u64,
    #[serde(default)]
    pub index_state: String,
    #[serde(default)]
    pub index_job_id: Option<String>,
    #[serde(default)]
    pub index_job_state: String,
    #[serde(default)]
    pub index_terminal_outcome: Option<String>,
    #[serde(default)]
    pub watch_events_seen: u64,
    #[serde(default)]
    pub watch_events_accepted: u64,
    #[serde(default)]
    pub watch_events_dropped: u64,
}
