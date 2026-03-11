use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const BUDI_HOME_ENV: &str = "BUDI_HOME";
pub const BUDI_HOME_DEFAULT_REL: &str = ".local/share/budi";
pub const BUDI_REPOS_DIR: &str = "repos";
pub const BUDI_CONFIG_FILE_NAME: &str = "config.toml";
pub const BUDI_IGNORE_FILE_NAME: &str = ".budiignore";
pub const BUDI_LOCAL_IGNORE_FILE_NAME: &str = "budiignore.local";
pub const BUDI_GLOBAL_IGNORE_FILE_NAME: &str = "global.budiignore";
pub const BUDI_REPO_ROOT_MARKER_FILE_NAME: &str = "repo-root.txt";
pub const BUDI_INDEX_DIR_NAME: &str = "index";
pub const BUDI_INDEX_DB_FILE_NAME: &str = "index.sqlite";
pub const BUDI_TANTIVY_DIR_NAME: &str = "tantivy";
pub const BUDI_LOG_DIR_NAME: &str = "logs";
pub const BUDI_BENCH_DIR_NAME: &str = "benchmarks";
pub const BUDI_FASTEMBED_CACHE_DIR_NAME: &str = "fastembed-cache";
pub const BUDI_EMBEDDING_CACHE_FILE_NAME: &str = "embedding-cache.sqlite";

pub const CLAUDE_LOCAL_SETTINGS: &str = ".claude/settings.local.json";

pub const DEFAULT_DAEMON_HOST: &str = "127.0.0.1";
pub const DEFAULT_DAEMON_PORT: u16 = 7878;
pub const DEFAULT_RETRIEVAL_LIMIT: usize = 8;
pub const DEFAULT_CONTEXT_CHAR_BUDGET: usize = 12_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudiConfig {
    // ── Daemon ────────────────────────────────────────────────────────────────
    /// Host the daemon listens on. Default: "127.0.0.1".
    pub daemon_host: String,
    /// Port the daemon listens on. Default: 7878.
    pub daemon_port: u16,

    // ── Retrieval ─────────────────────────────────────────────────────────────
    /// Maximum number of code snippets returned per query. Per-intent limits
    /// (5–8) apply automatically unless this is explicitly set. Default: 8.
    pub retrieval_limit: usize,
    /// Maximum total characters of injected context per prompt. Default: 12000.
    pub context_char_budget: usize,
    /// Minimum per-snippet injection score to include any context at all.
    /// Raise to be more conservative; lower to inject more aggressively. Default: 0.05.
    pub min_inject_score: f32,
    /// Number of candidate hits fetched from the lexical (BM25) channel. Default: 20.
    pub topk_lexical: usize,
    /// Number of candidate hits fetched from the vector (HNSW) channel. Default: 20.
    pub topk_vector: usize,
    /// Skip injection when the prompt looks like a non-code question (e.g. "what time is it"). Default: true.
    pub skip_non_code_prompts: bool,
    /// When true, apply smart skip heuristics to suppress low-confidence injections. Default: true.
    pub smart_skip_enabled: bool,

    // ── Indexing ──────────────────────────────────────────────────────────────
    /// File extensions to include in the index (without leading dot). Default: rs, ts, tsx, js, jsx, py, go, …
    pub index_extensions: Vec<String>,
    /// Exact filenames (no extension) to include regardless of extension. Default: Dockerfile, Makefile, …
    pub index_basenames: Vec<String>,
    /// Maximum file size in bytes to index. Files larger than this are skipped. Default: 1500000.
    pub max_file_bytes: usize,
    /// Hard cap on total indexed files per repo. Default: 20000.
    pub max_index_files: usize,
    /// Hard cap on total indexed chunks per repo. Default: 250000.
    pub max_index_chunks: usize,
    /// Target chunk size in lines (sliding window). Default: 80.
    pub chunk_lines: usize,
    /// Overlap in lines between adjacent chunks. Default: 20.
    pub chunk_overlap: usize,

    // ── Embeddings ────────────────────────────────────────────────────────────
    /// Number of chunks to embed in a single batch call. Default: 96.
    pub embedding_batch_size: usize,
    /// How many times to retry a failed embedding batch. Default: 3.
    pub embedding_retry_attempts: usize,
    /// Milliseconds to wait between embedding retries (exponential backoff base). Default: 75.
    pub embedding_retry_backoff_ms: u64,

    // ── Debug / Telemetry ─────────────────────────────────────────────────────
    /// Enable hook I/O telemetry. When true, every hook event (query, prefetch, session-start)
    /// is logged to `~/.local/share/budi/repos/<id>/logs/hook-io.jsonl`. Default: false.
    pub debug_io: bool,
    /// Include full injected context text in telemetry log entries. Requires `debug_io = true`. Default: false.
    pub debug_io_full_text: bool,
    /// Maximum characters of context text to include per telemetry entry. Requires `debug_io_full_text = true`. Default: 1200.
    pub debug_io_max_chars: usize,
}

impl Default for BudiConfig {
    fn default() -> Self {
        Self {
            // Daemon
            daemon_host: DEFAULT_DAEMON_HOST.to_string(),
            daemon_port: DEFAULT_DAEMON_PORT,
            // Retrieval
            retrieval_limit: DEFAULT_RETRIEVAL_LIMIT,
            context_char_budget: DEFAULT_CONTEXT_CHAR_BUDGET,
            min_inject_score: 0.05,
            topk_lexical: 20,
            topk_vector: 20,
            skip_non_code_prompts: true,
            smart_skip_enabled: true,
            // Indexing
            index_extensions: default_index_extensions(),
            index_basenames: default_index_basenames(),
            max_file_bytes: 1_500_000,
            max_index_files: 20_000,
            max_index_chunks: 250_000,
            chunk_lines: 80,
            chunk_overlap: 20,
            // Embeddings
            embedding_batch_size: 32,
            embedding_retry_attempts: 3,
            embedding_retry_backoff_ms: 75,
            // Debug
            debug_io: false,
            debug_io_full_text: false,
            debug_io_max_chars: 1200,
        }
    }
}

fn default_index_extensions() -> Vec<String> {
    vec![
        "rs".to_string(),
        "ts".to_string(),
        "tsx".to_string(),
        "js".to_string(),
        "jsx".to_string(),
        "py".to_string(),
        "go".to_string(),
        "java".to_string(),
        "kt".to_string(),
        "swift".to_string(),
        "cpp".to_string(),
        "cc".to_string(),
        "cxx".to_string(),
        "c".to_string(),
        "h".to_string(),
        "hpp".to_string(),
        "cs".to_string(),
        "rb".to_string(),
        "php".to_string(),
        "scala".to_string(),
        "sql".to_string(),
        "sh".to_string(),
        "graphql".to_string(),
        "proto".to_string(),
    ]
}

fn default_index_basenames() -> Vec<String> {
    vec![
        "Dockerfile".to_string(),
        "Makefile".to_string(),
        "Rakefile".to_string(),
        "Gemfile".to_string(),
        "Procfile".to_string(),
    ]
}

impl BudiConfig {
    pub fn daemon_base_url(&self) -> String {
        format!("http://{}:{}", self.daemon_host, self.daemon_port)
    }
}

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub index_dir: PathBuf,
    pub index_db_file: PathBuf,
    pub tantivy_dir: PathBuf,
    pub log_dir: PathBuf,
    pub bench_dir: PathBuf,
}

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            anyhow::bail!("Unable to locate a git repository from {}", start.display());
        }
    }
}

/// For git worktrees, resolve to the main repo root for shared index storage.
/// Returns the main repo root if in a worktree, otherwise returns the input path.
/// This ensures all worktrees of the same repo share a single index.
pub fn resolve_storage_root(repo_root: &Path) -> PathBuf {
    let git_path = repo_root.join(".git");
    if git_path.is_file()
        && let Some(main_root) = resolve_worktree_main_root(&git_path)
    {
        return main_root;
    }
    repo_root.to_path_buf()
}

/// Given a `.git` file (as found in worktrees), resolve the main repo root.
/// The file contains `gitdir: /path/to/main/.git/worktrees/<name>`.
fn resolve_worktree_main_root(git_file: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(git_file).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    let gitdir_path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        git_file.parent()?.join(gitdir)
    };
    // Expected: /path/to/main/.git/worktrees/<name>
    // Walk up to find the .git directory, then its parent is the main repo root.
    let mut candidate = gitdir_path;
    loop {
        if candidate.file_name().map(|n| n == ".git").unwrap_or(false) && candidate.is_dir() {
            return candidate.parent().map(|p| p.to_path_buf());
        }
        if !candidate.pop() {
            return None;
        }
    }
}

pub fn budi_home_dir() -> Result<PathBuf> {
    if let Ok(override_dir) = env::var(BUDI_HOME_ENV) {
        return Ok(PathBuf::from(override_dir));
    }
    let home_dir = env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home_dir).join(BUDI_HOME_DEFAULT_REL))
}

pub fn repo_paths(repo_root: &Path) -> Result<RepoPaths> {
    let repos_root = repos_root_dir()?;
    let repo_id = repo_storage_id(repo_root);
    let data_dir = repos_root.join(repo_id);
    let index_dir = data_dir.join(BUDI_INDEX_DIR_NAME);
    let tantivy_dir = index_dir.join(BUDI_TANTIVY_DIR_NAME);
    let log_dir = data_dir.join(BUDI_LOG_DIR_NAME);
    let bench_dir = data_dir.join(BUDI_BENCH_DIR_NAME);
    Ok(RepoPaths {
        config_file: data_dir.join(BUDI_CONFIG_FILE_NAME),
        index_db_file: index_dir.join(BUDI_INDEX_DB_FILE_NAME),
        data_dir,
        index_dir,
        tantivy_dir,
        log_dir,
        bench_dir,
    })
}

pub fn repos_root_dir() -> Result<PathBuf> {
    Ok(budi_home_dir()?.join(BUDI_REPOS_DIR))
}

pub fn config_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.config_file)
}

pub fn ignore_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_root.join(BUDI_IGNORE_FILE_NAME))
}

pub fn global_ignore_path() -> Result<PathBuf> {
    Ok(budi_home_dir()?.join(BUDI_GLOBAL_IGNORE_FILE_NAME))
}

/// Per-repo local ignore file stored in budi's data directory (not in the repo).
/// Useful for enterprise repos where you can't commit a `.budiignore` file.
pub fn local_ignore_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?
        .data_dir
        .join(BUDI_LOCAL_IGNORE_FILE_NAME))
}

pub fn layered_ignore_paths(repo_root: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![
        global_ignore_path()?,
        local_ignore_path(repo_root)?,
        ignore_path(repo_root)?,
    ])
}

pub fn index_db_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.index_db_file)
}

pub fn tantivy_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.tantivy_dir)
}

pub fn hook_log_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.log_dir.join("hook-io.jsonl"))
}

pub fn daemon_log_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.log_dir.join("daemon.log"))
}

pub fn benchmark_root(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.bench_dir)
}

pub fn repo_root_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BUDI_REPO_ROOT_MARKER_FILE_NAME)
}

pub fn read_repo_root_marker(data_dir: &Path) -> Option<PathBuf> {
    let marker = repo_root_marker_path(data_dir);
    let Ok(raw) = fs::read_to_string(&marker) else {
        return None;
    };
    let value = raw.trim();
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

pub fn fastembed_cache_dir() -> Result<PathBuf> {
    Ok(budi_home_dir()?.join(BUDI_FASTEMBED_CACHE_DIR_NAME))
}

pub fn embedding_cache_path() -> Result<PathBuf> {
    Ok(budi_home_dir()?.join(BUDI_EMBEDDING_CACHE_FILE_NAME))
}

pub fn ensure_repo_layout(repo_root: &Path) -> Result<()> {
    let paths = repo_paths(repo_root)?;
    fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("Failed to create {}", paths.data_dir.display()))?;
    fs::create_dir_all(&paths.index_dir)
        .with_context(|| format!("Failed to create {}", paths.index_dir.display()))?;
    fs::create_dir_all(&paths.log_dir)
        .with_context(|| format!("Failed to create {}", paths.log_dir.display()))?;
    fs::create_dir_all(&paths.bench_dir)
        .with_context(|| format!("Failed to create {}", paths.bench_dir.display()))?;
    fs::create_dir_all(repo_root.join(".claude"))
        .with_context(|| "Failed to create .claude".to_string())?;
    let canonical_repo_root =
        fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let marker_path = repo_root_marker_path(&paths.data_dir);
    fs::write(&marker_path, canonical_repo_root.display().to_string())
        .with_context(|| format!("Failed writing {}", marker_path.display()))?;

    let global_ignore_file = global_ignore_path()?;
    if let Some(parent) = global_ignore_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if !global_ignore_file.exists() {
        fs::write(
            &global_ignore_file,
            "# budi global index exclusions (applies to every repo)\n# Prefix with ! to unignore an included path\n",
        )
        .with_context(|| format!("Failed writing {}", global_ignore_file.display()))?;
    }

    Ok(())
}

pub fn load_or_default(repo_root: &Path) -> Result<BudiConfig> {
    let config_path = config_path(repo_root)?;
    if !config_path.exists() {
        return Ok(BudiConfig::default());
    }
    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed reading {}", config_path.display()))?;
    let config: BudiConfig =
        toml::from_str(&raw).with_context(|| "Invalid budi config TOML".to_string())?;
    Ok(config)
}

pub fn save(repo_root: &Path, config: &BudiConfig) -> Result<()> {
    let config_path = config_path(repo_root)?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed creating {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(config)?;
    fs::write(&config_path, raw)
        .with_context(|| format!("Failed writing {}", config_path.display()))?;
    Ok(())
}

fn repo_storage_id(repo_root: &Path) -> String {
    // Resolve worktree → main repo root so all worktrees share one index directory.
    let storage_root = resolve_storage_root(repo_root);
    let canonical = fs::canonicalize(&storage_root).unwrap_or_else(|_| storage_root.to_path_buf());
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    let hash_hex = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let mut slug = storage_root
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("repo")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "repo".to_string();
    }
    if slug.len() > 32 {
        slug.truncate(32);
    }
    format!("{slug}-{}", &hash_hex[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_storage_id_uses_slug_plus_short_hash() {
        let id = repo_storage_id(Path::new("/tmp/My Repo"));
        assert!(id.starts_with("my-repo-"));
        let hash_part = id.rsplit('-').next().unwrap_or_default();
        assert_eq!(hash_part.len(), 12);
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_storage_root_returns_self_for_normal_repo() {
        let repo = Path::new("/tmp/normal-repo");
        // No .git file → returns self
        assert_eq!(resolve_storage_root(repo), repo);
    }

    #[test]
    fn resolve_worktree_main_root_parses_gitdir() {
        let tmp = std::env::temp_dir().join("budi-worktree-test");
        let main_root = tmp.join("main-repo");
        let main_git = main_root.join(".git");
        let wt_dir = main_git.join("worktrees").join("feature-branch");
        std::fs::create_dir_all(&wt_dir).unwrap();

        let wt_root = tmp.join("feature-branch");
        std::fs::create_dir_all(&wt_root).unwrap();
        let wt_git_file = wt_root.join(".git");
        std::fs::write(&wt_git_file, format!("gitdir: {}", wt_dir.display())).unwrap();

        let resolved = resolve_storage_root(&wt_root);
        assert_eq!(
            std::fs::canonicalize(&resolved).unwrap(),
            std::fs::canonicalize(&main_root).unwrap(),
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_root_marker_round_trip() {
        let data_dir = PathBuf::from("/tmp/budi-marker-test");
        let marker_path = repo_root_marker_path(&data_dir);
        assert!(marker_path.ends_with(BUDI_REPO_ROOT_MARKER_FILE_NAME));
    }

    #[test]
    fn layered_ignore_paths_include_global_then_repo() {
        let repo_root = Path::new("/tmp/repo");
        let paths = layered_ignore_paths(repo_root).expect("layered paths");
        assert_eq!(paths.len(), 3);
        assert!(paths[0].ends_with(BUDI_GLOBAL_IGNORE_FILE_NAME));
        assert!(paths[1].ends_with(BUDI_LOCAL_IGNORE_FILE_NAME));
        assert_eq!(paths[2], repo_root.join(BUDI_IGNORE_FILE_NAME));
    }
}
