use serde::{Deserialize, Serialize};

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

pub fn chunk_text(content: &str, lines_per_chunk: usize, overlap: usize) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let stride = lines_per_chunk.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let end = (start + lines_per_chunk).min(lines.len());
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
        if end == lines.len() {
            break;
        }
        start += stride;
    }
    chunks
}
