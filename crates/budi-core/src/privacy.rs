use std::env;

use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_RAW_RETENTION_DAYS: u32 = 30;
const DEFAULT_SESSION_METADATA_RETENTION_DAYS: u32 = 90;

/// Privacy behavior for sensitive fields in raw payloads/session metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyMode {
    /// Store values as-is.
    Full,
    /// Store deterministic hashes instead of raw values.
    Hash,
    /// Drop sensitive values entirely.
    Omit,
}

impl PrivacyMode {
    fn from_env(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" | "off" | "none" => Self::Full,
            "hash" | "hashed" => Self::Hash,
            "omit" | "redact" | "strict" => Self::Omit,
            _ => {
                tracing::warn!(
                    "Unknown BUDI_PRIVACY_MODE value '{}'; falling back to 'full'",
                    raw
                );
                Self::Full
            }
        }
    }
}

/// Runtime policy loaded from environment variables.
#[derive(Debug, Clone)]
pub struct PrivacyPolicy {
    pub mode: PrivacyMode,
    /// Retention window for raw payload columns (`raw_json`) in days.
    pub raw_retention_days: Option<u32>,
    /// Retention window for sensitive session metadata in days.
    pub session_metadata_retention_days: Option<u32>,
}

impl Default for PrivacyPolicy {
    fn default() -> Self {
        Self {
            mode: PrivacyMode::Full,
            raw_retention_days: Some(DEFAULT_RAW_RETENTION_DAYS),
            session_metadata_retention_days: Some(DEFAULT_SESSION_METADATA_RETENTION_DAYS),
        }
    }
}

/// Read privacy policy from env vars.
///
/// Supported env vars:
/// - `BUDI_PRIVACY_MODE` = `full` | `hash` | `omit` (default: `full`)
/// - `BUDI_RETENTION_RAW_DAYS` = non-negative integer or `off` (default: 30)
/// - `BUDI_RETENTION_SESSION_METADATA_DAYS` = non-negative integer or `off` (default: 90)
pub fn load_privacy_policy() -> PrivacyPolicy {
    let mut policy = PrivacyPolicy::default();

    if let Ok(mode) = env::var("BUDI_PRIVACY_MODE") {
        policy.mode = PrivacyMode::from_env(&mode);
    }
    policy.raw_retention_days =
        read_retention_days("BUDI_RETENTION_RAW_DAYS", DEFAULT_RAW_RETENTION_DAYS);
    policy.session_metadata_retention_days = read_retention_days(
        "BUDI_RETENTION_SESSION_METADATA_DAYS",
        DEFAULT_SESSION_METADATA_RETENTION_DAYS,
    );
    policy
}

fn read_retention_days(key: &str, default_days: u32) -> Option<u32> {
    let Ok(raw) = env::var(key) else {
        return Some(default_days);
    };
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("off")
        || raw.eq_ignore_ascii_case("none")
        || raw.eq_ignore_ascii_case("disable")
        || raw.eq_ignore_ascii_case("disabled")
    {
        return None;
    }
    match raw.parse::<u32>() {
        Ok(days) => Some(days),
        Err(_) => {
            tracing::warn!(
                "Invalid {key} value '{}'; expected non-negative integer or 'off'. Using default {default_days}.",
                raw
            );
            Some(default_days)
        }
    }
}

fn retention_modifier(days: u32) -> String {
    format!("-{days} days")
}

/// Deterministically hash a sensitive value.
pub fn hash_sensitive_value(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::from("sha256:");
    for b in digest.iter().take(12) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn normalize_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Minimize a scalar field according to policy mode.
pub fn minimize_sensitive_field(value: Option<&str>, mode: PrivacyMode) -> Option<String> {
    let normalized = normalize_nonempty(value);
    match mode {
        PrivacyMode::Full => normalized.map(str::to_string),
        PrivacyMode::Hash => normalized.map(hash_sensitive_value),
        PrivacyMode::Omit => None,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key,
        "user_email" | "cwd" | "workspace_root" | "workspace_roots" | "project_dir"
    )
}

fn sanitize_sensitive_json_value(value: &mut Value, mode: PrivacyMode) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if is_sensitive_key(&key) {
                    match mode {
                        PrivacyMode::Full => {}
                        PrivacyMode::Hash => {
                            if let Some(field_value) = map.get_mut(&key) {
                                hash_json_field(field_value);
                            }
                        }
                        PrivacyMode::Omit => {
                            map.remove(&key);
                            continue;
                        }
                    }
                }
                if let Some(field_value) = map.get_mut(&key) {
                    sanitize_sensitive_json_value(field_value, mode);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_sensitive_json_value(item, mode);
            }
        }
        _ => {}
    }
}

fn hash_json_field(value: &mut Value) {
    match value {
        Value::String(s) if !s.trim().is_empty() => {
            *s = hash_sensitive_value(s);
        }
        Value::String(_) => {}
        Value::Array(items) => {
            for item in items {
                if let Value::String(s) = item
                    && !s.trim().is_empty()
                {
                    *s = hash_sensitive_value(s);
                }
            }
        }
        _ => {}
    }
}

/// Sanitize hook raw payload JSON according to policy.
///
/// Returns `None` when payload storage should be omitted.
pub fn sanitize_hook_raw_json(raw_json: &str, mode: PrivacyMode) -> Option<String> {
    match mode {
        PrivacyMode::Full => Some(raw_json.to_string()),
        PrivacyMode::Omit => None,
        PrivacyMode::Hash => {
            let mut payload: Value = match serde_json::from_str(raw_json) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Failed to parse hook raw_json for sanitization: {e}");
                    return None;
                }
            };
            sanitize_sensitive_json_value(&mut payload, mode);
            Some(payload.to_string())
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionReport {
    pub session_raw_scrubbed: usize,
    pub session_metadata_scrubbed: usize,
}

impl RetentionReport {
    pub fn touched_rows(&self) -> usize {
        self.session_raw_scrubbed + self.session_metadata_scrubbed
    }
}

/// Apply retention policy to raw payload/session metadata columns.
pub fn enforce_retention(conn: &Connection) -> Result<RetentionReport> {
    let policy = load_privacy_policy();
    enforce_retention_with_policy(conn, &policy)
}

/// Apply retention using an explicit policy (primarily for tests).
pub fn enforce_retention_with_policy(
    conn: &Connection,
    policy: &PrivacyPolicy,
) -> Result<RetentionReport> {
    let mut report = RetentionReport::default();

    if let Some(days) = policy.raw_retention_days {
        let cutoff = retention_modifier(days);
        report.session_raw_scrubbed = conn.execute(
            "UPDATE sessions
             SET raw_json = NULL
             WHERE raw_json IS NOT NULL
               AND COALESCE(ended_at, started_at) IS NOT NULL
               AND julianday(COALESCE(ended_at, started_at)) < julianday('now', ?1)",
            params![cutoff],
        )?;
    }

    if let Some(days) = policy.session_metadata_retention_days {
        let cutoff = retention_modifier(days);
        report.session_metadata_scrubbed = conn.execute(
            "UPDATE sessions
             SET user_email = NULL,
                 workspace_root = NULL
             WHERE (user_email IS NOT NULL OR workspace_root IS NOT NULL)
               AND COALESCE(ended_at, started_at) IS NOT NULL
               AND julianday(COALESCE(ended_at, started_at)) < julianday('now', ?1)",
            params![cutoff],
        )?;
    }

    if report.touched_rows() > 0 {
        tracing::info!(
            "Privacy retention scrubbed {} rows (session_raw={}, session_metadata={})",
            report.touched_rows(),
            report.session_raw_scrubbed,
            report.session_metadata_scrubbed
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn minimize_sensitive_field_hashes_or_omits() {
        let raw = Some("test@example.com");
        assert_eq!(
            minimize_sensitive_field(raw, PrivacyMode::Hash),
            Some(hash_sensitive_value("test@example.com"))
        );
        assert_eq!(minimize_sensitive_field(raw, PrivacyMode::Omit), None);
        assert_eq!(
            minimize_sensitive_field(raw, PrivacyMode::Full),
            Some("test@example.com".to_string())
        );
    }

    #[test]
    fn sanitize_hook_raw_json_hashes_sensitive_keys() {
        let raw = serde_json::json!({
            "user_email": "dev@example.com",
            "cwd": "/Users/dev/repo",
            "workspace_roots": ["/Users/dev/repo", "/tmp/other"],
            "safe": "keep"
        })
        .to_string();

        let sanitized = sanitize_hook_raw_json(&raw, PrivacyMode::Hash).unwrap();
        let parsed: Value = serde_json::from_str(&sanitized).unwrap();
        assert_ne!(
            parsed.get("user_email").and_then(|v| v.as_str()),
            Some("dev@example.com")
        );
        assert_ne!(
            parsed.get("cwd").and_then(|v| v.as_str()),
            Some("/Users/dev/repo")
        );
        assert_eq!(parsed.get("safe").and_then(|v| v.as_str()), Some("keep"));
    }

    #[test]
    fn sanitize_hook_raw_json_omit_drops_payload() {
        let raw = r#"{"user_email":"dev@example.com"}"#;
        assert_eq!(sanitize_hook_raw_json(raw, PrivacyMode::Omit), None);
    }

    #[test]
    fn enforce_retention_scrubs_old_raw_payloads_and_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migration::migrate(&conn).unwrap();

        let old_ts = (Utc::now() - Duration::days(45)).to_rfc3339();
        let fresh_ts = (Utc::now() - Duration::days(2)).to_rfc3339();

        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, user_email, workspace_root, raw_json)
             VALUES ('s-old', 'claude_code', ?1, 'old@example.com', '/old/path', '{\"old\":true}')",
            params![old_ts],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, provider, started_at, user_email, workspace_root, raw_json)
             VALUES ('s-new', 'claude_code', ?1, 'new@example.com', '/new/path', '{\"new\":true}')",
            params![fresh_ts],
        )
        .unwrap();

        let policy = PrivacyPolicy {
            mode: PrivacyMode::Full,
            raw_retention_days: Some(30),
            session_metadata_retention_days: Some(30),
        };
        let report = enforce_retention_with_policy(&conn, &policy).unwrap();
        assert_eq!(report.session_raw_scrubbed, 1);
        assert_eq!(report.session_metadata_scrubbed, 1);

        let old_user_email: Option<String> = conn
            .query_row(
                "SELECT user_email FROM sessions WHERE id='s-old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let new_user_email: Option<String> = conn
            .query_row(
                "SELECT user_email FROM sessions WHERE id='s-new'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(old_user_email.is_none());
        assert_eq!(new_user_email.as_deref(), Some("new@example.com"));
    }
}
