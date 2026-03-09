use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use hnsw_rs::prelude::*;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    FAST, INDEXED, STORED, STRING, Schema, SchemaBuilder, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term, doc};
use tracing::{info, warn};

use crate::chunking::chunk_text;
use crate::config::{self, BudiConfig};
use crate::index_scope::{
    build_basename_allowlist, build_extension_allowlist, is_always_skipped_dir_name,
    is_supported_code_file,
};

const SQLITE_CHUNK_ID_MAX: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub hash: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub modified_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRecord {
    pub id: u64,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub symbol_hint: Option<String>,
    pub text: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoIndexState {
    pub repo_root: String,
    pub files: Vec<FileRecord>,
    pub chunks: Vec<ChunkRecord>,
    pub updated_at_ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildReport {
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub embedded_chunks: usize,
    pub missing_embeddings: usize,
    pub repaired_embeddings: usize,
    pub invalid_embeddings: usize,
    pub changed_files: usize,
    pub limit_reached: bool,
}

pub struct RuntimeIndex {
    pub state: RepoIndexState,
    id_to_chunk: HashMap<u64, ChunkRecord>,
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    tantivy: TantivyBundle,
    symbol_to_chunk_ids: HashMap<String, Vec<u64>>,
    symbol_family_to_tokens: FamilyTokenLookup,
    symbol_family_prefix_to_families: FamilyPrefixLookup,
    path_token_to_chunk_ids: HashMap<String, Vec<u64>>,
    graph_token_to_chunk_ids: HashMap<String, Vec<u64>>,
    graph_family_to_tokens: FamilyTokenLookup,
    graph_family_prefix_to_families: FamilyPrefixLookup,
    doc_like_chunk_ids: HashSet<u64>,
    /// Forward index: chunk ID → resolved callee tokens (symbols this chunk calls)
    chunk_to_graph_tokens: HashMap<u64, Vec<String>>,
}

impl RuntimeIndex {
    pub fn from_state(repo_root: &Path, state: RepoIndexState) -> Result<Self> {
        let mut id_to_chunk = HashMap::new();
        for chunk in &state.chunks {
            id_to_chunk.insert(chunk.id, chunk.clone());
        }
        let hnsw = build_hnsw(&state.chunks)?;
        let tantivy = TantivyBundle::open_or_rebuild(repo_root, &state.chunks)?;
        let (
            symbol_to_chunk_ids,
            symbol_family_to_tokens,
            symbol_family_prefix_to_families,
            path_token_to_chunk_ids,
            graph_token_to_chunk_ids,
            graph_family_to_tokens,
            graph_family_prefix_to_families,
            doc_like_chunk_ids,
            chunk_to_graph_tokens,
        ) = build_retrieval_signal_indexes(repo_root, &state.chunks);
        Ok(Self {
            state,
            id_to_chunk,
            hnsw,
            tantivy,
            symbol_to_chunk_ids,
            symbol_family_to_tokens,
            symbol_family_prefix_to_families,
            path_token_to_chunk_ids,
            graph_token_to_chunk_ids,
            graph_family_to_tokens,
            graph_family_prefix_to_families,
            doc_like_chunk_ids,
            chunk_to_graph_tokens,
        })
    }

    pub fn chunk(&self, id: u64) -> Option<&ChunkRecord> {
        self.id_to_chunk.get(&id)
    }

    /// Find the next chunk in the same file after `after_start_line` — the chunk with the
    /// smallest `start_line > after_start_line`. Used by Phase AR to inject the continuation
    /// of a long function body. Works correctly with overlapping chunks (overlap=20, stride=60):
    /// given a chunk at 961-1040, the next chunk starts at 1021 (not 1041).
    pub fn adjacent_chunk(&self, path: &str, after_start_line: usize) -> Option<u64> {
        self.id_to_chunk
            .iter()
            .filter(|(_, c)| c.path == path && c.start_line > after_start_line)
            .min_by_key(|(_, c)| c.start_line)
            .map(|(id, _)| *id)
    }

    pub fn search_lexical(&self, query: &str, limit: usize) -> Result<Vec<(u64, f32)>> {
        self.tantivy.search(query, limit)
    }

    pub fn search_vector(&self, query_embedding: &[f32], limit: usize) -> Vec<(u64, f32)> {
        if query_embedding.is_empty() {
            return Vec::new();
        }
        let Some(index) = &self.hnsw else {
            return Vec::new();
        };
        let ef_search = limit.max(32);
        let neighbors = index.search(query_embedding, limit, ef_search);
        neighbors
            .into_iter()
            .map(|n| {
                // hnsw_rs distance is cosine-distance style. Convert to score where higher is better.
                let score = 1.0f32 - n.distance;
                (n.d_id as u64, score)
            })
            .collect()
    }

    pub fn search_symbol_tokens(&self, query_tokens: &[String], limit: usize) -> Vec<(u64, f32)> {
        score_from_token_map_with_family_lookup(
            &self.symbol_to_chunk_ids,
            Some(&self.symbol_family_to_tokens),
            Some(&self.symbol_family_prefix_to_families),
            query_tokens,
            limit,
            true,
        )
    }

    pub fn search_path_tokens(&self, query_tokens: &[String], limit: usize) -> Vec<(u64, f32)> {
        score_from_token_map_with_family_lookup(
            &self.path_token_to_chunk_ids,
            None,
            None,
            query_tokens,
            limit,
            false,
        )
    }

    pub fn search_graph_tokens(&self, query_tokens: &[String], limit: usize) -> Vec<(u64, f32)> {
        score_from_token_map_with_family_lookup(
            &self.graph_token_to_chunk_ids,
            Some(&self.graph_family_to_tokens),
            Some(&self.graph_family_prefix_to_families),
            query_tokens,
            limit,
            true,
        )
    }

    pub fn is_doc_like_chunk(&self, chunk_id: u64) -> bool {
        self.doc_like_chunk_ids.contains(&chunk_id)
    }

    pub fn all_chunks(&self) -> &[ChunkRecord] {
        &self.state.chunks
    }

    /// Returns chunks that call/reference `symbol`, deduplicated by file path.
    pub fn callers_of(&self, symbol: &str) -> Vec<&ChunkRecord> {
        let Some(ids) = self.graph_token_to_chunk_ids.get(symbol) else {
            return Vec::new();
        };
        let mut seen_paths = std::collections::HashSet::new();
        ids.iter()
            .filter_map(|id| self.id_to_chunk.get(id))
            .filter(|chunk| seen_paths.insert(chunk.path.clone()))
            .collect()
    }

    /// Returns callee symbol names that `chunk_id` calls.
    pub fn callees_of(&self, chunk_id: u64) -> Vec<String> {
        let Some(tokens) = self.chunk_to_graph_tokens.get(&chunk_id) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        tokens
            .iter()
            .filter(|t| seen.insert((*t).clone()))
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
pub struct IndexWorkspace {
    pub state: RepoIndexState,
    pub report: IndexBuildReport,
}

type RetrievalSignalIndexes = (
    HashMap<String, Vec<u64>>,
    FamilyTokenLookup,
    FamilyPrefixLookup,
    HashMap<String, Vec<u64>>,
    HashMap<String, Vec<u64>>,
    FamilyTokenLookup,
    FamilyPrefixLookup,
    HashSet<u64>,
    HashMap<u64, Vec<String>>,
);
type FamilyTokenLookup = HashMap<String, Vec<String>>;
type FamilyPrefixLookup = HashMap<String, Vec<String>>;

#[derive(Debug, Clone, Default)]
pub struct PersistedIndexProgress {
    pub active: bool,
    pub hard: bool,
    pub state: String,
    pub phase: String,
    pub total_files: usize,
    pub processed_files: usize,
    pub changed_files: usize,
    pub current_file: Option<String>,
    pub started_at_unix_ms: u128,
    pub last_update_unix_ms: u128,
    pub last_error: Option<String>,
    pub job_id: Option<String>,
    pub job_state: String,
    pub terminal_outcome: Option<String>,
}

#[derive(Debug)]
struct PendingChunk {
    id: u64,
    path: String,
    start_line: usize,
    end_line: usize,
    symbol_hint: Option<String>,
    text: String,
    embedding_cache_key: String,
    embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EmbeddingCacheState {
    #[serde(default)]
    entries: HashMap<String, Vec<f32>>,
    #[serde(default)]
    updated_at_ts: i64,
}

#[derive(Debug, Clone)]
pub struct IndexBuildProgress {
    pub phase: String,
    pub total_files: usize,
    pub processed_files: usize,
    pub changed_files: usize,
    pub current_file: Option<String>,
    pub done: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IndexBuildOptions {
    pub include_extensions: Vec<String>,
    pub ignore_patterns: Vec<String>,
}

pub fn build_or_update(
    repo_root: &Path,
    config: &BudiConfig,
    hard: bool,
    changed_hint: Option<&[String]>,
    options: Option<&IndexBuildOptions>,
    mut progress_cb: Option<&mut dyn FnMut(IndexBuildProgress)>,
) -> Result<IndexWorkspace> {
    let previous = load_state(repo_root)?.unwrap_or_default();
    let previous_files_by_path: HashMap<String, FileRecord> = previous
        .files
        .iter()
        .map(|file| (file.path.clone(), file.clone()))
        .collect();
    let previous_hashes: HashMap<String, String> = previous
        .files
        .iter()
        .map(|f| (f.path.clone(), f.hash.clone()))
        .collect();
    let previous_chunks_by_path: HashMap<String, Vec<ChunkRecord>> =
        group_chunks_by_path(&previous.chunks);

    let hinted_paths = collect_hint_paths(repo_root, changed_hint);
    let use_metadata_delta = should_use_metadata_delta(
        repo_root,
        hard,
        &hinted_paths,
        &previous_files_by_path,
        &previous_chunks_by_path,
    );
    let (current_files, current_hashes) = if use_metadata_delta {
        build_current_files_from_metadata_delta(
            repo_root,
            config,
            &previous_files_by_path,
            &hinted_paths,
            options,
        )?
    } else {
        build_current_files_from_discovery(
            repo_root,
            config,
            &previous_files_by_path,
            hard,
            &hinted_paths,
            options,
        )?
    };
    let mut current_files = current_files;
    let mut current_hashes = current_hashes;
    let mut limit_reached = false;
    if current_files.len() > config.max_index_files {
        current_files.truncate(config.max_index_files);
        let retained_paths = current_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<HashSet<_>>();
        current_hashes.retain(|path, _| retained_paths.contains(path));
        limit_reached = true;
        info!(
            "Index file budget reached (max_index_files={}): truncating scan set.",
            config.max_index_files
        );
    }

    let changed_set = calculate_changed_set(hard, &hinted_paths, &previous_hashes, &current_hashes);

    let total_files_to_process = current_files
        .iter()
        .filter(|file| {
            hard || changed_set.contains(&file.path)
                || !previous_chunks_by_path.contains_key(&file.path)
        })
        .count();

    let deleted_files = count_deleted_paths(&changed_set, &current_hashes);
    let mut embedder = EmbeddingEngine::new();
    let previous_embeddings_by_fingerprint: HashMap<[u8; 32], Vec<f32>> = previous
        .chunks
        .iter()
        .filter(|chunk| !chunk.embedding.is_empty())
        .map(|chunk| {
            (
                chunk_fingerprint(&chunk.path, chunk.start_line, chunk.end_line, &chunk.text),
                chunk.embedding.clone(),
            )
        })
        .collect();
    let mut embedding_cache = load_embedding_cache_or_default();
    let mut embedding_cache_dirty = false;
    let mut used_chunk_ids = HashSet::new();
    let mut chunks = Vec::new();
    let mut changed_files = deleted_files;
    let mut processed_files = 0usize;
    let embedding_batch_size = config.embedding_batch_size.max(1);
    let embedding_retry_attempts = config.embedding_retry_attempts.max(1);
    let embedding_retry_backoff_ms = config.embedding_retry_backoff_ms.max(1);
    let mut pending_chunks: Vec<PendingChunk> = Vec::new();
    let mut missing_embedding_queue: Vec<(usize, String)> = Vec::new();
    let mut repaired_embeddings = 0usize;

    emit_progress(
        &mut progress_cb,
        IndexBuildProgress {
            phase: "scanning-files".to_string(),
            total_files: total_files_to_process,
            processed_files,
            changed_files,
            current_file: None,
            done: false,
        },
    );

    for file in &current_files {
        if chunks.len() + pending_chunks.len() >= config.max_index_chunks {
            limit_reached = true;
            info!(
                "Index chunk budget reached (max_index_chunks={}): stopping chunk ingestion.",
                config.max_index_chunks
            );
            break;
        }
        let should_process = hard
            || changed_set.contains(&file.path)
            || !previous_chunks_by_path.contains_key(&file.path);
        if !should_process {
            if let Some(existing) = previous_chunks_by_path.get(&file.path) {
                let remaining = config
                    .max_index_chunks
                    .saturating_sub(chunks.len() + pending_chunks.len());
                if remaining == 0 {
                    limit_reached = true;
                    break;
                }
                for chunk in existing {
                    if chunks.len() + pending_chunks.len() >= config.max_index_chunks {
                        limit_reached = true;
                        break;
                    }
                    used_chunk_ids.insert(chunk.id);
                    chunks.push(chunk.clone());
                }
                if limit_reached {
                    break;
                }
            }
            continue;
        }

        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "embedding-chunks".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: Some(file.path.clone()),
                done: false,
            },
        );

        changed_files += 1;
        let absolute = repo_root.join(&file.path);
        let content = match fs::read_to_string(&absolute) {
            Ok(raw) => raw,
            Err(err) => {
                warn!("Skipping unreadable file {}: {}", file.path, err);
                processed_files += 1;
                emit_progress(
                    &mut progress_cb,
                    IndexBuildProgress {
                        phase: "embedding-chunks".to_string(),
                        total_files: total_files_to_process,
                        processed_files,
                        changed_files,
                        current_file: None,
                        done: false,
                    },
                );
                continue;
            }
        };
        let chunked = chunk_text(
            &file.path,
            &content,
            config.chunk_lines,
            config.chunk_overlap,
        );
        if chunked.is_empty() {
            processed_files += 1;
            emit_progress(
                &mut progress_cb,
                IndexBuildProgress {
                    phase: "embedding-chunks".to_string(),
                    total_files: total_files_to_process,
                    processed_files,
                    changed_files,
                    current_file: None,
                    done: false,
                },
            );
            continue;
        }
        for chunk in chunked {
            if chunks.len() + pending_chunks.len() >= config.max_index_chunks {
                limit_reached = true;
                break;
            }
            let fingerprint =
                chunk_fingerprint(&file.path, chunk.start_line, chunk.end_line, &chunk.text);
            let id = allocate_chunk_id(&fingerprint, &mut used_chunk_ids);
            let embedding_cache_key = embedding_content_hash(&chunk.text);
            let embedding = previous_embeddings_by_fingerprint
                .get(&fingerprint)
                .cloned()
                .or_else(|| embedding_cache.entries.get(&embedding_cache_key).cloned());
            let pending_index = pending_chunks.len();
            if embedding.is_none() {
                missing_embedding_queue.push((pending_index, format!("passage: {}", chunk.text)));
            }
            pending_chunks.push(PendingChunk {
                id,
                path: file.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol_hint: chunk.symbol_hint,
                text: chunk.text,
                embedding_cache_key,
                embedding,
            });
        }
        if limit_reached {
            break;
        }

        if missing_embedding_queue.len() >= embedding_batch_size {
            emit_progress(
                &mut progress_cb,
                IndexBuildProgress {
                    phase: "embedding-batch".to_string(),
                    total_files: total_files_to_process,
                    processed_files,
                    changed_files,
                    current_file: None,
                    done: false,
                },
            );
            flush_missing_embedding_queue(
                &mut embedder,
                &mut pending_chunks,
                &mut missing_embedding_queue,
                &mut embedding_cache,
                &mut embedding_cache_dirty,
                embedding_retry_attempts,
                embedding_retry_backoff_ms,
            )?;
        }

        processed_files += 1;
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "embedding-chunks".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        if limit_reached {
            break;
        }
    }

    if !missing_embedding_queue.is_empty() {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "embedding-batch".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        flush_missing_embedding_queue(
            &mut embedder,
            &mut pending_chunks,
            &mut missing_embedding_queue,
            &mut embedding_cache,
            &mut embedding_cache_dirty,
            embedding_retry_attempts,
            embedding_retry_backoff_ms,
        )?;
    }

    if !pending_chunks.is_empty() {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "finalizing-chunks".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
    }
    for chunk in pending_chunks {
        if chunks.len() >= config.max_index_chunks {
            limit_reached = true;
            break;
        }
        let embedding = chunk.embedding.unwrap_or_default();
        if !embedding.is_empty()
            && embedding_cache
                .entries
                .insert(chunk.embedding_cache_key.clone(), embedding.clone())
                .is_none()
        {
            embedding_cache_dirty = true;
        }
        chunks.push(ChunkRecord {
            id: chunk.id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            symbol_hint: chunk.symbol_hint,
            text: chunk.text,
            embedding,
        });
    }

    if embedder.is_available() {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "reconciling-embeddings".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        repaired_embeddings = reconcile_missing_chunk_embeddings(
            &mut chunks,
            &mut embedder,
            &mut embedding_cache,
            &mut embedding_cache_dirty,
            embedding_batch_size,
            embedding_retry_attempts,
            embedding_retry_backoff_ms,
        )?;
    }

    let mut invalid_embeddings = sanitize_chunk_embeddings(&mut chunks);
    if invalid_embeddings > 0 && embedder.is_available() {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "repairing-invalid-embeddings".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        repaired_embeddings =
            repaired_embeddings.saturating_add(reconcile_missing_chunk_embeddings(
                &mut chunks,
                &mut embedder,
                &mut embedding_cache,
                &mut embedding_cache_dirty,
                embedding_batch_size,
                embedding_retry_attempts,
                embedding_retry_backoff_ms,
            )?);
        invalid_embeddings = sanitize_chunk_embeddings(&mut chunks);
    }

    let missing_embeddings = chunks
        .iter()
        .filter(|chunk| chunk.embedding.is_empty())
        .count();
    let embedded_chunks = chunks.len().saturating_sub(missing_embeddings);

    if embedding_cache_dirty {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "saving-embedding-cache".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        embedding_cache.updated_at_ts = Utc::now().timestamp();
        if let Err(err) = save_embedding_cache(&embedding_cache) {
            warn!("Failed saving global embedding cache: {:#}", err);
        }
    }

    chunks.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));

    let state = RepoIndexState {
        repo_root: repo_root.display().to_string(),
        files: current_files,
        chunks,
        updated_at_ts: Utc::now().timestamp(),
    };
    let incremental_noop = !hard && changed_files == 0;
    if incremental_noop {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "noop-skip-write".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
    } else {
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: "saving-state".to_string(),
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        if hard {
            save_state(repo_root, &state, None)?;
        } else {
            save_state(repo_root, &state, Some(&changed_set))?;
        }
        emit_progress(
            &mut progress_cb,
            IndexBuildProgress {
                phase: if hard {
                    "rebuilding-lexical-index".to_string()
                } else {
                    "updating-lexical-index".to_string()
                },
                total_files: total_files_to_process,
                processed_files,
                changed_files,
                current_file: None,
                done: false,
            },
        );
        if hard {
            TantivyBundle::rebuild(repo_root, &state.chunks)?;
        } else {
            TantivyBundle::apply_delta(repo_root, &state.chunks, &changed_set)?;
        }
    }

    let report = IndexBuildReport {
        indexed_files: state.files.len(),
        indexed_chunks: state.chunks.len(),
        embedded_chunks,
        missing_embeddings,
        repaired_embeddings,
        invalid_embeddings,
        changed_files,
        limit_reached,
    };
    emit_progress(
        &mut progress_cb,
        IndexBuildProgress {
            phase: "complete".to_string(),
            total_files: total_files_to_process,
            processed_files,
            changed_files,
            current_file: None,
            done: true,
        },
    );
    Ok(IndexWorkspace { state, report })
}

pub fn load_state(repo_root: &Path) -> Result<Option<RepoIndexState>> {
    let path = config::index_db_path(repo_root)?;
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_index_db(repo_root)?;
    ensure_index_db_schema(&conn)?;

    let file_count: i64 = conn.query_row("SELECT COUNT(1) FROM files", [], |row| row.get(0))?;
    let chunk_count: i64 = conn.query_row("SELECT COUNT(1) FROM chunks", [], |row| row.get(0))?;
    if file_count == 0 && chunk_count == 0 {
        return Ok(None);
    }

    let mut files = Vec::new();
    let mut file_stmt =
        conn.prepare("SELECT path, hash, size_bytes, modified_unix_ms FROM files ORDER BY path")?;
    let file_rows = file_stmt.query_map([], |row| {
        let size_bytes_i64: i64 = row.get(2)?;
        let modified_unix_ms_i64: i64 = row.get(3)?;
        Ok(FileRecord {
            path: row.get(0)?,
            hash: row.get(1)?,
            size_bytes: u64::try_from(size_bytes_i64).unwrap_or_default(),
            modified_unix_ms: u64::try_from(modified_unix_ms_i64).unwrap_or_default(),
        })
    })?;
    for row in file_rows {
        files.push(row?);
    }

    let mut chunks = Vec::new();
    let mut chunk_stmt = conn.prepare(
        "SELECT id, path, start_line, end_line, symbol_hint, text, embedding
         FROM chunks
         ORDER BY path, start_line, id",
    )?;
    let chunk_rows = chunk_stmt.query_map([], |row| {
        let id_i64: i64 = row.get(0)?;
        let start_line_i64: i64 = row.get(2)?;
        let end_line_i64: i64 = row.get(3)?;
        let path: String = row.get(1)?;
        let text: String = row.get(5)?;
        let embedding_blob: Vec<u8> = row.get(6)?;
        let embedding = decode_embedding(&embedding_blob).unwrap_or_else(|| {
            warn!(
                "Invalid embedding payload for chunk id={} path={}, using lexical-only fallback",
                id_i64, path
            );
            Vec::new()
        });
        Ok(ChunkRecord {
            id: u64::try_from(id_i64).unwrap_or_default(),
            path,
            start_line: usize::try_from(start_line_i64).unwrap_or_default(),
            end_line: usize::try_from(end_line_i64).unwrap_or_default(),
            symbol_hint: row.get(4)?,
            text,
            embedding,
        })
    })?;
    for row in chunk_rows {
        chunks.push(row?);
    }

    let repo_root_value =
        load_meta_value(&conn, "repo_root")?.unwrap_or_else(|| repo_root.display().to_string());
    let updated_at_ts = load_meta_value(&conn, "updated_at_ts")?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();

    Ok(Some(RepoIndexState {
        repo_root: repo_root_value,
        files,
        chunks,
        updated_at_ts,
    }))
}

pub fn save_state(
    repo_root: &Path,
    state: &RepoIndexState,
    delta_paths: Option<&HashSet<String>>,
) -> Result<()> {
    let path = config::index_db_path(repo_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }

    let mut conn = open_index_db(repo_root)?;
    ensure_index_db_schema(&conn)?;
    let tx = conn.transaction()?;
    upsert_meta_value(&tx, "repo_root", &state.repo_root)?;
    upsert_meta_value(&tx, "updated_at_ts", &state.updated_at_ts.to_string())?;

    if let Some(delta_paths) = delta_paths {
        if !delta_paths.is_empty() {
            let mut sorted_paths = delta_paths.iter().cloned().collect::<Vec<_>>();
            sorted_paths.sort();

            {
                let mut delete_files = tx.prepare("DELETE FROM files WHERE path = ?1")?;
                let mut delete_chunks = tx.prepare("DELETE FROM chunks WHERE path = ?1")?;
                for path in &sorted_paths {
                    delete_files.execute(params![path])?;
                    delete_chunks.execute(params![path])?;
                }
            }

            {
                let mut file_stmt = tx.prepare(
                    "INSERT INTO files(path, hash, size_bytes, modified_unix_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(path) DO UPDATE SET
                       hash = excluded.hash,
                       size_bytes = excluded.size_bytes,
                       modified_unix_ms = excluded.modified_unix_ms",
                )?;
                for file in state
                    .files
                    .iter()
                    .filter(|file| delta_paths.contains(&file.path))
                {
                    file_stmt.execute(params![
                        &file.path,
                        &file.hash,
                        i64::try_from(file.size_bytes).unwrap_or(i64::MAX),
                        i64::try_from(file.modified_unix_ms).unwrap_or(i64::MAX),
                    ])?;
                }
            }

            {
                let mut chunk_stmt = tx.prepare(
                    "INSERT INTO chunks(id, path, start_line, end_line, symbol_hint, text, embedding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(id) DO UPDATE SET
                       path = excluded.path,
                       start_line = excluded.start_line,
                       end_line = excluded.end_line,
                       symbol_hint = excluded.symbol_hint,
                       text = excluded.text,
                       embedding = excluded.embedding",
                )?;
                for chunk in state
                    .chunks
                    .iter()
                    .filter(|chunk| delta_paths.contains(&chunk.path))
                {
                    chunk_stmt.execute(params![
                        chunk_id_to_sql(chunk.id),
                        &chunk.path,
                        i64::try_from(chunk.start_line).unwrap_or(i64::MAX),
                        i64::try_from(chunk.end_line).unwrap_or(i64::MAX),
                        chunk.symbol_hint.as_deref(),
                        &chunk.text,
                        encode_embedding(&chunk.embedding),
                    ])?;
                }
            }
        }
    } else {
        tx.execute("DELETE FROM files", [])?;
        tx.execute("DELETE FROM chunks", [])?;

        {
            let mut file_stmt = tx.prepare(
                "INSERT INTO files(path, hash, size_bytes, modified_unix_ms)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for file in &state.files {
                file_stmt.execute(params![
                    &file.path,
                    &file.hash,
                    i64::try_from(file.size_bytes).unwrap_or(i64::MAX),
                    i64::try_from(file.modified_unix_ms).unwrap_or(i64::MAX),
                ])?;
            }
        }

        {
            let mut chunk_stmt = tx.prepare(
                "INSERT INTO chunks(id, path, start_line, end_line, symbol_hint, text, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for chunk in &state.chunks {
                chunk_stmt.execute(params![
                    chunk_id_to_sql(chunk.id),
                    &chunk.path,
                    i64::try_from(chunk.start_line).unwrap_or(i64::MAX),
                    i64::try_from(chunk.end_line).unwrap_or(i64::MAX),
                    chunk.symbol_hint.as_deref(),
                    &chunk.text,
                    encode_embedding(&chunk.embedding),
                ])?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

fn upsert_meta_value(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn open_index_db(repo_root: &Path) -> Result<Connection> {
    let path = config::index_db_path(repo_root)?;
    let conn = Connection::open(&path)
        .with_context(|| format!("Failed opening index database {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;
    Ok(conn)
}

fn ensure_index_db_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            hash TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            modified_unix_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS chunks (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            symbol_hint TEXT,
            text TEXT NOT NULL,
            embedding BLOB NOT NULL
        );

        CREATE TABLE IF NOT EXISTS index_progress (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            active INTEGER NOT NULL,
            hard INTEGER NOT NULL,
            state TEXT NOT NULL,
            phase TEXT NOT NULL,
            total_files INTEGER NOT NULL,
            processed_files INTEGER NOT NULL,
            changed_files INTEGER NOT NULL,
            current_file TEXT,
            started_at_unix_ms TEXT NOT NULL,
            last_update_unix_ms TEXT NOT NULL,
            last_error TEXT,
            job_id TEXT,
            job_state TEXT NOT NULL DEFAULT 'idle',
            terminal_outcome TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
        ",
    )?;
    if chunks_table_has_legacy_timestamp(conn)? {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS chunks_legacy;
            ALTER TABLE chunks RENAME TO chunks_legacy;
            CREATE TABLE chunks (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                symbol_hint TEXT,
                text TEXT NOT NULL,
                embedding BLOB NOT NULL
            );
            INSERT INTO chunks(id, path, start_line, end_line, symbol_hint, text, embedding)
            SELECT id, path, start_line, end_line, symbol_hint, text, embedding
            FROM chunks_legacy;
            DROP TABLE chunks_legacy;
            CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
            ",
        )?;
    }
    ensure_index_progress_columns(conn)?;
    Ok(())
}

fn ensure_index_progress_columns(conn: &Connection) -> Result<()> {
    if !table_has_column(conn, "index_progress", "job_id")? {
        conn.execute("ALTER TABLE index_progress ADD COLUMN job_id TEXT", [])?;
    }
    if !table_has_column(conn, "index_progress", "job_state")? {
        conn.execute(
            "ALTER TABLE index_progress ADD COLUMN job_state TEXT NOT NULL DEFAULT 'idle'",
            [],
        )?;
    }
    if !table_has_column(conn, "index_progress", "terminal_outcome")? {
        conn.execute(
            "ALTER TABLE index_progress ADD COLUMN terminal_outcome TEXT",
            [],
        )?;
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

fn chunks_table_has_legacy_timestamp(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(chunks)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "updated_at_ts" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_meta_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(value)
}

pub fn load_index_progress_snapshot(repo_root: &Path) -> Result<Option<PersistedIndexProgress>> {
    let path = config::index_db_path(repo_root)?;
    if !path.exists() {
        return Ok(None);
    }
    let conn = open_index_db(repo_root)?;
    ensure_index_db_schema(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT active, hard, state, phase, total_files, processed_files, changed_files,
                current_file, started_at_unix_ms, last_update_unix_ms, last_error,
                job_id, job_state, terminal_outcome
         FROM index_progress
         WHERE id = 1",
    )?;
    let snapshot = stmt
        .query_row([], |row| {
            let total_files_i64: i64 = row.get(4)?;
            let processed_files_i64: i64 = row.get(5)?;
            let changed_files_i64: i64 = row.get(6)?;
            let started_raw: String = row.get(8)?;
            let last_update_raw: String = row.get(9)?;
            Ok(PersistedIndexProgress {
                active: row.get::<_, i64>(0)? != 0,
                hard: row.get::<_, i64>(1)? != 0,
                state: row.get(2)?,
                phase: row.get(3)?,
                total_files: usize::try_from(total_files_i64).unwrap_or_default(),
                processed_files: usize::try_from(processed_files_i64).unwrap_or_default(),
                changed_files: usize::try_from(changed_files_i64).unwrap_or_default(),
                current_file: row.get(7)?,
                started_at_unix_ms: started_raw.parse::<u128>().unwrap_or_default(),
                last_update_unix_ms: last_update_raw.parse::<u128>().unwrap_or_default(),
                last_error: row.get(10)?,
                job_id: row.get(11)?,
                job_state: row
                    .get::<_, Option<String>>(12)?
                    .unwrap_or_else(|| "idle".to_string()),
                terminal_outcome: row.get(13)?,
            })
        })
        .optional()?;
    Ok(snapshot)
}

pub fn save_index_progress_snapshot(
    repo_root: &Path,
    snapshot: &PersistedIndexProgress,
) -> Result<()> {
    let path = config::index_db_path(repo_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let conn = open_index_db(repo_root)?;
    ensure_index_db_schema(&conn)?;
    conn.execute(
        "INSERT INTO index_progress(
             id, active, hard, state, phase, total_files, processed_files, changed_files,
             current_file, started_at_unix_ms, last_update_unix_ms, last_error,
             job_id, job_state, terminal_outcome
         ) VALUES (
             1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )
         ON CONFLICT(id) DO UPDATE SET
             active = excluded.active,
             hard = excluded.hard,
             state = excluded.state,
             phase = excluded.phase,
             total_files = excluded.total_files,
             processed_files = excluded.processed_files,
             changed_files = excluded.changed_files,
             current_file = excluded.current_file,
             started_at_unix_ms = excluded.started_at_unix_ms,
             last_update_unix_ms = excluded.last_update_unix_ms,
             last_error = excluded.last_error,
             job_id = excluded.job_id,
             job_state = excluded.job_state,
             terminal_outcome = excluded.terminal_outcome",
        params![
            if snapshot.active { 1i64 } else { 0i64 },
            if snapshot.hard { 1i64 } else { 0i64 },
            &snapshot.state,
            &snapshot.phase,
            i64::try_from(snapshot.total_files).unwrap_or(i64::MAX),
            i64::try_from(snapshot.processed_files).unwrap_or(i64::MAX),
            i64::try_from(snapshot.changed_files).unwrap_or(i64::MAX),
            snapshot.current_file.as_deref(),
            snapshot.started_at_unix_ms.to_string(),
            snapshot.last_update_unix_ms.to_string(),
            snapshot.last_error.as_deref(),
            snapshot.job_id.as_deref(),
            &snapshot.job_state,
            snapshot.terminal_outcome.as_deref(),
        ],
    )?;
    Ok(())
}

fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for value in embedding {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

fn load_embedding_cache_or_default() -> EmbeddingCacheState {
    match load_embedding_cache() {
        Ok(cache) => cache,
        Err(err) => {
            warn!(
                "Failed loading global embedding cache, starting empty: {:#}",
                err
            );
            EmbeddingCacheState::default()
        }
    }
}

fn load_embedding_cache() -> Result<EmbeddingCacheState> {
    let path = config::embedding_cache_path()?;
    if !path.exists() {
        return Ok(EmbeddingCacheState::default());
    }
    let conn =
        Connection::open(&path).with_context(|| format!("Failed opening {}", path.display()))?;
    ensure_embedding_cache_db_schema(&conn)?;

    let mut entries = HashMap::new();
    let mut statement = conn.prepare("SELECT content_hash, embedding FROM embeddings")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (content_hash, embedding_blob) = row?;
        let Some(embedding) = decode_embedding(&embedding_blob) else {
            warn!(
                "Skipping invalid cached embedding payload for hash {} in {}",
                content_hash,
                path.display()
            );
            continue;
        };
        if !embedding.is_empty() {
            entries.insert(content_hash, embedding);
        }
    }
    let updated_at_ts = load_meta_value(&conn, "updated_at_ts")?
        .and_then(|raw| raw.parse::<i64>().ok())
        .unwrap_or_default();
    Ok(EmbeddingCacheState {
        entries,
        updated_at_ts,
    })
}

fn save_embedding_cache(cache: &EmbeddingCacheState) -> Result<()> {
    let path = config::embedding_cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let mut conn =
        Connection::open(&path).with_context(|| format!("Failed opening {}", path.display()))?;
    ensure_embedding_cache_db_schema(&conn)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM embeddings", [])?;
    {
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO embeddings (content_hash, embedding) VALUES (?1, ?2)",
        )?;
        for (content_hash, embedding) in &cache.entries {
            if embedding.is_empty() {
                continue;
            }
            let payload = encode_embedding(embedding);
            insert.execute(params![content_hash, payload])?;
        }
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('updated_at_ts', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![cache.updated_at_ts.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_embedding_cache_db_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS embeddings (
            content_hash TEXT PRIMARY KEY,
            embedding BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn build_hnsw(chunks: &[ChunkRecord]) -> Result<Option<Hnsw<'static, f32, DistCosine>>> {
    if chunks.is_empty() {
        return Ok(None);
    }
    let mut embedding_dim: Option<usize> = None;
    let mut insert_data: Vec<(&[f32], usize)> = Vec::new();
    for chunk in chunks {
        if chunk.embedding.is_empty() {
            continue;
        }
        let dim = chunk.embedding.len();
        if let Some(expected_dim) = embedding_dim {
            if expected_dim != dim {
                warn!(
                    "Skipping chunk {} due to embedding dimension mismatch (expected {}, got {})",
                    chunk.id, expected_dim, dim
                );
                continue;
            }
        } else {
            embedding_dim = Some(dim);
        }
        insert_data.push((chunk.embedding.as_slice(), chunk.id as usize));
    }
    if insert_data.is_empty() {
        return Ok(None);
    }
    let max_nb_conn = 32usize;
    let nb_elem = insert_data.len();
    let nb_layer = 16usize.min((nb_elem as f32).ln().trunc() as usize).max(1);
    let ef_construction = 256usize;
    let mut hnsw: Hnsw<'static, f32, DistCosine> = Hnsw::new(
        max_nb_conn,
        nb_elem,
        nb_layer,
        ef_construction,
        DistCosine {},
    );
    hnsw.parallel_insert_slice(&insert_data);
    hnsw.set_searching_mode(true);
    Ok(Some(hnsw))
}

fn calculate_changed_set(
    hard: bool,
    hinted_paths: &HashSet<String>,
    previous_hashes: &HashMap<String, String>,
    current_hashes: &HashMap<String, String>,
) -> HashSet<String> {
    if hard {
        return current_hashes.keys().cloned().collect();
    }
    let mut changed = HashSet::new();
    for rel in hinted_paths {
        if current_hashes.contains_key(rel) || previous_hashes.contains_key(rel) {
            changed.insert(rel.clone());
        }
    }
    for (path, hash) in current_hashes {
        if previous_hashes.get(path) != Some(hash) {
            changed.insert(path.clone());
        }
    }
    for path in previous_hashes.keys() {
        if !current_hashes.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed
}

fn count_deleted_paths(
    changed_set: &HashSet<String>,
    current_hashes: &HashMap<String, String>,
) -> usize {
    changed_set
        .iter()
        .filter(|path| !current_hashes.contains_key(*path))
        .count()
}

fn collect_hint_paths(repo_root: &Path, changed_hint: Option<&[String]>) -> HashSet<String> {
    let Some(changed_hint) = changed_hint else {
        return HashSet::new();
    };
    changed_hint
        .iter()
        .filter_map(|raw| normalize_hint_path(repo_root, raw))
        .collect()
}

fn normalize_hint_path(repo_root: &Path, raw: &str) -> Option<String> {
    let candidate = PathBuf::from(raw);
    let relative = if candidate.is_absolute() {
        candidate.strip_prefix(repo_root).ok()?.to_path_buf()
    } else {
        candidate
    };
    let mut cleaned = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => cleaned.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if cleaned.as_os_str().is_empty() {
        return None;
    }
    Some(cleaned.to_string_lossy().replace('\\', "/"))
}

fn should_use_metadata_delta(
    repo_root: &Path,
    hard: bool,
    hinted_paths: &HashSet<String>,
    previous_files_by_path: &HashMap<String, FileRecord>,
    previous_chunks_by_path: &HashMap<String, Vec<ChunkRecord>>,
) -> bool {
    if hard
        || hinted_paths.is_empty()
        || previous_files_by_path.is_empty()
        || previous_chunks_by_path.is_empty()
    {
        return false;
    }
    // Use metadata delta fast-path only when all hinted existing files are already tracked.
    // This keeps ignore semantics correct for brand new files by falling back to full discovery.
    hinted_paths
        .iter()
        .all(|path| previous_files_by_path.contains_key(path) || !repo_root.join(path).exists())
}

fn build_current_files_from_discovery(
    repo_root: &Path,
    config: &BudiConfig,
    previous_files_by_path: &HashMap<String, FileRecord>,
    hard: bool,
    hinted_paths: &HashSet<String>,
    options: Option<&IndexBuildOptions>,
) -> Result<(Vec<FileRecord>, HashMap<String, String>)> {
    let files = discover_source_files(repo_root, config, options)?;
    let mut current_files = Vec::with_capacity(files.len());
    let mut current_hashes = HashMap::with_capacity(files.len());

    for file in files {
        let relative = file
            .strip_prefix(repo_root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = match fs::metadata(&file) {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("Skipping unreadable file metadata {}: {}", relative, err);
                continue;
            }
        };
        let size_bytes = metadata.len();
        let modified_unix_ms = file_modified_unix_ms(&metadata);
        let force_rehash = hard || hinted_paths.contains(&relative);
        let hash = if should_rehash_file(
            previous_files_by_path.get(&relative),
            size_bytes,
            modified_unix_ms,
            force_rehash,
        ) {
            match hash_file(&file) {
                Ok(hash) => hash,
                Err(err) => {
                    warn!("Skipping unhashed file {}: {}", relative, err);
                    continue;
                }
            }
        } else if let Some(entry) = previous_files_by_path.get(&relative) {
            entry.hash.clone()
        } else {
            match hash_file(&file) {
                Ok(hash) => hash,
                Err(err) => {
                    warn!("Skipping unhashed file {}: {}", relative, err);
                    continue;
                }
            }
        };
        if hash.is_empty() {
            warn!("Skipping file with empty hash {}", relative);
            continue;
        }
        current_hashes.insert(relative.clone(), hash.clone());
        current_files.push(FileRecord {
            path: relative,
            hash,
            size_bytes,
            modified_unix_ms,
        });
    }

    current_files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((current_files, current_hashes))
}

fn build_current_files_from_metadata_delta(
    repo_root: &Path,
    config: &BudiConfig,
    previous_files_by_path: &HashMap<String, FileRecord>,
    hinted_paths: &HashSet<String>,
    options: Option<&IndexBuildOptions>,
) -> Result<(Vec<FileRecord>, HashMap<String, String>)> {
    let extension_allowlist = build_effective_extension_allowlist(config, options);
    let basename_allowlist = build_basename_allowlist(config);
    let mut files_by_path = previous_files_by_path.clone();
    for relative in hinted_paths {
        let absolute = repo_root.join(relative);
        if !absolute.exists()
            || !absolute.is_file()
            || !is_supported_code_file(&absolute, &extension_allowlist, &basename_allowlist)
        {
            files_by_path.remove(relative);
            continue;
        }
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("Skipping unreadable file metadata {}: {}", relative, err);
                continue;
            }
        };
        if metadata.len() as usize > config.max_file_bytes {
            files_by_path.remove(relative);
            continue;
        }
        let hash = match hash_file(&absolute) {
            Ok(hash) => hash,
            Err(err) => {
                warn!("Skipping unhashed file {}: {}", relative, err);
                continue;
            }
        };
        if hash.is_empty() {
            warn!("Skipping file with empty hash {}", relative);
            continue;
        }
        files_by_path.insert(
            relative.clone(),
            FileRecord {
                path: relative.clone(),
                hash,
                size_bytes: metadata.len(),
                modified_unix_ms: file_modified_unix_ms(&metadata),
            },
        );
    }

    let mut current_files = files_by_path.into_values().collect::<Vec<_>>();
    current_files.sort_by(|a, b| a.path.cmp(&b.path));
    let current_hashes = current_files
        .iter()
        .map(|record| (record.path.clone(), record.hash.clone()))
        .collect();
    Ok((current_files, current_hashes))
}

fn file_modified_unix_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn group_chunks_by_path(chunks: &[ChunkRecord]) -> HashMap<String, Vec<ChunkRecord>> {
    let mut grouped: HashMap<String, Vec<ChunkRecord>> = HashMap::new();
    for chunk in chunks {
        grouped
            .entry(chunk.path.clone())
            .or_default()
            .push(chunk.clone());
    }
    grouped
}

fn emit_progress(
    progress_cb: &mut Option<&mut dyn FnMut(IndexBuildProgress)>,
    progress: IndexBuildProgress,
) {
    if let Some(callback) = progress_cb.as_mut() {
        callback(progress);
    }
}

fn build_retrieval_signal_indexes(
    repo_root: &Path,
    chunks: &[ChunkRecord],
) -> RetrievalSignalIndexes {
    let mut symbol_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut path_token_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut graph_token_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut chunk_to_graph_tokens: HashMap<u64, Vec<String>> = HashMap::new();
    let mut doc_like_chunk_ids: HashSet<u64> = HashSet::new();
    let mut defined_tokens: HashSet<String> = HashSet::new();
    let file_import_aliases = build_file_import_aliases(repo_root, chunks);
    let mut chunk_path_by_id: HashMap<u64, String> = HashMap::new();
    let mut chunk_reference_tokens: HashMap<u64, HashSet<String>> = HashMap::new();
    let mut chunk_call_sites: HashMap<u64, Vec<CallSite>> = HashMap::new();

    for chunk in chunks {
        if is_doc_like_path(&chunk.path) {
            doc_like_chunk_ids.insert(chunk.id);
        }

        let symbol_tokens = extract_symbol_tokens(&chunk.text);
        for token in symbol_tokens {
            symbol_to_chunk_ids.entry(token).or_default().push(chunk.id);
        }
        for token in extract_definition_tokens(&chunk.text, chunk.symbol_hint.as_deref()) {
            defined_tokens.insert(token);
        }
        let mut references = extract_reference_tokens(&chunk.text);
        references.extend(extract_call_tokens(&chunk.text));
        if !references.is_empty() {
            chunk_reference_tokens.insert(chunk.id, references.into_iter().collect());
        }
        let call_sites = extract_call_sites(&chunk.text);
        if !call_sites.is_empty() {
            chunk_call_sites.insert(chunk.id, call_sites);
        }
        chunk_path_by_id.insert(chunk.id, chunk.path.clone());

        let path_tokens = extract_path_tokens(&chunk.path);
        for token in path_tokens {
            path_token_to_chunk_ids
                .entry(token)
                .or_default()
                .push(chunk.id);
        }
    }

    for (chunk_id, call_sites) in &chunk_call_sites {
        let Some(path) = chunk_path_by_id.get(chunk_id) else {
            continue;
        };
        for call_site in call_sites {
            let resolved_candidates =
                resolve_call_site_candidates(path, call_site, &file_import_aliases);
            if let Some(resolved) = first_defined_candidate(&resolved_candidates, &defined_tokens) {
                graph_token_to_chunk_ids
                    .entry(resolved.clone())
                    .or_default()
                    .push(*chunk_id);
                chunk_to_graph_tokens
                    .entry(*chunk_id)
                    .or_default()
                    .push(resolved);
            }
        }
    }

    for (chunk_id, references) in &chunk_reference_tokens {
        let Some(path) = chunk_path_by_id.get(chunk_id) else {
            continue;
        };
        for token in references {
            let resolved_candidates =
                resolve_reference_candidates(path, token, &file_import_aliases);
            if let Some(resolved) = first_defined_candidate(&resolved_candidates, &defined_tokens) {
                graph_token_to_chunk_ids
                    .entry(resolved)
                    .or_default()
                    .push(*chunk_id);
            }
        }
    }

    dedup_index_values(&mut symbol_to_chunk_ids);
    dedup_index_values(&mut path_token_to_chunk_ids);
    dedup_index_values(&mut graph_token_to_chunk_ids);
    let (symbol_family_to_tokens, symbol_family_prefix_to_families) =
        build_family_lookup_indexes(&symbol_to_chunk_ids);
    let (graph_family_to_tokens, graph_family_prefix_to_families) =
        build_family_lookup_indexes(&graph_token_to_chunk_ids);

    dedup_index_values_forward(&mut chunk_to_graph_tokens);

    (
        symbol_to_chunk_ids,
        symbol_family_to_tokens,
        symbol_family_prefix_to_families,
        path_token_to_chunk_ids,
        graph_token_to_chunk_ids,
        graph_family_to_tokens,
        graph_family_prefix_to_families,
        doc_like_chunk_ids,
        chunk_to_graph_tokens,
    )
}

fn build_file_import_aliases(
    repo_root: &Path,
    chunks: &[ChunkRecord],
) -> HashMap<String, HashMap<String, String>> {
    let mut aliases_by_path: HashMap<String, HashMap<String, String>> = HashMap::new();
    let grouped_chunks = group_chunks_by_path(chunks);
    let mut paths = grouped_chunks.keys().cloned().collect::<Vec<_>>();
    paths.sort();

    for path in paths {
        let absolute = repo_root.join(&path);
        if let Ok(content) = fs::read_to_string(&absolute) {
            for (alias, target) in extract_import_aliases(&content) {
                aliases_by_path
                    .entry(path.clone())
                    .or_default()
                    .insert(alias, target);
            }
            continue;
        }
        if let Some(file_chunks) = grouped_chunks.get(&path) {
            for chunk in file_chunks {
                for (alias, target) in extract_import_aliases(&chunk.text) {
                    aliases_by_path
                        .entry(path.clone())
                        .or_default()
                        .insert(alias, target);
                }
            }
        }
    }
    aliases_by_path
}

fn dedup_index_values(map: &mut HashMap<String, Vec<u64>>) {
    for ids in map.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
}

fn dedup_index_values_forward(map: &mut HashMap<u64, Vec<String>>) {
    for tokens in map.values_mut() {
        tokens.sort_unstable();
        tokens.dedup();
    }
}

fn build_family_lookup_indexes(
    token_map: &HashMap<String, Vec<u64>>,
) -> (FamilyTokenLookup, FamilyPrefixLookup) {
    let mut family_to_tokens: FamilyTokenLookup = HashMap::new();
    for token in token_map.keys() {
        let family = normalize_symbol_family(token);
        if family.len() < 7 {
            continue;
        }
        family_to_tokens
            .entry(family)
            .or_default()
            .push(token.clone());
    }
    for tokens in family_to_tokens.values_mut() {
        tokens.sort();
        tokens.dedup();
    }

    let mut family_prefix_to_families: FamilyPrefixLookup = HashMap::new();
    for family in family_to_tokens.keys() {
        let prefix = family_prefix_key(family);
        family_prefix_to_families
            .entry(prefix)
            .or_default()
            .push(family.clone());
    }
    for families in family_prefix_to_families.values_mut() {
        families.sort();
        families.dedup();
    }
    (family_to_tokens, family_prefix_to_families)
}

fn family_prefix_key(family: &str) -> String {
    family.chars().take(7).collect()
}

fn collect_family_candidate_tokens(
    query_token: &str,
    token_map: &HashMap<String, Vec<u64>>,
    family_to_tokens: Option<&FamilyTokenLookup>,
    family_prefix_to_families: Option<&FamilyPrefixLookup>,
) -> Vec<String> {
    let Some(family_to_tokens) = family_to_tokens else {
        return token_map.keys().cloned().collect();
    };
    let Some(family_prefix_to_families) = family_prefix_to_families else {
        return token_map.keys().cloned().collect();
    };

    let query_family = normalize_symbol_family(query_token);
    if query_family.len() < 7 {
        return Vec::new();
    }
    let prefix = family_prefix_key(&query_family);
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(families) = family_prefix_to_families.get(prefix.as_str()) {
        for family in families {
            if !is_symbol_family_match(family, &query_family) {
                continue;
            }
            if let Some(tokens) = family_to_tokens.get(family) {
                for token in tokens {
                    if seen.insert(token.clone()) {
                        out.push(token.clone());
                    }
                }
            }
        }
    }

    if out.is_empty() {
        for (family, tokens) in family_to_tokens {
            if !is_symbol_family_match(family, &query_family) {
                continue;
            }
            for token in tokens {
                if seen.insert(token.clone()) {
                    out.push(token.clone());
                }
            }
            if out.len() > 160 {
                break;
            }
        }
    }

    out
}

fn score_from_token_map_with_family_lookup(
    token_map: &HashMap<String, Vec<u64>>,
    family_to_tokens: Option<&FamilyTokenLookup>,
    family_prefix_to_families: Option<&FamilyPrefixLookup>,
    query_tokens: &[String],
    limit: usize,
    allow_symbol_family_match: bool,
) -> Vec<(u64, f32)> {
    if limit == 0 || query_tokens.is_empty() {
        return Vec::new();
    }
    let mut scores: HashMap<u64, f32> = HashMap::new();
    let mut seen = HashSet::new();

    for token in query_tokens {
        if token.is_empty() || !seen.insert(token.as_str()) {
            continue;
        }
        let mut had_exact_match = false;
        if let Some(ids) = token_map.get(token) {
            had_exact_match = true;
            let rarity = 1.0 / ((ids.len() as f32).ln_1p() + 1.0);
            let token_weight = token_weight(token) * rarity;
            for id in ids {
                *scores.entry(*id).or_insert(0.0) += token_weight;
            }
        }
        if !allow_symbol_family_match || token.len() < 7 {
            continue;
        }
        let mut family_matches = 0usize;
        let family_candidates = collect_family_candidate_tokens(
            token,
            token_map,
            family_to_tokens,
            family_prefix_to_families,
        );
        for indexed_token in family_candidates {
            let Some(ids) = token_map.get(indexed_token.as_str()) else {
                continue;
            };
            if indexed_token == *token || !is_symbol_family_match(indexed_token.as_str(), token) {
                continue;
            }
            family_matches += 1;
            if family_matches > 80 {
                break;
            }
            let rarity = 1.0 / ((ids.len() as f32).ln_1p() + 1.0);
            let token_weight =
                token_weight(token) * rarity * if had_exact_match { 0.35 } else { 0.55 };
            for id in ids {
                *scores.entry(*id).or_insert(0.0) += token_weight;
            }
        }
    }

    let mut ranked: Vec<(u64, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(limit);
    ranked
}

fn token_weight(token: &str) -> f32 {
    (token.len().min(24) as f32 / 24.0) + 0.15
}

fn is_symbol_family_match(candidate: &str, query_symbol: &str) -> bool {
    let candidate_norm = normalize_symbol_family(candidate);
    let query_norm = normalize_symbol_family(query_symbol);
    if candidate_norm.is_empty() || query_norm.is_empty() {
        return false;
    }
    if candidate_norm == query_norm {
        return true;
    }
    if candidate_norm.len() < 7 || query_norm.len() < 7 {
        return false;
    }
    candidate_norm.starts_with(query_norm.as_str())
        || query_norm.starts_with(candidate_norm.as_str())
}

fn normalize_symbol_family(token: &str) -> String {
    let mut normalized = token.trim_matches('_').to_ascii_lowercase();
    if normalized.ends_with('s') && normalized.len() > 4 {
        normalized.pop();
    }
    normalized
}

fn extract_symbol_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
    {
        if !is_symbol_like_token(raw) {
            continue;
        }
        let token = raw.to_ascii_lowercase();
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

fn extract_definition_tokens(text: &str, symbol_hint: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Some(hint) = symbol_hint {
        for token in extract_signal_tokens(hint) {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
    }

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !looks_like_definition_line(trimmed) {
            continue;
        }
        if let Some(token) = extract_definition_name(trimmed) {
            if !is_signal_token(&token) {
                continue;
            }
            let normalized = token.to_ascii_lowercase();
            if seen.insert(normalized.clone()) {
                out.push(normalized);
            }
        }
    }

    out
}

fn looks_like_definition_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("fn ")
        || lower.starts_with("func ")
        || lower.starts_with("pub fn ")
        || lower.starts_with("async fn ")
        || lower.starts_with("pub async fn ")
        || lower.starts_with("def ")
        || lower.starts_with("class ")
        || lower.starts_with("interface ")
        || lower.starts_with("struct ")
        || lower.starts_with("enum ")
        || lower.starts_with("trait ")
        || lower.starts_with("impl ")
        || lower.starts_with("function ")
        || lower.starts_with("export function ")
        || lower.starts_with("export class ")
        || lower.starts_with("export const ")
        || lower.starts_with("const ")
        || lower.starts_with("let ")
        || lower.starts_with("var ")
        || lower.starts_with("type ")
}

fn extract_definition_name(line: &str) -> Option<String> {
    let mut normalized = line.trim_start();
    for prefix in [
        "pub ", "async ", "export ", "default ", "static ", "const ", "let ", "var ",
    ] {
        while normalized.starts_with(prefix) {
            normalized = normalized[prefix.len()..].trim_start();
        }
    }
    for keyword in [
        "fn ",
        "func ",
        "def ",
        "class ",
        "interface ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "function ",
        "type ",
    ] {
        if normalized.starts_with(keyword) {
            normalized = normalized[keyword.len()..].trim_start();
            break;
        }
    }
    if let Some(rest) = normalized.strip_prefix('(') {
        if let Some((_, after_receiver)) = rest.split_once(')') {
            normalized = after_receiver.trim_start();
        }
    }
    let candidate = normalized
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or_default();
    if candidate.len() < 3 {
        return None;
    }
    Some(candidate.to_string())
}

fn extract_reference_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !looks_like_reference_line(trimmed) {
            continue;
        }
        for token in extract_signal_tokens(trimmed) {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
struct CallSite {
    callee: String,
    receiver: Option<String>,
}

fn extract_call_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for site in extract_call_sites(text) {
        if seen.insert(site.callee.clone()) {
            out.push(site.callee.clone());
        }
        if let Some(receiver) = site.receiver.as_deref()
            && let Some(combined) = combine_receiver_method_token(receiver, &site.callee)
            && seen.insert(combined.clone())
        {
            out.push(combined);
        }
    }
    out
}

fn extract_call_sites(text: &str) -> Vec<CallSite> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if looks_like_definition_line(trimmed) {
            continue;
        }
        let bytes = line.as_bytes();
        for idx in 0..bytes.len() {
            if bytes[idx] != b'(' {
                continue;
            }
            let mut end = idx;
            while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            if end == 0 {
                continue;
            }
            let mut start = end;
            while start > 0 {
                let ch = bytes[start - 1] as char;
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':') {
                    start -= 1;
                } else {
                    break;
                }
            }
            if start == end {
                continue;
            }
            let raw = &line[start..end];
            let Some(site) = parse_call_site(raw) else {
                continue;
            };
            let key = format!(
                "{}::{}",
                site.receiver.as_deref().unwrap_or_default(),
                site.callee
            );
            if seen.insert(key) {
                out.push(site);
            }
        }
    }
    out
}

fn parse_call_site(raw: &str) -> Option<CallSite> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (receiver_raw, callee_raw) = split_call_receiver(raw);
    let callee = first_signal_token(callee_raw)?;
    let receiver = receiver_raw.and_then(normalize_receiver_chain);
    Some(CallSite { callee, receiver })
}

fn split_call_receiver(raw: &str) -> (Option<&str>, &str) {
    let dot_pos = raw.rfind('.');
    let namespace_pos = raw.rfind("::");
    let split = match (dot_pos, namespace_pos) {
        (Some(dot), Some(namespace)) => {
            if namespace > dot {
                Some((namespace, 2usize))
            } else {
                Some((dot, 1usize))
            }
        }
        (Some(dot), None) => Some((dot, 1usize)),
        (None, Some(namespace)) => Some((namespace, 2usize)),
        (None, None) => None,
    };
    if let Some((position, width)) = split {
        let receiver = raw[..position].trim();
        let callee = raw[position + width..].trim();
        if receiver.is_empty() || callee.is_empty() {
            return (None, raw);
        }
        return (Some(receiver), callee);
    }
    (None, raw)
}

fn normalize_receiver_chain(raw: &str) -> Option<String> {
    let tokens = receiver_chain_tokens(raw);
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join("."))
    }
}

fn receiver_chain_tokens(raw: &str) -> Vec<String> {
    raw.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
        .filter(|token| is_signal_token(token))
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn first_defined_candidate(
    candidates: &[String],
    defined_tokens: &HashSet<String>,
) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| defined_tokens.contains(*candidate))
        .cloned()
}

fn push_unique_candidate(out: &mut Vec<String>, seen: &mut HashSet<String>, token: String) {
    if !token.is_empty() && seen.insert(token.clone()) {
        out.push(token);
    }
}

fn resolve_reference_candidates(
    path: &str,
    token: &str,
    file_import_aliases: &HashMap<String, HashMap<String, String>>,
) -> Vec<String> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    let token = token.trim();
    if token.is_empty() {
        return resolved;
    }
    if let Some(aliases) = file_import_aliases.get(path) {
        // 1) Exact import aliases are highest confidence.
        if let Some(target) = aliases.get(token) {
            push_unique_candidate(&mut resolved, &mut seen, target.clone());
        }
        // 2) Wildcard imports are next, sorted by import distance.
        for target in wildcard_import_targets(path, aliases) {
            if let Some(expanded) = combine_receiver_method_token(&target, token) {
                push_unique_candidate(&mut resolved, &mut seen, expanded);
            }
        }
    }
    // 3) Plain token fallback last.
    push_unique_candidate(&mut resolved, &mut seen, token.to_string());
    resolved
}

fn resolve_call_site_candidates(
    path: &str,
    call_site: &CallSite,
    file_import_aliases: &HashMap<String, HashMap<String, String>>,
) -> Vec<String> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    if let Some(receiver) = call_site.receiver.as_deref() {
        // Namespace receiver chains should be resolved before broad fallback tokens.
        for receiver_candidate in
            expand_receiver_candidates_ordered(path, receiver, file_import_aliases)
        {
            if let Some(combined) =
                combine_receiver_method_token(&receiver_candidate, &call_site.callee)
            {
                push_unique_candidate(&mut resolved, &mut seen, combined);
            }
        }
    }
    for candidate in resolve_reference_candidates(path, &call_site.callee, file_import_aliases) {
        push_unique_candidate(&mut resolved, &mut seen, candidate);
    }
    resolved
}

fn expand_receiver_candidates_ordered(
    path: &str,
    receiver: &str,
    file_import_aliases: &HashMap<String, HashMap<String, String>>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let chain = receiver_chain_tokens(receiver);
    if chain.is_empty() {
        return candidates;
    }
    let tail = chain.last().cloned().unwrap_or_default();
    if !tail.is_empty() {
        push_unique_candidate(&mut candidates, &mut seen, tail);
    }
    if chain.len() > 1 {
        push_unique_candidate(&mut candidates, &mut seen, chain.join("_"));
    }

    let head = chain.first().cloned().unwrap_or_default();
    if !head.is_empty() {
        let head_targets = resolve_reference_candidates(path, &head, file_import_aliases);
        for target in head_targets {
            if target.is_empty() {
                continue;
            }
            push_unique_candidate(&mut candidates, &mut seen, target.clone());
            if chain.len() > 1 {
                let mut expanded = vec![target];
                expanded.extend(chain.iter().skip(1).cloned());
                push_unique_candidate(&mut candidates, &mut seen, expanded.join("_"));
            }
        }
    }
    candidates
}

fn combine_receiver_method_token(receiver: &str, callee: &str) -> Option<String> {
    let receiver_token = last_signal_token(receiver)?;
    let callee_token = first_signal_token(callee)?;
    let combined = format!("{}_{}", receiver_token, callee_token);
    if is_signal_token(&combined) {
        Some(combined)
    } else {
        None
    }
}

fn extract_import_aliases(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut in_go_import_block = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if in_go_import_block {
            if trimmed.starts_with(')') {
                in_go_import_block = false;
                continue;
            }
            if let Some((alias, target)) = parse_go_import_clause(trimmed) {
                push_alias_pair(&mut out, &mut seen, (alias, target));
            }
            continue;
        }
        if trimmed.starts_with("import ") && trimmed.contains(" from ") {
            let after_import = trimmed.trim_start_matches("import ").trim();
            if let Some((lhs, import_source)) = after_import.split_once(" from ") {
                let lhs = lhs.trim();
                let namespace_aliases = lhs
                    .split(',')
                    .filter_map(parse_namespace_import_alias)
                    .collect::<Vec<_>>();
                let source_token = import_source_token(import_source);
                if let Some((default_part, brace_part)) = lhs.split_once('{') {
                    if let Some(alias) =
                        first_signal_token(default_part.trim().trim_end_matches(','))
                    {
                        push_alias_pair(&mut out, &mut seen, (alias.clone(), alias));
                    }
                    if let Some((inside, _)) = brace_part.split_once('}') {
                        for clause in inside.split(',') {
                            if let Some((alias, target)) = parse_import_alias_clause(clause) {
                                push_alias_pair(&mut out, &mut seen, (alias, target));
                            }
                        }
                    }
                } else if let Some(alias) = first_signal_token(lhs) {
                    push_alias_pair(&mut out, &mut seen, (alias.clone(), alias));
                }
                for alias in namespace_aliases {
                    let target = source_token.clone().unwrap_or_else(|| alias.clone());
                    push_alias_pair(&mut out, &mut seen, (alias, target));
                }
            }
            continue;
        }
        if trimmed.starts_with("from ") && trimmed.contains(" import ") {
            if let Some((module_path, imported)) = trimmed.split_once(" import ") {
                let source_token =
                    import_source_token(module_path.trim_start_matches("from ").trim());
                for clause in imported.split(',') {
                    let piece = clause.trim();
                    if piece == "*"
                        && let Some(target) = source_token.clone()
                    {
                        push_alias_pair(
                            &mut out,
                            &mut seen,
                            (format!("*{}", target.clone()), target),
                        );
                        continue;
                    }
                    if let Some((alias, target)) = parse_import_alias_clause(piece) {
                        push_alias_pair(&mut out, &mut seen, (alias, target));
                    }
                }
            }
            continue;
        }
        if trimmed == "import (" || trimmed.starts_with("import(") {
            in_go_import_block = true;
            continue;
        }
        if trimmed.starts_with("import static ")
            && let Some((alias, target)) = parse_java_static_import_clause(trimmed)
        {
            push_alias_pair(&mut out, &mut seen, (alias, target));
            continue;
        }
        if trimmed.starts_with("import ")
            && let Some((alias, target)) = parse_java_import_clause(trimmed)
        {
            push_alias_pair(&mut out, &mut seen, (alias, target));
            continue;
        }
        if trimmed.starts_with("import ") {
            let imported = trimmed.trim_start_matches("import ").trim();
            if let Some((alias, target)) = parse_go_import_clause(imported) {
                push_alias_pair(&mut out, &mut seen, (alias, target));
                continue;
            }
            for clause in imported.split(',') {
                if let Some((alias, target)) = parse_import_alias_clause(clause) {
                    push_alias_pair(&mut out, &mut seen, (alias, target));
                }
            }
            continue;
        }
        if trimmed.starts_with("use ") {
            let body = trimmed
                .trim_start_matches("use ")
                .trim()
                .trim_end_matches(';');
            if body.ends_with("::*")
                && let Some(target) = import_source_token(body.trim_end_matches("::*"))
            {
                push_alias_pair(
                    &mut out,
                    &mut seen,
                    (format!("*{}", target.clone()), target),
                );
            }
            if body.contains('{') && body.contains('}') {
                if let Some((prefix, rest)) = body.split_once('{')
                    && let Some((inside, _)) = rest.split_once('}')
                {
                    for clause in inside.split(',') {
                        let piece = clause.trim();
                        if piece.is_empty() {
                            continue;
                        }
                        if piece == "*"
                            && let Some(target) = import_source_token(prefix.trim())
                        {
                            push_alias_pair(
                                &mut out,
                                &mut seen,
                                (format!("*{}", target.clone()), target),
                            );
                            continue;
                        }
                        let combined = format!("{}::{}", prefix.trim(), piece);
                        if let Some((alias, target)) = parse_import_alias_clause(&combined) {
                            push_alias_pair(&mut out, &mut seen, (alias, target));
                        }
                    }
                }
            } else if let Some((alias, target)) = parse_import_alias_clause(body) {
                push_alias_pair(&mut out, &mut seen, (alias, target));
            }
            continue;
        }
        if trimmed.starts_with("using ")
            && !trimmed.starts_with("using namespace ")
            && let Some((alias, target)) = parse_csharp_using_clause(trimmed)
        {
            push_alias_pair(&mut out, &mut seen, (alias, target));
            continue;
        }
        if trimmed.starts_with("using namespace ")
            && let Some(target) = import_source_token(
                trimmed
                    .trim_start_matches("using namespace ")
                    .trim()
                    .trim_end_matches(';'),
            )
        {
            push_alias_pair(
                &mut out,
                &mut seen,
                (format!("*{}", target.clone()), target),
            );
            continue;
        }
        if let Some(alias) = parse_require_alias(trimmed) {
            push_alias_pair(&mut out, &mut seen, (alias.clone(), alias));
        }
    }
    out
}

fn parse_java_import_clause(line: &str) -> Option<(String, String)> {
    if !line.starts_with("import ") || !line.ends_with(';') {
        return None;
    }
    let body = line
        .trim_start_matches("import ")
        .trim()
        .trim_end_matches(';')
        .trim();
    if body.is_empty()
        || body.starts_with("static ")
        || body.starts_with('"')
        || body.starts_with('\'')
        || body.starts_with('`')
    {
        return None;
    }
    if body.ends_with(".*") {
        let target = import_source_token(body.trim_end_matches(".*"))?;
        return Some((format!("*{}", target.clone()), target));
    }
    let alias = last_signal_token(body)?;
    let target = import_source_token(body).unwrap_or_else(|| alias.clone());
    Some((alias, target))
}

fn parse_java_static_import_clause(line: &str) -> Option<(String, String)> {
    if !line.starts_with("import static ") || !line.ends_with(';') {
        return None;
    }
    let body = line
        .trim_start_matches("import static ")
        .trim()
        .trim_end_matches(';')
        .trim();
    if body.is_empty() {
        return None;
    }
    if body.ends_with(".*") {
        let target = import_source_token(body.trim_end_matches(".*"))?;
        return Some((format!("*{}", target.clone()), target));
    }
    let alias = last_signal_token(body)?;
    let target = import_source_token(body).unwrap_or_else(|| alias.clone());
    Some((alias, target))
}

fn parse_csharp_using_clause(line: &str) -> Option<(String, String)> {
    if !line.starts_with("using ") || !line.ends_with(';') {
        return None;
    }
    let body = line
        .trim_start_matches("using ")
        .trim()
        .trim_end_matches(';')
        .trim();
    if body.is_empty() || body.starts_with("namespace ") {
        return None;
    }
    if let Some((alias_raw, target_raw)) = body.split_once('=') {
        let alias = first_signal_token(alias_raw)?;
        let target = import_source_token(target_raw).unwrap_or_else(|| alias.clone());
        return Some((alias, target));
    }
    if body.starts_with("static ") {
        let target = import_source_token(body.trim_start_matches("static ").trim())?;
        return Some((format!("*{}", target.clone()), target));
    }
    let target = import_source_token(body)?;
    Some((format!("*{}", target.clone()), target))
}

fn parse_go_import_clause(raw: &str) -> Option<(String, String)> {
    let clause = raw.trim().trim_end_matches(',').trim();
    if clause.is_empty() || clause == "(" || clause == ")" || clause.starts_with("//") {
        return None;
    }
    if !(clause.contains('"') || clause.contains('`')) {
        return None;
    }

    let mut parts = clause.split_whitespace();
    let first = parts.next()?;
    let second = parts.next();
    if second.is_none() {
        let target = import_source_token(first)?;
        let alias = last_signal_token(first).unwrap_or_else(|| target.clone());
        return Some((alias, target));
    }

    let alias_raw = first;
    let source_raw = second.unwrap_or_default();
    if alias_raw == "_" {
        return None;
    }
    let target = import_source_token(source_raw)?;
    if alias_raw == "." {
        return Some((format!("*{}", target.clone()), target));
    }
    let alias = first_signal_token(alias_raw)?;
    Some((alias, target))
}

fn parse_namespace_import_alias(clause: &str) -> Option<String> {
    let normalized = clause.trim();
    let lower = normalized.to_ascii_lowercase();
    let position = lower.find("* as ")?;
    first_signal_token(normalized[position + 5..].trim())
}

fn import_source_token(raw: &str) -> Option<String> {
    let cleaned = raw
        .trim()
        .trim_end_matches(';')
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    if cleaned.is_empty() {
        return None;
    }
    let normalized = cleaned.replace("::", "_").replace(['\\', '/', '.'], "_");
    let tokens = extract_signal_tokens(&normalized);
    if tokens.is_empty() {
        last_signal_token(cleaned)
    } else {
        Some(tokens.join("_"))
    }
}

fn wildcard_import_targets(path: &str, aliases: &HashMap<String, String>) -> Vec<String> {
    let mut targets = aliases
        .iter()
        .filter_map(|(alias, target)| {
            if alias.starts_with('*') && !target.is_empty() {
                Some(target.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        import_distance_from_path(path, left)
            .cmp(&import_distance_from_path(path, right))
            .then_with(|| left.cmp(right))
    });
    targets.dedup();
    targets.truncate(6);
    targets
}

fn import_distance_from_path(path: &str, target: &str) -> usize {
    let path_tokens = expand_distance_tokens(path);
    let target_tokens = expand_distance_tokens(target);
    if path_tokens.is_empty() || target_tokens.is_empty() {
        return usize::MAX / 4;
    }
    let shared_prefix = path_tokens
        .iter()
        .zip(target_tokens.iter())
        .take_while(|(left, right)| left == right)
        .count();
    path_tokens.len() + target_tokens.len() - (shared_prefix * 2)
}

fn expand_distance_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in extract_signal_tokens(raw) {
        let mut emitted_subparts = false;
        for subpart in token.split('_').filter(|part| part.len() >= 2) {
            emitted_subparts = true;
            out.push(subpart.to_string());
        }
        if !emitted_subparts {
            out.push(token);
        }
    }
    out
}

fn parse_import_alias_clause(clause: &str) -> Option<(String, String)> {
    let normalized = clause.trim().trim_matches(|c: char| {
        c.is_ascii_whitespace() || matches!(c, '{' | '}' | '(' | ')' | ';')
    });
    if normalized.is_empty() {
        return None;
    }
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with("* as ") {
        let alias = first_signal_token(normalized[5..].trim())?;
        return Some((alias.clone(), alias));
    }
    if let Some(position) = lower.find(" as ") {
        let target = last_signal_token(normalized[..position].trim())?;
        let alias = first_signal_token(normalized[position + 4..].trim())?;
        return Some((alias, target));
    }
    let token = last_signal_token(normalized)?;
    Some((token.clone(), token))
}

fn parse_require_alias(line: &str) -> Option<String> {
    for prefix in ["const ", "let ", "var "] {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        if !rest.contains("require(") {
            continue;
        }
        let alias_raw = rest.split('=').next().unwrap_or_default().trim();
        if let Some(alias) = first_signal_token(alias_raw) {
            return Some(alias);
        }
    }
    None
}

fn first_signal_token(raw: &str) -> Option<String> {
    extract_signal_tokens(raw).into_iter().next()
}

fn last_signal_token(raw: &str) -> Option<String> {
    extract_signal_tokens(raw).into_iter().last()
}

fn push_alias_pair(
    out: &mut Vec<(String, String)>,
    seen: &mut HashSet<(String, String)>,
    pair: (String, String),
) {
    if seen.insert(pair.clone()) {
        out.push(pair);
    }
}

fn looks_like_reference_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("import ")
        || lower.starts_with("from ")
        || lower.starts_with("use ")
        || lower.contains(" require(")
        || lower.contains("::")
        || lower.contains("->")
        || lower.contains("router.")
        || lower.contains("service.")
        || lower.contains("client.")
        || (lower.contains('.') && lower.contains('('))
}

fn extract_signal_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
    {
        if !is_signal_token(raw) {
            continue;
        }
        let token = raw.to_ascii_lowercase();
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

fn is_signal_token(raw: &str) -> bool {
    if raw.len() < 3 || raw.len() > 64 {
        return false;
    }
    if !raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "const"
            | "return"
            | "import"
            | "from"
            | "class"
            | "false"
            | "true"
            | "default"
            | "export"
            | "interface"
            | "struct"
            | "enum"
            | "value"
            | "self"
            | "this"
            | "null"
            | "none"
            | "some"
            | "let"
            | "var"
            | "fn"
            | "def"
            | "type"
            | "impl"
            | "trait"
            | "for"
    )
}

fn is_symbol_like_token(raw: &str) -> bool {
    if raw.len() < 3 || raw.len() > 64 {
        return false;
    }
    let has_underscore = raw.contains('_');
    let has_digit = raw.chars().any(|c| c.is_ascii_digit());
    if !(has_underscore
        || has_digit
        || has_symbol_case_pattern(raw)
        || is_titlecase_symbol_candidate(raw))
    {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "const"
            | "return"
            | "import"
            | "from"
            | "class"
            | "false"
            | "true"
            | "default"
            | "export"
            | "interface"
            | "struct"
            | "enum"
            | "value"
    )
}

fn is_titlecase_symbol_candidate(raw: &str) -> bool {
    const STOP: &[&str] = &[
        "what", "where", "which", "when", "why", "how", "describe", "trace", "show", "list",
        "explain", "tell", "give",
    ];
    if raw.len() < 3 || raw.len() > 64 {
        return false;
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    let rest = chars.collect::<Vec<_>>();
    if rest.is_empty() {
        return false;
    }
    if !rest.iter().all(|ch| ch.is_ascii_lowercase()) {
        return false;
    }
    !STOP.contains(&raw.to_ascii_lowercase().as_str())
}

fn has_symbol_case_pattern(raw: &str) -> bool {
    let chars: Vec<char> = raw.chars().collect();
    let has_lower = chars.iter().any(|c| c.is_ascii_lowercase());
    let has_upper = chars.iter().any(|c| c.is_ascii_uppercase());
    if !(has_lower && has_upper) {
        return false;
    }
    // Ignore simple title-cased natural words like "Where".
    chars
        .iter()
        .enumerate()
        .any(|(idx, c)| c.is_ascii_uppercase() && idx > 0)
}

fn extract_path_tokens(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let lower_segment = segment.to_ascii_lowercase();
        if lower_segment.len() >= 2 && seen.insert(lower_segment.clone()) {
            out.push(lower_segment);
        }
        for piece in segment
            .split(['.', '-', '_', '[', ']', '(', ')'])
            .filter(|part| !part.is_empty())
        {
            let lower_piece = piece.to_ascii_lowercase();
            if lower_piece.len() >= 2 && seen.insert(lower_piece.clone()) {
                out.push(lower_piece);
            }
        }
    }
    out
}

fn is_doc_like_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains("/docs/") {
        return true;
    }
    let file_name = lower.rsplit('/').next().unwrap_or(lower.as_str());
    if matches!(
        file_name,
        "readme"
            | "readme.md"
            | "changelog"
            | "changelog.md"
            | "contributing.md"
            | "license"
            | "license.md"
            | "agents.md"
            | "claude.md"
    ) {
        return true;
    }
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default(),
        "md" | "mdx" | "txt" | "rst" | "adoc"
    )
}

#[derive(Debug, Clone)]
struct RepoIgnoreRules {
    excludes: Gitignore,
    unignores: Gitignore,
}

#[derive(Clone)]
pub struct CompiledIndexScope {
    extension_allowlist: HashSet<String>,
    basename_allowlist: HashSet<String>,
    ignore_rules: RepoIgnoreRules,
}

impl CompiledIndexScope {
    pub fn allows_relative_file_path(&self, relative_path: &str) -> bool {
        if relative_path.trim().is_empty() {
            return false;
        }
        if should_skip_index_path(relative_path, false, &self.ignore_rules) {
            return false;
        }
        is_supported_code_file(
            Path::new(relative_path),
            &self.extension_allowlist,
            &self.basename_allowlist,
        )
    }
}

pub fn compile_index_scope(
    repo_root: &Path,
    config: &BudiConfig,
    options: Option<&IndexBuildOptions>,
) -> Result<CompiledIndexScope> {
    let extension_allowlist = build_effective_extension_allowlist(config, options);
    let basename_allowlist = build_basename_allowlist(config);
    let ignore_rules = load_repo_ignore_rules(
        repo_root,
        options
            .map(|value| value.ignore_patterns.as_slice())
            .unwrap_or(&[]),
    )?;
    Ok(CompiledIndexScope {
        extension_allowlist,
        basename_allowlist,
        ignore_rules,
    })
}

fn discover_source_files(
    repo_root: &Path,
    config: &BudiConfig,
    options: Option<&IndexBuildOptions>,
) -> Result<Vec<PathBuf>> {
    let scope = compile_index_scope(repo_root, config, options)?;

    discover_source_files_from_git(
        repo_root,
        config,
        &scope.extension_allowlist,
        &scope.basename_allowlist,
        &scope.ignore_rules,
    )
}

fn discover_source_files_from_git(
    repo_root: &Path,
    config: &BudiConfig,
    extension_allowlist: &HashSet<String>,
    basename_allowlist: &HashSet<String>,
    ignore_rules: &RepoIgnoreRules,
) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("Failed running git ls-files in {}", repo_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git ls-files failed in {}: {}",
            repo_root.display(),
            stderr.trim()
        );
    }

    let mut files = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let relative = String::from_utf8_lossy(raw).replace('\\', "/");
        if relative.is_empty() || should_skip_index_path(&relative, false, ignore_rules) {
            continue;
        }
        let absolute = repo_root.join(&relative);
        if !absolute.exists() || !absolute.is_file() {
            continue;
        }
        if !is_supported_code_file(&absolute, extension_allowlist, basename_allowlist) {
            continue;
        }
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.len() as usize > config.max_file_bytes {
            continue;
        }
        files.push(absolute);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn build_effective_extension_allowlist(
    config: &BudiConfig,
    options: Option<&IndexBuildOptions>,
) -> HashSet<String> {
    let mut allowlist = build_extension_allowlist(config);
    if let Some(overrides) = options {
        for extension in &overrides.include_extensions {
            let normalized = extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase();
            if !normalized.is_empty() {
                allowlist.insert(normalized);
            }
        }
    }
    allowlist
}

fn root_ignore_files(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = fs::read_dir(repo_root)
        .with_context(|| format!("Failed reading {}", repo_root.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if file_name.ends_with("ignore") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn load_repo_ignore_rules(repo_root: &Path, extra_patterns: &[String]) -> Result<RepoIgnoreRules> {
    let mut excludes = Vec::new();
    let mut unignores = Vec::new();

    let mut ignore_paths = config::layered_ignore_paths(repo_root)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for root_ignore in root_ignore_files(repo_root)? {
        ignore_paths.insert(root_ignore);
    }

    for ignore_path in ignore_paths {
        if !ignore_path.exists() {
            continue;
        }
        let raw = fs::read_to_string(&ignore_path)
            .with_context(|| format!("Failed reading {}", ignore_path.display()))?;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(unignore) = trimmed.strip_prefix('!') {
                let pattern = unignore.trim();
                if !pattern.is_empty() {
                    unignores.push(pattern.to_string());
                }
            } else {
                excludes.push(trimmed.to_string());
            }
        }
    }
    excludes.extend(
        extra_patterns
            .iter()
            .map(|pattern| pattern.trim())
            .filter(|pattern| !pattern.is_empty())
            .map(ToOwned::to_owned),
    );

    Ok(RepoIgnoreRules {
        excludes: build_gitignore_matcher(repo_root, &excludes)?,
        unignores: build_gitignore_matcher(repo_root, &unignores)?,
    })
}

fn build_gitignore_matcher(repo_root: &Path, patterns: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(repo_root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("Invalid ignore pattern `{pattern}`"))?;
    }
    builder
        .build()
        .with_context(|| "Failed to build ignore matcher")
}

fn should_skip_index_path(
    relative_path: &str,
    is_dir: bool,
    ignore_rules: &RepoIgnoreRules,
) -> bool {
    let normalized = relative_path.trim_start_matches("./");
    if normalized.is_empty() {
        return false;
    }
    let path = Path::new(normalized);
    if ignore_rules
        .excludes
        .matched_path_or_any_parents(path, is_dir)
        .is_ignore()
    {
        return true;
    }
    if ignore_rules
        .unignores
        .matched_path_or_any_parents(path, is_dir)
        .is_ignore()
    {
        return false;
    }
    has_always_skipped_component(normalized, is_dir)
}

fn has_always_skipped_component(relative_path: &str, is_dir: bool) -> bool {
    let mut parts = relative_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if !is_dir {
        parts.pop();
    }
    parts.into_iter().any(is_always_skipped_dir_name)
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("Failed reading {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn should_rehash_file(
    previous: Option<&FileRecord>,
    size_bytes: u64,
    modified_unix_ms: u64,
    force_rehash: bool,
) -> bool {
    if force_rehash {
        return true;
    }
    let Some(previous) = previous else {
        return true;
    };
    previous.hash.is_empty()
        || previous.size_bytes != size_bytes
        || previous.modified_unix_ms != modified_unix_ms
}

fn chunk_fingerprint(path: &str, start_line: usize, end_line: usize, text: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_bytes());
    hasher.update(&[0]);
    hasher.update(&(start_line as u64).to_le_bytes());
    hasher.update(&(end_line as u64).to_le_bytes());
    hasher.update(text.as_bytes());
    *hasher.finalize().as_bytes()
}

fn embedding_content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn allocate_chunk_id(fingerprint: &[u8; 32], used_chunk_ids: &mut HashSet<u64>) -> u64 {
    let mut nonce: u64 = 0;
    loop {
        let mut hasher = blake3::Hasher::new();
        hasher.update(fingerprint);
        hasher.update(&nonce.to_le_bytes());
        let digest = hasher.finalize();
        let bytes = digest.as_bytes();
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&bytes[..8]);
        // SQLite integer primary keys are signed 64-bit; keep ids in-range so we never
        // collapse distinct u64 ids during DB writes.
        let id = u64::from_le_bytes(id_bytes) & SQLITE_CHUNK_ID_MAX;
        if used_chunk_ids.insert(id) {
            return id;
        }
        nonce = nonce.saturating_add(1);
    }
}

fn chunk_id_to_sql(id: u64) -> i64 {
    i64::try_from(id & SQLITE_CHUNK_ID_MAX).unwrap_or(i64::MAX)
}

fn flush_missing_embedding_queue(
    embedder: &mut EmbeddingEngine,
    pending_chunks: &mut [PendingChunk],
    missing_embedding_queue: &mut Vec<(usize, String)>,
    embedding_cache: &mut EmbeddingCacheState,
    embedding_cache_dirty: &mut bool,
    retry_attempts: usize,
    retry_backoff_ms: u64,
) -> Result<()> {
    if missing_embedding_queue.is_empty() {
        return Ok(());
    }
    let requests = std::mem::take(missing_embedding_queue);
    let passages = requests
        .iter()
        .map(|(_, passage)| passage.clone())
        .collect::<Vec<_>>();
    let embeddings =
        embed_passages_with_retry(embedder, &passages, retry_attempts, retry_backoff_ms)?;
    if embeddings.len() != requests.len() {
        warn!(
            "Embedding batch count mismatch: requested={} returned={}",
            requests.len(),
            embeddings.len()
        );
    }
    for ((pending_index, _), embedding) in requests.into_iter().zip(embeddings.into_iter()) {
        let Some(chunk) = pending_chunks.get_mut(pending_index) else {
            continue;
        };
        if !embedding.is_empty()
            && embedding_cache
                .entries
                .insert(chunk.embedding_cache_key.clone(), embedding.clone())
                .is_none()
        {
            *embedding_cache_dirty = true;
        }
        chunk.embedding = Some(embedding);
    }
    Ok(())
}

fn embed_passages_with_retry(
    embedder: &mut EmbeddingEngine,
    docs: &[String],
    retry_attempts: usize,
    retry_backoff_ms: u64,
) -> Result<Vec<Vec<f32>>> {
    let attempts = retry_attempts.max(1);
    for attempt in 1..=attempts {
        match embedder.embed_passages(docs) {
            Ok(rows) => return Ok(rows),
            Err(err) => {
                if attempt == attempts {
                    return Err(err);
                }
                let backoff_factor = 1u64 << (attempt - 1).min(10);
                let wait_ms = retry_backoff_ms.saturating_mul(backoff_factor).max(1);
                warn!(
                    "Embedding batch failed (attempt {}/{}): {}. Retrying in {}ms.",
                    attempt, attempts, err, wait_ms
                );
                std::thread::sleep(Duration::from_millis(wait_ms));
            }
        }
    }
    Ok(Vec::new())
}

fn reconcile_missing_chunk_embeddings(
    chunks: &mut [ChunkRecord],
    embedder: &mut EmbeddingEngine,
    embedding_cache: &mut EmbeddingCacheState,
    embedding_cache_dirty: &mut bool,
    batch_size: usize,
    retry_attempts: usize,
    retry_backoff_ms: u64,
) -> Result<usize> {
    let mut missing_indices = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        if chunk.embedding.is_empty() {
            missing_indices.push(idx);
        }
    }
    if missing_indices.is_empty() {
        return Ok(0);
    }

    let mut repaired = 0usize;
    for batch in missing_indices.chunks(batch_size.max(1)) {
        let passages = batch
            .iter()
            .map(|idx| format!("passage: {}", chunks[*idx].text))
            .collect::<Vec<_>>();
        let embeddings =
            embed_passages_with_retry(embedder, &passages, retry_attempts, retry_backoff_ms)?;
        if embeddings.len() != batch.len() {
            warn!(
                "Embedding reconcile count mismatch: requested={} returned={}",
                batch.len(),
                embeddings.len()
            );
        }
        for (chunk_idx, embedding) in batch.iter().zip(embeddings.into_iter()) {
            if embedding.is_empty() {
                continue;
            }
            if let Some(chunk) = chunks.get_mut(*chunk_idx) {
                chunk.embedding = embedding.clone();
                let cache_key = embedding_content_hash(&chunk.text);
                if embedding_cache
                    .entries
                    .insert(cache_key, embedding)
                    .is_none()
                {
                    *embedding_cache_dirty = true;
                }
                repaired = repaired.saturating_add(1);
            }
        }
    }
    if repaired > 0 {
        info!(
            "Reconciled {} missing chunk embeddings after indexing.",
            repaired
        );
    }
    Ok(repaired)
}

fn infer_expected_embedding_dims(chunks: &[ChunkRecord]) -> Option<usize> {
    let mut dims_to_counts: HashMap<usize, usize> = HashMap::new();
    for chunk in chunks {
        if chunk.embedding.is_empty() || chunk.embedding.iter().any(|value| !value.is_finite()) {
            continue;
        }
        *dims_to_counts.entry(chunk.embedding.len()).or_insert(0) += 1;
    }
    dims_to_counts
        .into_iter()
        .max_by(|(left_dims, left_count), (right_dims, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| left_dims.cmp(right_dims))
        })
        .map(|(dims, _)| dims)
}

fn sanitize_chunk_embeddings(chunks: &mut [ChunkRecord]) -> usize {
    let expected_dims = infer_expected_embedding_dims(chunks);
    let mut invalid = 0usize;
    for chunk in chunks {
        if chunk.embedding.is_empty() {
            continue;
        }
        let has_non_finite = chunk.embedding.iter().any(|value| !value.is_finite());
        let dims_mismatch = expected_dims.is_some_and(|dims| chunk.embedding.len() != dims);
        if has_non_finite || dims_mismatch {
            chunk.embedding.clear();
            invalid = invalid.saturating_add(1);
        }
    }
    invalid
}

enum EmbeddingBackend {
    Fast(Box<TextEmbedding>),
    Unavailable,
}

pub struct EmbeddingEngine {
    backend: EmbeddingBackend,
}

impl EmbeddingEngine {
    fn new() -> Self {
        let cache_dir = resolve_fastembed_cache_dir();
        let options = InitOptions::new(EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir)
            .with_show_download_progress(false);
        match TextEmbedding::try_new(options) {
            Ok(embedder) => {
                info!("Using fastembed backend (AllMiniLML6V2)");
                Self {
                    backend: EmbeddingBackend::Fast(Box::new(embedder)),
                }
            }
            Err(err) => {
                warn!(
                    "Semantic embeddings unavailable because fastembed failed to initialize: {}",
                    err
                );
                Self {
                    backend: EmbeddingBackend::Unavailable,
                }
            }
        }
    }

    fn embed_passages(&mut self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        match &mut self.backend {
            EmbeddingBackend::Fast(embedder) => Ok(embedder.embed(docs, None)?),
            EmbeddingBackend::Unavailable => Ok(vec![Vec::new(); docs.len()]),
        }
    }

    fn is_available(&self) -> bool {
        matches!(self.backend, EmbeddingBackend::Fast(_))
    }

    pub fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>> {
        match &mut self.backend {
            EmbeddingBackend::Fast(embedder) => {
                let rows = embedder.embed(vec![format!("query: {}", query)], None)?;
                Ok(rows.into_iter().next())
            }
            EmbeddingBackend::Unavailable => Ok(None),
        }
    }
}

fn resolve_fastembed_cache_dir() -> PathBuf {
    let fallback = std::env::temp_dir().join("budi-fastembed-cache");
    let Ok(path) = config::fastembed_cache_dir() else {
        warn!(
            "Failed resolving budi fastembed cache directory, using fallback {}",
            fallback.display()
        );
        return fallback;
    };
    if let Err(err) = fs::create_dir_all(&path) {
        warn!(
            "Failed creating fastembed cache dir {}: {}",
            path.display(),
            err
        );
    }
    path
}

struct TantivyBundle {
    index: Index,
    reader: IndexReader,
    id_field: tantivy::schema::Field,
    path_field: tantivy::schema::Field,
    text_field: tantivy::schema::Field,
}

impl TantivyBundle {
    fn schema() -> (
        Schema,
        tantivy::schema::Field,
        tantivy::schema::Field,
        tantivy::schema::Field,
    ) {
        let mut builder = SchemaBuilder::default();
        let id_field = builder.add_u64_field("id", STORED | FAST | INDEXED);
        let path_field = builder.add_text_field("path", STRING | STORED);
        let text_options = TextOptions::default()
            .set_stored()
            .set_indexing_options(TextFieldIndexing::default().set_tokenizer("default"));
        let text_field = builder.add_text_field("text", text_options);
        let schema = builder.build();
        (schema, id_field, path_field, text_field)
    }

    fn open_or_rebuild(repo_root: &Path, chunks: &[ChunkRecord]) -> Result<Self> {
        let tantivy_dir = config::tantivy_path(repo_root)?;
        if !tantivy_dir.exists() {
            Self::rebuild(repo_root, chunks)?;
        }
        let (schema, id_field, path_field, text_field) = Self::schema();
        let index = Index::open_in_dir(&tantivy_dir)
            .or_else(|_| Index::create_in_dir(&tantivy_dir, schema))
            .with_context(|| {
                format!("Failed opening tantivy index at {}", tantivy_dir.display())
            })?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            id_field,
            path_field,
            text_field,
        })
    }

    fn rebuild(repo_root: &Path, chunks: &[ChunkRecord]) -> Result<()> {
        let tantivy_dir = config::tantivy_path(repo_root)?;
        if tantivy_dir.exists() {
            fs::remove_dir_all(&tantivy_dir)
                .with_context(|| format!("Failed cleaning {}", tantivy_dir.display()))?;
        }
        fs::create_dir_all(&tantivy_dir)
            .with_context(|| format!("Failed creating {}", tantivy_dir.display()))?;

        let (schema, id_field, path_field, text_field) = Self::schema();
        let index = Index::create_in_dir(&tantivy_dir, schema)?;
        let mut writer = index.writer(50_000_000)?;
        for chunk in chunks {
            writer.add_document(doc!(
                id_field => chunk.id,
                path_field => chunk.path.clone(),
                text_field => chunk.text.clone(),
            ))?;
        }
        writer.commit()?;
        Ok(())
    }

    fn apply_delta(
        repo_root: &Path,
        chunks: &[ChunkRecord],
        changed_paths: &HashSet<String>,
    ) -> Result<()> {
        if changed_paths.is_empty() {
            return Ok(());
        }
        let tantivy_dir = config::tantivy_path(repo_root)?;
        if !tantivy_dir.exists() {
            return Self::rebuild(repo_root, chunks);
        }

        let bundle = match Self::open_or_rebuild(repo_root, chunks) {
            Ok(bundle) => bundle,
            Err(err) => {
                warn!(
                    "Falling back to full tantivy rebuild after open failure: {:#}",
                    err
                );
                return Self::rebuild(repo_root, chunks);
            }
        };
        let mut writer = bundle.index.writer(50_000_000)?;
        for path in changed_paths {
            writer.delete_term(Term::from_field_text(bundle.path_field, path));
        }
        for chunk in chunks
            .iter()
            .filter(|chunk| changed_paths.contains(&chunk.path))
        {
            writer.add_document(doc!(
                bundle.id_field => chunk.id,
                bundle.path_field => chunk.path.clone(),
                bundle.text_field => chunk.text.clone(),
            ))?;
        }
        writer.commit()?;
        Ok(())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<(u64, f32)>> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let query_parser =
            QueryParser::for_index(&self.index, vec![self.text_field, self.path_field]);
        let parsed = match query_parser.parse_query(query) {
            Ok(parsed) => parsed,
            Err(raw_error) => {
                let sanitized = sanitize_tantivy_query(query);
                if sanitized.is_empty() {
                    warn!(
                        "Skipping lexical search due unparsable empty query fallback. query={:?} error={}",
                        query, raw_error
                    );
                    return Ok(Vec::new());
                }
                match query_parser.parse_query(&sanitized) {
                    Ok(parsed) => parsed,
                    Err(sanitized_error) => {
                        warn!(
                            "Skipping lexical search due tantivy parse failure. query={:?} sanitized={:?} raw_error={} sanitized_error={}",
                            query, sanitized, raw_error, sanitized_error
                        );
                        return Ok(Vec::new());
                    }
                }
            }
        };
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(limit * 3))?;
        let mut out = Vec::new();
        for (score, addr) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(addr)?;
            if let Some(id) = retrieved.get_first(self.id_field).and_then(|v| v.as_u64()) {
                out.push((id, score));
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

fn sanitize_tantivy_query(query: &str) -> String {
    query
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || c.is_ascii_whitespace()
                || matches!(c, '_' | '-' | '/' | '.')
            {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn embed_query(repo_root: &Path, query: &str) -> Result<Option<Vec<f32>>> {
    let _ = repo_root;
    static QUERY_EMBEDDER: OnceLock<Mutex<EmbeddingEngine>> = OnceLock::new();
    let shared = QUERY_EMBEDDER.get_or_init(|| Mutex::new(EmbeddingEngine::new()));
    let mut guard = shared
        .lock()
        .map_err(|_| anyhow::anyhow!("Query embedder lock poisoned"))?;
    guard.embed_query(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_scope::{
        build_basename_allowlist, build_extension_allowlist, is_always_skipped_dir_name,
        is_supported_code_file,
    };

    #[test]
    fn symbol_family_match_supports_pluralized_prefixes() {
        assert!(is_symbol_family_match(
            "usefeaturetogglessetreleasebatchmutation",
            "usefeaturetoggle"
        ));
        assert!(is_symbol_family_match(
            "usefeaturetoggle",
            "usefeaturetogglessetreleasebatchmutation"
        ));
    }

    #[test]
    fn symbol_scoring_includes_family_matches() {
        let token_map = HashMap::from([
            ("usefeaturetoggle".to_string(), vec![1u64]),
            (
                "usefeaturetogglessetreleasebatchmutation".to_string(),
                vec![2u64],
            ),
        ]);
        let query = vec!["usefeaturetoggle".to_string()];
        let ranked =
            score_from_token_map_with_family_lookup(&token_map, None, None, &query, 10, true);
        assert!(ranked.iter().any(|(id, _)| *id == 1));
        assert!(ranked.iter().any(|(id, _)| *id == 2));
    }

    #[test]
    fn extract_symbol_tokens_keeps_simple_titlecase_symbols() {
        let tokens = extract_symbol_tokens("type Plan struct {}\n");
        assert!(tokens.contains(&"plan".to_string()), "got: {tokens:?}");
        assert!(!tokens.contains(&"type".to_string()), "got: {tokens:?}");
    }

    #[test]
    fn extract_definition_name_handles_go_func_lines() {
        assert_eq!(
            extract_definition_name("func (c *Context) Plan() (*plans.Plan, error) {"),
            Some("Plan".to_string())
        );
    }

    #[test]
    fn family_lookup_indexes_return_related_tokens() {
        let token_map = HashMap::from([
            ("usefeaturetoggle".to_string(), vec![1u64]),
            (
                "usefeaturetogglessetreleasebatchmutation".to_string(),
                vec![2u64],
            ),
        ]);
        let (family_to_tokens, family_prefix_to_families) = build_family_lookup_indexes(&token_map);
        let candidates = collect_family_candidate_tokens(
            "usefeaturetoggle",
            &token_map,
            Some(&family_to_tokens),
            Some(&family_prefix_to_families),
        );
        assert!(candidates.iter().any(|token| token == "usefeaturetoggle"));
        assert!(
            candidates
                .iter()
                .any(|token| token == "usefeaturetogglessetreleasebatchmutation")
        );
    }

    #[test]
    fn deleted_paths_are_counted_in_changed_files() {
        let changed = HashSet::from([
            "src/active.rs".to_string(),
            "src/deleted.rs".to_string(),
            "src/new.rs".to_string(),
        ]);
        let current = HashMap::from([
            ("src/active.rs".to_string(), "hash1".to_string()),
            ("src/new.rs".to_string(), "hash2".to_string()),
        ]);
        assert_eq!(count_deleted_paths(&changed, &current), 1);
    }

    #[test]
    fn chunk_fingerprint_and_id_are_stable() {
        let fingerprint = chunk_fingerprint("src/lib.rs", 10, 20, "fn test() {}");
        let mut used_a = HashSet::new();
        let mut used_b = HashSet::new();
        let id_a = allocate_chunk_id(&fingerprint, &mut used_a);
        let id_b = allocate_chunk_id(&fingerprint, &mut used_b);
        assert_eq!(
            fingerprint,
            chunk_fingerprint("src/lib.rs", 10, 20, "fn test() {}")
        );
        assert_eq!(id_a, id_b);
        assert!(id_a <= SQLITE_CHUNK_ID_MAX);
    }

    #[test]
    fn allocated_chunk_ids_are_sqlite_compatible_and_unique() {
        let mut used = HashSet::new();
        for i in 0..2_000 {
            let fingerprint =
                chunk_fingerprint("src/lib.rs", i, i + 1, &format!("fn generated_{i}() {{}}"));
            let id = allocate_chunk_id(&fingerprint, &mut used);
            assert!(id <= SQLITE_CHUNK_ID_MAX);
        }
        assert_eq!(used.len(), 2_000);
    }

    #[test]
    fn embedding_content_hash_is_stable() {
        let a = embedding_content_hash("fn test() { println!(\"hi\"); }");
        let b = embedding_content_hash("fn test() { println!(\"hi\"); }");
        let c = embedding_content_hash("fn test() { println!(\"bye\"); }");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn should_rehash_when_metadata_changes_or_forced() {
        let previous = FileRecord {
            path: "src/lib.rs".to_string(),
            hash: "abc".to_string(),
            size_bytes: 100,
            modified_unix_ms: 1_000,
        };
        assert!(!should_rehash_file(Some(&previous), 100, 1_000, false));
        assert!(should_rehash_file(Some(&previous), 101, 1_000, false));
        assert!(should_rehash_file(Some(&previous), 100, 1_001, false));
        assert!(should_rehash_file(Some(&previous), 100, 1_000, true));
        assert!(should_rehash_file(None, 100, 1_000, false));
    }

    #[test]
    fn hint_paths_are_normalized_and_confined_to_repo() {
        let repo_root = Path::new("/tmp/budi-repo");
        assert_eq!(
            normalize_hint_path(repo_root, "src/lib.rs"),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(
            normalize_hint_path(repo_root, "./src/lib.rs"),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(normalize_hint_path(repo_root, "../secrets.txt"), None);
        assert_eq!(normalize_hint_path(repo_root, "/tmp/other/app.rs"), None);
        assert_eq!(
            normalize_hint_path(repo_root, "/tmp/budi-repo/src/main.rs"),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn changed_set_marks_hinted_deleted_file() {
        let hints = HashSet::from(["src/deleted.rs".to_string()]);
        let previous = HashMap::from([("src/deleted.rs".to_string(), "hash1".to_string())]);
        let current = HashMap::new();
        let changed = calculate_changed_set(false, &hints, &previous, &current);
        assert!(changed.contains("src/deleted.rs"));
    }

    #[test]
    fn graph_signal_links_references_to_defined_symbols() {
        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/service.rs".to_string(),
                start_line: 1,
                end_line: 3,
                symbol_hint: Some("process_order".to_string()),
                text: "pub fn process_order() {}".to_string(),
                embedding: vec![0.0; 4],
            },
            ChunkRecord {
                id: 2,
                path: "src/controller.rs".to_string(),
                start_line: 1,
                end_line: 5,
                symbol_hint: Some("handle_request".to_string()),
                text:
                    "use crate::service::process_order;\nfn handle_request() { process_order(); }"
                        .to_string(),
                embedding: vec![0.0; 4],
            },
        ];
        let (
            _symbol,
            _symbol_family,
            _symbol_family_prefix,
            _path,
            graph,
            _graph_family,
            _graph_family_prefix,
            _doc,
            _chunk_to_graph,
        ) = build_retrieval_signal_indexes(Path::new("."), &chunks);
        let refs = graph.get("process_order").cloned().unwrap_or_default();
        assert!(refs.contains(&2));
        assert!(!refs.contains(&1));
    }

    #[test]
    fn graph_signal_resolves_import_alias_calls() {
        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/service.ts".to_string(),
                start_line: 1,
                end_line: 3,
                symbol_hint: Some("process_order".to_string()),
                text: "export function process_order() { return true; }".to_string(),
                embedding: vec![0.0; 4],
            },
            ChunkRecord {
                id: 2,
                path: "src/controller.ts".to_string(),
                start_line: 1,
                end_line: 5,
                symbol_hint: Some("handle_request".to_string()),
                text: "import { process_order as processOrderAlias } from './service';\nfunction handle_request() { processOrderAlias(); }".to_string(),
                embedding: vec![0.0; 4],
            },
        ];
        let (
            _symbol,
            _symbol_family,
            _symbol_family_prefix,
            _path,
            graph,
            _graph_family,
            _graph_family_prefix,
            _doc,
            _chunk_to_graph,
        ) = build_retrieval_signal_indexes(Path::new("."), &chunks);
        let refs = graph.get("process_order").cloned().unwrap_or_default();
        assert!(refs.contains(&2));
    }

    #[test]
    fn graph_signal_resolves_file_level_imports_when_chunk_omits_import_line() {
        let stamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_nanos();
        let repo_root = std::env::temp_dir().join(format!("budi-file-imports-{stamp}"));
        let src_dir = repo_root.join("src");
        fs::create_dir_all(&src_dir).expect("create temp src dir");
        fs::write(
            src_dir.join("controller.ts"),
            "import { process_order as processOrderAlias } from './service';\nfunction handle_request() { processOrderAlias(); }\n",
        )
        .expect("write temp controller file");

        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/service.ts".to_string(),
                start_line: 1,
                end_line: 3,
                symbol_hint: Some("process_order".to_string()),
                text: "export function process_order() { return true; }".to_string(),
                embedding: vec![0.0; 4],
            },
            ChunkRecord {
                id: 2,
                path: "src/controller.ts".to_string(),
                start_line: 2,
                end_line: 2,
                symbol_hint: Some("handle_request".to_string()),
                text: "processOrderAlias();".to_string(),
                embedding: vec![0.0; 4],
            },
        ];
        let (
            _symbol,
            _symbol_family,
            _symbol_family_prefix,
            _path,
            graph,
            _graph_family,
            _graph_family_prefix,
            _doc,
            _chunk_to_graph,
        ) = build_retrieval_signal_indexes(&repo_root, &chunks);
        let _ = fs::remove_dir_all(&repo_root);
        let refs = graph.get("process_order").cloned().unwrap_or_default();
        assert!(refs.contains(&2));
    }

    #[test]
    fn graph_signal_tracks_callers_per_chunk_not_whole_file() {
        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/service.rs".to_string(),
                start_line: 1,
                end_line: 3,
                symbol_hint: Some("process_order".to_string()),
                text: "pub fn process_order() {}".to_string(),
                embedding: vec![0.0; 4],
            },
            ChunkRecord {
                id: 2,
                path: "src/controller.rs".to_string(),
                start_line: 1,
                end_line: 5,
                symbol_hint: Some("handle_request".to_string()),
                text:
                    "use crate::service::process_order;\nfn handle_request() { process_order(); }"
                        .to_string(),
                embedding: vec![0.0; 4],
            },
            ChunkRecord {
                id: 3,
                path: "src/controller.rs".to_string(),
                start_line: 6,
                end_line: 10,
                symbol_hint: Some("unrelated_helper".to_string()),
                text: "fn unrelated_helper() { let local_flag = true; }".to_string(),
                embedding: vec![0.0; 4],
            },
        ];
        let (
            _symbol,
            _symbol_family,
            _symbol_family_prefix,
            _path,
            graph,
            _graph_family,
            _graph_family_prefix,
            _doc,
            _chunk_to_graph,
        ) = build_retrieval_signal_indexes(Path::new("."), &chunks);
        let refs = graph.get("process_order").cloned().unwrap_or_default();
        assert!(refs.contains(&2));
        assert!(!refs.contains(&3));
    }

    #[test]
    fn call_token_extraction_picks_member_invocations() {
        let tokens =
            extract_call_tokens("await featureClient.fetchFlags(userId);\nrouter.post('/x')");
        assert!(tokens.iter().any(|token| token == "fetchflags"));
        assert!(
            tokens
                .iter()
                .any(|token| token == "featureclient_fetchflags")
        );
        assert!(tokens.iter().any(|token| token == "post"));
    }

    #[test]
    fn call_site_resolution_uses_alias_and_receiver_signals() {
        let aliases = HashMap::from([(
            "src/controller.ts".to_string(),
            HashMap::from([
                ("svc".to_string(), "service".to_string()),
                ("processorderalias".to_string(), "process_order".to_string()),
            ]),
        )]);
        let site = CallSite {
            callee: "processorderalias".to_string(),
            receiver: Some("svc".to_string()),
        };
        let resolved = resolve_call_site_candidates("src/controller.ts", &site, &aliases)
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(resolved.contains("process_order"));
        assert!(resolved.contains("service_processorderalias"));
    }

    #[test]
    fn extract_import_aliases_tracks_namespace_import_sources() {
        let aliases = extract_import_aliases("import * as api from \"./service-client\";");
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "api" && (target == "service_client" || target == "api")
        }));
    }

    #[test]
    fn parse_call_site_preserves_receiver_chain() {
        let site = parse_call_site("api.client.fetchFlags").expect("expected call site");
        assert_eq!(site.callee, "fetchflags");
        assert_eq!(site.receiver.as_deref(), Some("api.client"));
    }

    #[test]
    fn call_site_resolution_expands_namespace_alias_receiver_chain() {
        let aliases = HashMap::from([(
            "src/controller.ts".to_string(),
            HashMap::from([("api".to_string(), "service".to_string())]),
        )]);
        let site = CallSite {
            callee: "fetchflags".to_string(),
            receiver: Some("api.client".to_string()),
        };
        let resolved = resolve_call_site_candidates("src/controller.ts", &site, &aliases)
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(resolved.contains("client_fetchflags"));
        assert!(resolved.contains("service_fetchflags"));
        assert!(resolved.contains("service_client_fetchflags"));
    }

    #[test]
    fn extract_import_aliases_parses_python_wildcard_imports() {
        let aliases = extract_import_aliases("from app.services import *");
        assert!(
            aliases
                .iter()
                .any(|(alias, target)| alias == "*app_services" && target == "app_services")
        );
    }

    #[test]
    fn extract_import_aliases_parses_rust_wildcard_use() {
        let aliases = extract_import_aliases("use crate::services::*;");
        assert!(
            aliases
                .iter()
                .any(|(alias, target)| alias == "*crate_services" && target == "crate_services")
        );
    }

    #[test]
    fn extract_import_aliases_parses_java_import_forms() {
        let aliases = extract_import_aliases(
            "import java.util.List;\nimport static com.acme.MathUtil.max;\nimport static com.acme.MathUtil.*;",
        );
        assert!(
            aliases
                .iter()
                .any(|(alias, target)| alias == "list" && target == "java_util_list")
        );
        assert!(
            aliases
                .iter()
                .any(|(alias, target)| alias == "max" && target == "com_acme_mathutil_max")
        );
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "*com_acme_mathutil" && target == "com_acme_mathutil"
        }));
    }

    #[test]
    fn extract_import_aliases_parses_csharp_using_forms() {
        let aliases = extract_import_aliases(
            "using Alias = Company.Product.FeatureClient;\nusing static Company.Product.MathUtil;\nusing Company.Product.Services;",
        );
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "alias" && target == "company_product_featureclient"
        }));
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "*company_product_mathutil" && target == "company_product_mathutil"
        }));
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "*company_product_services" && target == "company_product_services"
        }));
    }

    #[test]
    fn extract_import_aliases_parses_go_import_block_forms() {
        let aliases = extract_import_aliases(
            "import (\n  \"fmt\"\n  api \"github.com/acme/service/api\"\n  . \"github.com/acme/shared/math\"\n  _ \"github.com/lib/pq\"\n)",
        );
        assert!(
            aliases
                .iter()
                .any(|(alias, target)| alias == "fmt" && target == "fmt")
        );
        assert!(
            aliases.iter().any(|(alias, target)| {
                alias == "api" && target == "github_com_acme_service_api"
            })
        );
        assert!(aliases.iter().any(|(alias, target)| {
            alias == "*github_com_acme_shared_math" && target == "github_com_acme_shared_math"
        }));
        assert!(!aliases.iter().any(|(alias, _)| alias == "_"));
    }

    #[test]
    fn reference_resolution_expands_wildcard_targets() {
        let aliases = HashMap::from([(
            "src/controller.py".to_string(),
            HashMap::from([("*app_services".to_string(), "app_services".to_string())]),
        )]);
        let resolved = resolve_reference_candidates("src/controller.py", "process_order", &aliases)
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(resolved.contains("process_order"));
        assert!(resolved.contains("app_services_process_order"));
    }

    #[test]
    fn reference_candidates_prioritize_exact_alias_over_wildcard() {
        let aliases = HashMap::from([(
            "src/controller.py".to_string(),
            HashMap::from([
                (
                    "process_order".to_string(),
                    "orders_process_order".to_string(),
                ),
                ("*legacy_orders".to_string(), "legacy_orders".to_string()),
            ]),
        )]);
        let candidates =
            resolve_reference_candidates("src/controller.py", "process_order", &aliases);
        assert_eq!(
            candidates.first().map(String::as_str),
            Some("orders_process_order")
        );

        let defined = HashSet::from([
            "orders_process_order".to_string(),
            "legacy_orders_process_order".to_string(),
            "process_order".to_string(),
        ]);
        let selected = first_defined_candidate(&candidates, &defined);
        assert_eq!(selected.as_deref(), Some("orders_process_order"));
    }

    #[test]
    fn reference_candidates_prioritize_exact_namespace_aliases() {
        let aliases = HashMap::from([(
            "src/service.ts".to_string(),
            HashMap::from([
                ("svc".to_string(), "internal_service_client".to_string()),
                ("*legacy_service".to_string(), "legacy_service".to_string()),
            ]),
        )]);
        let candidates = resolve_reference_candidates("src/service.ts", "svc", &aliases);
        assert_eq!(
            candidates.first().map(String::as_str),
            Some("internal_service_client")
        );
    }

    #[test]
    fn first_defined_candidate_prefers_nearest_wildcard_target() {
        let aliases = HashMap::from([(
            "app/services/controller.py".to_string(),
            HashMap::from([
                ("*app_services".to_string(), "app_services".to_string()),
                (
                    "*legacy_services".to_string(),
                    "legacy_services".to_string(),
                ),
            ]),
        )]);
        let candidates =
            resolve_reference_candidates("app/services/controller.py", "process_order", &aliases);
        let defined = HashSet::from([
            "legacy_services_process_order".to_string(),
            "app_services_process_order".to_string(),
        ]);
        let selected = first_defined_candidate(&candidates, &defined);
        assert_eq!(selected.as_deref(), Some("app_services_process_order"));
    }

    #[test]
    fn wildcard_targets_sort_by_import_distance_then_name() {
        let aliases = HashMap::from([
            ("*app_utils".to_string(), "app_utils".to_string()),
            ("*app_services".to_string(), "app_services".to_string()),
            ("*app_core".to_string(), "app_core".to_string()),
        ]);
        let targets = wildcard_import_targets("app/services/controller.py", &aliases);
        assert_eq!(
            targets,
            vec![
                "app_services".to_string(),
                "app_core".to_string(),
                "app_utils".to_string()
            ]
        );
    }

    #[test]
    fn builtin_skip_dirs_cover_common_large_trees() {
        assert!(is_always_skipped_dir_name("node_modules"));
        assert!(is_always_skipped_dir_name("target"));
        assert!(is_always_skipped_dir_name(".venv"));
        assert!(!is_always_skipped_dir_name("src"));
    }

    #[test]
    fn unignore_rules_can_reinclude_builtin_skipped_dirs() {
        let rules = RepoIgnoreRules {
            excludes: build_gitignore_matcher(Path::new("."), &[]).expect("empty excludes"),
            unignores: build_gitignore_matcher(Path::new("."), &["vendor".to_string()])
                .expect("unignore matcher"),
        };
        assert!(!should_skip_index_path("vendor/lib.rs", false, &rules));
    }

    #[test]
    fn exclude_rules_take_precedence_over_unignore_rules() {
        let rules = RepoIgnoreRules {
            excludes: build_gitignore_matcher(Path::new("."), &["vendor".to_string()])
                .expect("exclude matcher"),
            unignores: build_gitignore_matcher(Path::new("."), &["vendor".to_string()])
                .expect("unignore matcher"),
        };
        assert!(should_skip_index_path("vendor/lib.rs", false, &rules));
    }

    #[test]
    fn root_ignore_files_include_dot_ignore_sources() {
        let temp_root = std::env::temp_dir().join(format!(
            "budi-root-ignore-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_millis()
        ));
        fs::create_dir_all(&temp_root).expect("create temp root");
        fs::write(temp_root.join(".cursorignore"), "scratch/**\n").expect("write cursor ignore");
        fs::write(temp_root.join(".codeiumignore"), "generated/**\n")
            .expect("write codeium ignore");
        fs::write(temp_root.join("notes.txt"), "not an ignore file").expect("write notes");

        let discovered = root_ignore_files(&temp_root).expect("discover root ignore files");
        let discovered_names = discovered
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert!(discovered_names.contains(&".cursorignore"));
        assert!(discovered_names.contains(&".codeiumignore"));
        assert!(!discovered_names.contains(&"notes.txt"));

        let rules = load_repo_ignore_rules(&temp_root, &[]).expect("load ignore rules");
        assert!(should_skip_index_path("scratch/tmp.rs", false, &rules));
        assert!(should_skip_index_path("generated/out.rs", false, &rules));

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn cli_override_ignore_patterns_are_applied() {
        let rules = load_repo_ignore_rules(Path::new("."), &["tmp_override/**".to_string()])
            .expect("load rules with overrides");
        assert!(should_skip_index_path(
            "tmp_override/file.rs",
            false,
            &rules
        ));
    }

    #[test]
    fn compiled_index_scope_applies_ignore_and_extension_policy() {
        let temp_root = std::env::temp_dir().join(format!(
            "budi-index-scope-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_millis()
        ));
        fs::create_dir_all(&temp_root).expect("create temp root");
        fs::write(temp_root.join(".cursorignore"), "ignored/**\n").expect("write cursorignore");
        let config = BudiConfig {
            index_extensions: vec!["rs".to_string()],
            ..BudiConfig::default()
        };
        let scope = compile_index_scope(&temp_root, &config, None).expect("compile scope");

        assert!(scope.allows_relative_file_path("src/lib.rs"));
        assert!(!scope.allows_relative_file_path("src/readme.md"));
        assert!(!scope.allows_relative_file_path("ignored/file.rs"));

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn extension_allowlist_normalizes_configured_values() {
        let config = BudiConfig {
            index_extensions: vec![".RS".to_string(), " tsx ".to_string(), "".to_string()],
            ..BudiConfig::default()
        };
        let allowlist = build_extension_allowlist(&config);
        assert!(allowlist.contains("rs"));
        assert!(allowlist.contains("tsx"));
        assert!(!allowlist.contains(".rs"));
    }

    #[test]
    fn basename_allowlist_normalizes_configured_values() {
        let config = BudiConfig {
            index_basenames: vec![
                " Dockerfile ".to_string(),
                "MAKEFILE".to_string(),
                "".to_string(),
            ],
            ..BudiConfig::default()
        };
        let allowlist = build_basename_allowlist(&config);
        assert!(allowlist.contains("dockerfile"));
        assert!(allowlist.contains("makefile"));
    }

    #[test]
    fn extensionless_basenames_can_be_indexed_by_policy() {
        let config = BudiConfig {
            index_extensions: vec!["rs".to_string()],
            index_basenames: vec!["Dockerfile".to_string(), "Makefile".to_string()],
            ..BudiConfig::default()
        };
        let ext_allowlist = build_extension_allowlist(&config);
        let basename_allowlist = build_basename_allowlist(&config);
        assert!(is_supported_code_file(
            Path::new("Dockerfile"),
            &ext_allowlist,
            &basename_allowlist
        ));
        assert!(is_supported_code_file(
            Path::new("Makefile"),
            &ext_allowlist,
            &basename_allowlist
        ));
        assert!(!is_supported_code_file(
            Path::new("README"),
            &ext_allowlist,
            &basename_allowlist
        ));
    }

    #[test]
    fn default_extension_policy_prunes_docs_and_config_noise() {
        let allowlist = build_extension_allowlist(&BudiConfig::default());
        assert!(!allowlist.contains("md"));
        assert!(!allowlist.contains("yaml"));
        assert!(!allowlist.contains("toml"));
        assert!(allowlist.contains("rs"));
        assert!(allowlist.contains("py"));
    }

    #[test]
    fn reconcile_missing_embeddings_is_noop_when_backend_unavailable() {
        let mut chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 8,
                symbol_hint: None,
                text: "fn alpha() {}".to_string(),
                embedding: Vec::new(),
            },
            ChunkRecord {
                id: 2,
                path: "src/lib.rs".to_string(),
                start_line: 10,
                end_line: 18,
                symbol_hint: None,
                text: "fn beta() {}".to_string(),
                embedding: vec![0.2, 0.4],
            },
        ];
        let mut embedder = EmbeddingEngine {
            backend: EmbeddingBackend::Unavailable,
        };
        let mut cache = EmbeddingCacheState::default();
        let mut cache_dirty = false;
        let repaired = reconcile_missing_chunk_embeddings(
            &mut chunks,
            &mut embedder,
            &mut cache,
            &mut cache_dirty,
            8,
            2,
            1,
        )
        .expect("reconcile should succeed");
        assert_eq!(repaired, 0);
        assert!(chunks[0].embedding.is_empty());
        assert!(!cache_dirty);
    }

    #[test]
    fn infer_expected_embedding_dims_prefers_majority_dimension() {
        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn a() {}".to_string(),
                embedding: vec![0.1, 0.2, 0.3],
            },
            ChunkRecord {
                id: 2,
                path: "src/b.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn b() {}".to_string(),
                embedding: vec![0.4, 0.5, 0.6],
            },
            ChunkRecord {
                id: 3,
                path: "src/c.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn c() {}".to_string(),
                embedding: vec![0.9, 1.0],
            },
        ];
        assert_eq!(infer_expected_embedding_dims(&chunks), Some(3));
    }

    #[test]
    fn sanitize_chunk_embeddings_clears_invalid_vectors() {
        let mut chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/a.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn a() {}".to_string(),
                embedding: vec![0.1, 0.2, 0.3],
            },
            ChunkRecord {
                id: 2,
                path: "src/b.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn b() {}".to_string(),
                embedding: vec![0.4, f32::NAN, 0.6],
            },
            ChunkRecord {
                id: 3,
                path: "src/c.rs".to_string(),
                start_line: 1,
                end_line: 1,
                symbol_hint: None,
                text: "fn c() {}".to_string(),
                embedding: vec![0.9, 1.0],
            },
        ];
        let invalid = sanitize_chunk_embeddings(&mut chunks);
        assert_eq!(invalid, 2);
        assert_eq!(chunks[0].embedding.len(), 3);
        assert!(chunks[1].embedding.is_empty());
        assert!(chunks[2].embedding.is_empty());
    }
}
