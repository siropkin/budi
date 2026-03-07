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
    // Rust / Python / bare declarations
    trimmed.starts_with("fn ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("pub async fn ")
        || trimmed.starts_with("async fn ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("interface ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("enum ")
        || trimmed.starts_with("def ")
        || trimmed.starts_with("async def ")
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
    // For AST boundary nodes the first non-blank line IS the declaration,
    // so skip the looks_like_symbol gate and extract directly.
    let symbol_hint = snippet
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(symbol_from_line);
    out.push(Chunk {
        start_line,
        end_line,
        symbol_hint,
        text: snippet,
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
        let hints: Vec<_> = chunks.iter().filter_map(|c| c.symbol_hint.as_deref()).collect();
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
        let hints: Vec<_> = chunks.iter().filter_map(|c| c.symbol_hint.as_deref()).collect();
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
        let hints: Vec<_> = chunks.iter().filter_map(|c| c.symbol_hint.as_deref()).collect();
        assert!(hints.contains(&"alpha") || hints.contains(&"beta"), "expected Rust symbol hints, got: {hints:?}");
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
        assert!(chunks.iter().any(|c| c.text.contains("Scheduler")), "expected Scheduler class");
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
        let lines_short = vec!["fn short_fn() {".to_string(), "}".to_string()];
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
        assert_eq!(hint.as_deref(), Some("long_fn"), "expected long_fn to dominate, got: {hint:?}");
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
            assert!(chunks[1].start_line < chunks[0].end_line, "chunks should overlap");
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
            assert!(chunk.end_line >= chunk.start_line, "end_line should be ≥ start_line");
        }
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
        let hints: Vec<_> = chunks.iter().filter_map(|c| c.symbol_hint.as_deref()).collect();
        assert!(hints.contains(&"WorkLoop"), "expected WorkLoop hint, got: {hints:?}");
    }
}
