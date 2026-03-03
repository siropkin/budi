use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub start_line: usize,
    pub end_line: usize,
    pub symbol_hint: Option<String>,
    pub text: String,
}

fn looks_like_symbol(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("interface ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("def ")
        || trimmed.starts_with("function ")
}

fn symbol_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut name = String::new();
    for ch in trimmed.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == ':' || ch == '.' {
            name.push(ch);
            continue;
        }
        if !name.is_empty() {
            break;
        }
    }
    if name.is_empty() { None } else { Some(name) }
}

#[derive(Debug, Clone, Copy)]
enum AstLanguageKind {
    JavaScript,
    TypeScript,
    Python,
    Rust,
    Go,
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
    }
}

fn ast_top_level_chunks(
    file_path: &str,
    content: &str,
    lines_per_chunk: usize,
    overlap: usize,
) -> Option<Vec<Chunk>> {
    let (language_kind, language) = ast_language_for_path(file_path)?;
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return None;
    }
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut chunks = Vec::new();
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
        append_node_chunks(&mut chunks, node, content, &lines, lines_per_chunk, overlap);
    }
    if chunks.is_empty() {
        return None;
    }
    chunks.sort_by_key(|chunk| (chunk.start_line, chunk.end_line));
    chunks.dedup_by(|left, right| {
        left.start_line == right.start_line
            && left.end_line == right.end_line
            && left.text == right.text
    });
    Some(chunks)
}

fn append_node_chunks(
    out: &mut Vec<Chunk>,
    node: Node<'_>,
    content: &str,
    lines: &[&str],
    lines_per_chunk: usize,
    overlap: usize,
) {
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    if start_line == 0 || end_line < start_line {
        return;
    }
    let snippet = match content.get(node.start_byte()..node.end_byte()) {
        Some(value) if !value.trim().is_empty() => value.to_string(),
        _ => return,
    };
    let span = end_line.saturating_sub(start_line) + 1;
    if span > lines_per_chunk.saturating_mul(2) {
        out.extend(line_chunks_from_range(
            lines,
            start_line.saturating_sub(1),
            end_line,
            lines_per_chunk,
            overlap,
        ));
        return;
    }
    let symbol_hint = snippet
        .lines()
        .find(|line| looks_like_symbol(line))
        .and_then(symbol_from_line);
    out.push(Chunk {
        start_line,
        end_line,
        symbol_hint,
        text: snippet,
    });
}

fn line_chunks_from_range(
    lines: &[&str],
    start_idx: usize,
    end_idx_exclusive: usize,
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
        let mut symbol_hint = None;
        for line in &lines[start..end] {
            if looks_like_symbol(line) {
                symbol_hint = symbol_from_line(line);
                break;
            }
        }
        chunks.push(Chunk {
            start_line: start + 1,
            end_line: end,
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

fn line_window_chunks(content: &str, lines_per_chunk: usize, overlap: usize) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    line_chunks_from_range(&lines, 0, lines.len(), lines_per_chunk, overlap)
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
    line_window_chunks(content, lines_per_chunk, overlap)
}
