//! File-level attribution for AI tool activity (R1.4, #292).
//!
//! Given tool-call arguments from a supported provider, extract and
//! normalize file paths so we can answer "which files did the AI touch"
//! per ticket / branch / repo / activity — without storing file contents
//! or diffs, and without surfacing absolute paths from the user's disk.
//!
//! ## Contract (ADR-0083)
//!
//! 1. Stored values are always repo-relative, forward-slashed, and
//!    strictly inside the resolved repo root.
//! 2. Absolute paths are stripped against the message `cwd` / repo root
//!    before storage. If the path cannot be proven to sit inside the
//!    repo, it is dropped — we never record "unattached" absolute paths.
//! 3. `..` segments that would escape the repo root are dropped.
//! 4. Maximum of [`MAX_FILES_PER_MESSAGE`] files per message; excess
//!    paths are truncated. This keeps per-message payloads small and
//!    caps the "pathological tool-use output" case.
//! 5. Only paths are recorded. No contents, diffs, mtimes, or sizes.
//!
//! The actual tag emission lives in `pipeline::enrichers::FileEnricher`,
//! which calls [`attribute_files`] to convert raw candidate paths into
//! the canonical form plus a dominant source/confidence label.

use std::path::{Component, Path, PathBuf};

/// Per-message cap on the number of file tags emitted. Picked to keep
/// worst-case tag fan-out in the same order of magnitude as the other
/// multi-valued tags (`tool`, `tool_use_id`) even for Grep/Glob-heavy
/// messages.
pub const MAX_FILES_PER_MESSAGE: usize = 16;

/// Path came from a known tool file argument (e.g. `file_path` on
/// Read/Write/Edit) and was already repo-relative.
pub const FILE_SOURCE_TOOL_ARG: &str = "tool_arg";
/// Path came from a known tool file argument but was absolute; it was
/// successfully stripped against cwd / the resolved repo root to make
/// it repo-relative.
pub const FILE_SOURCE_CWD_RELATIVE: &str = "cwd_relative";

/// Attribution was verbatim from a tool argument (already repo-relative).
pub const FILE_CONFIDENCE_HIGH: &str = "high";
/// Attribution required normalization against cwd/repo; still deterministic
/// but one step removed from the raw argument.
pub const FILE_CONFIDENCE_MEDIUM: &str = "medium";

/// Extract candidate file paths from a Claude Code `tool_use` block and
/// push them into `out`. `tool_name` is the tool's display name (e.g.
/// `Read`, `Write`, `Edit`, `Grep`, `Glob`). `input` is the raw JSON
/// value from the tool_use block's `input` field.
///
/// Unknown tools are ignored — we only extract from tools whose file
/// arguments are well-defined. Bash, WebFetch, Task, TodoWrite, etc. are
/// deliberately not parsed in 8.1 because their arguments are free-form
/// and would require speculative parsing (see #292 non-goals).
pub fn collect_claude_tool_paths(
    tool_name: &str,
    input: &serde_json::Value,
    out: &mut Vec<String>,
) {
    // Claude Code tool input shapes. Normalised to lowercase so any future
    // provider that forwards the same names in a different case stays
    // covered without a brittle exact-match.
    let lower = tool_name.trim().to_ascii_lowercase();
    match lower.as_str() {
        // Single file path.
        "read" | "write" | "edit" | "multiedit" => {
            push_str(input.get("file_path"), out);
        }
        // Jupyter notebook tools (file_path and notebook_path forms).
        "notebookread" | "notebookedit" => {
            push_str(input.get("notebook_path"), out);
            push_str(input.get("file_path"), out);
        }
        // Grep: `path` optionally scopes the search; include it when present
        // so "files touched by a search" is queryable.
        "grep" => {
            push_str(input.get("path"), out);
        }
        // Glob: `path` is the search root, `pattern` is the glob itself.
        // The glob pattern can contain `*`/`?` — we still record it for the
        // "files the AI was looking at" signal; consumers can detect it by
        // its unresolved wildcards.
        "glob" => {
            push_str(input.get("path"), out);
            push_str(input.get("pattern"), out);
        }
        _ => {}
    }
}

/// Extract candidate file paths from a Cursor tool_calls entry. Cursor
/// argument shapes vary by version, so we read the union of known fields.
/// See `providers::cursor` for the CursorToolCall glue.
pub fn collect_cursor_tool_paths(tool_name: &str, args: &serde_json::Value, out: &mut Vec<String>) {
    let lower = tool_name.trim().to_ascii_lowercase();
    // Cursor's first-party tool names in use today: read_file, edit_file,
    // write_file, search_replace, codebase_search, grep_search, glob_file_search,
    // delete_file, file_search. We intentionally stay lenient on shape.
    match lower.as_str() {
        "read_file" | "edit_file" | "write_file" | "search_replace" | "delete_file"
        | "file_search" | "apply_patch" => {
            push_str(args.get("target_file"), out);
            push_str(args.get("file_path"), out);
            push_str(args.get("path"), out);
            push_str(args.get("filePath"), out);
        }
        "grep_search" | "grep" => {
            push_str(args.get("path"), out);
            push_str(args.get("include_pattern"), out);
        }
        "glob_file_search" | "glob" => {
            push_str(args.get("path"), out);
            push_str(args.get("glob_pattern"), out);
            push_str(args.get("pattern"), out);
        }
        "codebase_search" => {
            push_str(args.get("path"), out);
        }
        _ => {
            // Unknown tool: be generous but bounded — a single `file_path`
            // / `target_file` field is a very strong signal across the
            // Cursor extension ecosystem and cheap to opt into.
            push_str(args.get("target_file"), out);
            push_str(args.get("file_path"), out);
        }
    }
}

fn push_str(v: Option<&serde_json::Value>, out: &mut Vec<String>) {
    if let Some(s) = v.and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
}

/// Outcome of normalizing a set of raw candidate paths for a single
/// assistant message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileAttribution {
    /// Canonical repo-relative paths, deduplicated and capped.
    pub paths: Vec<String>,
    /// Dominant source label across the accepted paths. `None` when
    /// `paths` is empty.
    pub source: Option<&'static str>,
    /// Confidence label for the batch. `None` when `paths` is empty.
    pub confidence: Option<&'static str>,
    /// Count of raw candidates dropped because they failed the privacy
    /// / repo-membership contract. Useful for the doctor check and
    /// regression tests.
    pub dropped: usize,
}

/// Normalize a batch of raw candidate paths against `cwd` and the
/// resolved `repo_root`. Returns the canonical repo-relative set plus a
/// dominant (source, confidence) label, or empty when no candidates
/// survive the privacy filter.
///
/// `repo_root` is the filesystem path of the message's repo root (as
/// determined by walking up from `cwd` to the enclosing `.git` dir). It
/// is required for absolute-path normalization; when `None`, absolute
/// paths are dropped.
pub fn attribute_files(
    raw: &[String],
    cwd: Option<&str>,
    repo_root: Option<&Path>,
) -> FileAttribution {
    let cwd_path: Option<PathBuf> = cwd.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    });

    let mut seen = std::collections::HashSet::new();
    let mut accepted: Vec<(String, &'static str, &'static str)> = Vec::new();
    let mut dropped = 0usize;

    for raw_path in raw {
        match normalize_one(raw_path, cwd_path.as_deref(), repo_root) {
            Some((rel, source, confidence)) => {
                if accepted.len() >= MAX_FILES_PER_MESSAGE {
                    // Truncation is also a drop for the purposes of the
                    // doctor check — it tells us the cap is biting.
                    dropped += 1;
                    continue;
                }
                if seen.insert(rel.clone()) {
                    accepted.push((rel, source, confidence));
                }
            }
            None => dropped += 1,
        }
    }

    if accepted.is_empty() {
        return FileAttribution {
            dropped,
            ..FileAttribution::default()
        };
    }

    // Dominant source: if any path needed cwd_relative normalization the
    // message as a whole is "cwd_relative"; otherwise it is "tool_arg".
    // Same story for confidence — any normalization drops the batch from
    // `high` to `medium`.
    let mut source = FILE_SOURCE_TOOL_ARG;
    let mut confidence = FILE_CONFIDENCE_HIGH;
    for (_, s, c) in &accepted {
        if *s == FILE_SOURCE_CWD_RELATIVE {
            source = FILE_SOURCE_CWD_RELATIVE;
        }
        if *c == FILE_CONFIDENCE_MEDIUM {
            confidence = FILE_CONFIDENCE_MEDIUM;
        }
    }

    let mut paths: Vec<String> = accepted.into_iter().map(|(p, _, _)| p).collect();
    paths.sort();

    FileAttribution {
        paths,
        source: Some(source),
        confidence: Some(confidence),
        dropped,
    }
}

/// Normalize one raw path. Returns `(repo_relative_path, source, confidence)`
/// on success, `None` when the candidate fails the privacy contract.
fn normalize_one(
    raw: &str,
    cwd: Option<&Path>,
    repo_root: Option<&Path>,
) -> Option<(String, &'static str, &'static str)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Normalize Windows-style separators so the wire format is stable
    // whether the agent ran on macOS, Linux, or Windows.
    let normalized = trimmed.replace('\\', "/");

    // Strip file:// scheme — some providers emit that form for file
    // arguments. Protocol-qualified URLs for anything else are dropped.
    let without_scheme = if let Some(rest) = normalized.strip_prefix("file://") {
        rest.to_string()
    } else if normalized.contains("://") {
        return None;
    } else {
        normalized
    };

    let as_path = Path::new(&without_scheme);

    if as_path.is_absolute() {
        let base = repo_root.or(cwd)?;
        let rel = as_path.strip_prefix(base).ok()?;
        let cleaned = clean_relative(rel)?;
        if cleaned.is_empty() {
            return None;
        }
        return Some((cleaned, FILE_SOURCE_CWD_RELATIVE, FILE_CONFIDENCE_MEDIUM));
    }

    // Relative path. If we have a cwd + repo root, join cwd with the
    // raw relative path and resolve via `clean_absolute` so that `..`
    // segments are evaluated against the full cwd + input chain (e.g.
    // `../sibling.rs` from `<repo>/src` is really `<repo>/sibling.rs`,
    // which is still inside the repo). Without a cwd we fall back to
    // cleaning the relative path in isolation and reject any `..` that
    // cannot be resolved within the path itself.
    if let (Some(cwd_path), Some(root)) = (cwd, repo_root) {
        let joined = cwd_path.join(Path::new(&without_scheme));
        // Canonicalize by string-walking rather than touching the FS so
        // tests are deterministic and privacy never depends on the
        // current working directory of the process.
        let resolved = clean_absolute(&joined)?;
        if !resolved.starts_with(root) {
            return None;
        }
        let rel = resolved.strip_prefix(root).ok()?;
        let rel_str = path_to_forward_slash(rel);
        if rel_str.is_empty() {
            return None;
        }
        return Some((rel_str, FILE_SOURCE_TOOL_ARG, FILE_CONFIDENCE_HIGH));
    }

    let cleaned = clean_relative(Path::new(&without_scheme))?;
    if cleaned.is_empty() {
        return None;
    }
    Some((cleaned, FILE_SOURCE_TOOL_ARG, FILE_CONFIDENCE_HIGH))
}

/// Clean a relative path: drop leading `./`, collapse inner `.`, and
/// reject if any `..` cannot be resolved inside the path (meaning it
/// would escape the base). Returns a forward-slashed string.
fn clean_relative(rel: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(os) => {
                let s = os.to_str()?;
                parts.push(s.to_string());
            }
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

/// Resolve an absolute path string-wise (no FS access) so it can be
/// compared against `repo_root`. Returns `None` if the path escapes
/// its root via unresolved `..`.
fn clean_absolute(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::Normal(os) => out.push(os),
            Component::ParentDir => {
                // Refuse to pop past the root so the repo-membership check
                // in the caller cannot be bypassed.
                if !out.pop() {
                    return None;
                }
            }
        }
    }
    Some(out)
}

fn path_to_forward_slash(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(os) => os.to_str().map(|s| s.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn raw(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn relative_path_without_cwd_is_accepted_as_high_confidence() {
        let a = attribute_files(&raw(&["src/main.rs", "Cargo.toml"]), None, None);
        assert_eq!(a.paths, vec!["Cargo.toml", "src/main.rs"]);
        assert_eq!(a.source, Some(FILE_SOURCE_TOOL_ARG));
        assert_eq!(a.confidence, Some(FILE_CONFIDENCE_HIGH));
        assert_eq!(a.dropped, 0);
    }

    // Absolute-path normalization is inherently platform-specific: a leading
    // `/` is absolute on Unix but not on Windows (Windows requires a drive or
    // UNC prefix). We cover both shapes so Windows CI exercises the same
    // `cwd_relative` / `medium` contract without relying on accidental
    // path-syntax coincidences.

    #[cfg(unix)]
    #[test]
    fn absolute_path_normalized_against_cwd_is_medium_confidence_unix() {
        let root = Path::new("/home/dev/repo");
        let a = attribute_files(
            &raw(&["/home/dev/repo/src/main.rs"]),
            Some("/home/dev/repo"),
            Some(root),
        );
        assert_eq!(a.paths, vec!["src/main.rs"]);
        assert_eq!(a.source, Some(FILE_SOURCE_CWD_RELATIVE));
        assert_eq!(a.confidence, Some(FILE_CONFIDENCE_MEDIUM));
    }

    #[cfg(windows)]
    #[test]
    fn absolute_path_normalized_against_cwd_is_medium_confidence_windows() {
        let root = Path::new(r"C:\dev\repo");
        let a = attribute_files(
            &raw(&[r"C:\dev\repo\src\main.rs"]),
            Some(r"C:\dev\repo"),
            Some(root),
        );
        assert_eq!(a.paths, vec!["src/main.rs"]);
        assert_eq!(a.source, Some(FILE_SOURCE_CWD_RELATIVE));
        assert_eq!(a.confidence, Some(FILE_CONFIDENCE_MEDIUM));
    }

    #[test]
    fn absolute_path_outside_repo_is_dropped() {
        let root = Path::new("/home/dev/repo");
        let a = attribute_files(
            &raw(&["/etc/passwd", "/home/dev/other/file.rs"]),
            Some("/home/dev/repo"),
            Some(root),
        );
        assert!(
            a.paths.is_empty(),
            "must never retain outside-of-repo paths"
        );
        assert_eq!(a.source, None);
        assert_eq!(a.dropped, 2);
    }

    #[test]
    fn parent_escape_is_dropped() {
        let root = Path::new("/home/dev/repo");
        let a = attribute_files(
            &raw(&["../other.rs"]),
            Some("/home/dev/repo/src"),
            Some(root),
        );
        assert_eq!(a.paths, vec!["other.rs".to_string()]);

        let a2 = attribute_files(
            &raw(&["../../../etc/passwd"]),
            Some("/home/dev/repo/src"),
            Some(root),
        );
        assert!(
            a2.paths.is_empty(),
            "parent traversal that escapes repo must drop"
        );
    }

    #[test]
    fn absolute_path_without_repo_root_is_dropped() {
        let a = attribute_files(&raw(&["/Users/dev/secret.rs"]), None, None);
        assert!(a.paths.is_empty());
        assert_eq!(a.dropped, 1);
    }

    #[test]
    fn caps_at_max_files_per_message() {
        let mut list = Vec::new();
        for i in 0..(MAX_FILES_PER_MESSAGE + 4) {
            list.push(format!("src/f{i}.rs"));
        }
        let a = attribute_files(&list, None, None);
        assert_eq!(a.paths.len(), MAX_FILES_PER_MESSAGE);
        assert_eq!(a.dropped, 4);
    }

    #[test]
    fn deduplicates_paths() {
        let a = attribute_files(&raw(&["src/a.rs", "src/a.rs", "src/b.rs"]), None, None);
        assert_eq!(a.paths, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn rejects_non_file_schemes() {
        let a = attribute_files(
            &raw(&["https://example.com/evil.rs", "scp://host/file"]),
            None,
            None,
        );
        assert!(a.paths.is_empty());
    }

    #[test]
    fn accepts_file_url_scheme() {
        let root = Path::new("/home/dev/repo");
        let a = attribute_files(
            &raw(&["file:///home/dev/repo/src/x.rs"]),
            Some("/home/dev/repo"),
            Some(root),
        );
        assert_eq!(a.paths, vec!["src/x.rs"]);
    }

    #[test]
    fn normalizes_windows_separators() {
        let a = attribute_files(&raw(&["src\\main.rs"]), None, None);
        assert_eq!(a.paths, vec!["src/main.rs"]);
    }

    #[test]
    fn collect_claude_read_write_edit_extracts_file_path() {
        let mut out = Vec::new();
        let input = serde_json::json!({ "file_path": "crates/budi-core/src/lib.rs" });
        collect_claude_tool_paths("Read", &input, &mut out);
        collect_claude_tool_paths("Edit", &input, &mut out);
        collect_claude_tool_paths("Write", &input, &mut out);
        assert_eq!(
            out,
            vec![
                "crates/budi-core/src/lib.rs".to_string(),
                "crates/budi-core/src/lib.rs".to_string(),
                "crates/budi-core/src/lib.rs".to_string(),
            ]
        );
    }

    #[test]
    fn collect_claude_ignores_unknown_tool() {
        let mut out = Vec::new();
        let input = serde_json::json!({ "command": "rm -rf /" });
        collect_claude_tool_paths("Bash", &input, &mut out);
        assert!(out.is_empty(), "Bash must not surface as a file tool");
    }

    #[test]
    fn collect_cursor_extracts_target_file_and_path() {
        let mut out = Vec::new();
        let args = serde_json::json!({
            "target_file": "src/main.rs",
            "instructions": "edit this",
        });
        collect_cursor_tool_paths("edit_file", &args, &mut out);
        assert_eq!(out, vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn collect_cursor_unknown_tool_is_lenient_for_file_path() {
        let mut out = Vec::new();
        let args = serde_json::json!({ "file_path": "README.md" });
        collect_cursor_tool_paths("some_future_tool", &args, &mut out);
        assert_eq!(out, vec!["README.md".to_string()]);
    }
}
