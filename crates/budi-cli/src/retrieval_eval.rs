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
        alias = "expected",
        alias = "expects"
    )]
    expect_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RetrievalEvalCaseResult {
    pub(crate) query: String,
    pub(crate) expected_paths: Vec<String>,
    pub(crate) rank: Option<usize>,
    pub(crate) top_paths: Vec<String>,
    pub(crate) intent: String,
    pub(crate) confidence: f32,
    pub(crate) recommended_injection: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RetrievalEvalReport {
    pub(crate) repo_root: String,
    pub(crate) fixtures_path: String,
    pub(crate) retrieval_mode: String,
    pub(crate) limit: usize,
    pub(crate) total_cases: usize,
    pub(crate) scored_cases: usize,
    pub(crate) cases_with_errors: usize,
    pub(crate) hit_at_1: f64,
    pub(crate) hit_at_3: f64,
    pub(crate) hit_at_5: f64,
    pub(crate) mrr: f64,
    pub(crate) results: Vec<RetrievalEvalCaseResult>,
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
        hit_at_1: 0.0,
        hit_at_3: 0.0,
        hit_at_5: 0.0,
        mrr: 0.0,
        results: Vec::with_capacity(cases.len()),
    };

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
                if !case.expect_paths.is_empty() {
                    report.scored_cases = report.scored_cases.saturating_add(1);
                    if let Some(position) = rank {
                        if position <= 1 {
                            report.hit_at_1 += 1.0;
                        }
                        if position <= 3 {
                            report.hit_at_3 += 1.0;
                        }
                        if position <= 5 {
                            report.hit_at_5 += 1.0;
                        }
                        report.mrr += 1.0 / (position as f64);
                    }
                }
                report.results.push(RetrievalEvalCaseResult {
                    query: case.query,
                    expected_paths: case.expect_paths,
                    rank,
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
                    expected_paths: case.expect_paths,
                    rank: None,
                    top_paths: Vec::new(),
                    intent: String::new(),
                    confidence: 0.0,
                    recommended_injection: false,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    if report.scored_cases > 0 {
        let denom = report.scored_cases as f64;
        report.hit_at_1 /= denom;
        report.hit_at_3 /= denom;
        report.hit_at_5 /= denom;
        report.mrr /= denom;
    }

    Ok(report)
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
}
