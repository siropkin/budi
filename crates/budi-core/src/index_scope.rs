use std::collections::HashSet;
use std::path::Path;

use crate::config::BudiConfig;

pub(crate) fn build_extension_allowlist(config: &BudiConfig) -> HashSet<String> {
    let source = if config.index_extensions.is_empty() {
        BudiConfig::default().index_extensions
    } else {
        config.index_extensions.clone()
    };
    source
        .iter()
        .filter_map(|ext| {
            let normalized = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if normalized.is_empty() {
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

pub(crate) fn build_basename_allowlist(config: &BudiConfig) -> HashSet<String> {
    let source = if config.index_basenames.is_empty() {
        BudiConfig::default().index_basenames
    } else {
        config.index_basenames.clone()
    };
    source
        .iter()
        .filter_map(|name| {
            let normalized = name.trim().to_ascii_lowercase();
            if normalized.is_empty() {
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

pub(crate) fn is_always_skipped_dir_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".nuxt"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | "coverage"
            | ".turbo"
            | ".cache"
            | "vendor"
    )
}

pub(crate) fn is_supported_code_file(
    path: &Path,
    extension_allowlist: &HashSet<String>,
    basename_allowlist: &HashSet<String>,
) -> bool {
    if let Some(ext) = path.extension().and_then(|value| value.to_str())
        && extension_allowlist.contains(&ext.to_ascii_lowercase())
    {
        return true;
    }
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    basename_allowlist.contains(&file_name.to_ascii_lowercase())
}
