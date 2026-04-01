use anyhow::{Context, Result};
use serde_json::Value;

/// Normalize and validate a release tag used in shell commands and URLs.
///
/// Accepts either `7.1.0` or `v7.1.0` style input (and other safe tag variants
/// like `v7.1.0-rc1`), then returns the normalized `v...` form.
pub fn normalize_release_tag(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let candidate = if trimmed.starts_with('v') {
        trimmed.to_string()
    } else {
        format!("v{trimmed}")
    };

    // Safety gate: shell command and URL interpolation must use a restricted charset.
    let is_safe = candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if !is_safe || candidate == "v" {
        anyhow::bail!(
            "Invalid version/tag `{raw}`. Allowed characters: letters, digits, `.`, `-`, `_`"
        );
    }

    Ok(candidate)
}

pub fn version_from_tag(tag: &str) -> String {
    tag.trim_start_matches('v').to_string()
}

pub fn parse_release_tag(release: &Value) -> Result<String> {
    release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("Could not parse release tag")
        .map(|s| s.to_string())
}

pub fn parse_and_normalize_release_tag(release: &Value) -> Result<String> {
    let raw_tag = parse_release_tag(release)?;
    normalize_release_tag(&raw_tag).with_context(|| {
        format!(
            "Latest release tag has unsupported format: {raw_tag}. \
             Please update manually or run `budi update --version <tag>`."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{normalize_release_tag, parse_and_normalize_release_tag, version_from_tag};
    use serde_json::json;

    #[test]
    fn normalize_release_tag_accepts_plain_semver() {
        assert_eq!(
            normalize_release_tag("7.1.0").expect("valid semver"),
            "v7.1.0"
        );
    }

    #[test]
    fn normalize_release_tag_accepts_existing_v_prefix() {
        assert_eq!(
            normalize_release_tag("v7.1.0-rc1").expect("valid prefixed tag"),
            "v7.1.0-rc1"
        );
    }

    #[test]
    fn normalize_release_tag_rejects_unsafe_characters() {
        assert!(normalize_release_tag("v7.1.0;rm -rf /").is_err());
        assert!(normalize_release_tag("v7.1.0\" && whoami").is_err());
        assert!(normalize_release_tag("").is_err());
    }

    #[test]
    fn version_from_tag_strips_prefix() {
        assert_eq!(version_from_tag("v7.5.0"), "7.5.0");
        assert_eq!(version_from_tag("7.5.0"), "7.5.0");
    }

    #[test]
    fn parse_and_normalize_release_tag_reads_json() {
        let release = json!({ "tag_name": "7.5.0" });
        assert_eq!(
            parse_and_normalize_release_tag(&release).expect("normalized"),
            "v7.5.0"
        );
    }
}
