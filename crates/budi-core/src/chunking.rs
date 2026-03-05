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
];

fn symbol_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Walk word tokens, skip language keywords, return first real identifier
    let mut token = String::new();
    for ch in trimmed.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            if !token.is_empty() {
                let tok = token.drain(..).collect::<String>();
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
    TypeScript,
    Python,
    Rust,
    Go,
    Java,
    Cpp,
    CSharp,
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

#[cfg(test)]
mod tests {
    use super::chunk_text;

    #[test]
    fn falls_back_to_line_windows_for_unknown_extensions() {
        let content = (0..180)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_text("README.unknown", &content, 40, 10);
        assert!(chunks.len() > 1);
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
}
