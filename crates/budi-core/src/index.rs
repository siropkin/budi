use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::Utc;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use hnsw_rs::prelude::*;
use ignore::WalkBuilder;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    FAST, INDEXED, STORED, STRING, Schema, SchemaBuilder, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, doc};
use tracing::{info, warn};

use crate::chunking::chunk_text;
use crate::config::{self, BudiConfig};
use crate::git;

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
    pub updated_at_ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoIndexState {
    pub repo_root: String,
    pub branch: String,
    pub head: String,
    pub files: Vec<FileRecord>,
    pub chunks: Vec<ChunkRecord>,
    pub updated_at_ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildReport {
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub changed_files: usize,
}

pub struct RuntimeIndex {
    pub state: RepoIndexState,
    id_to_chunk: HashMap<u64, ChunkRecord>,
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
    tantivy: TantivyBundle,
    symbol_to_chunk_ids: HashMap<String, Vec<u64>>,
    path_token_to_chunk_ids: HashMap<String, Vec<u64>>,
    graph_token_to_chunk_ids: HashMap<String, Vec<u64>>,
    doc_like_chunk_ids: HashSet<u64>,
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
            path_token_to_chunk_ids,
            graph_token_to_chunk_ids,
            doc_like_chunk_ids,
        ) = build_retrieval_signal_indexes(&state.chunks);
        Ok(Self {
            state,
            id_to_chunk,
            hnsw,
            tantivy,
            symbol_to_chunk_ids,
            path_token_to_chunk_ids,
            graph_token_to_chunk_ids,
            doc_like_chunk_ids,
        })
    }

    pub fn chunk(&self, id: u64) -> Option<&ChunkRecord> {
        self.id_to_chunk.get(&id)
    }

    pub fn search_lexical(&self, query: &str, limit: usize) -> Result<Vec<(u64, f32)>> {
        self.tantivy.search(query, limit)
    }

    pub fn search_vector(&self, query_embedding: &[f32], limit: usize) -> Vec<(u64, f32)> {
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
        score_from_token_map(&self.symbol_to_chunk_ids, query_tokens, limit, true)
    }

    pub fn search_path_tokens(&self, query_tokens: &[String], limit: usize) -> Vec<(u64, f32)> {
        score_from_token_map(&self.path_token_to_chunk_ids, query_tokens, limit, false)
    }

    pub fn search_graph_tokens(&self, query_tokens: &[String], limit: usize) -> Vec<(u64, f32)> {
        score_from_token_map(&self.graph_token_to_chunk_ids, query_tokens, limit, true)
    }

    pub fn is_doc_like_chunk(&self, chunk_id: u64) -> bool {
        self.doc_like_chunk_ids.contains(&chunk_id)
    }

    pub fn all_chunks(&self) -> &[ChunkRecord] {
        &self.state.chunks
    }
}

#[derive(Debug)]
pub struct IndexWorkspace {
    pub state: RepoIndexState,
    pub report: IndexBuildReport,
}

type RetrievalSignalIndexes = (
    HashMap<String, Vec<u64>>,
    HashMap<String, Vec<u64>>,
    HashMap<String, Vec<u64>>,
    HashSet<u64>,
);

#[derive(Debug)]
struct PendingChunk {
    id: u64,
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

pub fn build_or_update(
    repo_root: &Path,
    config: &BudiConfig,
    hard: bool,
    changed_hint: Option<&[String]>,
    mut progress_cb: Option<&mut dyn FnMut(IndexBuildProgress)>,
) -> Result<IndexWorkspace> {
    let git_snapshot = git::snapshot(repo_root)?;
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
        )?
    } else {
        build_current_files_from_discovery(
            repo_root,
            config,
            &previous_files_by_path,
            hard,
            &hinted_paths,
        )?
    };

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
        let should_process = hard
            || changed_set.contains(&file.path)
            || !previous_chunks_by_path.contains_key(&file.path);
        if !should_process {
            if let Some(existing) = previous_chunks_by_path.get(&file.path) {
                for chunk in existing {
                    used_chunk_ids.insert(chunk.id);
                }
                chunks.extend(existing.clone());
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

        let mut pending = Vec::with_capacity(chunked.len());
        let mut missing_passages = Vec::new();
        let mut missing_positions = Vec::new();
        for chunk in chunked {
            let fingerprint =
                chunk_fingerprint(&file.path, chunk.start_line, chunk.end_line, &chunk.text);
            let id = allocate_chunk_id(&fingerprint, &mut used_chunk_ids);
            let embedding_cache_key = embedding_content_hash(&chunk.text);
            let embedding = previous_embeddings_by_fingerprint
                .get(&fingerprint)
                .cloned()
                .or_else(|| embedding_cache.entries.get(&embedding_cache_key).cloned());
            if embedding.is_none() {
                missing_positions.push(pending.len());
                missing_passages.push(format!("passage: {}", chunk.text));
            }
            pending.push(PendingChunk {
                id,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol_hint: chunk.symbol_hint,
                text: chunk.text,
                embedding_cache_key,
                embedding,
            });
        }
        if !missing_passages.is_empty() {
            let new_embeddings = embedder.embed_passages(&missing_passages)?;
            for (position, embedding) in missing_positions.into_iter().zip(new_embeddings) {
                if let Some(chunk) = pending.get_mut(position) {
                    if embedding_cache
                        .entries
                        .insert(chunk.embedding_cache_key.clone(), embedding.clone())
                        .is_none()
                    {
                        embedding_cache_dirty = true;
                    }
                    chunk.embedding = Some(embedding);
                }
            }
        }
        for chunk in pending {
            let embedding = chunk
                .embedding
                .unwrap_or_else(|| hash_embedding(chunk.text.as_str()));
            if embedding_cache
                .entries
                .insert(chunk.embedding_cache_key.clone(), embedding.clone())
                .is_none()
            {
                embedding_cache_dirty = true;
            }
            chunks.push(ChunkRecord {
                id: chunk.id,
                path: file.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol_hint: chunk.symbol_hint,
                text: chunk.text.clone(),
                embedding,
                updated_at_ts: Utc::now().timestamp(),
            });
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
    }
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
        branch: git_snapshot.branch,
        head: git_snapshot.head,
        files: current_files,
        chunks,
        updated_at_ts: Utc::now().timestamp(),
    };
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
    save_state(repo_root, &state)?;
    emit_progress(
        &mut progress_cb,
        IndexBuildProgress {
            phase: "rebuilding-lexical-index".to_string(),
            total_files: total_files_to_process,
            processed_files,
            changed_files,
            current_file: None,
            done: false,
        },
    );
    TantivyBundle::rebuild(repo_root, &state.chunks)?;

    let report = IndexBuildReport {
        indexed_files: state.files.len(),
        indexed_chunks: state.chunks.len(),
        changed_files,
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
        "SELECT id, path, start_line, end_line, symbol_hint, text, embedding, updated_at_ts
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
                "Invalid embedding payload for chunk id={} path={}, recomputing fallback embedding",
                id_i64, path
            );
            hash_embedding(text.as_str())
        });
        Ok(ChunkRecord {
            id: u64::try_from(id_i64).unwrap_or_default(),
            path,
            start_line: usize::try_from(start_line_i64).unwrap_or_default(),
            end_line: usize::try_from(end_line_i64).unwrap_or_default(),
            symbol_hint: row.get(4)?,
            text,
            embedding,
            updated_at_ts: row.get(7)?,
        })
    })?;
    for row in chunk_rows {
        chunks.push(row?);
    }

    let repo_root_value =
        load_meta_value(&conn, "repo_root")?.unwrap_or_else(|| repo_root.display().to_string());
    let branch = load_meta_value(&conn, "branch")?.unwrap_or_else(|| "unknown".to_string());
    let head = load_meta_value(&conn, "head")?.unwrap_or_else(|| "unknown".to_string());
    let updated_at_ts = load_meta_value(&conn, "updated_at_ts")?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();

    Ok(Some(RepoIndexState {
        repo_root: repo_root_value,
        branch,
        head,
        files,
        chunks,
        updated_at_ts,
    }))
}

pub fn save_state(repo_root: &Path, state: &RepoIndexState) -> Result<()> {
    let path = config::index_db_path(repo_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }

    let mut conn = open_index_db(repo_root)?;
    ensure_index_db_schema(&conn)?;
    let tx = conn.transaction()?;

    tx.execute("DELETE FROM meta", [])?;
    tx.execute("DELETE FROM files", [])?;
    tx.execute("DELETE FROM chunks", [])?;

    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)",
        params!["repo_root", &state.repo_root],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)",
        params!["branch", &state.branch],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)",
        params!["head", &state.head],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)",
        params!["updated_at_ts", state.updated_at_ts.to_string()],
    )?;

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
            "INSERT INTO chunks(id, path, start_line, end_line, symbol_hint, text, embedding, updated_at_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for chunk in &state.chunks {
            chunk_stmt.execute(params![
                i64::try_from(chunk.id).unwrap_or(i64::MAX),
                &chunk.path,
                i64::try_from(chunk.start_line).unwrap_or(i64::MAX),
                i64::try_from(chunk.end_line).unwrap_or(i64::MAX),
                chunk.symbol_hint.as_deref(),
                &chunk.text,
                encode_embedding(&chunk.embedding),
                chunk.updated_at_ts,
            ])?;
        }
    }

    tx.commit()?;

    cleanup_legacy_index_files(repo_root)?;
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
            embedding BLOB NOT NULL,
            updated_at_ts INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
        ",
    )?;
    Ok(())
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

fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for value in embedding {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

fn cleanup_legacy_index_files(repo_root: &Path) -> Result<()> {
    let paths = config::repo_paths(repo_root)?;
    let legacy_state = paths.index_dir.join("state.json");
    let legacy_manifest = paths.index_dir.join("manifest.json");
    for file in [legacy_state, legacy_manifest] {
        if file.exists() {
            fs::remove_file(&file)
                .with_context(|| format!("Failed removing legacy file {}", file.display()))?;
        }
    }
    Ok(())
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
    let raw =
        fs::read_to_string(&path).with_context(|| format!("Failed reading {}", path.display()))?;
    let cache = serde_json::from_str(&raw)
        .with_context(|| "Invalid global embedding cache JSON".to_string())?;
    Ok(cache)
}

fn save_embedding_cache(cache: &EmbeddingCacheState) -> Result<()> {
    let path = config::embedding_cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(cache)?;
    fs::write(&path, raw).with_context(|| format!("Failed writing {}", path.display()))?;
    Ok(())
}

fn build_hnsw(chunks: &[ChunkRecord]) -> Result<Option<Hnsw<'static, f32, DistCosine>>> {
    if chunks.is_empty() {
        return Ok(None);
    }
    let max_nb_conn = 32usize;
    let nb_elem = chunks.len();
    let nb_layer = 16usize.min((nb_elem as f32).ln().trunc() as usize).max(1);
    let ef_construction = 256usize;
    let mut hnsw: Hnsw<'static, f32, DistCosine> = Hnsw::new(
        max_nb_conn,
        nb_elem,
        nb_layer,
        ef_construction,
        DistCosine {},
    );

    let insert_data: Vec<(&[f32], usize)> = chunks
        .iter()
        .map(|chunk| (chunk.embedding.as_slice(), chunk.id as usize))
        .collect();
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
) -> Result<(Vec<FileRecord>, HashMap<String, String>)> {
    let files = discover_source_files(repo_root, config)?;
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
) -> Result<(Vec<FileRecord>, HashMap<String, String>)> {
    let mut files_by_path = previous_files_by_path.clone();
    for relative in hinted_paths {
        let absolute = repo_root.join(relative);
        if !absolute.exists() || !absolute.is_file() || !is_supported_code_file(&absolute) {
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

fn build_retrieval_signal_indexes(chunks: &[ChunkRecord]) -> RetrievalSignalIndexes {
    let mut symbol_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut path_token_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut graph_token_to_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();
    let mut doc_like_chunk_ids: HashSet<u64> = HashSet::new();
    let mut defined_tokens: HashSet<String> = HashSet::new();
    let mut file_import_aliases: HashMap<String, HashMap<String, String>> = HashMap::new();
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
        for (alias, target) in extract_import_aliases(&chunk.text) {
            file_import_aliases
                .entry(chunk.path.clone())
                .or_default()
                .insert(alias, target);
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
                resolve_call_site_tokens(path, call_site, &file_import_aliases);
            for resolved in resolved_candidates {
                if !defined_tokens.contains(&resolved) {
                    continue;
                }
                graph_token_to_chunk_ids
                    .entry(resolved)
                    .or_default()
                    .push(*chunk_id);
            }
        }
    }

    for (chunk_id, references) in &chunk_reference_tokens {
        let Some(path) = chunk_path_by_id.get(chunk_id) else {
            continue;
        };
        for token in references {
            let resolved_candidates = resolve_reference_token(path, token, &file_import_aliases);
            for resolved in resolved_candidates {
                if !defined_tokens.contains(&resolved) {
                    continue;
                }
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

    (
        symbol_to_chunk_ids,
        path_token_to_chunk_ids,
        graph_token_to_chunk_ids,
        doc_like_chunk_ids,
    )
}

fn dedup_index_values(map: &mut HashMap<String, Vec<u64>>) {
    for ids in map.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
}

fn score_from_token_map(
    token_map: &HashMap<String, Vec<u64>>,
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
        for (indexed_token, ids) in token_map {
            if indexed_token == token || !is_symbol_family_match(indexed_token, token) {
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
    let receiver = receiver_raw.and_then(last_signal_token);
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

fn resolve_reference_token(
    path: &str,
    token: &str,
    file_import_aliases: &HashMap<String, HashMap<String, String>>,
) -> HashSet<String> {
    let mut resolved = HashSet::new();
    if !token.is_empty() {
        resolved.insert(token.to_string());
    }
    if let Some(target) = file_import_aliases
        .get(path)
        .and_then(|aliases| aliases.get(token))
    {
        resolved.insert(target.clone());
    }
    resolved
}

fn resolve_call_site_tokens(
    path: &str,
    call_site: &CallSite,
    file_import_aliases: &HashMap<String, HashMap<String, String>>,
) -> HashSet<String> {
    let mut resolved = resolve_reference_token(path, &call_site.callee, file_import_aliases);
    if let Some(receiver) = call_site.receiver.as_deref() {
        let receiver_candidates = resolve_reference_token(path, receiver, file_import_aliases);
        for receiver_candidate in receiver_candidates {
            if let Some(combined) =
                combine_receiver_method_token(&receiver_candidate, &call_site.callee)
            {
                resolved.insert(combined);
            }
        }
        if let Some(combined) = combine_receiver_method_token(receiver, &call_site.callee) {
            resolved.insert(combined);
        }
    }
    resolved
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
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("import ") && trimmed.contains(" from ") {
            let after_import = trimmed.trim_start_matches("import ").trim();
            if let Some((lhs, _)) = after_import.split_once(" from ") {
                let lhs = lhs.trim();
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
            }
            continue;
        }
        if trimmed.starts_with("from ") && trimmed.contains(" import ") {
            if let Some((_, imported)) = trimmed.split_once(" import ") {
                for clause in imported.split(',') {
                    if let Some((alias, target)) = parse_import_alias_clause(clause) {
                        push_alias_pair(&mut out, &mut seen, (alias, target));
                    }
                }
            }
            continue;
        }
        if trimmed.starts_with("import ") {
            let imported = trimmed.trim_start_matches("import ").trim();
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
            if body.contains('{') && body.contains('}') {
                if let Some((prefix, rest)) = body.split_once('{')
                    && let Some((inside, _)) = rest.split_once('}')
                {
                    for clause in inside.split(',') {
                        let piece = clause.trim();
                        if piece.is_empty() {
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
        if let Some(alias) = parse_require_alias(trimmed) {
            push_alias_pair(&mut out, &mut seen, (alias.clone(), alias));
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
    if !(has_underscore || has_digit || has_symbol_case_pattern(raw)) {
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

fn discover_source_files(repo_root: &Path, config: &BudiConfig) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(repo_root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    let local_ignore = config::ignore_path(repo_root)?;
    if local_ignore.exists() {
        builder.add_ignore(local_ignore);
    }

    for entry in builder.build() {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_supported_code_file(path) {
            continue;
        }
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() as usize > config.max_file_bytes {
            continue;
        }
        files.push(path.to_path_buf());
    }
    Ok(files)
}

fn is_supported_code_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|v| v.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "cpp"
            | "cc"
            | "cxx"
            | "c"
            | "h"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "scala"
            | "sql"
            | "sh"
            | "yaml"
            | "yml"
            | "toml"
            | "md"
            | "graphql"
            | "proto"
            | "tf"
    )
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
        let id = u64::from_le_bytes(id_bytes);
        if used_chunk_ids.insert(id) {
            return id;
        }
        nonce = nonce.saturating_add(1);
    }
}

enum EmbeddingBackend {
    Fast(Box<TextEmbedding>),
    Fallback,
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
                    "Falling back to deterministic hashing embeddings because fastembed failed: {}",
                    err
                );
                Self {
                    backend: EmbeddingBackend::Fallback,
                }
            }
        }
    }

    fn embed_passages(&mut self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        match &mut self.backend {
            EmbeddingBackend::Fast(embedder) => Ok(embedder.embed(docs, None)?),
            EmbeddingBackend::Fallback => Ok(docs.iter().map(|d| hash_embedding(d)).collect()),
        }
    }

    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        match &mut self.backend {
            EmbeddingBackend::Fast(embedder) => {
                let rows = embedder.embed(vec![format!("query: {}", query)], None)?;
                Ok(rows
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| hash_embedding(query)))
            }
            EmbeddingBackend::Fallback => Ok(hash_embedding(query)),
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

fn hash_embedding(text: &str) -> Vec<f32> {
    const DIMS: usize = 384;
    let mut vec = vec![0f32; DIMS];
    for token in text.split_whitespace() {
        let mut hasher = blake3::Hasher::new();
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();
        let bytes = digest.as_bytes();
        let idx = (u16::from_le_bytes([bytes[0], bytes[1]]) as usize) % DIMS;
        let sign = if bytes[2].is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        let mag = (bytes[3] as f32 / 255.0) + 0.01;
        vec[idx] += sign * mag;
    }
    normalize(vec)
}

fn normalize(mut vec: Vec<f32>) -> Vec<f32> {
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for item in &mut vec {
            *item /= norm;
        }
    }
    vec
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

pub fn embed_query(repo_root: &Path, query: &str) -> Result<Vec<f32>> {
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
        let ranked = score_from_token_map(&token_map, &query, 10, true);
        assert!(ranked.iter().any(|(id, _)| *id == 1));
        assert!(ranked.iter().any(|(id, _)| *id == 2));
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
                updated_at_ts: 0,
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
                updated_at_ts: 0,
            },
        ];
        let (_symbol, _path, graph, _doc) = build_retrieval_signal_indexes(&chunks);
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
                updated_at_ts: 0,
            },
            ChunkRecord {
                id: 2,
                path: "src/controller.ts".to_string(),
                start_line: 1,
                end_line: 5,
                symbol_hint: Some("handle_request".to_string()),
                text: "import { process_order as processOrderAlias } from './service';\nfunction handle_request() { processOrderAlias(); }".to_string(),
                embedding: vec![0.0; 4],
                updated_at_ts: 0,
            },
        ];
        let (_symbol, _path, graph, _doc) = build_retrieval_signal_indexes(&chunks);
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
                updated_at_ts: 0,
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
                updated_at_ts: 0,
            },
            ChunkRecord {
                id: 3,
                path: "src/controller.rs".to_string(),
                start_line: 6,
                end_line: 10,
                symbol_hint: Some("unrelated_helper".to_string()),
                text: "fn unrelated_helper() { let local_flag = true; }".to_string(),
                embedding: vec![0.0; 4],
                updated_at_ts: 0,
            },
        ];
        let (_symbol, _path, graph, _doc) = build_retrieval_signal_indexes(&chunks);
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
        let resolved = resolve_call_site_tokens("src/controller.ts", &site, &aliases);
        assert!(resolved.contains("process_order"));
        assert!(resolved.contains("service_processorderalias"));
    }
}
