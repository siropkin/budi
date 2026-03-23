use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use budi_core::rpc::QueryResponse;
use serde::{Deserialize, Serialize};

use crate::prompt_controls::sanitize_prompt_for_query;

#[derive(Debug, Clone, Deserialize)]
struct RetrievalEvalCase {
    query: String,
    #[serde(
        default,
        alias = "expected_paths",
        alias = "oracle_paths",
        alias = "oracle",
        alias = "expected",
        alias = "expects"
    )]
    expect_paths: Vec<String>,
    #[serde(default, alias = "intent", alias = "intent_label")]
    expected_intent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RetrievalEvalCaseResult {
    pub(crate) query: String,
    pub(crate) expected_intent: Option<String>,
    pub(crate) expected_paths: Vec<String>,
    pub(crate) rank: Option<usize>,
    pub(crate) matched_at_1: usize,
    pub(crate) matched_at_3: usize,
    pub(crate) matched_at_5: usize,
    pub(crate) top_paths: Vec<String>,
    pub(crate) intent: String,
    pub(crate) confidence: f32,
    pub(crate) recommended_injection: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RetrievalEvalMetrics {
    pub(crate) cases: usize,
    pub(crate) hit_at_1: f64,
    pub(crate) hit_at_3: f64,
    pub(crate) hit_at_5: f64,
    pub(crate) mrr: f64,
    pub(crate) precision_at_1: f64,
    pub(crate) precision_at_3: f64,
    pub(crate) precision_at_5: f64,
    pub(crate) recall_at_1: f64,
    pub(crate) recall_at_3: f64,
    pub(crate) recall_at_5: f64,
    pub(crate) f1_at_1: f64,
    pub(crate) f1_at_3: f64,
    pub(crate) f1_at_5: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RetrievalEvalReport {
    pub(crate) repo_root: String,
    pub(crate) fixtures_path: String,
    pub(crate) retrieval_mode: String,
    pub(crate) limit: usize,
    pub(crate) total_cases: usize,
    pub(crate) scored_cases: usize,
    pub(crate) cases_with_errors: usize,
    pub(crate) metrics: RetrievalEvalMetrics,
    pub(crate) per_intent_metrics: HashMap<String, RetrievalEvalMetrics>,
    pub(crate) results: Vec<RetrievalEvalCaseResult>,
}

#[derive(Debug, Clone, Default)]
struct MetricsAccumulator {
    cases: usize,
    hit_at_1_sum: f64,
    hit_at_3_sum: f64,
    hit_at_5_sum: f64,
    mrr_sum: f64,
    precision_at_1_sum: f64,
    precision_at_3_sum: f64,
    precision_at_5_sum: f64,
    recall_at_1_sum: f64,
    recall_at_3_sum: f64,
    recall_at_5_sum: f64,
    f1_at_1_sum: f64,
    f1_at_3_sum: f64,
    f1_at_5_sum: f64,
}

pub(crate) fn run_retrieval_eval<F>(
    repo_root: &Path,
    fixtures_path: &Path,
    retrieval_mode: &str,
    limit: usize,
    mut query_runner: F,
) -> Result<RetrievalEvalReport>
where
    F: FnMut(&str) -> Result<QueryResponse>,
{
    if limit == 0 {
        anyhow::bail!("--limit must be at least 1");
    }
    if !fixtures_path.exists() {
        anyhow::bail!(
            "Retrieval fixture file not found: {} (use --fixtures or create default fixture file)",
            fixtures_path.display()
        );
    }

    let fixture_text = fs::read_to_string(fixtures_path)
        .with_context(|| format!("Failed reading {}", fixtures_path.display()))?;
    let cases: Vec<RetrievalEvalCase> = serde_json::from_str(&fixture_text)
        .with_context(|| format!("Failed parsing JSON fixture {}", fixtures_path.display()))?;
    if cases.is_empty() {
        anyhow::bail!("Retrieval fixture {} has no cases", fixtures_path.display());
    }

    let mut report = RetrievalEvalReport {
        repo_root: repo_root.display().to_string(),
        fixtures_path: fixtures_path.display().to_string(),
        retrieval_mode: retrieval_mode.to_string(),
        limit,
        total_cases: cases.len(),
        scored_cases: 0,
        cases_with_errors: 0,
        metrics: RetrievalEvalMetrics::default(),
        per_intent_metrics: HashMap::new(),
        results: Vec::with_capacity(cases.len()),
    };
    let mut metrics = MetricsAccumulator::default();
    let mut per_intent_metrics: HashMap<String, MetricsAccumulator> = HashMap::new();

    for case in cases {
        let sanitized_query = sanitize_prompt_for_query(&case.query);
        match query_runner(&sanitized_query) {
            Ok(response) => {
                let top_paths = response
                    .snippets
                    .iter()
                    .take(limit)
                    .map(|item| item.path.clone())
                    .collect::<Vec<_>>();
                let rank = evaluate_retrieval_rank(&top_paths, &case.expect_paths);
                let mut matched_at_1 = 0usize;
                let mut matched_at_3 = 0usize;
                let mut matched_at_5 = 0usize;
                if !case.expect_paths.is_empty() {
                    report.scored_cases = report.scored_cases.saturating_add(1);
                    let bucket = score_bucket(case.expected_intent.as_deref(), &response);
                    (matched_at_1, matched_at_3, matched_at_5) =
                        metrics.record_case(&top_paths, &case.expect_paths, rank);
                    per_intent_metrics.entry(bucket).or_default().record_case(
                        &top_paths,
                        &case.expect_paths,
                        rank,
                    );
                }
                report.results.push(RetrievalEvalCaseResult {
                    query: case.query,
                    expected_intent: case.expected_intent,
                    expected_paths: case.expect_paths,
                    rank,
                    matched_at_1,
                    matched_at_3,
                    matched_at_5,
                    top_paths,
                    intent: response.diagnostics.intent,
                    confidence: response.diagnostics.confidence,
                    recommended_injection: response.diagnostics.recommended_injection,
                    error: None,
                });
            }
            Err(err) => {
                report.cases_with_errors = report.cases_with_errors.saturating_add(1);
                report.results.push(RetrievalEvalCaseResult {
                    query: case.query,
                    expected_intent: case.expected_intent,
                    expected_paths: case.expect_paths,
                    rank: None,
                    matched_at_1: 0,
                    matched_at_3: 0,
                    matched_at_5: 0,
                    top_paths: Vec::new(),
                    intent: String::new(),
                    confidence: 0.0,
                    recommended_injection: false,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    report.metrics = metrics.into_metrics();
    report.per_intent_metrics = per_intent_metrics
        .into_iter()
        .map(|(intent, accumulator)| (intent, accumulator.into_metrics()))
        .collect();

    Ok(report)
}

impl MetricsAccumulator {
    fn record_case(
        &mut self,
        top_paths: &[String],
        expected_paths: &[String],
        rank: Option<usize>,
    ) -> (usize, usize, usize) {
        let matched_at_1 = count_matched_expected(top_paths, expected_paths, 1);
        let matched_at_3 = count_matched_expected(top_paths, expected_paths, 3);
        let matched_at_5 = count_matched_expected(top_paths, expected_paths, 5);
        self.cases = self.cases.saturating_add(1);
        self.hit_at_1_sum += if matched_at_1 > 0 { 1.0 } else { 0.0 };
        self.hit_at_3_sum += if matched_at_3 > 0 { 1.0 } else { 0.0 };
        self.hit_at_5_sum += if matched_at_5 > 0 { 1.0 } else { 0.0 };
        if let Some(position) = rank {
            self.mrr_sum += 1.0 / (position as f64);
        }
        let returned_at_1 = top_paths.len().min(1);
        let returned_at_3 = top_paths.len().min(3);
        let returned_at_5 = top_paths.len().min(5);
        let expected_total = expected_paths.len();
        let precision_at_1 = precision(matched_at_1, returned_at_1);
        let precision_at_3 = precision(matched_at_3, returned_at_3);
        let precision_at_5 = precision(matched_at_5, returned_at_5);
        let recall_at_1 = recall(matched_at_1, expected_total);
        let recall_at_3 = recall(matched_at_3, expected_total);
        let recall_at_5 = recall(matched_at_5, expected_total);
        self.precision_at_1_sum += precision_at_1;
        self.precision_at_3_sum += precision_at_3;
        self.precision_at_5_sum += precision_at_5;
        self.recall_at_1_sum += recall_at_1;
        self.recall_at_3_sum += recall_at_3;
        self.recall_at_5_sum += recall_at_5;
        self.f1_at_1_sum += f1(precision_at_1, recall_at_1);
        self.f1_at_3_sum += f1(precision_at_3, recall_at_3);
        self.f1_at_5_sum += f1(precision_at_5, recall_at_5);
        (matched_at_1, matched_at_3, matched_at_5)
    }

    fn into_metrics(self) -> RetrievalEvalMetrics {
        if self.cases == 0 {
            return RetrievalEvalMetrics::default();
        }
        let denom = self.cases as f64;
        RetrievalEvalMetrics {
            cases: self.cases,
            hit_at_1: self.hit_at_1_sum / denom,
            hit_at_3: self.hit_at_3_sum / denom,
            hit_at_5: self.hit_at_5_sum / denom,
            mrr: self.mrr_sum / denom,
            precision_at_1: self.precision_at_1_sum / denom,
            precision_at_3: self.precision_at_3_sum / denom,
            precision_at_5: self.precision_at_5_sum / denom,
            recall_at_1: self.recall_at_1_sum / denom,
            recall_at_3: self.recall_at_3_sum / denom,
            recall_at_5: self.recall_at_5_sum / denom,
            f1_at_1: self.f1_at_1_sum / denom,
            f1_at_3: self.f1_at_3_sum / denom,
            f1_at_5: self.f1_at_5_sum / denom,
        }
    }
}

pub(crate) fn load_retrieval_eval_report(path: &Path) -> Result<RetrievalEvalReport> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Failed reading retrieval eval artifact {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("Invalid retrieval eval artifact JSON {}", path.display()))
}

fn score_bucket(expected_intent: Option<&str>, response: &QueryResponse) -> String {
    if let Some(label) = expected_intent
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        return label.to_string();
    }
    let observed = response.diagnostics.intent.trim();
    if observed.is_empty() {
        "unlabeled".to_string()
    } else {
        observed.to_string()
    }
}

fn count_matched_expected(top_paths: &[String], expected_paths: &[String], k: usize) -> usize {
    if expected_paths.is_empty() || top_paths.is_empty() || k == 0 {
        return 0;
    }
    expected_paths
        .iter()
        .filter(|expected| {
            top_paths
                .iter()
                .take(k)
                .any(|path| path_matches_expected(path, expected))
        })
        .count()
}

fn precision(matched: usize, returned: usize) -> f64 {
    if returned == 0 {
        0.0
    } else {
        matched as f64 / returned as f64
    }
}

fn recall(matched: usize, expected_total: usize) -> f64 {
    if expected_total == 0 {
        0.0
    } else {
        matched as f64 / expected_total as f64
    }
}

fn f1(precision: f64, recall: f64) -> f64 {
    if (precision + recall) == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

fn evaluate_retrieval_rank(top_paths: &[String], expected_paths: &[String]) -> Option<usize> {
    for (idx, path) in top_paths.iter().enumerate() {
        if expected_paths
            .iter()
            .any(|expected| path_matches_expected(path, expected))
        {
            return Some(idx + 1);
        }
    }
    None
}

fn path_matches_expected(actual: &str, expected: &str) -> bool {
    let expected = expected.trim().trim_start_matches("./");
    if expected.is_empty() {
        return false;
    }
    let actual = actual.trim_start_matches("./");
    actual == expected || actual.ends_with(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matches_expected_supports_exact_and_suffix_matches() {
        assert!(path_matches_expected(
            "crates/budi-core/src/retrieval.rs",
            "crates/budi-core/src/retrieval.rs"
        ));
        assert!(path_matches_expected(
            "crates/budi-core/src/retrieval.rs",
            "src/retrieval.rs"
        ));
        assert!(!path_matches_expected(
            "crates/budi-core/src/index.rs",
            "src/retrieval.rs"
        ));
    }

    #[test]
    fn evaluate_retrieval_rank_returns_first_match_rank() {
        let top_paths = vec![
            "crates/budi-core/src/index.rs".to_string(),
            "crates/budi-core/src/retrieval.rs".to_string(),
            "crates/budi-cli/src/main.rs".to_string(),
        ];
        let expected = vec![
            "src/retrieval.rs".to_string(),
            "src/prompt_controls.rs".to_string(),
        ];
        let rank = evaluate_retrieval_rank(&top_paths, &expected);
        assert_eq!(rank, Some(2));
    }

    #[test]
    fn metrics_accumulator_tracks_hit_precision_recall_and_f1() {
        let top_paths = vec![
            "crates/budi-core/src/retrieval.rs".to_string(),
            "crates/budi-core/src/index.rs".to_string(),
            "crates/budi-cli/src/main.rs".to_string(),
        ];
        let expected = vec![
            "src/retrieval.rs".to_string(),
            "src/main.rs".to_string(),
            "src/other.rs".to_string(),
        ];
        let rank = evaluate_retrieval_rank(&top_paths, &expected);
        let mut accumulator = MetricsAccumulator::default();
        let (matched_at_1, matched_at_3, matched_at_5) =
            accumulator.record_case(&top_paths, &expected, rank);
        assert_eq!((matched_at_1, matched_at_3, matched_at_5), (1, 2, 2));
        let metrics = accumulator.into_metrics();
        assert_eq!(metrics.cases, 1);
        assert!((metrics.hit_at_1 - 1.0).abs() < f64::EPSILON);
        assert!((metrics.hit_at_3 - 1.0).abs() < f64::EPSILON);
        assert!((metrics.precision_at_3 - (2.0 / 3.0)).abs() < 1e-9);
        assert!((metrics.recall_at_3 - (2.0 / 3.0)).abs() < 1e-9);
        assert!((metrics.f1_at_3 - (2.0 / 3.0)).abs() < 1e-9);
        assert!((metrics.mrr - 1.0).abs() < f64::EPSILON);
    }
}
