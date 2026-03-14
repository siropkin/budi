use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_chunk_language() -> String {
    "unknown".to_string()
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub repo_root: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub retrieval_mode: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// When true, return all fused candidates with raw channel scores in diagnostics.
    #[serde(default)]
    pub dump_candidates: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefetchRequest {
    pub repo_root: String,
    pub file_path: String,
    pub session_id: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefetchResponse {
    pub context: String,
    pub neighbor_paths: Vec<String>,
    pub skipped: bool,
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
    #[serde(default = "default_chunk_language")]
    pub language: String,
    pub score: f32,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub channel_scores: QueryChannelScores,
    pub text: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "slm_relevance_note"
    )]
    pub context_note: Option<String>,
    /// Caller symbol names (populated by daemon from call graph).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<String>,
    /// Callee/ref symbol names (populated by daemon from call graph).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetRef {
    pub path: String,
    pub score: f32,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default = "default_chunk_language")]
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub total_candidates: usize,
    pub context: String,
    pub snippets: Vec<QueryResultItem>,
    #[serde(default)]
    pub diagnostics: QueryDiagnostics,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_graph_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_ms: Option<HashMap<String, u64>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub snippet_refs: Vec<SnippetRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryDiagnostics {
    pub intent: String,
    pub confidence: f32,
    pub top_score: f32,
    pub margin: f32,
    pub signals: Vec<String>,
    #[serde(default)]
    pub top_language: Option<String>,
    #[serde(default)]
    pub snippet_languages: Vec<String>,
    #[serde(default)]
    pub repo_ecosystems: Vec<String>,
    #[serde(default)]
    pub top_ecosystem: Option<String>,
    #[serde(default)]
    pub snippet_ecosystems: Vec<String>,
    pub recommended_injection: bool,
    pub skip_reason: Option<String>,
    /// Number of snippets removed by session deduplication.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dedup_count: usize,
    /// All fused candidates with raw channel scores (only populated when dump_candidates=true).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<DiagnosticCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCandidate {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub fused_score: f32,
    pub channel_scores: QueryChannelScores,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_hint: Option<String>,
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
    pub updates_noop: u64,
    #[serde(default)]
    pub updates_applied: u64,
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
