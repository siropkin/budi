use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub start_line: usize,
    pub end_line: usize,
    pub language: String,
    pub symbol_hint: Option<String>,
    pub text: String,
}

fn looks_like_symbol(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Rust / Python / bare declarations
    trimmed.starts_with("fn ")
        || trimmed.starts_with("func ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub async fn ")
        || trimmed.starts_with("async fn ")
        || trimmed.starts_with("pub struct ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("interface ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("pub enum ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("pub trait ")
        || trimmed.starts_with("trait ")
        || trimmed.starts_with("pub type ")
        || trimmed.starts_with("type ")
        || trimmed.starts_with("def ")
        || trimmed.starts_with("async def ")
        // Kotlin
        || trimmed.starts_with("fun ")
        || trimmed.starts_with("open fun ")
        || trimmed.starts_with("override fun ")
        || trimmed.starts_with("private fun ")
        || trimmed.starts_with("internal fun ")
        || trimmed.starts_with("data class ")
        || trimmed.starts_with("sealed class ")
        || trimmed.starts_with("object ")
        // Ruby
        || trimmed.starts_with("module ")
        // Swift
        || trimmed.starts_with("protocol ")
        // JS/TS bare and exported forms
        || trimmed.starts_with("function ")
        || trimmed.starts_with("async function ")
        || trimmed.starts_with("export function ")
        || trimmed.starts_with("export async function ")
        || trimmed.starts_with("export default function ")
        || trimmed.starts_with("export class ")
        || trimmed.starts_with("export interface ")
        || trimmed.starts_with("export enum ")
        || trimmed.starts_with("export const ")
        || trimmed.starts_with("export type ")
}

/// Language keywords to skip when extracting the symbol name.
const SYMBOL_KEYWORDS: &[&str] = &[
    // Rust
    "pub",
    "fn",
    "async",
    "unsafe",
    "extern",
    "const",
    "static",
    "mut",
    "impl",
    "trait",
    "struct",
    "enum",
    "type",
    "let",
    "use",
    // JS/TS
    "function",
    "export",
    "default",
    "interface",
    "class",
    "var",
    "let",
    "const",
    "abstract",
    "override",
    // Python
    "def",
    "class",
    "async",
    // Java/C#/Go
    "public",
    "private",
    "protected",
    "static",
    "final",
    "abstract",
    "virtual",
    "override",
    "inline",
    "func",
    // Ruby
    "module",
    "end",
    // Kotlin/Scala
    "fun",
    "object",
    "companion",
    "data",
    "sealed",
    "open",
    "internal",
    "val",
    "var",
    "case",
    "trait",
    // Swift
    "protocol",
    "extension",
    "mutating",
    "init",
    "deinit",
    "subscript",
    "actor",
];

fn symbol_from_line(line: &str) -> Option<String> {
    let mut trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('@') || trimmed.starts_with("#[") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("func") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('(')
            && let Some((_, after_receiver)) = rest.split_once(')')
        {
            trimmed = after_receiver.trim_start();
        }
    }
    // Walk word tokens, skip language keywords, return first real identifier
    let mut token = String::new();
    for ch in trimmed.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            if !token.is_empty() {
                let tok = std::mem::take(&mut token);
                if !SYMBOL_KEYWORDS.contains(&tok.as_str()) && tok.len() >= 2 {
                    return Some(tok);
                }
            }
            // Stop at '(' — don't read into parameter types
            if ch == '(' {
                break;
            }
        }
    }
    // Handle token at end of string
    if !token.is_empty() && !SYMBOL_KEYWORDS.contains(&token.as_str()) && token.len() >= 2 {
        return Some(token);
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum AstLanguageKind {
    JavaScript,
    // TypeScript grammar fallback for Flow-typed .js files that fail JS parsing.
    // Skips lexical_declaration/variable_declaration boundary kinds to avoid
    // 1-line const/let chunks inside large reconciler functions.
    JavaScriptTSFallback,
    TypeScript,
    Python,
    Rust,
    Go,
    Java,
    Cpp,
    CSharp,
    Ruby,
    Kotlin,
    Swift,
    Scala,
    Php,
}

impl AstLanguageKind {
    fn as_label(self) -> &'static str {
        match self {
            AstLanguageKind::JavaScript | AstLanguageKind::JavaScriptTSFallback => "javascript",
            AstLanguageKind::TypeScript => "typescript",
            AstLanguageKind::Python => "python",
            AstLanguageKind::Rust => "rust",
            AstLanguageKind::Go => "go",
            AstLanguageKind::Java => "java",
            AstLanguageKind::Cpp => "cpp",
            AstLanguageKind::CSharp => "csharp",
            AstLanguageKind::Ruby => "ruby",
            AstLanguageKind::Kotlin => "kotlin",
            AstLanguageKind::Swift => "swift",
            AstLanguageKind::Scala => "scala",
            AstLanguageKind::Php => "php",
        }
    }
}

pub fn language_label_for_path(file_path: &str) -> String {
    if let Some((kind, _)) = ast_language_for_path(file_path) {
        return kind.as_label().to_string();
    }
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".json") {
        "json".to_string()
    } else if lower.ends_with(".toml") {
        "toml".to_string()
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        "yaml".to_string()
    } else if lower.ends_with(".md") {
        "markdown".to_string()
    } else if lower.ends_with(".sh") || lower.ends_with(".bash") || lower.ends_with(".zsh") {
        "shell".to_string()
    } else if lower.ends_with(".sql") {
        "sql".to_string()
    } else if lower.ends_with(".graphql") || lower.ends_with(".gql") {
        "graphql".to_string()
    } else if lower.ends_with(".proto") {
        "protobuf".to_string()
    } else {
        "unknown".to_string()
    }
}

pub fn ecosystem_tags_for_chunk(file_path: &str, language: &str, text: &str) -> Vec<String> {
    crate::repo_plugins::ecosystem_tags_for_chunk(file_path, language, text)
}

pub fn detect_repo_ecosystems(
    repo_root: &std::path::Path,
    files: &[crate::index::FileRecord],
) -> Vec<String> {
    crate::repo_plugins::detect_repo_ecosystems(repo_root, files)
}

fn ast_language_for_path(file_path: &str) -> Option<(AstLanguageKind, Language)> {
    let lower = file_path.to_ascii_lowercase();
    if lower.ends_with(".ts") {
        return Some((
            AstLanguageKind::TypeScript,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ));
    }
    if lower.ends_with(".tsx") {
        return Some((
            AstLanguageKind::TypeScript,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
        ));
    }
    if lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
    {
        return Some((
            AstLanguageKind::JavaScript,
            tree_sitter_javascript::LANGUAGE.into(),
        ));
    }
    if lower.ends_with(".py") {
        return Some((AstLanguageKind::Python, tree_sitter_python::LANGUAGE.into()));
    }
    if lower.ends_with(".rs") {
        return Some((AstLanguageKind::Rust, tree_sitter_rust::LANGUAGE.into()));
    }
    if lower.ends_with(".go") {
        return Some((AstLanguageKind::Go, tree_sitter_go::LANGUAGE.into()));
    }
    if lower.ends_with(".java") {
        return Some((AstLanguageKind::Java, tree_sitter_java::LANGUAGE.into()));
    }
    if lower.ends_with(".cs") {
        return Some((
            AstLanguageKind::CSharp,
            tree_sitter_c_sharp::LANGUAGE.into(),
        ));
    }
    if lower.ends_with(".c")
        || lower.ends_with(".cc")
        || lower.ends_with(".cpp")
        || lower.ends_with(".cxx")
        || lower.ends_with(".h")
        || lower.ends_with(".hh")
        || lower.ends_with(".hpp")
        || lower.ends_with(".hxx")
    {
        return Some((AstLanguageKind::Cpp, tree_sitter_cpp::LANGUAGE.into()));
    }
    if lower.ends_with(".rb") {
        return Some((AstLanguageKind::Ruby, tree_sitter_ruby::LANGUAGE.into()));
    }
    if lower.ends_with(".kt") || lower.ends_with(".kts") {
        return Some((
            AstLanguageKind::Kotlin,
            tree_sitter_kotlin_ng::LANGUAGE.into(),
        ));
    }
    if lower.ends_with(".swift") {
        return Some((AstLanguageKind::Swift, tree_sitter_swift::LANGUAGE.into()));
    }
    if lower.ends_with(".scala") || lower.ends_with(".sc") {
        return Some((AstLanguageKind::Scala, tree_sitter_scala::LANGUAGE.into()));
    }
    if lower.ends_with(".php") {
        return Some((AstLanguageKind::Php, tree_sitter_php::LANGUAGE_PHP.into()));
    }
    None
}

fn is_boundary_kind(kind: &str, language: AstLanguageKind) -> bool {
    match language {
        AstLanguageKind::JavaScript => matches!(
            kind,
            "export_statement"
                | "function_declaration"
                | "class_declaration"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        // TS-parsed JS files omit lexical/variable declarations to avoid
        // creating 1-line const/let chunks inside large Flow-typed functions.
        AstLanguageKind::JavaScriptTSFallback => matches!(
            kind,
            "export_statement" | "function_declaration" | "class_declaration"
        ),
        AstLanguageKind::TypeScript => matches!(
            kind,
            "export_statement"
                | "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "lexical_declaration"
                | "variable_declaration"
        ),
        AstLanguageKind::Python => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        AstLanguageKind::Rust => matches!(
            kind,
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "impl_item" | "mod_item"
        ),
        AstLanguageKind::Go => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "var_declaration"
                | "const_declaration"
        ),
        AstLanguageKind::Java => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "field_declaration"
        ),
        AstLanguageKind::Cpp => matches!(
            kind,
            "function_definition"
                | "class_specifier"
                | "struct_specifier"
                | "namespace_definition"
                | "enum_specifier"
                | "template_declaration"
                | "declaration"
        ),
        AstLanguageKind::CSharp => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "struct_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "field_declaration"
        ),
        AstLanguageKind::Ruby => matches!(kind, "method" | "singleton_method" | "class" | "module"),
        AstLanguageKind::Kotlin => matches!(
            kind,
            "function_declaration" | "class_declaration" | "object_declaration"
        ),
        AstLanguageKind::Swift => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "protocol_declaration"
                | "init_declaration"
                | "deinit_declaration"
                | "subscript_declaration"
        ),
        AstLanguageKind::Scala => matches!(
            kind,
            "function_definition"
                | "class_definition"
                | "object_definition"
                | "trait_definition"
                | "enum_definition"
        ),
        AstLanguageKind::Php => matches!(
            kind,
            "function_definition"
                | "class_declaration"
                | "method_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
        ),
    }
}

fn collect_top_level_chunks(
    root: Node<'_>,
    content: &str,
    lines: &[&str],
    chunk_language: &str,
    language_kind: AstLanguageKind,
    lines_per_chunk: usize,
    overlap: usize,
) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let context = NodeChunkingContext {
        content,
        lines,
        chunk_language,
        lines_per_chunk,
        overlap,
        language_kind,
    };
    for idx in 0..root.named_child_count() {
        let Ok(idx) = u32::try_from(idx) else {
            continue;
        };
        let Some(node) = root.named_child(idx) else {
            continue;
        };
        if !is_boundary_kind(node.kind(), language_kind) {
            continue;
        }
        append_node_chunks(&mut chunks, node, &context);
    }
    chunks.sort_by_key(|chunk| (chunk.start_line, chunk.end_line));
    chunks.dedup_by(|left, right| {
        left.start_line == right.start_line
            && left.end_line == right.end_line
            && left.text == right.text
    });
    merge_small_siblings(chunks, lines, chunk_language, lines_per_chunk)
}

fn ast_top_level_chunks(
    file_path: &str,
    content: &str,
    lines_per_chunk: usize,
    overlap: usize,
) -> Option<Vec<Chunk>> {
    let (language_kind, language) = ast_language_for_path(file_path)?;
    let chunk_language = language_kind.as_label();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return None;
    }
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    // If JS grammar reports errors (e.g. Flow/TS type annotations in .js files),
    // retry with TypeScript grammar. Only files that currently error get different treatment
    // (pure JS files parse cleanly and are unaffected). Use JavaScriptTSFallback boundary
    // kinds to avoid chunk explosion from lexical_declaration / type_alias_declaration.
    if root.has_error() && matches!(language_kind, AstLanguageKind::JavaScript) {
        let ts_language: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        if let Ok(()) = parser.set_language(&ts_language)
            && let Some(ts_tree) = parser.parse(content, None)
        {
            let ts_root = ts_tree.root_node();
            if !ts_root.has_error() {
                // TypeScript grammar parsed cleanly — use it with JavaScriptTSFallback
                // boundary kinds (no lexical_declaration) to avoid tiny const/let chunks.
                let lines: Vec<&str> = content.lines().collect();
                if lines.is_empty() {
                    return None;
                }
                return Some(collect_top_level_chunks(
                    ts_root,
                    content,
                    &lines,
                    AstLanguageKind::JavaScriptTSFallback.as_label(),
                    AstLanguageKind::JavaScriptTSFallback,
                    lines_per_chunk,
                    overlap,
                ));
            }
        }
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let chunks = collect_top_level_chunks(
        root,
        content,
        &lines,
        chunk_language,
        language_kind,
        lines_per_chunk,
        overlap,
    );
    if chunks.is_empty() {
        return None;
    }
    Some(chunks)
}

/// Collect all boundary-kind descendant nodes within `node`, stopping at each boundary
/// (don't recurse into boundary nodes — they'll be chunked independently).
fn collect_boundary_descendants<'a>(
    node: Node<'a>,
    language_kind: AstLanguageKind,
    out: &mut Vec<Node<'a>>,
) {
    let count = node.named_child_count();
    for i in 0..count {
        let Ok(i) = u32::try_from(i) else {
            continue;
        };
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if is_boundary_kind(child.kind(), language_kind) {
            out.push(child);
        } else {
            collect_boundary_descendants(child, language_kind, out);
        }
    }
}

struct NodeChunkingContext<'a> {
    content: &'a str,
    lines: &'a [&'a str],
    chunk_language: &'a str,
    lines_per_chunk: usize,
    overlap: usize,
    language_kind: AstLanguageKind,
}

fn append_node_chunks(out: &mut Vec<Chunk>, node: Node<'_>, context: &NodeChunkingContext<'_>) {
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    if start_line == 0 || end_line < start_line {
        return;
    }
    let snippet = match context.content.get(node.start_byte()..node.end_byte()) {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => return,
    };
    let span = end_line.saturating_sub(start_line) + 1;
    if span > context.lines_per_chunk.saturating_mul(2) {
        if matches!(context.language_kind, AstLanguageKind::Go) {
            // Large Go methods often contain many inner var/const declarations. Recursing into
            // those descendants fragments the method body into tiny chunks and breaks same-file
            // continuation for wrapper->implementation flows like Context.Plan -> PlanAndEval.
            out.extend(line_chunks_from_range(
                context.lines,
                start_line.saturating_sub(1),
                end_line,
                context.chunk_language,
                context.lines_per_chunk,
                context.overlap,
            ));
            return;
        }
        // Try boundary-kind descendants before falling back to fixed stride.
        // This handles cases like createChildReconciler (1600+ lines) containing many
        // nested function declarations that each deserve their own chunk.
        let mut boundary_children: Vec<Node<'_>> = Vec::new();
        collect_boundary_descendants(node, context.language_kind, &mut boundary_children);
        if !boundary_children.is_empty() {
            // Emit a chunk for the preamble BEFORE the first boundary child.
            // For Python/Java classes, this captures the class declaration, docstring,
            // and attribute annotations (e.g. `class Session(...):` + 50 lines of
            // type hints) that would otherwise fall through the cracks.
            // Restrict to Python and Java where large class preambles are common.
            // JS/TS export wrappers don't need this — their preambles are typically
            // just 1-2 lines of export/function boilerplate.
            if matches!(
                context.language_kind,
                AstLanguageKind::Python
                    | AstLanguageKind::Java
                    | AstLanguageKind::Ruby
                    | AstLanguageKind::Kotlin
                    | AstLanguageKind::Scala
                    | AstLanguageKind::Php
            ) {
                let first_child_start = boundary_children[0].start_position().row + 1;
                let preamble_start = start_line.saturating_sub(1);
                let preamble_end = first_child_start.saturating_sub(1);
                if preamble_end > preamble_start + 2 {
                    out.extend(line_chunks_from_range(
                        context.lines,
                        preamble_start,
                        preamble_end,
                        context.chunk_language,
                        context.lines_per_chunk,
                        context.overlap,
                    ));
                }
            }
            for child in boundary_children {
                append_node_chunks(out, child, context);
            }
            return;
        }
        out.extend(line_chunks_from_range(
            context.lines,
            start_line.saturating_sub(1),
            end_line,
            context.chunk_language,
            context.lines_per_chunk,
            context.overlap,
        ));
        return;
    }
    // For AST boundary nodes the first non-blank line IS the declaration,
    // so skip the looks_like_symbol gate and extract directly.
    let symbol_hint = snippet
        .lines()
        .find(|line| looks_like_symbol(line))
        .and_then(symbol_from_line);
    out.push(Chunk {
        start_line,
        end_line,
        language: context.chunk_language.to_string(),
        symbol_hint,
        text: snippet,
    });
}

/// Merge consecutive small chunks into combined chunks.
///
/// When adjacent AST boundary nodes are each small (≤ `SMALL_THRESHOLD` lines),
/// they produce weak embeddings individually (e.g. single-line `var x = require(...)`,
/// `export { default as X }`, `const Y = 5`). Merging them into a single chunk
/// produces better embeddings and reduces HNSW index clutter.
///
/// Inspired by the cAST paper (arXiv 2506.15655) which showed +4.3 Recall@5
/// from sibling merging in AST-based chunking.
fn merge_small_siblings(
    chunks: Vec<Chunk>,
    lines: &[&str],
    chunk_language: &str,
    lines_per_chunk: usize,
) -> Vec<Chunk> {
    const SMALL_THRESHOLD: usize = 5;

    if chunks.len() <= 1 {
        return chunks;
    }

    let mut merged: Vec<Chunk> = Vec::with_capacity(chunks.len());
    let mut run_start: Option<usize> = None; // index of first small chunk in current run

    for (i, chunk) in chunks.iter().enumerate() {
        let span = chunk.end_line.saturating_sub(chunk.start_line) + 1;
        // Only merge chunks that are small AND have no symbol hint.
        // Function/class/method definitions should stay as separate chunks
        // even when short — they're semantically distinct units.
        let is_small = span <= SMALL_THRESHOLD && chunk.symbol_hint.is_none();

        if is_small {
            // Check gap from previous chunk in the run — if too large, flush first.
            // This prevents merging scattered declarations with unrelated code
            // between them (e.g., var at line 3 and var at line 79 with describe
            // blocks in between).
            if let Some(rs) = run_start {
                let prev_end = chunks[i - 1].end_line;
                let gap = chunk.start_line.saturating_sub(prev_end);
                if gap > SMALL_THRESHOLD {
                    flush_small_run(&chunks[rs..i], lines, chunk_language, &mut merged);
                    run_start = None;
                }
            }
            if run_start.is_none() {
                run_start = Some(i);
            }
            // Check if adding this chunk would exceed lines_per_chunk
            let rs = run_start.unwrap();
            let run_span = chunk.end_line.saturating_sub(chunks[rs].start_line) + 1;
            if run_span > lines_per_chunk {
                // Flush the run so far (excluding current chunk)
                flush_small_run(&chunks[rs..i], lines, chunk_language, &mut merged);
                run_start = Some(i);
            }
        } else {
            // Non-small chunk: flush any pending run, then emit this chunk as-is
            if let Some(rs) = run_start.take() {
                flush_small_run(&chunks[rs..i], lines, chunk_language, &mut merged);
            }
            merged.push(chunk.clone());
        }
    }

    // Flush trailing run
    if let Some(rs) = run_start {
        flush_small_run(&chunks[rs..], lines, chunk_language, &mut merged);
    }

    merged
}

/// Flush a run of small chunks. If 1 chunk, emit as-is. If >=2, merge into one
/// combined chunk spanning the full range (including any gap lines between them).
fn flush_small_run(run: &[Chunk], lines: &[&str], chunk_language: &str, out: &mut Vec<Chunk>) {
    if run.is_empty() {
        return;
    }
    if run.len() == 1 {
        out.push(run[0].clone());
        return;
    }
    let start_line = run[0].start_line;
    let end_line = run.last().unwrap().end_line;
    let start_idx = start_line.saturating_sub(1);
    let end_idx = end_line.min(lines.len());
    let text = lines[start_idx..end_idx].join("\n");
    let symbol_hint = dominant_symbol_hint(&lines[start_idx..end_idx]);
    out.push(Chunk {
        start_line,
        end_line,
        language: chunk_language.to_string(),
        symbol_hint,
        text,
    });
}

fn dominant_symbol_hint(lines: &[&str]) -> Option<String> {
    let mut best_symbol: Option<String> = None;
    let mut best_span: usize = 0;
    let mut current_symbol: Option<String> = None;
    let mut current_start: usize = 0;
    for (i, line) in lines.iter().enumerate() {
        if looks_like_symbol(line) {
            if let Some(sym) = current_symbol.take() {
                let span = i - current_start;
                if span > best_span {
                    best_span = span;
                    best_symbol = Some(sym);
                }
            }
            current_symbol = symbol_from_line(line);
            current_start = i;
        }
    }
    if let Some(sym) = current_symbol {
        let span = lines.len() - current_start;
        if span > best_span {
            best_symbol = Some(sym);
        }
    }
    best_symbol
}

fn line_chunks_from_range(
    lines: &[&str],
    start_idx: usize,
    end_idx_exclusive: usize,
    chunk_language: &str,
    lines_per_chunk: usize,
    overlap: usize,
) -> Vec<Chunk> {
    if start_idx >= end_idx_exclusive || start_idx >= lines.len() {
        return Vec::new();
    }
    let end_limit = end_idx_exclusive.min(lines.len());
    let stride = lines_per_chunk.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = start_idx;
    while start < end_limit {
        let end = (start + lines_per_chunk).min(end_limit);
        // Pick the symbol that spans the most lines in this window (dominant function).
        // This avoids a short function at the start of a window stealing the hint from
        // a longer function that makes up most of the chunk's content.
        let symbol_hint = dominant_symbol_hint(&lines[start..end]);
        chunks.push(Chunk {
            start_line: start + 1,
            end_line: end,
            language: chunk_language.to_string(),
            symbol_hint,
            text: lines[start..end].join("\n"),
        });
        if end == end_limit {
            break;
        }
        start += stride;
    }
    chunks
}

fn line_window_chunks(
    file_path: &str,
    content: &str,
    lines_per_chunk: usize,
    overlap: usize,
) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let chunk_language = language_label_for_path(file_path);
    line_chunks_from_range(
        &lines,
        0,
        lines.len(),
        &chunk_language,
        lines_per_chunk,
        overlap,
    )
}

pub fn chunk_text(
    file_path: &str,
    content: &str,
    lines_per_chunk: usize,
    overlap: usize,
) -> Vec<Chunk> {
    if let Some(chunks) = ast_top_level_chunks(file_path, content, lines_per_chunk, overlap)
        && !chunks.is_empty()
    {
        return chunks;
    }
    line_window_chunks(file_path, content, lines_per_chunk, overlap)
}

#[cfg(test)]
mod tests {
    use super::{chunk_text, ecosystem_tags_for_chunk};

    #[test]
    fn falls_back_to_line_windows_for_unknown_extensions() {
        let content = (0..180)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_text("README.unknown", &content, 40, 10);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.language == "unknown"));
    }

    #[test]
    fn python_chunks_record_language_label() {
        let content = "def build_app():\n    return app\n";
        let chunks = chunk_text("src/app.py", content, 40, 10);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.language == "python"));
    }

    #[test]
    fn ecosystem_tags_identify_nextjs_react_chunk() {
        let tags = ecosystem_tags_for_chunk(
            "app/dashboard/page.tsx",
            "typescript",
            "\"use client\";\nimport { useState } from \"react\";\nexport default function Page() { const [count] = useState(0); return <div>{count}</div>; }",
        );
        assert!(tags.iter().any(|tag| tag == "nextjs"), "got: {tags:?}");
        assert!(tags.iter().any(|tag| tag == "react"), "got: {tags:?}");
    }

    #[test]
    fn ecosystem_tags_identify_root_level_nextjs_app_router_chunk() {
        let tags = ecosystem_tags_for_chunk(
            "app/layout.tsx",
            "typescript",
            "export default function RootLayout({ children }) { return <html>{children}</html>; }",
        );
        assert!(tags.iter().any(|tag| tag == "nextjs"), "got: {tags:?}");
        assert!(tags.iter().any(|tag| tag == "react"), "got: {tags:?}");
    }

    #[test]
    fn ecosystem_tags_identify_flask_chunk() {
        let tags = ecosystem_tags_for_chunk(
            "src/app.py",
            "python",
            "from flask import Flask, Blueprint\napp = Flask(__name__)\n@app.route(\"/\")\ndef index():\n    return \"ok\"\n",
        );
        assert!(tags.iter().any(|tag| tag == "flask"), "got: {tags:?}");
    }

    #[test]
    fn ecosystem_tags_identify_fastapi_chunk() {
        let tags = ecosystem_tags_for_chunk(
            "src/api.py",
            "python",
            "from fastapi import FastAPI, APIRouter\napp = FastAPI()\nrouter = APIRouter()\n",
        );
        assert!(tags.iter().any(|tag| tag == "fastapi"), "got: {tags:?}");
    }

    #[test]
    fn ecosystem_tags_identify_react_from_framework_repo_path() {
        let tags = ecosystem_tags_for_chunk(
            "packages/react-reconciler/src/ReactFiberHooks.js",
            "javascript",
            "export function renderWithHooks() {}",
        );
        assert!(tags.iter().any(|tag| tag == "react"), "got: {tags:?}");
    }

    #[test]
    fn exported_js_function_gets_correct_symbol_hint() {
        let content = r#"
export function scheduleUpdateOnFiber(root, fiber, lane) {
  if (root === null) return;
  doWork(root);
}

export function performSyncWorkOnRoot(root) {
  return root;
}
"#;
        let chunks = chunk_text("ReactFiberWorkLoop.js", content, 80, 20);
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"scheduleUpdateOnFiber"),
            "expected scheduleUpdateOnFiber in hints, got: {hints:?}"
        );
        assert!(
            hints.contains(&"performSyncWorkOnRoot"),
            "expected performSyncWorkOnRoot in hints, got: {hints:?}"
        );
    }

    #[test]
    fn async_exported_function_gets_symbol_hint() {
        let content = r#"
export async function flushPassiveEffects() {
  return flushPassiveEffectsImpl();
}
"#;
        let chunks = chunk_text("ReactFiberWorkLoop.js", content, 80, 20);
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"flushPassiveEffects"),
            "expected flushPassiveEffects in hints, got: {hints:?}"
        );
    }

    #[test]
    fn ast_chunking_splits_python_definitions() {
        let content = r#"
def alpha():
    return 1

def beta():
    return 2
"#;
        let chunks = chunk_text("example.py", content, 80, 20);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().any(|chunk| chunk.text.contains("def alpha")));
        assert!(chunks.iter().any(|chunk| chunk.text.contains("def beta")));
    }

    #[test]
    fn ast_chunking_splits_java_methods() {
        let content = r#"
class Demo {
    int alpha() { return 1; }
    int beta() { return 2; }
}
"#;
        let chunks = chunk_text("Demo.java", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|chunk| chunk.text.contains("alpha")));
        assert!(chunks.iter().any(|chunk| chunk.text.contains("beta")));
    }

    #[test]
    fn ast_chunking_splits_csharp_methods() {
        let content = r#"
public class Demo {
    public int Alpha() { return 1; }
    public int Beta() { return 2; }
}
"#;
        let chunks = chunk_text("Demo.cs", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|chunk| chunk.text.contains("Alpha")));
        assert!(chunks.iter().any(|chunk| chunk.text.contains("Beta")));
    }

    #[test]
    fn ast_chunking_splits_rust_functions() {
        let content = r#"
pub fn alpha() -> i32 {
    1
}

pub fn beta() -> i32 {
    2
}
"#;
        let chunks = chunk_text("lib.rs", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|c| c.text.contains("fn alpha")));
        assert!(chunks.iter().any(|c| c.text.contains("fn beta")));
        // Symbol hints should be extracted for Rust functions
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"alpha") || hints.contains(&"beta"),
            "expected Rust symbol hints, got: {hints:?}"
        );
    }

    #[test]
    fn ast_chunking_splits_typescript_class() {
        let content = r#"
export class Scheduler {
    schedule(work: () => void): void {
        work();
    }

    cancel(id: number): void {
        return;
    }
}
"#;
        let chunks = chunk_text("Scheduler.ts", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().any(|c| c.text.contains("Scheduler")),
            "expected Scheduler class"
        );
    }

    #[test]
    fn ast_chunking_splits_go_functions() {
        let content = r#"
package main

func Alpha() int {
    return 1
}

func Beta() int {
    return 2
}
"#;
        let chunks = chunk_text("main.go", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|c| c.text.contains("Alpha")));
        assert!(chunks.iter().any(|c| c.text.contains("Beta")));
    }

    #[test]
    fn go_receiver_method_gets_symbol_hint() {
        let content = r#"
package terraform

// Plan computes the next plan state.
func (c *Context) Plan(opts *PlanOpts) (*plans.Plan, error) {
    return nil, nil
}
"#;
        let chunks = chunk_text("context_plan.go", content, 80, 20);
        assert!(
            chunks
                .iter()
                .filter_map(|c| c.symbol_hint.as_deref())
                .any(|hint| hint == "Plan"),
            "expected Plan symbol hint, got: {:?}",
            chunks
                .iter()
                .map(|c| c.symbol_hint.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn decorated_python_function_prefers_def_name_for_symbol_hint() {
        let content = r#"
@setupmethod
def register_blueprint(blueprint, **options):
    return blueprint
"#;
        let chunks = chunk_text("app.py", content, 80, 20);
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"register_blueprint"),
            "expected register_blueprint in hints, got: {hints:?}"
        );
        assert!(
            !hints.contains(&"setupmethod"),
            "decorator should not become the symbol hint: {hints:?}"
        );
    }

    #[test]
    fn large_go_method_prefers_line_chunks_over_var_declarations() {
        let mut content = r#"
package terraform

func (c *Context) Plan() {}

// PlanAndEval contains the real planning steps.
func (c *Context) PlanAndEval() {
    var diags Diagnostics
"#
        .to_string();
        for idx in 0..180 {
            content.push_str(&format!("    step_{idx}()\n"));
        }
        content.push_str("}\n");

        let chunks = chunk_text("context_plan.go", &content, 80, 20);
        assert!(
            chunks
                .iter()
                .filter_map(|c| c.symbol_hint.as_deref())
                .any(|hint| hint == "PlanAndEval"),
            "expected PlanAndEval chunk, got: {:?}",
            chunks
                .iter()
                .map(|c| (c.start_line, c.end_line, c.symbol_hint.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            !chunks
                .iter()
                .any(|c| c.text.trim() == "var diags Diagnostics"),
            "expected large Go method to avoid var-declaration micro-chunks, got: {:?}",
            chunks
                .iter()
                .map(|c| c.text.trim().to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_file_returns_no_chunks() {
        let chunks = chunk_text("empty.rs", "", 80, 20);
        assert!(chunks.is_empty(), "empty file should produce no chunks");
    }

    #[test]
    fn single_line_file_produces_one_chunk() {
        let chunks = chunk_text("tiny.rs", "fn main() {}", 80, 20);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("fn main"));
    }

    #[test]
    fn dominant_symbol_hint_picks_longest_spanning_symbol() {
        use super::dominant_symbol_hint;
        // Build a file with two functions: a short one and a long one.
        // The long one should win as the dominant hint.
        let lines_short = ["fn short_fn() {".to_string(), "}".to_string()];
        let mut lines_long = vec!["fn long_fn() {".to_string()];
        for i in 0..50 {
            lines_long.push(format!("    let _ = {i};"));
        }
        lines_long.push("}".to_string());
        let all_lines: Vec<&str> = lines_short
            .iter()
            .chain(lines_long.iter())
            .map(|s| s.as_str())
            .collect();
        let hint = dominant_symbol_hint(&all_lines);
        assert_eq!(
            hint.as_deref(),
            Some("long_fn"),
            "expected long_fn to dominate, got: {hint:?}"
        );
    }

    #[test]
    fn dominant_symbol_hint_returns_none_for_empty_input() {
        use super::dominant_symbol_hint;
        let hint = dominant_symbol_hint(&[]);
        assert!(hint.is_none());
    }

    #[test]
    fn dominant_symbol_hint_returns_none_for_non_symbol_lines() {
        use super::dominant_symbol_hint;
        let lines = ["let x = 1;", "let y = 2;", "x + y"];
        let hint = dominant_symbol_hint(&lines);
        assert!(hint.is_none(), "got: {hint:?}");
    }

    #[test]
    fn line_window_chunking_produces_overlapping_chunks() {
        // File with exactly 90 lines and chunk_size=80, overlap=20.
        // stride = 80 - 20 = 60. So we get chunks at 1 and 61.
        let content = (0..90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_text("mystery.unknown", &content, 80, 20);
        assert!(chunks.len() >= 2, "expected ≥2 chunks for 90-line file");
        // The second chunk should start before the end of the first
        if chunks.len() >= 2 {
            assert!(
                chunks[1].start_line < chunks[0].end_line,
                "chunks should overlap"
            );
        }
    }

    #[test]
    fn chunks_have_valid_line_ranges() {
        let content = r#"
export function alpha(x: number): number {
    return x + 1;
}

export function beta(x: number): number {
    return x * 2;
}
"#;
        let chunks = chunk_text("math.ts", content, 80, 20);
        for chunk in &chunks {
            assert!(chunk.start_line >= 1, "start_line should be ≥1");
            assert!(
                chunk.end_line >= chunk.start_line,
                "end_line should be ≥ start_line"
            );
        }
    }

    #[test]
    fn ast_chunking_recurses_into_large_js_function() {
        // Simulate createChildReconciler-style: one large outer function (>160 lines)
        // containing several named inner function declarations.
        // Should produce separate chunks for innerAlpha and innerBeta.
        let mut content = "function outerLarge() {\n".to_string();
        content.push_str("function innerAlpha(a) {\n");
        for i in 0..10 {
            content.push_str(&format!("  var a{i} = {i};\n"));
        }
        content.push_str("  return a;\n}\n");
        content.push_str("function innerBeta(b) {\n");
        for i in 0..10 {
            content.push_str(&format!("  var b{i} = {i};\n"));
        }
        content.push_str("  return b;\n}\n");
        // Pad to exceed lines_per_chunk * 2 = 160 lines
        for i in 0..140 {
            content.push_str(&format!("  var x{i} = {i};\n"));
        }
        content.push_str("}\n");
        let chunks = chunk_text("example.js", &content, 80, 20);
        assert!(
            chunks.iter().any(|c| c.text.contains("innerAlpha")),
            "expected chunk with innerAlpha, got: {chunks:?}"
        );
        assert!(
            chunks.iter().any(|c| c.text.contains("innerBeta")),
            "expected chunk with innerBeta, got: {chunks:?}"
        );
    }

    #[test]
    fn rust_struct_gets_symbol_hint() {
        let content = r#"
pub struct WorkLoop {
    queue: Vec<Work>,
    priority: u8,
}
"#;
        let chunks = chunk_text("work_loop.rs", content, 80, 20);
        assert!(!chunks.is_empty());
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"WorkLoop"),
            "expected WorkLoop hint, got: {hints:?}"
        );
    }

    #[test]
    fn go_type_struct_gets_symbol_hint() {
        let content = r#"
package plans

// Plan represents a saved plan.
type Plan struct {
    UIMode Mode
    VariableValues map[string]DynamicValue
    Changes *Changes
}
"#;
        let chunks = chunk_text("plan.go", content, 80, 20);
        assert!(!chunks.is_empty());
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"Plan"),
            "expected Plan hint, got: {hints:?}"
        );
    }

    #[test]
    fn ast_chunking_splits_ruby_methods() {
        let content = r#"
class UserController
  def index
    @users = User.all
    render json: @users
  end

  def show
    @user = User.find(params[:id])
    render json: @user
  end
end
"#;
        let chunks = chunk_text("user_controller.rb", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.language == "ruby"),
            "expected ruby language label"
        );
        assert!(
            chunks.iter().any(|c| c.text.contains("def index")),
            "expected index method chunk"
        );
    }

    #[test]
    fn ast_chunking_splits_kotlin_functions() {
        let content = r#"
class UserService {
    fun findUser(id: Long): User {
        return userRepository.findById(id)
    }

    fun createUser(name: String): User {
        return userRepository.save(User(name = name))
    }
}
"#;
        let chunks = chunk_text("UserService.kt", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.language == "kotlin"),
            "expected kotlin language label"
        );
        assert!(
            chunks.iter().any(|c| c.text.contains("findUser")),
            "expected findUser in chunks"
        );
    }

    #[test]
    fn ast_chunking_splits_swift_functions() {
        let content = r#"
class ViewModel {
    func fetchData() {
        let url = URL(string: "https://api.example.com")!
        URLSession.shared.dataTask(with: url) { data, _, _ in
            guard let data = data else { return }
            print(data)
        }.resume()
    }

    func clearCache() {
        cache.removeAll()
    }
}
"#;
        let chunks = chunk_text("ViewModel.swift", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.language == "swift"),
            "expected swift language label"
        );
        assert!(
            chunks.iter().any(|c| c.text.contains("fetchData")),
            "expected fetchData in chunks"
        );
    }

    #[test]
    fn ast_chunking_splits_scala_definitions() {
        let content = r#"
object UserRoutes {
  def routes: Route = pathPrefix("users") {
    get {
      complete("ok")
    }
  }
}

trait UserRepository {
  def findById(id: Long): Option[User]
  def save(user: User): User
}
"#;
        let chunks = chunk_text("UserRoutes.scala", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.language == "scala"),
            "expected scala language label"
        );
        assert!(
            chunks
                .iter()
                .any(|c| c.text.contains("UserRoutes") || c.text.contains("UserRepository")),
            "expected Scala definitions in chunks"
        );
    }

    #[test]
    fn kotlin_function_gets_symbol_hint() {
        let content = r#"
fun calculateTotal(items: List<Item>): Double {
    return items.sumOf { it.price }
}
"#;
        let chunks = chunk_text("utils.kt", content, 80, 20);
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"calculateTotal"),
            "expected calculateTotal hint, got: {hints:?}"
        );
    }

    #[test]
    fn ruby_method_gets_symbol_hint() {
        let content = r#"
def process_payment(amount)
  charge = Stripe::Charge.create(amount: amount)
  charge.status
end
"#;
        let chunks = chunk_text("payments.rb", content, 80, 20);
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"process_payment"),
            "expected process_payment hint, got: {hints:?}"
        );
    }

    #[test]
    fn ast_chunking_splits_php_functions() {
        let content = r#"<?php

function getUsers(): array {
    return User::all();
}

class UserController {
    public function index(): Response {
        return response()->json(getUsers());
    }

    public function show(int $id): Response {
        return response()->json(User::find($id));
    }
}
"#;
        let chunks = chunk_text("UserController.php", content, 80, 20);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.language == "php"),
            "expected php language label"
        );
        assert!(
            chunks.iter().any(|c| c.text.contains("getUsers")),
            "expected getUsers in chunks"
        );
    }

    #[test]
    fn sibling_merging_combines_adjacent_small_declarations() {
        // Five single-line require statements should be merged into one chunk.
        let content = r#"var express = require('express');
var path = require('path');
var logger = require('morgan');
var cookieParser = require('cookie-parser');
var session = require('express-session');
"#;
        let chunks = chunk_text("app.js", content, 80, 20);
        assert_eq!(
            chunks.len(),
            1,
            "5 single-line vars should merge into 1 chunk, got: {:?}",
            chunks
                .iter()
                .map(|c| (c.start_line, c.end_line, &c.text))
                .collect::<Vec<_>>()
        );
        assert!(chunks[0].text.contains("express"));
        assert!(chunks[0].text.contains("session"));
    }

    #[test]
    fn sibling_merging_splits_on_large_gaps() {
        // Vars at top, then a large gap (10+ lines), then more vars.
        // Should produce two separate merged chunks, not one spanning the gap.
        let mut content = String::new();
        content.push_str("var a = require('a');\n");
        content.push_str("var b = require('b');\n");
        for _ in 0..10 {
            content.push_str("// filler comment line\n");
        }
        content.push_str("var c = require('c');\n");
        content.push_str("var d = require('d');\n");
        let chunks = chunk_text("gap.js", &content, 80, 20);
        // Should NOT be a single chunk spanning the entire file
        assert!(
            chunks.len() >= 2,
            "expected >=2 chunks due to gap, got {} chunks: {:?}",
            chunks.len(),
            chunks
                .iter()
                .map(|c| (c.start_line, c.end_line))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn sibling_merging_preserves_function_chunks() {
        // Short functions should NOT be merged even though they're small.
        let content = r#"var x = require('x');
var y = require('y');

function alpha() {
  return 1;
}

function beta() {
  return 2;
}
"#;
        let chunks = chunk_text("funcs.js", content, 80, 20);
        // Should have: 1 merged var chunk + 2 function chunks = 3
        let hints: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol_hint.as_deref())
            .collect();
        assert!(
            hints.contains(&"alpha"),
            "alpha should be its own chunk, hints: {hints:?}"
        );
        assert!(
            hints.contains(&"beta"),
            "beta should be its own chunk, hints: {hints:?}"
        );
    }
}
