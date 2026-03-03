use std::collections::{HashMap, HashSet};

use crate::rpc::QueryResultItem;

#[derive(Debug, Default)]
pub(super) struct SnippetSelectionState {
    pub(super) snippets: Vec<QueryResultItem>,
    pub(super) selected_chunk_ids: Vec<u64>,
    pub(super) seen_fingerprints: HashSet<String>,
    pub(super) snippets_per_path: HashMap<String, usize>,
    pub(super) snippets_per_bucket: HashMap<String, usize>,
    pub(super) per_file_limit: usize,
    pub(super) per_bucket_limit: usize,
}

pub(super) fn path_diversity_bucket(path: &str) -> String {
    let mut parts = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase());
    let first = parts.next().unwrap_or_else(|| "root".to_string());
    if let Some(second) = parts.next() {
        format!("{first}/{second}")
    } else {
        first
    }
}

pub(super) fn build_context(snippets: &[QueryResultItem], budget: usize) -> String {
    let mut out = String::new();
    out.push_str("[budi deterministic context]\n");
    out.push_str("snippets:\n");

    for snippet in snippets {
        let reasons = if snippet.reasons.is_empty() {
            "semantic+lexical".to_string()
        } else {
            snippet.reasons.join(",")
        };
        let header = format!(
            "### {}:{}-{} score={:.4} reasons={}\n",
            snippet.path, snippet.start_line, snippet.end_line, snippet.score, reasons
        );
        if out.len() + header.len() >= budget {
            break;
        }
        out.push_str(&header);
        let mut body = snippet.text.clone();
        body.push('\n');
        if out.len() + body.len() > budget {
            let remaining = budget.saturating_sub(out.len());
            let truncated = body.chars().take(remaining).collect::<String>();
            out.push_str(&truncated);
            break;
        }
        out.push_str(&body);
    }
    out
}

pub(super) fn snippet_fingerprint(text: &str) -> String {
    let normalized = text
        .split_whitespace()
        .take(80)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}
