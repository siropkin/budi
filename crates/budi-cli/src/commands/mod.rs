use std::path::PathBuf;

use budi_core::config;

pub mod doctor;
pub mod hook;
pub mod init;
pub mod open;
pub mod stats;
pub mod statusline;
pub mod sync;
pub mod uninstall;
pub mod update;

/// Returns true if color output should be used (NO_COLOR env var is not set).
pub fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

/// Returns the ANSI escape code if color is enabled, otherwise empty string.
pub fn ansi(code: &str) -> &str {
    if use_color() { code } else { "" }
}

/// Try to resolve a repo root, but return None if not in a git repository.
pub fn try_resolve_repo_root(candidate: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = candidate {
        return Some(path);
    }
    let cwd = std::env::current_dir().ok()?;
    config::find_repo_root(&cwd).ok()
}

/// Format a cost value in dollars: $1.2K, $123, $12.50, $0.42, $0.00
pub fn format_cost(dollars: f64) -> String {
    if dollars >= 1000.0 {
        format!("${:.1}K", dollars / 1000.0)
    } else if dollars >= 100.0 {
        format!("${:.0}", dollars)
    } else if dollars > 0.0 {
        format!("${:.2}", dollars)
    } else {
        "$0.00".to_string()
    }
}
