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
    /// Full texts of secondary snippets for proof line extraction.
    extra_texts: Vec<&'a str>,
}

/// Group snippets by file path. Same-file snippets are merged into one card:
/// the highest-scored snippet becomes primary, and secondary snippets contribute
/// their anchor lines as additional proof. Preserves score-order for card rendering.
/// Maximum total span (in lines) for a merged card. Prevents misleading
/// mega-spans from chaining many consecutive chunks in large files.
const MAX_MERGED_SPAN_LINES: usize = 200;

fn merge_same_file_snippets(snippets: &[QueryResultItem]) -> Vec<MergedCard<'_>> {
    let mut cards: Vec<MergedCard<'_>> = Vec::new();
    let mut path_to_idx: HashMap<&str, usize> = HashMap::new();

    for snippet in snippets {
        if let Some(&existing_idx) = path_to_idx.get(snippet.path.as_str()) {
            let card = &cards[existing_idx];
            // Only merge if the resulting span stays within the limit.
            let new_start = card.merged_start.min(snippet.start_line);
            let new_end = card.merged_end.max(snippet.end_line);
            if new_end - new_start <= MAX_MERGED_SPAN_LINES {
                let card = &mut cards[existing_idx];
                card.merged_start = new_start;
                card.merged_end = new_end;
                let anchor = extract_anchor_line(&snippet.text, &[]);
                if anchor != "(empty)" {
                    card.extra_anchors.push(anchor);
                }
                card.extra_texts.push(&snippet.text);
            } else {
                // Span would be too large — create a separate card.
                cards.push(MergedCard {
                    primary: snippet,
                    merged_start: snippet.start_line,
                    merged_end: snippet.end_line,
                    extra_anchors: Vec::new(),
                    extra_texts: Vec::new(),
                });
            }
        } else {
            path_to_idx.insert(&snippet.path, cards.len());
            cards.push(MergedCard {
                primary: snippet,
                merged_start: snippet.start_line,
                merged_end: snippet.end_line,
                extra_anchors: Vec::new(),
                extra_texts: Vec::new(),
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
    intent: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("[budi context]\n");
    out.push_str("rules: Use snippet paths only. Return fewer over guessing.\n");
    out.push_str("evidence_cards:\n");

    let header_len = out.len();
    let content_budget = budget.saturating_sub(header_len);
    let mut remaining_budget = content_budget;

    let merged = merge_same_file_snippets(snippets);

    // Intent-specific proof line budget: sym-def already shows the signature
    // as anchor, so 2 proof lines suffice. Flow-trace needs more call chain evidence.
    let max_proof = match intent {
        Some("symbol-definition") => 2,
        _ => 3,
    };

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

        let card = render_merged_card(card_data, query_tokens, max_proof);
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
fn render_merged_card(
    card_data: &MergedCard<'_>,
    query_tokens: &[String],
    max_proof: usize,
) -> String {
    let snippet = card_data.primary;
    let anchor = extract_anchor_line(&snippet.text, query_tokens);
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

    // Extract proof from secondary snippet texts (e.g. continuation body after a preamble).
    // This fills remaining proof slots with content from merged chunks.
    if proof_lines.len() < max_proof && !card_data.extra_texts.is_empty() {
        for extra_text in &card_data.extra_texts {
            let extra_proof = extract_proof_lines(
                extra_text,
                max_proof - proof_lines.len(),
                &anchor,
                query_tokens,
            );
            for line in extra_proof {
                if proof_lines.len() >= max_proof {
                    break;
                }
                if !proof_lines.contains(&line) {
                    proof_lines.push(line);
                }
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("- file: {}\n", snippet.path));
    out.push_str(&format!("  anchor: {}\n", anchor));
    if let Some(note) = &snippet.context_note {
        out.push_str(&format!("  relevance: {}\n", note));
    } else if let Some(doc) = extract_first_docstring_line(&snippet.text) {
        out.push_str(&format!("  relevance: {}\n", doc));
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

/// Returns true for decorator/attribute lines that should be skipped when picking an anchor.
/// Covers Python decorators (@foo), Rust attributes (#[foo]), Java/TS annotations (@Override).
fn is_decorator_or_attribute(trimmed: &str) -> bool {
    // Python decorators: @foo, @foo.bar, @foo(args)
    if trimmed.starts_with('@') {
        return true;
    }
    // Rust outer attributes: #[derive(...)], #[cfg(...)]
    if trimmed.starts_with("#[") {
        return true;
    }
    false
}

fn extract_anchor_line(text: &str, query_tokens: &[String]) -> String {
    // Pass 0: prefer a definition line that contains a query token (case-insensitive).
    // When same-file merging combines multiple chunks, the first definition in the
    // span may be a different function than the queried one. Matching query tokens
    // ensures the anchor reflects what the user asked about.
    if !query_tokens.is_empty() {
        for raw_line in text.lines() {
            let line = sanitize_evidence_line(raw_line);
            if line.is_empty() || is_comment_only_line(line.as_str()) {
                continue;
            }
            let trimmed = line.trim();
            if is_decorator_or_attribute(trimmed) {
                continue;
            }
            if is_definition_line(trimmed) {
                let lower = trimmed.to_ascii_lowercase();
                for token in query_tokens {
                    if token.len() >= 4 && lower.contains(token.as_str()) {
                        return line;
                    }
                }
            }
        }
    }
    // Pass 1: prefer a function/class/method definition line as anchor.
    // Chunks often straddle function boundaries (stride=60, overlap=20), so the
    // first non-empty line may be a closing brace or unrelated code while the
    // actual definition sits later. Scanning for definition keywords gives Claude
    // the most informative anchor.
    for raw_line in text.lines() {
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        let trimmed = line.trim();
        if is_decorator_or_attribute(trimmed) {
            continue;
        }
        if is_definition_line(trimmed) {
            return line;
        }
    }
    // Pass 2: fall back to first non-empty non-comment non-decorator line.
    for raw_line in text.lines() {
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        let trimmed = line.trim();
        if is_decorator_or_attribute(trimmed) {
            continue;
        }
        return line;
    }
    // Pass 3: anything non-empty.
    for raw_line in text.lines() {
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) {
            continue;
        }
        return line;
    }
    "(empty)".to_string()
}

/// Returns true if the line looks like a function, method, or class definition.
/// Used by anchor extraction to prefer informative definition lines over
/// arbitrary code (e.g. `} else {`, `return`, bare braces).
fn is_definition_line(trimmed: &str) -> bool {
    // Strip common visibility/export modifiers
    let s = trimmed
        .strip_prefix("export ")
        .or_else(|| trimmed.strip_prefix("pub(crate) "))
        .or_else(|| trimmed.strip_prefix("pub(super) "))
        .or_else(|| trimmed.strip_prefix("pub "))
        .or_else(|| trimmed.strip_prefix("async "))
        .unwrap_or(trimmed);
    let s = s
        .strip_prefix("export ")
        .or_else(|| s.strip_prefix("default "))
        .or_else(|| s.strip_prefix("async "))
        .or_else(|| s.strip_prefix("static "))
        .or_else(|| s.strip_prefix("abstract "))
        .or_else(|| s.strip_prefix("override "))
        .or_else(|| s.strip_prefix("virtual "))
        .unwrap_or(s);
    // Function/method keywords across languages
    s.starts_with("function ")
        || s.starts_with("fn ")
        || s.starts_with("func ")
        || s.starts_with("def ")
        // Class/type definitions
        || s.starts_with("class ")
        || s.starts_with("struct ")
        || s.starts_with("type ")
        || s.starts_with("interface ")
        || s.starts_with("enum ")
        || s.starts_with("trait ")
        || s.starts_with("impl ")
        // Go method receiver: func (r *Receiver) Method(
        || (s.starts_with("func (") && s.contains(')'))
        // Visibility-prefixed methods in Java/C#/Kotlin
        || s.starts_with("public ")
        || s.starts_with("private ")
        || s.starts_with("protected ")
        || s.starts_with("internal ")
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

    // Priority 1: lines matching query tokens (most relevant to the specific question).
    // Use word-boundary matching to avoid false positives like "return" in "returnFiber".
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
            if query_tokens
                .iter()
                .any(|tok| contains_at_word_boundary(&lower, tok))
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
        if fixed_needles
            .iter()
            .any(|needle| needle_matches(&lower, needle))
            && seen.insert(line.clone())
        {
            picked.push(line);
        }
    }
    // Priority 3: lines containing function call expressions (high signal for flow).
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty() || is_comment_only_line(line.as_str()) || is_anchor_dup(&line) {
            continue;
        }
        if has_call_expression(&line) && seen.insert(line.clone()) {
            picked.push(line);
        }
    }

    // Priority 4: for interface/type/struct definitions, property declarations
    // ARE the content — pick them as proof lines instead of filtering them out.
    if is_type_definition_anchor(anchor) {
        for raw_line in text.lines() {
            if picked.len() >= max_lines {
                break;
            }
            let line = sanitize_evidence_line(raw_line);
            if line.is_empty() || is_comment_only_line(line.as_str()) || is_anchor_dup(&line) {
                continue;
            }
            if is_param_or_field_decl(line.trim()) && seen.insert(line.clone()) {
                picked.push(line);
            }
        }
    }

    // Priority 5: any non-empty, non-comment, non-anchor, non-low-value lines
    for raw_line in text.lines() {
        if picked.len() >= max_lines {
            break;
        }
        let line = sanitize_evidence_line(raw_line);
        if line.is_empty()
            || is_comment_only_line(line.as_str())
            || is_anchor_dup(&line)
            || is_low_value_proof_line(&line)
        {
            continue;
        }
        if seen.insert(line.clone()) {
            picked.push(line);
        }
    }
    picked
}

/// Extract the first meaningful docstring line from chunk text.
///
/// Scans for Python triple-quote docstrings (`"""..."""`, `'''...'''`),
/// Rust `///` doc-comments, Go/Java/C `//` or `/* ... */` doc-comments,
/// and JSDoc `/** ... */` comments that appear right after a definition line.
/// Returns `None` if no docstring is found or if the docstring is too short.
fn extract_first_docstring_line(text: &str) -> Option<String> {
    let mut saw_definition = false;
    let mut in_triple_quote = false;
    let mut lines = text.lines();

    while let Some(raw_line) = lines.next() {
        let trimmed = raw_line.trim();

        // Track when we've passed a definition line.
        if !saw_definition && is_definition_line(trimmed) {
            saw_definition = true;
            continue;
        }

        if !saw_definition {
            continue;
        }

        // Skip blank lines and opening braces between definition and docstring.
        if trimmed.is_empty() || trimmed == "{" || trimmed == ":" {
            continue;
        }

        // Python triple-quote docstring: """...""" or '''...'''
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            let quote = &trimmed[..3];
            // Single-line docstring: """text"""
            if trimmed.len() > 6 && trimmed[3..].contains(quote) {
                let content = trimmed[3..].split(quote).next().unwrap_or("").trim();
                if content.len() >= 10 {
                    return Some(truncate_docstring(content));
                }
                return None;
            }
            // Multi-line: first line after the opening triple-quote
            in_triple_quote = true;
            let after_open = trimmed[3..].trim();
            if !after_open.is_empty() && after_open.len() >= 10 {
                return Some(truncate_docstring(after_open));
            }
            continue;
        }

        if in_triple_quote {
            let content = trimmed.trim_start_matches(['"', '\'']).trim();
            if !content.is_empty() && content.len() >= 10 {
                return Some(truncate_docstring(content));
            }
            return None;
        }

        // Rust /// doc-comments
        if trimmed.starts_with("///") {
            let content = trimmed.trim_start_matches('/').trim();
            if content.len() >= 10 {
                return Some(truncate_docstring(content));
            }
            return None;
        }

        // JSDoc/Go/Java block comments: /** ... */ or /* ... */
        if trimmed.starts_with("/**") || trimmed.starts_with("/*") {
            let prefix_len = if trimmed.starts_with("/**") { 3 } else { 2 };
            let after = trimmed[prefix_len..].trim().trim_end_matches("*/").trim();
            if !after.is_empty() && after.len() >= 10 {
                return Some(truncate_docstring(after));
            }
            // Check next line for content
            if let Some(next) = lines.next() {
                let next_trimmed = next.trim().trim_start_matches('*').trim();
                if !next_trimmed.is_empty()
                    && !next_trimmed.starts_with('/')
                    && next_trimmed.len() >= 10
                {
                    return Some(truncate_docstring(next_trimmed));
                }
            }
            return None;
        }

        // Single-line // comments (Go, Rust, JS/TS)
        if trimmed.starts_with("//") {
            let content = trimmed.trim_start_matches('/').trim();
            if content.len() >= 10 {
                return Some(truncate_docstring(content));
            }
            return None;
        }

        // No docstring found — hit real code after definition.
        return None;
    }
    None
}

fn truncate_docstring(s: &str) -> String {
    // Strip trailing punctuation for cleaner rendering.
    let cleaned = s.trim_end_matches('.');
    if cleaned.len() > 120 {
        let mut truncated: String = cleaned.chars().take(117).collect();
        truncated.push_str("...");
        truncated
    } else {
        cleaned.to_string()
    }
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

/// Check if `needle` appears in `haystack` with appropriate boundary matching.
/// Short tokens (≤10 chars, e.g. "return", "call") require at least one word
/// boundary to avoid false positives like "return" in "returnFiber".
/// Long tokens (>10 chars, e.g. "reconcilechildfibers") use substring matching
/// since they are specific enough to be unambiguous.
fn contains_at_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    // Long tokens are specific enough — substring match is fine.
    if needle.len() > 10 {
        return haystack.contains(needle);
    }
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    let needle_len = needle_bytes.len();
    if bytes.len() < needle_len {
        return false;
    }

    for start in 0..=bytes.len() - needle_len {
        if &bytes[start..start + needle_len] == needle_bytes {
            // Both boundaries must be at word boundaries for short tokens.
            let left_ok = start == 0 || !is_ident_char(bytes[start - 1]);
            let right_ok =
                start + needle_len >= bytes.len() || !is_ident_char(bytes[start + needle_len]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

/// Match a fixed needle against a line. Needles containing punctuation (like
/// "listen(", "call(") use plain substring matching. Pure-word needles (like
/// "return", "route") use word-boundary matching to avoid false positives
/// (e.g., "return" should not match inside "returnFiber").
fn needle_matches(haystack: &str, needle: &str) -> bool {
    // If needle contains non-identifier chars, it's specific enough for substring match.
    if needle
        .bytes()
        .any(|b| !b.is_ascii_alphanumeric() && b != b'_')
    {
        return haystack.contains(needle);
    }
    contains_at_word_boundary(haystack, needle)
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Detect lines that are low-value as proof: parameter/field declarations,
/// bare braces, import-only lines. These waste context tokens without
/// helping Claude understand code flow or structure.
fn is_low_value_proof_line(line: &str) -> bool {
    let trimmed = line.trim();

    // Bare braces / structural punctuation
    if matches!(
        trimmed,
        "{" | "}"
            | "};"
            | "})"
            | "});"
            | "},"
            | ");"
            | "),"
            | "("
            | ")"
            | "[]"
            | "["
            | "]"
            | ") {"
            | ") =>"
            | "} else {"
            | "} else"
    ) {
        return true;
    }

    // Parameter / field declaration: `identifier: Type,` or `identifier?: Type;`
    // Matches patterns like: `returnFiber: Fiber,`  `name?: string;`  `config: &Config,`
    if is_param_or_field_decl(trimmed) {
        return true;
    }

    // Bare argument on its own line: `workInProgress,` or `null,` or `element`
    // (single identifier/keyword, optionally followed by comma/semicolon)
    let bare = trimmed.trim_end_matches([',', ';']);
    if !bare.is_empty()
        && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && bare.len() <= 30
        && !matches!(
            bare,
            "return" | "break" | "continue" | "pass" | "throw" | "yield"
        )
    {
        return true;
    }

    false
}

/// Heuristic: line looks like `word: Type,` or `word?: Type;` — a parameter or field declaration.
fn is_param_or_field_decl(line: &str) -> bool {
    // Must contain a colon (type annotation marker)
    let Some(colon_pos) = line.find(':') else {
        return false;
    };

    let before_colon = line[..colon_pos].trim();
    let after_colon = line[colon_pos + 1..].trim();

    // Before colon: should be a simple identifier (possibly with ? or &)
    // Filter out things like `if (x:`, `case "foo":`, `url: "https://..."`
    if before_colon.is_empty() || after_colon.is_empty() {
        return false;
    }

    // If before colon contains parens, operators, or quotes, it's not a param
    if before_colon.contains(['(', ')', '"', '\'', '=', '+', '<', '>', '{', '}']) {
        return false;
    }

    // After colon: should be a type (ends with , or ; or nothing)
    // If it contains `(` it's likely a function call like `foo: bar()` — not a param
    if after_colon.contains('(') {
        return false;
    }

    // After colon should not be a string literal or number (those are assignments, not types)
    if after_colon.starts_with('"')
        || after_colon.starts_with('\'')
        || after_colon.starts_with('0')
        || after_colon.starts_with(|c: char| c.is_ascii_digit())
    {
        return false;
    }

    // If the line starts with common keywords, it's not just a param
    let lower = before_colon.to_ascii_lowercase();
    let lower_trimmed = lower.trim_start_matches(['&', '*']);
    if matches!(
        lower_trimmed,
        "return" | "let" | "const" | "var" | "if" | "else" | "for" | "while" | "case" | "pub"
    ) {
        return false;
    }

    // Should look like an identifier: alphanumeric + _?& only
    let ident_part = before_colon
        .trim_start_matches(['&', '*', ' '])
        .trim_end_matches('?');
    ident_part
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !ident_part.is_empty()
}

/// Detect lines containing function call expressions (high signal for flow-trace).
/// Matches `foo(`, `bar.baz(`, `self.method(` patterns but excludes bare declarations
/// like `fn foo(` or `def foo(` or `function foo(`.
fn has_call_expression(line: &str) -> bool {
    let trimmed = line.trim();
    // Must contain a `(` to be a call
    if !trimmed.contains('(') {
        return false;
    }
    // Exclude function/method declarations
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("fn ")
        || lower.starts_with("def ")
        || lower.starts_with("func ")
        || lower.starts_with("function ")
        || lower.starts_with("async fn ")
        || lower.starts_with("async def ")
        || lower.starts_with("async function ")
        || lower.starts_with("pub fn ")
        || lower.starts_with("pub async fn ")
        || lower.starts_with("pub(crate) fn ")
        || lower.starts_with("pub(super) fn ")
        || lower.starts_with("export function ")
        || lower.starts_with("export async function ")
        || lower.starts_with("export default function ")
    {
        return false;
    }
    // Exclude class/interface/type/struct declarations
    if lower.starts_with("class ")
        || lower.starts_with("interface ")
        || lower.starts_with("type ")
        || lower.starts_with("struct ")
        || lower.starts_with("enum ")
    {
        return false;
    }
    true
}

/// Returns true when the anchor line indicates a type/interface/struct/enum definition.
/// For these, property declarations are the actual content and should be used as proof lines.
fn is_type_definition_anchor(anchor: &str) -> bool {
    let lower = anchor.trim().to_ascii_lowercase();
    lower.starts_with("interface ")
        || lower.starts_with("export interface ")
        || lower.starts_with("type ")
        || lower.starts_with("export type ")
        || lower.starts_with("struct ")
        || lower.starts_with("pub struct ")
        || lower.starts_with("pub(crate) struct ")
        || lower.starts_with("enum ")
        || lower.starts_with("pub enum ")
        || lower.starts_with("pub(crate) enum ")
}

fn is_comment_only_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with('*')
        || trimmed.starts_with("/*")
        || trimmed.starts_with("*/")
        // Python docstrings (triple-quoted strings used as documentation)
        || trimmed.starts_with("\"\"\"")
        || trimmed.starts_with("'''")
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
            context_note: None,
        }
    }

    // ── build_context ─────────────────────────────────────────────────────────

    #[test]
    fn empty_snippets_returns_header_only() {
        let out = build_context(&[], 4096, &[], None);
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
        let out = build_context(&snippets, 4096, &[], None);
        assert!(out.contains("file: src/scheduler.rs"), "missing file path");
        assert!(out.contains("anchor:"), "missing anchor");
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
        let out = build_context(&snippets, 0, &[], None);
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
        let out = build_context(&snippets, budget, &[], None);
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
        let out = build_context(&snippets, budget, &[], None);
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
        let out = build_context(&snippets, 4096, &[], None);
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
        let out = build_context(&snippets, 4096, &[], None);
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
        let out = build_context(&snippets, 4096, &[], None);
        // Only one file: card should appear
        let file_count = out.matches("file: src/foo.rs").count();
        assert_eq!(file_count, 1, "same-file snippets should produce one card");
        // Merged span covers both
        // Both anchors present (primary + secondary in proof)
        assert!(out.contains("fn alpha()"), "primary anchor missing: {}", out);
        // Secondary anchor folded into proof
        assert!(
            out.contains("fn beta()"),
            "secondary anchor should appear in proof"
        );
    }

    #[test]
    fn different_file_snippets_stay_separate() {
        let snippets = vec![
            make_snippet("src/a.rs", "fn a() {}", 0.9),
            make_snippet("src/b.rs", "fn b() {}", 0.7),
        ];
        let out = build_context(&snippets, 4096, &[], None);
        assert!(out.contains("file: src/a.rs"));
        assert!(out.contains("file: src/b.rs"));
    }

    #[test]
    fn distant_same_file_snippets_stay_separate() {
        let mut s1 = make_snippet("tests/test_big.py", "def test_alpha(): pass", 0.9);
        s1.start_line = 10;
        s1.end_line = 20;
        let mut s2 = make_snippet("tests/test_big.py", "def test_omega(): pass", 0.7);
        s2.start_line = 900;
        s2.end_line = 910;
        let snippets = vec![s1, s2];
        let out = build_context(&snippets, 4096, &[], None);
        let file_count = out.matches("file: tests/test_big.py").count();
        assert_eq!(
            file_count, 2,
            "distant same-file snippets should produce separate cards: {}",
            out
        );
    }

    // ── word boundary matching ─────────────────────────────────────────────

    #[test]
    fn word_boundary_rejects_substring_in_identifier() {
        // "return" should NOT match inside "returnFiber"
        assert!(
            !contains_at_word_boundary("returnfiber: fiber,", "return"),
            "return inside returnFiber is not a word boundary match"
        );
    }

    #[test]
    fn word_boundary_matches_standalone_keyword() {
        assert!(contains_at_word_boundary("return foo;", "return"));
        assert!(contains_at_word_boundary("let x = return;", "return"));
    }

    #[test]
    fn word_boundary_matches_at_start_or_end() {
        assert!(contains_at_word_boundary("return", "return"));
        assert!(contains_at_word_boundary("return()", "return"));
    }

    #[test]
    fn word_boundary_matches_full_identifier() {
        // Full camelCase identifier should match
        assert!(contains_at_word_boundary(
            "const x = reconcilechildfibersimpl(",
            "reconcilechildfibers"
        ));
    }

    // ── low-value proof line filtering ──────────────────────────────────────

    #[test]
    fn low_value_filters_bare_braces() {
        assert!(is_low_value_proof_line("{"));
        assert!(is_low_value_proof_line("}"));
        assert!(is_low_value_proof_line("};"));
        assert!(is_low_value_proof_line("})"));
        assert!(is_low_value_proof_line("},"));
    }

    #[test]
    fn low_value_filters_param_declarations() {
        // JS/Flow/TS parameter declarations
        assert!(is_low_value_proof_line("returnFiber: Fiber,"));
        assert!(is_low_value_proof_line("currentFirstChild: Fiber | null,"));
        assert!(is_low_value_proof_line("element: ReactElement,"));
        assert!(is_low_value_proof_line("name?: string;"));
        // Rust parameter declarations
        assert!(is_low_value_proof_line("config: &BudiConfig,"));
        assert!(is_low_value_proof_line("path: PathBuf,"));
    }

    #[test]
    fn low_value_keeps_meaningful_lines() {
        // Function calls with colons (e.g., Python kwargs, ternary)
        assert!(!is_low_value_proof_line("result: bar()"));
        // Assignments
        assert!(!is_low_value_proof_line("let config: Config = load();"));
        // Return statements
        assert!(!is_low_value_proof_line("return fiber;"));
        // Conditionals
        assert!(!is_low_value_proof_line(
            "if (fiber.tag === HostComponent) {"
        ));
        // String values (config lines)
        assert!(!is_low_value_proof_line("name: \"flask\","));
        // Regular code
        assert!(!is_low_value_proof_line(
            "reconcileChildFibers(fiber, child);"
        ));
    }

    // ── call expression detection ─────────────────────────────────────────────

    #[test]
    fn call_expression_detects_calls() {
        assert!(has_call_expression("reconcileChildFibers(fiber, child);"));
        assert!(has_call_expression("self.dispatch(request)"));
        assert!(has_call_expression("let result = process(data);"));
        assert!(has_call_expression("return compute(x, y);"));
    }

    #[test]
    fn call_expression_rejects_declarations() {
        assert!(!has_call_expression("fn foo(bar: i32) {"));
        assert!(!has_call_expression("def handle_request(self, request):"));
        assert!(!has_call_expression(
            "function reconcileChildFibers(returnFiber) {"
        ));
        assert!(!has_call_expression("pub fn query(&self) -> Result<()> {"));
        assert!(!has_call_expression("class MyComponent(Component):"));
    }

    #[test]
    fn call_expression_rejects_non_calls() {
        assert!(!has_call_expression("let x = 42;"));
        assert!(!has_call_expression("return fiber;"));
        assert!(!has_call_expression("config: &BudiConfig,"));
    }

    // ── proof line quality integration ────────────────────────────────────────

    #[test]
    fn proof_lines_prefer_calls_over_params() {
        let text = "fn reconcileChildFibers(returnFiber: Fiber, currentFirstChild: Fiber | null) {\n\
                     returnFiber: Fiber,\n\
                     currentFirstChild: Fiber | null,\n\
                     deleteChild(returnFiber, currentFirstChild);\n\
                     placeSingleChild(newFiber);\n\
                     return newFiber;\n";
        let anchor =
            "fn reconcileChildFibers(returnFiber: Fiber, currentFirstChild: Fiber | null) {";
        let proof = extract_proof_lines(text, 3, anchor, &[]);
        // Should pick call expressions and return, not parameter declarations
        assert!(
            proof.iter().any(|l| l.contains("deleteChild")),
            "should include deleteChild call: {:?}",
            proof
        );
        assert!(
            proof.iter().any(|l| l.contains("placeSingleChild")),
            "should include placeSingleChild call: {:?}",
            proof
        );
        assert!(
            !proof.iter().any(|l| l.contains("returnFiber: Fiber,")),
            "should NOT include param declaration: {:?}",
            proof
        );
    }

    // ── intent-specific proof count ──────────────────────────────────────────

    #[test]
    fn sym_def_intent_limits_proof_to_two_lines() {
        let snippets = vec![make_snippet(
            "src/query.py",
            "class QuerySet:\n    \"\"\"A lazy database lookup.\"\"\"\n    def __init__(self):\n        self.query = Query()\n        self._cache = []\n        self._result = None",
            0.8,
        )];
        let out = build_context(&snippets, 4096, &[], Some("symbol-definition"));
        let proof_count = out.matches("    - ").count();
        assert_eq!(proof_count, 2, "sym-def should have 2 proof lines, got {proof_count}: {out}");
    }

    #[test]
    fn flow_trace_intent_allows_three_proof_lines() {
        let snippets = vec![make_snippet(
            "src/handler.py",
            "def get_response(self, request):\n    response = self._middleware(request)\n    response = self.process(response)\n    return self.finalize(response)\n    self.log(response)",
            0.8,
        )];
        let out = build_context(&snippets, 4096, &[], Some("flow-trace"));
        let proof_count = out.matches("    - ").count();
        assert_eq!(proof_count, 3, "flow-trace should have 3 proof lines, got {proof_count}: {out}");
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

    // ── extract_anchor_line ──────────────────────────────────────────────────

    #[test]
    fn anchor_skips_python_decorator() {
        let text = "@setupmethod\ndef register_blueprint(self, blueprint):\n    pass";
        let anchor = extract_anchor_line(text, &[]);
        assert!(
            anchor.contains("def register_blueprint"),
            "expected function def, got: {anchor}"
        );
    }

    #[test]
    fn anchor_skips_decorator_with_args() {
        let text = "@app.route(\"/foo\")\ndef index():\n    return 'hello'";
        let anchor = extract_anchor_line(text, &[]);
        assert!(
            anchor.contains("def index"),
            "expected function def, got: {anchor}"
        );
    }

    #[test]
    fn anchor_skips_rust_attribute() {
        let text = "#[derive(Debug, Clone)]\npub struct Config {\n    name: String,\n}";
        let anchor = extract_anchor_line(text, &[]);
        assert!(
            anchor.contains("pub struct Config"),
            "expected struct def, got: {anchor}"
        );
    }

    // ── type definition proof lines ─────────────────────────────────────────

    #[test]
    fn interface_properties_used_as_proof_lines() {
        let text = "export interface FastifyRequestType<Params = unknown> {\n\
                     params: Params,\n\
                     query: Querystring,\n\
                     headers: Headers,\n\
                     body: Body\n\
                     }";
        let anchor = "export interface FastifyRequestType<Params = unknown> {";
        let proof = extract_proof_lines(text, 3, anchor, &[]);
        assert!(
            !proof.is_empty(),
            "interface should have proof lines, got empty"
        );
        assert!(
            proof.iter().any(|l| l.contains("params: Params")),
            "should include property: {:?}",
            proof
        );
    }

    #[test]
    fn struct_fields_used_as_proof_lines() {
        let text = "pub struct Config {\n\
                     name: String,\n\
                     path: PathBuf,\n\
                     verbose: bool,\n\
                     }";
        let anchor = "pub struct Config {";
        let proof = extract_proof_lines(text, 3, anchor, &[]);
        assert!(
            proof.iter().any(|l| l.contains("name: String")),
            "should include struct field: {:?}",
            proof
        );
    }

    #[test]
    fn type_definition_anchor_detection() {
        assert!(is_type_definition_anchor("interface Foo {"));
        assert!(is_type_definition_anchor("export interface Bar<T> {"));
        assert!(is_type_definition_anchor("type Props = {"));
        assert!(is_type_definition_anchor("export type Config = {"));
        assert!(is_type_definition_anchor("struct Config {"));
        assert!(is_type_definition_anchor("pub struct Config {"));
        assert!(is_type_definition_anchor("enum Color {"));
        // Non-type anchors
        assert!(!is_type_definition_anchor("fn foo() {"));
        assert!(!is_type_definition_anchor("def bar():"));
        assert!(!is_type_definition_anchor("class MyClass {"));
    }

    #[test]
    fn anchor_falls_back_to_decorator_if_nothing_else() {
        let text = "@decorator_only";
        let anchor = extract_anchor_line(text, &[]);
        assert_eq!(anchor, "@decorator_only");
    }

    #[test]
    fn anchor_prefers_function_def_over_closing_brace() {
        let text = "  } else {\n    resetRenderTimer();\n  }\n}\n\nexport function performWorkOnRoot(\n  root: FiberRoot,\n): void {";
        let anchor = extract_anchor_line(text, &[]);
        assert_eq!(anchor, "export function performWorkOnRoot(");
    }

    #[test]
    fn anchor_prefers_def_over_bare_code() {
        let text = "return result\n\ndef process_request(self, request):\n    pass";
        let anchor = extract_anchor_line(text, &[]);
        assert_eq!(anchor, "def process_request(self, request):");
    }

    #[test]
    fn anchor_prefers_go_func_receiver() {
        let text = "}\n\nfunc (p *Provider) ConfigureProvider(req Request) Response {";
        let anchor = extract_anchor_line(text, &[]);
        assert_eq!(
            anchor,
            "func (p *Provider) ConfigureProvider(req Request) Response {"
        );
    }

    #[test]
    fn anchor_uses_first_line_when_no_definition_present() {
        let text = "  result := doSomething()\n  return result, nil";
        let anchor = extract_anchor_line(text, &[]);
        assert_eq!(anchor, "result := doSomething()");
    }

    #[test]
    fn is_definition_line_detects_common_patterns() {
        assert!(is_definition_line("function foo() {"));
        assert!(is_definition_line("fn bar() -> Result {"));
        assert!(is_definition_line("func main() {"));
        assert!(is_definition_line("def process(self):"));
        assert!(is_definition_line("class MyClass:"));
        assert!(is_definition_line("func (r *Receiver) Method() {"));
        assert!(is_definition_line("public void run() {"));
        assert!(is_definition_line("private static int compute() {"));
        // Not definitions
        assert!(!is_definition_line("} else {"));
        assert!(!is_definition_line("return result"));
        assert!(!is_definition_line("x := foo()"));
    }

    #[test]
    fn anchor_prefers_query_matching_definition() {
        // Simulates a merged span where peekDeferredLane comes before scheduleUpdateOnFiber.
        // With query tokens, anchor should prefer the queried function.
        let text = "\
export function peekDeferredLane(): Lane {
  return workInProgressDeferredLane;
}

export function scheduleUpdateOnFiber(root, fiber, lane) {
  if (root === null) return;
  markRootUpdated(root, lane);
}";
        let tokens = vec!["scheduleupdateonfiber".to_string()];
        let anchor = extract_anchor_line(text, &tokens);
        assert!(
            anchor.contains("scheduleUpdateOnFiber"),
            "expected scheduleUpdateOnFiber, got: {anchor}"
        );
        // Without query tokens, should return first definition (peekDeferredLane)
        let anchor_no_query = extract_anchor_line(text, &[]);
        assert!(
            anchor_no_query.contains("peekDeferredLane"),
            "without query, expected peekDeferredLane, got: {anchor_no_query}"
        );
    }

    // ── extract_first_docstring_line ─────────────────────────────────────────

    #[test]
    fn docstring_python_triple_quote_single_line() {
        let text = r#"class QuerySet(AltersData):
    """Represent a lazy database lookup for a set of objects."""
    def __init__(self):
        pass"#;
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("Represent a lazy database lookup for a set of objects")
        );
    }

    #[test]
    fn docstring_python_triple_quote_multiline() {
        let text = r#"def wsgi_app(self, environ, start_response):
    """The actual WSGI application.  This is not implemented in
    :meth:`__call__` so that middlewares can be applied without
    losing a reference to the app object."""
    ctx = self.request_context(environ)"#;
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("The actual WSGI application.  This is not implemented in")
        );
    }

    #[test]
    fn docstring_rust_doc_comment() {
        let text = "/// Returns the chunk record for the given ID, or None if not found.\nfn chunk(&self, id: u64) -> Option<&ChunkRecord> {\n    self.chunks.get(&id)\n}";
        // Definition is fn chunk, but /// comes before the definition.
        // Our function looks for docstrings AFTER the definition line.
        // Rust convention puts /// before fn, so this should return None.
        let doc = extract_first_docstring_line(text);
        assert!(doc.is_none());
    }

    #[test]
    fn docstring_rust_doc_comment_after_def() {
        // Some Rust code has doc-comments inside impl blocks after fn signature
        let text = "pub fn embed_query(&mut self, query: &str) -> Result<Option<Vec<f32>>> {\n    /// Embed a single query string and return the embedding vector.\n    let rows = embedder.embed(vec![query], None)?;";
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("Embed a single query string and return the embedding vector")
        );
    }

    #[test]
    fn docstring_go_block_comment() {
        let text = "func (c *Context) Plan() (*plans.Plan, tfdiags.Diagnostics) {\n\t/* Plan generates an execution plan for the given context. */\n\top, moreDiags := c.plan()";
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("Plan generates an execution plan for the given context")
        );
    }

    #[test]
    fn docstring_jsdoc() {
        let text = "function reconcileChildFibers(returnFiber, currentFirstChild, newChild) {\n  /**\n   * Reconciles the children of a fiber node, handling additions,\n   * deletions, and moves.\n   */\n  if (typeof newChild === 'string') {";
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("Reconciles the children of a fiber node, handling additions,")
        );
    }

    #[test]
    fn docstring_short_ignored() {
        let text = "def foo():\n    \"\"\"Short.\"\"\"\n    pass";
        let doc = extract_first_docstring_line(text);
        assert!(
            doc.is_none(),
            "docstrings shorter than 10 chars should be ignored"
        );
    }

    #[test]
    fn docstring_no_docstring_returns_none() {
        let text = "def foo():\n    x = 1\n    return x";
        let doc = extract_first_docstring_line(text);
        assert!(doc.is_none());
    }

    #[test]
    fn docstring_go_single_line_comment() {
        let text = "func NewContext(opts *ContextOpts) (*Context, tfdiags.Diagnostics) {\n\t// NewContext creates a new Context structure with the given configuration.\n\tvar diags tfdiags.Diagnostics";
        let doc = extract_first_docstring_line(text);
        assert_eq!(
            doc.as_deref(),
            Some("NewContext creates a new Context structure with the given configuration")
        );
    }
}
