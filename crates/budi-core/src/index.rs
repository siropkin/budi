use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use chrono::Utc;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use hnsw_rs::prelude::*;
use ignore::WalkBuilder;
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
    pub branch: String,
    pub head: String,
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
    pub next_id: u64,
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

    pub fn search_lexical(
        &self,
        query: &str,
        branch: &str,
        limit: usize,
    ) -> Result<Vec<(u64, f32)>> {
        self.tantivy.search(query, branch, limit)
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
    embedding: Option<Vec<f32>>,
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

    let files = discover_source_files(repo_root, config)?;
    let mut current_files = Vec::new();
    let mut current_hashes = HashMap::new();
    let hinted_paths: HashSet<String> = changed_hint
        .map(|paths| {
            paths
                .iter()
                .map(|path| relativize(repo_root, path))
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();

    for file in files {
        let relative = file
            .strip_prefix(repo_root)
            .unwrap_or(&file)
            .to_string_lossy()
            .to_string();
        let metadata = match fs::metadata(&file) {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!("Skipping unreadable file metadata {}: {}", relative, err);
                continue;
            }
        };
        let size_bytes = metadata.len();
        let modified_unix_ms = metadata
            .modified()
            .ok()
            .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or_default();
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

    let changed_set = calculate_changed_set(
        hard,
        changed_hint,
        &previous_hashes,
        &current_hashes,
        repo_root,
    )?;

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
            let embedding = previous_embeddings_by_fingerprint
                .get(&fingerprint)
                .cloned();
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
                embedding,
            });
        }
        if !missing_passages.is_empty() {
            let new_embeddings = embedder.embed_passages(&missing_passages)?;
            for (position, embedding) in missing_positions.into_iter().zip(new_embeddings) {
                if let Some(chunk) = pending.get_mut(position) {
                    chunk.embedding = Some(embedding);
                }
            }
        }
        for chunk in pending {
            chunks.push(ChunkRecord {
                id: chunk.id,
                path: file.path.clone(),
                branch: git_snapshot.branch.clone(),
                head: git_snapshot.head.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol_hint: chunk.symbol_hint,
                text: chunk.text.clone(),
                embedding: chunk
                    .embedding
                    .unwrap_or_else(|| hash_embedding(chunk.text.as_str())),
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

    chunks.sort_by(|a, b| (&a.path, a.start_line).cmp(&(&b.path, b.start_line)));
    let next_id = chunks
        .iter()
        .map(|chunk| chunk.id)
        .max()
        .unwrap_or_default()
        .saturating_add(1);

    let state = RepoIndexState {
        repo_root: repo_root.display().to_string(),
        branch: git_snapshot.branch,
        head: git_snapshot.head,
        files: current_files,
        chunks,
        next_id,
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
    let path = config::state_path(repo_root)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("Failed reading {}", path.display()))?;
    let state: RepoIndexState =
        serde_json::from_str(&raw).with_context(|| "Invalid budi index state".to_string())?;
    Ok(Some(state))
}

pub fn save_state(repo_root: &Path, state: &RepoIndexState) -> Result<()> {
    let path = config::state_path(repo_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(state)?;
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
    changed_hint: Option<&[String]>,
    previous_hashes: &HashMap<String, String>,
    current_hashes: &HashMap<String, String>,
    repo_root: &Path,
) -> Result<HashSet<String>> {
    if hard {
        return Ok(current_hashes.keys().cloned().collect());
    }
    let mut changed = HashSet::new();
    if let Some(hint) = changed_hint {
        for file in hint {
            let rel = relativize(repo_root, file);
            if current_hashes.contains_key(&rel) {
                changed.insert(rel);
            }
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
    Ok(changed)
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

fn relativize(repo_root: &Path, file: &str) -> String {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        match path.strip_prefix(repo_root) {
            Ok(stripped) => stripped.to_string_lossy().to_string(),
            Err(_) => file.to_string(),
        }
    } else {
        file.to_string()
    }
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
    let mut file_reference_tokens: HashMap<String, HashSet<String>> = HashMap::new();
    let mut file_chunk_ids: HashMap<String, Vec<u64>> = HashMap::new();

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
        let references = extract_reference_tokens(&chunk.text);
        if !references.is_empty() {
            let entry = file_reference_tokens.entry(chunk.path.clone()).or_default();
            for token in references {
                entry.insert(token);
            }
        }
        file_chunk_ids
            .entry(chunk.path.clone())
            .or_default()
            .push(chunk.id);

        let path_tokens = extract_path_tokens(&chunk.path);
        for token in path_tokens {
            path_token_to_chunk_ids
                .entry(token)
                .or_default()
                .push(chunk.id);
        }
    }

    for (path, references) in &file_reference_tokens {
        let Some(chunk_ids) = file_chunk_ids.get(path) else {
            continue;
        };
        for token in references {
            if !defined_tokens.contains(token) {
                continue;
            }
            graph_token_to_chunk_ids
                .entry(token.clone())
                .or_default()
                .extend(chunk_ids.iter().copied());
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
    branch_field: tantivy::schema::Field,
    text_field: tantivy::schema::Field,
}

impl TantivyBundle {
    fn schema() -> (
        Schema,
        tantivy::schema::Field,
        tantivy::schema::Field,
        tantivy::schema::Field,
        tantivy::schema::Field,
    ) {
        let mut builder = SchemaBuilder::default();
        let id_field = builder.add_u64_field("id", STORED | FAST | INDEXED);
        let path_field = builder.add_text_field("path", STRING | STORED);
        let branch_field = builder.add_text_field("branch", STRING | STORED);
        let text_options = TextOptions::default()
            .set_stored()
            .set_indexing_options(TextFieldIndexing::default().set_tokenizer("default"));
        let text_field = builder.add_text_field("text", text_options);
        let schema = builder.build();
        (schema, id_field, path_field, branch_field, text_field)
    }

    fn open_or_rebuild(repo_root: &Path, chunks: &[ChunkRecord]) -> Result<Self> {
        let tantivy_dir = config::tantivy_path(repo_root)?;
        if !tantivy_dir.exists() {
            Self::rebuild(repo_root, chunks)?;
        }
        let (schema, id_field, path_field, branch_field, text_field) = Self::schema();
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
            branch_field,
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

        let (schema, id_field, path_field, branch_field, text_field) = Self::schema();
        let index = Index::create_in_dir(&tantivy_dir, schema)?;
        let mut writer = index.writer(50_000_000)?;
        for chunk in chunks {
            writer.add_document(doc!(
                id_field => chunk.id,
                path_field => chunk.path.clone(),
                branch_field => chunk.branch.clone(),
                text_field => chunk.text.clone(),
            ))?;
        }
        writer.commit()?;
        Ok(())
    }

    fn search(&self, query: &str, branch: &str, limit: usize) -> Result<Vec<(u64, f32)>> {
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
            let doc_branch = retrieved
                .get_first(self.branch_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if doc_branch != branch {
                continue;
            }
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
    fn graph_signal_links_references_to_defined_symbols() {
        let chunks = vec![
            ChunkRecord {
                id: 1,
                path: "src/service.rs".to_string(),
                branch: "main".to_string(),
                head: "abc".to_string(),
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
                branch: "main".to_string(),
                head: "abc".to_string(),
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
}
