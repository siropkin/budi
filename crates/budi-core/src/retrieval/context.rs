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

/// Build the injected context string from selected snippets.
/// Always renders evidence cards. Uses score-weighted progressive truncation so
/// the highest-scoring snippet gets proportionally more budget than later ones.
pub(super) fn build_context(snippets: &[QueryResultItem], budget: usize) -> String {
    let mut out = String::new();
    out.push_str("[budi context]\n");
    out.push_str("rules:\n");
    out.push_str("- Use only file paths shown in snippets for exact-path answers.\n");
    out.push_str(
        "- If snippets support fewer files than requested, return fewer instead of guessing.\n",
    );
    out.push_str("evidence_cards:\n");

    let header_len = out.len();
    let content_budget = budget.saturating_sub(header_len);
    let mut remaining_budget = content_budget;

    for (idx, snippet) in snippets.iter().enumerate() {
        if remaining_budget == 0 {
            break;
        }
        // Progressive truncation: top snippet gets up to 40% of content budget;
        // each subsequent snippet gets up to 60% of what remains.
        let snippet_budget = if idx == 0 {
            (content_budget as f32 * 0.40).ceil() as usize
        } else {
            (remaining_budget as f32 * 0.60).ceil() as usize
        }
        .min(remaining_budget);

        let card = render_evidence_card(snippet);
        if card.len() <= snippet_budget {
            out.push_str(&card);
            remaining_budget = remaining_budget.saturating_sub(card.len());
        } else if snippet_budget > 0 {
            out.push_str(&card.chars().take(snippet_budget).collect::<String>());
            break;
        } else {
            break;
        }
    }
    out
}

fn render_evidence_card(snippet: &QueryResultItem) -> String {
    let reasons = if snippet.reasons.is_empty() {
        "semantic+lexical".to_string()
    } else {
        snippet
            .reasons
            .iter()
            .take(6)
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    let anchor = extract_anchor_line(&snippet.text);
    let proof_lines = extract_proof_lines(&snippet.text, 3);
    let mut out = String::new();
    out.push_str(&format!("- file: {}\n", snippet.path));
    out.push_str(&format!(
        "  span: {}-{}\n",
        snippet.start_line, snippet.end_line
    ));
    out.push_str(&format!("  score: {:.4}\n", snippet.score));
    out.push_str(&format!("  signals: {}\n", reasons));
    out.push_str(&format!("  anchor: {}\n", anchor));
    if let Some(note) = &snippet.slm_relevance_note {
        out.push_str(&format!("  relevance: {}\n", note));
    }
    out.push_str("  proof:\n");
    if proof_lines.is_empty() {
        out.push_str("    - (no concise proof line found)\n");
    } else {
        for line in proof_lines {
            out.push_str(&format!("    - {}\n", line));
        }
    }
    out
}

fn extract_anchor_line(text: &str) -> String {
    for raw_line in text.lines() {
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        return line;
    }
    "(empty)".to_string()
}

fn extract_proof_lines(text: &str, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    // General code proof needles (not intent-specific)
    let needles: &[&str] = &[
        "listen(",
        "route",
        "router",
        "handler",
        "middleware",
        "dispatch",
        "request",
        "response",
        "return",
        "process.env",
        "import.meta.env",
        "os.environ",
        "env::var",
    ];

    let mut picked = Vec::new();
    let mut seen = HashSet::new();

    // Priority: lines matching code needles
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if needles.iter().any(|needle| lower.contains(needle)) && seen.insert(line.clone()) {
            picked.push(line);
        }
    }
    // Fill with any non-empty, non-comment lines
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        if seen.insert(line.clone()) {
            picked.push(line);
        }
    }
    picked
}

fn sanitize_evidence_line(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    let mut out = if trimmed.len() > 180 {
        let mut truncated = trimmed.chars().take(177).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        trimmed.to_string()
    };
    if out.contains('\t') {
        out = out.replace('\t', " ");
    }
    out
}

fn is_comment_only_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
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
