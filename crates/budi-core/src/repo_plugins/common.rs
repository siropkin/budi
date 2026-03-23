use std::collections::HashSet;

use crate::index::{ChunkRecord, RuntimeIndex};

pub(crate) fn contains_any(input: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| input.contains(pattern))
}

pub(crate) fn contains_any_literal(input: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| input.contains(needle))
}

pub(crate) fn path_matches_any(input: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| input.contains(needle) || input.starts_with(needle))
}

pub(crate) fn extract_chunk_line_with_needle(
    chunk: &ChunkRecord,
    needles: &[&str],
) -> Option<(usize, String)> {
    for (idx, raw_line) in chunk.text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if needles.iter().any(|needle| line.contains(needle)) {
            return Some((
                chunk.start_line + idx,
                raw_line.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
        }
    }
    None
}

pub(crate) fn extract_first_meaningful_line(chunk: &ChunkRecord) -> Option<(usize, String)> {
    for (idx, raw_line) in chunk.text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with('#')
            || line == "{"
            || line == "}"
        {
            continue;
        }
        return Some((
            chunk.start_line + idx,
            raw_line.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
    }
    None
}

pub(crate) fn push_compact_evidence_line(
    out: &mut Vec<String>,
    seen: &mut HashSet<(usize, String)>,
    label: &str,
    evidence: Option<(usize, String)>,
) {
    let Some((line_no, text)) = evidence else {
        return;
    };
    if seen.insert((line_no, text.clone())) {
        out.push(format!("{label}@{line_no}: {text}"));
    }
}

pub(crate) fn find_symbol_chunk<'a>(
    runtime: &'a RuntimeIndex,
    path: Option<&str>,
    symbol: &str,
) -> Option<&'a ChunkRecord> {
    runtime
        .all_chunks()
        .iter()
        .filter(|chunk| path.is_none_or(|p| chunk.path == p))
        .find(|chunk| chunk.symbol_hint.as_deref() == Some(symbol))
}

pub(crate) fn find_symbolish_chunk<'a>(
    runtime: &'a RuntimeIndex,
    path: Option<&str>,
    symbol: &str,
) -> Option<&'a ChunkRecord> {
    if let Some(chunk) = find_symbol_chunk(runtime, path, symbol) {
        return Some(chunk);
    }
    let exported = format!("export function {symbol}");
    let plain = format!("function {symbol}");
    let async_exported = format!("export async function {symbol}");
    let async_plain = format!("async function {symbol}");
    let call_form = format!("{symbol}(");
    let mut best: Option<(&ChunkRecord, i32)> = None;
    for chunk in runtime.all_chunks() {
        if path.is_some_and(|expected| chunk.path != expected) {
            continue;
        }
        let mut score = 0i32;
        if chunk.text.contains(&exported) {
            score += 6;
        }
        if chunk.text.contains(&async_exported) {
            score += 6;
        }
        if chunk.text.contains(&plain) {
            score += 5;
        }
        if chunk.text.contains(&async_plain) {
            score += 5;
        }
        if chunk.text.contains(&call_form) {
            score += 2;
        }
        if score == 0 {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, best_score)| score > *best_score)
        {
            best = Some((chunk, score));
        }
    }
    best.map(|(chunk, _)| chunk)
}
