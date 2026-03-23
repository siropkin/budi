use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::index::RuntimeIndex;
use crate::retrieval::{is_devtools_path, is_test_path};

const MAX_ENTRY_POINTS: usize = 10;
const MAX_HOTSPOT_FILES: usize = 10;
const MAX_SYMBOLS: usize = 20;

/// True if the path is in a fixture, example, script, or type-definition directory.
fn is_non_source_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("fixtures/")
        || lower.starts_with("examples/")
        || lower.starts_with("scripts/")
        || lower.starts_with("flow-typed/")
        || lower.starts_with("__mocks__/")
        || lower.contains("/__mocks__/")
}

/// True if the symbol name is too generic to be informative in a project map.
fn is_generic_symbol(sym: &str) -> bool {
    // Short names are almost always too generic (e.g., get, set, run, new, add).
    // But keep 4-char names if they start with uppercase (likely meaningful types: Addr, Apps).
    if sym.len() <= 3 {
        return true;
    }
    if sym.len() == 4 && !sym.starts_with(char::is_uppercase) {
        return true;
    }
    matches!(
        sym.to_ascii_lowercase().as_str(),
        "error"
            | "push"
            | "render"
            | "props"
            | "state"
            | "event"
            | "node"
            | "container"
            | "store"
            | "module"
            | "expect"
            | "jest"
            | "describe"
            | "test"
            | "main"
            | "init"
            | "setup"
            | "config"
            | "utils"
            | "helpers"
            | "default"
            | "index"
            | "page"
            | "root"
            | "data"
            | "item"
            | "list"
            | "view"
            | "model"
            | "type"
            | "value"
            | "result"
            | "name"
            | "field"
            | "append"
            | "__init__"
            | "self"
            | "args"
            | "kwargs"
            | "func"
            | "callback"
            | "handler"
            | "context"
            | "request"
            | "response"
            | "path"
            | "join"
            | "items"
            | "raise"
            | "save"
            | "clean"
            | "delete"
            | "update"
            | "create"
            | "close"
            | "open"
            | "write"
            | "read"
            | "send"
            | "start"
            | "stop"
            | "reset"
            | "copy"
            | "check"
            | "parse"
            | "apply"
            | "match"
            | "label"
            | "text"
            | "body"
            | "form"
            | "table"
            | "query"
            | "build"
            | "keys"
            | "values"
            | "string"
            | "filter"
            | "fields"
            | "tuple"
            | "using"
            | "clone"
            | "format"
            | "print"
            | "length"
            | "count"
            | "size"
            | "equals"
            | "equal"
            | "valid"
            | "validate"
            | "process"
            | "execute"
            | "resolve"
            | "encode"
            | "decode"
            | "token"
            | "option"
            | "options"
            | "params"
            | "input"
            | "output"
            | "source"
            | "target"
            | "parent"
            | "child"
            | "children"
            | "extend"
            | "lower"
            | "upper"
            | "strip"
            | "split"
            | "replace"
            | "contains"
            | "remove"
            | "insert"
            | "clear"
            | "empty"
            | "register"
            | "compiler"
    )
}

/// True if the path looks like a build/config/tooling file (not production source).
fn is_build_or_config_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let stem = Path::new(&lower)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    // Config/build files at any depth
    matches!(
        stem,
        ".eslintrc"
            | "eslintrc"
            | ".prettierrc"
            | "prettierrc"
            | "babel.config"
            | "jest.config"
            | "tsconfig"
            | "webpack.config"
            | "rollup.config"
            | "vite.config"
            | "dangerfile"
    ) || lower.ends_with(".config.js")
        || lower.ends_with(".config.ts")
        || lower.ends_with(".config.mjs")
}

pub fn generate_project_map(runtime: &RuntimeIndex) -> String {
    let chunks = runtime.all_chunks();

    // Count chunks per file (proxy for code density / centrality)
    let mut chunks_per_file: HashMap<&str, usize> = HashMap::new();
    for chunk in chunks {
        *chunks_per_file.entry(chunk.path.as_str()).or_insert(0) += 1;
    }

    // Entry points by basename heuristic — exclude test/devtools/fixture paths.
    // Sort by path depth (shallowest first) to prioritize top-level entry points.
    let mut entry_points: Vec<&str> = chunks_per_file
        .keys()
        .copied()
        .filter(|path| {
            is_entry_point(path)
                && !is_test_path(path)
                && !is_devtools_path(path)
                && !is_non_source_path(path)
        })
        .collect();
    entry_points.sort_by(|a, b| {
        let depth_a = a.matches('/').count();
        let depth_b = b.matches('/').count();
        depth_a.cmp(&depth_b).then(a.cmp(b))
    });
    entry_points.truncate(MAX_ENTRY_POINTS);

    // Top files by chunk count — exclude test/devtools/config/build/fixture files
    let mut hotspots: Vec<(&str, usize)> = chunks_per_file
        .iter()
        .filter(|(p, _)| {
            !is_test_path(p)
                && !is_devtools_path(p)
                && !is_build_or_config_path(p)
                && !is_non_source_path(p)
        })
        .map(|(p, c)| (*p, *c))
        .collect();
    hotspots.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    hotspots.truncate(MAX_HOTSPOT_FILES);

    // Top symbols ranked by call graph centrality (how many chunks reference them).
    // This surfaces the most-connected functions rather than just the first N encountered.
    let mut symbol_map: HashMap<&str, (&str, usize)> = HashMap::new();
    for chunk in chunks {
        if is_test_path(&chunk.path)
            || is_devtools_path(&chunk.path)
            || is_build_or_config_path(&chunk.path)
            || is_non_source_path(&chunk.path)
        {
            continue;
        }
        if let Some(sym) = &chunk.symbol_hint {
            let sym = sym.trim();
            if !sym.is_empty() && !is_generic_symbol(sym) && !symbol_map.contains_key(sym) {
                symbol_map.insert(sym, (chunk.path.as_str(), chunk.start_line));
            }
        }
    }
    let mut symbol_entries: Vec<(&str, &str, usize, usize)> = symbol_map
        .into_iter()
        .map(|(sym, (path, line))| {
            let callers = runtime.caller_count(&sym.to_ascii_lowercase());
            (sym, path, line, callers)
        })
        .collect();
    // Sort by caller count descending, then alphabetically for ties.
    symbol_entries.sort_by(|a, b| b.3.cmp(&a.3).then(a.0.cmp(b.0)));
    // Enforce directory diversity: max 5 symbols per top-level directory.
    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    symbol_entries.retain(|(_, path, _, _)| {
        let dir = top_dir(path);
        let count = dir_counts.entry(dir).or_insert(0);
        *count += 1;
        *count <= 5
    });
    symbol_entries.truncate(MAX_SYMBOLS);

    // Directory overview (count unique files per top-level dir, only real dirs)
    let mut dir_file_counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in chunks_per_file.keys() {
        let dir = top_dir(path);
        // Skip root-level files (no directory component)
        if dir == "." || dir.contains('.') {
            continue;
        }
        *dir_file_counts.entry(dir).or_insert(0) += 1;
    }

    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut out = format!("# Project Map (generated by budi)\nLast updated: {now}\n\n");

    out.push_str("## Entry Points\n");
    for path in &entry_points {
        out.push_str(&format!("- {path}\n"));
    }
    if entry_points.is_empty() {
        out.push_str("- (none detected)\n");
    }

    out.push_str("\n## Top Files by Code Density\n");
    for (i, (path, count)) in hotspots.iter().enumerate() {
        out.push_str(&format!("{}. {} ({} chunks)\n", i + 1, path, count));
    }

    out.push_str("\n## Top Symbols\n");
    for (sym, path, line, callers) in &symbol_entries {
        if *callers > 0 {
            out.push_str(&format!("- {sym} ({path}:{line}, {callers} refs)\n"));
        } else {
            out.push_str(&format!("- {sym} ({path}:{line})\n"));
        }
    }

    out.push_str("\n## Directory Overview\n");
    for (dir, count) in &dir_file_counts {
        out.push_str(&format!("- {dir}/ ({count} files)\n"));
    }

    out
}

pub fn write_project_map(runtime: &RuntimeIndex, repo_root: &Path) -> Result<()> {
    let map = generate_project_map(runtime);
    let claude_dir = repo_root.join(".claude");
    fs::create_dir_all(&claude_dir)?;
    let map_path = claude_dir.join("budi-project-map.md");
    fs::write(map_path, map)?;
    Ok(())
}

pub fn read_project_map(repo_root: &Path) -> Option<String> {
    let map_path = repo_root.join(".claude").join("budi-project-map.md");
    fs::read_to_string(map_path).ok()
}

fn is_entry_point(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let stem = Path::new(&lower)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(stem, "index" | "main" | "lib" | "mod" | "app" | "root")
}

fn top_dir(path: &str) -> String {
    path.split('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(".")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_build_or_config_path_detects_configs() {
        assert!(is_build_or_config_path(".eslintrc.js"));
        assert!(is_build_or_config_path("babel.config.js"));
        assert!(is_build_or_config_path("jest.config.js"));
        assert!(is_build_or_config_path("dangerfile.js"));
        assert!(is_build_or_config_path("webpack.config.js"));
        assert!(is_build_or_config_path("src/vite.config.ts"));
    }

    #[test]
    fn is_build_or_config_path_allows_source() {
        assert!(!is_build_or_config_path("src/app.ts"));
        assert!(!is_build_or_config_path("lib/hooks.js"));
        assert!(!is_build_or_config_path("packages/react/index.js"));
    }
}
