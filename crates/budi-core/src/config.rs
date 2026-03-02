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
pub const BUDI_IGNORE_FILE_NAME: &str = "ignore";
pub const BUDI_INDEX_DIR_NAME: &str = "index";
pub const BUDI_STATE_FILE_NAME: &str = "state.json";
pub const BUDI_TANTIVY_DIR_NAME: &str = "tantivy";
pub const BUDI_LOG_DIR_NAME: &str = "logs";
pub const BUDI_BENCH_DIR_NAME: &str = "benchmarks";

pub const CLAUDE_LOCAL_SETTINGS: &str = ".claude/settings.local.json";

pub const DEFAULT_DAEMON_HOST: &str = "127.0.0.1";
pub const DEFAULT_DAEMON_PORT: u16 = 7878;
pub const DEFAULT_RETRIEVAL_LIMIT: usize = 20;
pub const DEFAULT_CONTEXT_CHAR_BUDGET: usize = 12_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudiConfig {
    pub daemon_host: String,
    pub daemon_port: u16,
    pub retrieval_limit: usize,
    pub context_char_budget: usize,
    pub max_file_bytes: usize,
    pub chunk_lines: usize,
    pub chunk_overlap: usize,
    pub topk_lexical: usize,
    pub topk_vector: usize,
    pub smart_skip_enabled: bool,
    pub skip_non_code_prompts: bool,
    pub min_confidence_to_inject: f32,
    pub debug_io: bool,
    pub debug_io_full_text: bool,
    pub debug_io_max_chars: usize,
}

impl Default for BudiConfig {
    fn default() -> Self {
        Self {
            daemon_host: DEFAULT_DAEMON_HOST.to_string(),
            daemon_port: DEFAULT_DAEMON_PORT,
            retrieval_limit: DEFAULT_RETRIEVAL_LIMIT,
            context_char_budget: DEFAULT_CONTEXT_CHAR_BUDGET,
            max_file_bytes: 1_500_000,
            chunk_lines: 80,
            chunk_overlap: 20,
            topk_lexical: 20,
            topk_vector: 20,
            smart_skip_enabled: true,
            skip_non_code_prompts: true,
            min_confidence_to_inject: 0.45,
            debug_io: false,
            debug_io_full_text: false,
            debug_io_max_chars: 1200,
        }
    }
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
    pub ignore_file: PathBuf,
    pub index_dir: PathBuf,
    pub state_file: PathBuf,
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

pub fn budi_home_dir() -> Result<PathBuf> {
    if let Ok(override_dir) = env::var(BUDI_HOME_ENV) {
        return Ok(PathBuf::from(override_dir));
    }
    let home_dir = env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home_dir).join(BUDI_HOME_DEFAULT_REL))
}

pub fn repo_paths(repo_root: &Path) -> Result<RepoPaths> {
    let home = budi_home_dir()?;
    let repos_root = home.join(BUDI_REPOS_DIR);
    let repo_id = repo_storage_id(repo_root);
    let data_dir = repos_root.join(repo_id);
    let index_dir = data_dir.join(BUDI_INDEX_DIR_NAME);
    let tantivy_dir = index_dir.join(BUDI_TANTIVY_DIR_NAME);
    let log_dir = data_dir.join(BUDI_LOG_DIR_NAME);
    let bench_dir = data_dir.join(BUDI_BENCH_DIR_NAME);
    Ok(RepoPaths {
        config_file: data_dir.join(BUDI_CONFIG_FILE_NAME),
        ignore_file: data_dir.join(BUDI_IGNORE_FILE_NAME),
        state_file: index_dir.join(BUDI_STATE_FILE_NAME),
        data_dir,
        index_dir,
        tantivy_dir,
        log_dir,
        bench_dir,
    })
}

pub fn config_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.config_file)
}

pub fn ignore_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.ignore_file)
}

pub fn state_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(repo_paths(repo_root)?.state_file)
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

    if !paths.ignore_file.exists() {
        fs::write(
            &paths.ignore_file,
            "# Additional ignore patterns for budi (one glob per line)\n",
        )
        .with_context(|| format!("Failed writing {}", paths.ignore_file.display()))?;
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
    let canonical = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    let hash_hex = digest
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let mut slug = repo_root
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
}
