//! Cross-encoder reranker using ms-marco-MiniLM-L-6-v2 (ONNX, int8 quantized).
//!
//! Sits on top of the 5-channel retrieval pipeline as a final precision pass.
//! Takes the top-N fused candidates, scores each (query, chunk_text) pair through
//! the cross-encoder, and returns adjusted scores.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use tokenizers::Tokenizer;
use tracing::{debug, info, warn};

use crate::config;

// ── Constants ─────────────────────────────────────────────────────────────────

const MODEL_REPO: &str = "cross-encoder/ms-marco-MiniLM-L-6-v2";
const TOKENIZER_FILENAME: &str = "tokenizer.json";

/// Platform-appropriate quantized model filename.
#[cfg(target_arch = "aarch64")]
const MODEL_FILENAME: &str = "model_qint8_arm64.onnx";
#[cfg(not(target_arch = "aarch64"))]
const MODEL_FILENAME: &str = "model_quint8_avx2.onnx";
/// Max sequence length for the cross-encoder (BERT-base limit).
const MAX_SEQ_LEN: usize = 512;
/// How many top candidates to rerank per query.
pub const DEFAULT_RERANK_TOPK: usize = 20;

// ── Model download / cache ────────────────────────────────────────────────────

fn reranker_cache_dir() -> Result<PathBuf> {
    let dir = config::budi_home_dir()?.join("reranker-cache");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating reranker cache dir: {}", dir.display()))?;
    Ok(dir)
}

fn model_path() -> Result<PathBuf> {
    Ok(reranker_cache_dir()?.join(MODEL_FILENAME))
}

fn tokenizer_path() -> Result<PathBuf> {
    Ok(reranker_cache_dir()?.join(TOKENIZER_FILENAME))
}

/// Download a file from HuggingFace if not already cached.
fn ensure_file(filename: &str) -> Result<PathBuf> {
    let cache_dir = reranker_cache_dir()?;
    let local_path = cache_dir.join(filename);
    if local_path.exists() {
        return Ok(local_path);
    }
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        MODEL_REPO,
        if filename == MODEL_FILENAME {
            format!("onnx/{}", filename)
        } else {
            filename.to_string()
        }
    );
    info!(
        "Downloading reranker file: {} → {}",
        url,
        local_path.display()
    );

    // Use a temp file + rename for atomicity.
    let tmp_path = cache_dir.join(format!(".{}.tmp", filename));
    let resp = reqwest::blocking::get(&url).with_context(|| format!("downloading {}", url))?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} downloading {}", resp.status(), url);
    }
    let bytes = resp.bytes()?;
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &local_path)?;
    info!(
        "Downloaded {} ({:.1} MB)",
        filename,
        bytes.len() as f64 / 1_048_576.0
    );
    Ok(local_path)
}

// ── Reranker engine ───────────────────────────────────────────────────────────

struct RerankerEngine {
    session: ort::session::Session,
    tokenizer: Tokenizer,
}

impl RerankerEngine {
    fn new() -> Result<Self> {
        let model_file = ensure_file(MODEL_FILENAME)?;
        let tokenizer_file = ensure_file(TOKENIZER_FILENAME)?;

        let session = ort::session::Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
            .with_intra_threads(2)?
            .commit_from_file(&model_file)
            .with_context(|| format!("loading ONNX model: {}", model_file.display()))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_file)
            .map_err(|e| anyhow::anyhow!("loading tokenizer: {}", e))?;

        info!("Reranker loaded: {}", model_file.display());
        Ok(Self { session, tokenizer })
    }

    /// Score a batch of (query, document) pairs. Returns one logit per pair.
    fn score_pairs(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let batch_size = documents.len();
        let mut all_input_ids: Vec<i64> = Vec::new();
        let mut all_attention_mask: Vec<i64> = Vec::new();
        let mut all_token_type_ids: Vec<i64> = Vec::new();

        // Tokenize all pairs first to find the max sequence length.
        let mut encodings = Vec::with_capacity(batch_size);
        for doc in documents {
            let encoding = self
                .tokenizer
                .encode((query, *doc), true)
                .map_err(|e| anyhow::anyhow!("tokenization error: {}", e))?;
            encodings.push(encoding);
        }

        // Find max length (capped at MAX_SEQ_LEN).
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len().min(MAX_SEQ_LEN))
            .max()
            .unwrap_or(0);

        // Pad to uniform length and flatten into batch tensors.
        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();
            let len = ids.len().min(seq_len);

            // Truncated tokens
            all_input_ids.extend(ids[..len].iter().map(|&x| x as i64));
            all_attention_mask.extend(mask[..len].iter().map(|&x| x as i64));
            all_token_type_ids.extend(type_ids[..len].iter().map(|&x| x as i64));

            // Padding
            let pad = seq_len - len;
            all_input_ids.extend(std::iter::repeat_n(0i64, pad));
            all_attention_mask.extend(std::iter::repeat_n(0i64, pad));
            all_token_type_ids.extend(std::iter::repeat_n(0i64, pad));
        }

        let shape = [batch_size, seq_len];

        let input_ids_tensor = ort::value::Tensor::from_array((shape, all_input_ids))?;
        let attention_mask_tensor = ort::value::Tensor::from_array((shape, all_attention_mask))?;
        let token_type_ids_tensor = ort::value::Tensor::from_array((shape, all_token_type_ids))?;

        let outputs = self.session.run(ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attention_mask_tensor,
            "token_type_ids" => token_type_ids_tensor,
        ])?;

        // Output shape: [batch_size, 1] — extract the logit for each pair.
        let output_tensor = outputs[0]
            .try_extract_tensor::<f32>()
            .context("extracting reranker output tensor")?;
        let logits: Vec<f32> = output_tensor.1.to_vec();

        // The model outputs raw logits. For ranking we only need relative ordering,
        // but we apply sigmoid to get [0, 1] scores for interpretability.
        let scores: Vec<f32> = logits.iter().map(|&l| sigmoid(l)).collect();
        if tracing::enabled!(tracing::Level::TRACE) {
            for (i, (logit, score)) in logits.iter().zip(scores.iter()).enumerate() {
                tracing::trace!("rerank[{i}]: logit={logit:.4}, sigmoid={score:.4}");
            }
        }
        Ok(scores)
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

// ── Global singleton ──────────────────────────────────────────────────────────

static RERANKER: OnceLock<Mutex<Option<RerankerEngine>>> = OnceLock::new();

fn get_reranker() -> &'static Mutex<Option<RerankerEngine>> {
    RERANKER.get_or_init(|| match RerankerEngine::new() {
        Ok(engine) => Mutex::new(Some(engine)),
        Err(e) => {
            warn!("Reranker unavailable: {:#}", e);
            Mutex::new(None)
        }
    })
}

// ── Public API ────────────────────────────────────────────────────────────────

/// A (chunk_id, original_score) pair to be reranked.
pub struct RerankCandidate {
    pub id: u64,
    pub score: f32,
    pub text: String,
}

/// Result of reranking: chunk_id → cross-encoder score [0, 1].
pub struct RerankResult {
    pub id: u64,
    /// Cross-encoder score (sigmoid of logit), range [0, 1].
    pub cross_encoder_score: f32,
    /// Blended score: alpha * cross_encoder + (1 - alpha) * original.
    pub blended_score: f32,
}

/// Rerank a set of candidates using the cross-encoder.
///
/// Returns `None` if the reranker is unavailable (download failed, ONNX error, etc).
/// When available, returns reranked scores for all candidates.
///
/// `alpha` controls the blend: 0.0 = original scores only, 1.0 = cross-encoder only.
/// Recommended: 0.3–0.5 for conservative blending.
pub fn rerank(
    query: &str,
    candidates: &[RerankCandidate],
    alpha: f32,
) -> Option<Vec<RerankResult>> {
    if candidates.is_empty() {
        return Some(Vec::new());
    }

    let mut guard = get_reranker().lock().ok()?;
    let engine = guard.as_mut()?;

    let documents: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();

    let start = std::time::Instant::now();
    let scores = match engine.score_pairs(query, &documents) {
        Ok(s) => s,
        Err(e) => {
            warn!("Reranker inference failed: {:#}", e);
            return None;
        }
    };
    let elapsed = start.elapsed();
    debug!(
        "Reranked {} candidates in {:.1}ms",
        candidates.len(),
        elapsed.as_secs_f64() * 1000.0
    );

    let results: Vec<RerankResult> = candidates
        .iter()
        .zip(scores.iter())
        .map(|(c, &ce_score)| {
            let blended = alpha * ce_score + (1.0 - alpha) * c.score;
            RerankResult {
                id: c.id,
                cross_encoder_score: ce_score,
                blended_score: blended,
            }
        })
        .collect();

    Some(results)
}

/// Pre-initialize the reranker in the background (e.g., at daemon startup).
/// Downloads the model if needed. Non-blocking if called from a tokio context.
pub fn warm_up() {
    let _ = get_reranker();
}

/// Check if the reranker model is available (cached locally).
pub fn is_model_cached() -> bool {
    model_path().ok().is_some_and(|p| p.exists())
        && tokenizer_path().ok().is_some_and(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_basic() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        assert!(sigmoid(10.0) > 0.99);
        assert!(sigmoid(-10.0) < 0.01);
    }

    #[test]
    fn rerank_empty_candidates() {
        let result = rerank("test query", &[], 0.3);
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    /// Integration test: downloads model, runs inference, checks relevance ordering.
    /// Run with: cargo test -p budi-core reranker_integration -- --ignored
    #[test]
    #[ignore]
    fn reranker_integration() {
        let candidates = vec![
            RerankCandidate {
                id: 1,
                score: 0.5,
                text: "fn fibonacci(n: u32) -> u32 { if n <= 1 { return n; } fibonacci(n-1) + fibonacci(n-2) }".to_string(),
            },
            RerankCandidate {
                id: 2,
                score: 0.5,
                text: "The weather in Paris is generally mild with warm summers and cool winters.".to_string(),
            },
            RerankCandidate {
                id: 3,
                score: 0.5,
                text: "impl Iterator for FibonacciIter { type Item = u64; fn next(&mut self) -> Option<u64> { let val = self.a; self.a = self.b; self.b = val + self.a; Some(val) } }".to_string(),
            },
        ];

        let results = rerank(
            "How is the fibonacci sequence implemented?",
            &candidates,
            0.5,
        )
        .expect("reranker should be available");

        assert_eq!(results.len(), 3);
        // The fibonacci-related chunks should score higher than the weather chunk.
        let weather_score = results
            .iter()
            .find(|r| r.id == 2)
            .unwrap()
            .cross_encoder_score;
        let fib_fn_score = results
            .iter()
            .find(|r| r.id == 1)
            .unwrap()
            .cross_encoder_score;
        let fib_iter_score = results
            .iter()
            .find(|r| r.id == 3)
            .unwrap()
            .cross_encoder_score;
        println!(
            "fib_fn={:.4}, fib_iter={:.4}, weather={:.4}",
            fib_fn_score, fib_iter_score, weather_score
        );
        assert!(
            fib_fn_score > weather_score,
            "fibonacci fn should score higher than weather"
        );
        assert!(
            fib_iter_score > weather_score,
            "fibonacci iter should score higher than weather"
        );
    }
}
