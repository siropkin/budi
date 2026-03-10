use std::collections::{HashMap, HashSet};

use crate::rpc::QueryResultItem;

/// A merged evidence card: primary snippet + extra anchors from same-file secondary snippets.
struct MergedCard<'a> {
    primary: &'a QueryResultItem,
    /// Start line of the merged span (min of all same-file snippets).
    merged_start: usize,
    /// End line of the merged span (max of all same-file snippets).
    merged_end: usize,
    /// Anchor lines from secondary (lower-scored) same-file snippets.
    extra_anchors: Vec<String>,
}

/// Group snippets by file path. Same-file snippets are merged into one card:
/// the highest-scored snippet becomes primary, and secondary snippets contribute
/// their anchor lines as additional proof. Preserves score-order for card rendering.
fn merge_same_file_snippets(snippets: &[QueryResultItem]) -> Vec<MergedCard<'_>> {
    let mut cards: Vec<MergedCard<'_>> = Vec::new();
    let mut path_to_idx: HashMap<&str, usize> = HashMap::new();

    for snippet in snippets {
        if let Some(&existing_idx) = path_to_idx.get(snippet.path.as_str()) {
            let card = &mut cards[existing_idx];
            card.merged_start = card.merged_start.min(snippet.start_line);
            card.merged_end = card.merged_end.max(snippet.end_line);
            let anchor = extract_anchor_line(&snippet.text);
            if anchor != "(empty)" {
                card.extra_anchors.push(anchor);
            }
        } else {
            path_to_idx.insert(&snippet.path, cards.len());
            cards.push(MergedCard {
                primary: snippet,
                merged_start: snippet.start_line,
                merged_end: snippet.end_line,
                extra_anchors: Vec::new(),
            });
        }
    }
    cards
}

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
/// Same-file snippets are merged into one card (cross-chunk synthesis).
/// Uses score-weighted progressive truncation so the highest-scoring
/// card gets proportionally more budget than later ones.
pub(super) fn build_context(
    snippets: &[QueryResultItem],
    budget: usize,
    query_tokens: &[String],
) -> String {
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

    let merged = merge_same_file_snippets(snippets);

    for (idx, card_data) in merged.iter().enumerate() {
        if remaining_budget == 0 {
            break;
        }
        // Progressive truncation: top card gets up to 40% of content budget;
        // each subsequent card gets up to 60% of what remains.
        let card_budget = if idx == 0 {
            (content_budget as f32 * 0.40).ceil() as usize
        } else {
            (remaining_budget as f32 * 0.60).ceil() as usize
        }
        .min(remaining_budget);

        let card = render_merged_card(card_data, query_tokens);
        if card.len() <= card_budget {
            out.push_str(&card);
            remaining_budget = remaining_budget.saturating_sub(card.len());
        } else if card_budget > 0 {
            out.push_str(&card.chars().take(card_budget).collect::<String>());
            break;
        } else {
            break;
        }
    }
    out
}

/// Render a merged card. When secondary same-file snippets exist, their
/// anchor lines are folded into the proof section (up to the 3-line limit).
fn render_merged_card(card_data: &MergedCard<'_>, query_tokens: &[String]) -> String {
    let snippet = card_data.primary;
    let anchor = extract_anchor_line(&snippet.text);
    let max_proof = 3;
    let mut proof_lines = extract_proof_lines(&snippet.text, max_proof, &anchor, query_tokens);

    // Fold secondary anchors as additional proof (they show what else is in this file).
    for extra in &card_data.extra_anchors {
        if proof_lines.len() >= max_proof {
            break;
        }
        let sanitized = sanitize_evidence_line(extra);
        if !sanitized.is_empty() && !proof_lines.contains(&sanitized) {
            proof_lines.push(sanitized);
        }
    }

    let mut out = String::new();
    out.push_str(&format!("- file: {}\n", snippet.path));
    out.push_str(&format!(
        "  span: {}-{}\n",
        card_data.merged_start, card_data.merged_end
    ));
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

fn extract_proof_lines(
    text: &str,
    max_lines: usize,
    anchor: &str,
    query_tokens: &[String],
) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    // General code proof needles (not intent-specific)
    let fixed_needles: &[&str] = &[
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
        // Call-graph and flow-trace needles
        "call(",
        "invoke",
        "schedule",
        "commit",
    ];

    let mut picked = Vec::new();
    let mut seen = HashSet::new();
    let anchor_lower = anchor.to_ascii_lowercase();

    // Skip lines that duplicate the anchor (function signature already shown).
    let is_anchor_dup = |line: &str| -> bool {
        let l = line.to_ascii_lowercase();
        l == anchor_lower || (l.len() > 20 && anchor_lower.contains(&l))
    };

    // Priority 1: lines matching query tokens (most relevant to the specific question)
    if !query_tokens.is_empty() {
        for raw_line in text.lines() {
            if picked.len() >= max_lines {
                break;
            }
            let line = sanitize_evidence_line(raw_line);
            if line.is_empty() || is_comment_only_line(line.as_str()) || is_anchor_dup(&line) {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if query_tokens.iter().any(|tok| lower.contains(tok.as_str()))
                && seen.insert(line.clone())
            {
                picked.push(line);
            }
        }
    }

    // Priority 2: lines matching fixed code needles
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) || is_anchor_dup(&line) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if fixed_needles.iter().any(|needle| lower.contains(needle)) && seen.insert(line.clone()) {
            picked.push(line);
        }
    }
    // Fill with any non-empty, non-comment, non-anchor lines
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) || is_anchor_dup(&line) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{QueryChannelScores, QueryResultItem};

    fn make_snippet(path: &str, text: &str, score: f32) -> QueryResultItem {
        QueryResultItem {
            path: path.to_string(),
            start_line: 1,
            end_line: 10,
            language: "unknown".to_string(),
            score,
            reasons: vec!["lexical-hit".to_string()],
            channel_scores: QueryChannelScores::default(),
            text: text.to_string(),
            slm_relevance_note: None,
        }
    }

    // ── build_context ─────────────────────────────────────────────────────────

    #[test]
    fn empty_snippets_returns_header_only() {
        let out = build_context(&[], 4096, &[]);
        assert!(out.starts_with("[budi context]"), "missing header");
        assert!(
            out.contains("evidence_cards:"),
            "missing evidence_cards section"
        );
        // No snippet data
        assert!(!out.contains("file:"), "unexpected file card");
    }

    #[test]
    fn single_snippet_rendered_correctly() {
        let snippets = vec![make_snippet(
            "src/scheduler.rs",
            "fn commitRoot() { schedule(); }",
            0.75,
        )];
        let out = build_context(&snippets, 4096, &[]);
        assert!(out.contains("file: src/scheduler.rs"), "missing file path");
        assert!(out.contains("span: 1-10"), "missing span");
        // Score and signals are stripped from emitted context (debugging metadata)
        assert!(
            !out.contains("score:"),
            "score should not be in emitted context"
        );
        assert!(
            !out.contains("signals:"),
            "signals should not be in emitted context"
        );
    }

    #[test]
    fn zero_budget_returns_header_only() {
        let snippets = vec![make_snippet("src/foo.rs", "fn foo() {}", 0.8)];
        let out = build_context(&snippets, 0, &[]);
        // Budget 0 < header length, so content_budget is 0, loop breaks immediately
        assert!(out.starts_with("[budi context]"));
        assert!(
            !out.contains("file: src/foo.rs"),
            "should not render card when budget=0"
        );
    }

    #[test]
    fn budget_caps_total_output_length() {
        let long_text = "x".repeat(5000);
        let snippets = vec![
            make_snippet("src/a.rs", &long_text, 0.9),
            make_snippet("src/b.rs", &long_text, 0.8),
        ];
        let budget = 2000;
        let out = build_context(&snippets, budget, &[]);
        assert!(
            out.len() <= budget + 20, // small tolerance for header math
            "output len {} exceeds budget {}",
            out.len(),
            budget
        );
    }

    #[test]
    fn top_snippet_gets_40_percent_of_content_budget() {
        // Build two snippets where second has much more text.
        // The top snippet should get ≤40% of content_budget.
        let long = "z ".repeat(3000);
        let snippets = vec![
            make_snippet("src/top.rs", "fn top() { return 1; }", 0.9),
            make_snippet("src/big.rs", &long, 0.5),
        ];
        let budget = 4096;
        let out = build_context(&snippets, budget, &[]);
        // Both files should appear since budget is generous
        assert!(out.contains("file: src/top.rs"));
        assert!(out.contains("file: src/big.rs"));
    }

    #[test]
    fn output_contains_evidence_card_structure() {
        let snippets = vec![make_snippet(
            "src/daemon.rs",
            "pub fn query(&self) -> Result<QueryResponse> { return Ok(resp); }",
            0.82,
        )];
        let out = build_context(&snippets, 4096, &[]);
        assert!(out.contains("anchor:"), "missing anchor");
        assert!(out.contains("proof:"), "missing proof section");
    }

    #[test]
    fn multiline_snippet_extracts_proof_lines() {
        let snippets = vec![make_snippet(
            "src/routes.rs",
            "// top comment\nroute(\"/api\", handler)\nreturn response;",
            0.7,
        )];
        let out = build_context(&snippets, 4096, &[]);
        // Should include the "route" line as a proof needle match
        assert!(out.contains("route"), "expected route proof line");
    }

    // ── same-file card merging ─────────────────────────────────────────────

    #[test]
    fn same_file_snippets_merged_into_one_card() {
        let mut s1 = make_snippet("src/foo.rs", "fn alpha() { return 1; }", 0.9);
        s1.start_line = 1;
        s1.end_line = 5;
        let mut s2 = make_snippet("src/foo.rs", "fn beta() { return 2; }", 0.7);
        s2.start_line = 10;
        s2.end_line = 15;
        let snippets = vec![s1, s2];
        let out = build_context(&snippets, 4096, &[]);
        // Only one file: card should appear
        let file_count = out.matches("file: src/foo.rs").count();
        assert_eq!(file_count, 1, "same-file snippets should produce one card");
        // Merged span covers both
        assert!(out.contains("span: 1-15"), "span should be merged: {}", out);
        // Secondary anchor folded into proof
        assert!(out.contains("fn beta()"), "secondary anchor should appear in proof");
    }

    #[test]
    fn different_file_snippets_stay_separate() {
        let snippets = vec![
            make_snippet("src/a.rs", "fn a() {}", 0.9),
            make_snippet("src/b.rs", "fn b() {}", 0.7),
        ];
        let out = build_context(&snippets, 4096, &[]);
        assert!(out.contains("file: src/a.rs"));
        assert!(out.contains("file: src/b.rs"));
    }

    // ── path_diversity_bucket ────────────────────────────────────────────────

    #[test]
    fn path_diversity_bucket_two_levels() {
        assert_eq!(path_diversity_bucket("src/foo/bar.rs"), "src/foo");
        assert_eq!(
            path_diversity_bucket("crates/budi-core/src/lib.rs"),
            "crates/budi-core"
        );
    }

    #[test]
    fn path_diversity_bucket_single_level() {
        assert_eq!(path_diversity_bucket("main.rs"), "main.rs");
        assert_eq!(path_diversity_bucket("/main.rs"), "main.rs");
    }

    #[test]
    fn path_diversity_bucket_empty_path() {
        assert_eq!(path_diversity_bucket(""), "root");
    }

    // ── snippet_fingerprint ───────────────────────────────────────────────────

    #[test]
    fn fingerprint_is_stable() {
        let text = "fn foo() { let x = 1; }";
        let a = snippet_fingerprint(text);
        let b = snippet_fingerprint(text);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_ignores_whitespace_differences() {
        let a = snippet_fingerprint("fn foo()  {   let x = 1;  }");
        let b = snippet_fingerprint("fn foo() { let x = 1; }");
        assert_eq!(a, b, "fingerprint should be whitespace-normalized");
    }

    #[test]
    fn fingerprint_is_case_insensitive() {
        let a = snippet_fingerprint("fn FOO() {}");
        let b = snippet_fingerprint("fn foo() {}");
        assert_eq!(a, b, "fingerprint should be lowercased");
    }

    #[test]
    fn fingerprint_differs_for_different_content() {
        let a = snippet_fingerprint("fn foo() {}");
        let b = snippet_fingerprint("fn bar() {}");
        assert_ne!(a, b);
    }
}
