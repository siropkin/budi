//! Read Claude Code local data files from ~/.claude/ for dashboard display.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

fn claude_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

// ---------------------------------------------------------------------------
// 1. Activity Timeline (stats-cache.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ActivityTimeline {
    pub daily_activity: Vec<DailyActivity>,
    pub hour_counts: HashMap<String, u64>,
    pub longest_session: Option<LongestSession>,
    pub total_sessions: u64,
    pub total_messages: u64,
    pub first_session_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyActivity {
    pub date: String,
    pub message_count: u64,
    pub session_count: u64,
    pub tool_call_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LongestSession {
    pub session_id: String,
    pub duration_ms: u64,
    pub message_count: u64,
}

pub fn read_activity_timeline() -> Result<ActivityTimeline> {
    let path = claude_home()?.join("stats-cache.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return Ok(ActivityTimeline {
                daily_activity: vec![],
                hour_counts: HashMap::new(),
                longest_session: None,
                total_sessions: 0,
                total_messages: 0,
                first_session_date: None,
            });
        }
    };

    let raw: serde_json::Value = serde_json::from_str(&content)?;

    let daily_activity = raw
        .get("dailyActivity")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(DailyActivity {
                        date: e.get("date")?.as_str()?.to_string(),
                        message_count: e.get("messageCount")?.as_u64().unwrap_or(0),
                        session_count: e.get("sessionCount")?.as_u64().unwrap_or(0),
                        tool_call_count: e.get("toolCallCount")?.as_u64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let hour_counts = raw
        .get("hourCounts")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();

    let longest_session = raw.get("longestSession").and_then(|ls| {
        Some(LongestSession {
            session_id: ls.get("sessionId")?.as_str()?.to_string(),
            duration_ms: ls.get("duration")?.as_u64()?,
            message_count: ls.get("messageCount")?.as_u64().unwrap_or(0),
        })
    });

    let total_sessions = raw
        .get("totalSessions")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_messages = raw
        .get("totalMessages")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let first_session_date = raw
        .get("firstSessionDate")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(ActivityTimeline {
        daily_activity,
        hour_counts,
        longest_session,
        total_sessions,
        total_messages,
        first_session_date,
    })
}

// ---------------------------------------------------------------------------
// 2. Installed Plugins (plugins/installed_plugins.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub description: String,
    pub version: String,
    pub scope: String,
    pub installed_at: String,
    pub last_updated: String,
}

pub fn read_installed_plugins() -> Result<Vec<PluginInfo>> {
    let path = claude_home()?
        .join("plugins")
        .join("installed_plugins.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Ok(vec![]),
    };

    let raw: serde_json::Value = serde_json::from_str(&content)?;
    let plugins_map = match raw.get("plugins").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return Ok(vec![]),
    };

    let mut results = Vec::new();
    for (key, installs) in plugins_map {
        let name = key.split('@').next().unwrap_or(key).to_string();
        if let Some(arr) = installs.as_array() {
            for inst in arr {
                // Try to read description from plugin.json in install path
                let description = inst
                    .get("installPath")
                    .and_then(|v| v.as_str())
                    .and_then(|p| {
                        let plugin_json =
                            PathBuf::from(p).join(".claude-plugin").join("plugin.json");
                        std::fs::read_to_string(&plugin_json).ok()
                    })
                    .and_then(|c| {
                        let pj: serde_json::Value = serde_json::from_str(&c).ok()?;
                        pj.get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default();

                results.push(PluginInfo {
                    name: name.clone(),
                    description,
                    version: inst
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    scope: inst
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .unwrap_or("user")
                        .to_string(),
                    installed_at: inst
                        .get("installedAt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    last_updated: inst
                        .get("lastUpdated")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

// ---------------------------------------------------------------------------
// 3. Active Sessions (sessions/*.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ActiveSession {
    pub pid: u64,
    pub session_id: String,
    pub cwd: String,
    pub started_at: u64,
    pub is_alive: bool,
}

pub fn read_active_sessions() -> Result<Vec<ActiveSession>> {
    let dir = claude_home()?.join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]),
    };

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content)
        {
            let pid = raw.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
            let is_alive = if pid > 0 {
                #[cfg(unix)]
                {
                    unsafe { libc::kill(pid as i32, 0) == 0 }
                }
                #[cfg(not(unix))]
                {
                    false
                }
            } else {
                false
            };
            results.push(ActiveSession {
                pid,
                session_id: raw
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                cwd: raw
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                started_at: raw.get("startedAt").and_then(|v| v.as_u64()).unwrap_or(0),
                is_alive,
            });
        }
    }
    // Only return alive sessions
    results.retain(|s| s.is_alive);
    Ok(results)
}

// ---------------------------------------------------------------------------
// 4. Plans (plans/*.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PlanFile {
    pub name: String,
    pub title: String,
    pub path: String,
    pub size_bytes: u64,
    pub est_tokens: u64,
    pub modified: String,
    pub preview: String,
}

pub fn read_plans() -> Result<Vec<PlanFile>> {
    let dir = claude_home()?.join("plans");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]),
    };

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        if let Ok(metadata) = std::fs::metadata(&path) {
            let modified = metadata
                .modified()
                .ok()
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_default();
            let size = metadata.len();
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let title = content
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l.trim_start_matches("# ").trim().to_string())
                .unwrap_or_default();
            let preview = {
                let trimmed = content.trim().replace('\n', " ");
                if trimmed.len() > 300 {
                    format!("{}...", &trimmed[..300])
                } else {
                    trimmed
                }
            };
            results.push(PlanFile {
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                title,
                path: path.display().to_string(),
                size_bytes: size,
                est_tokens: size / 4,
                modified,
                preview,
            });
        }
    }
    results.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(results)
}

// ---------------------------------------------------------------------------
// 5. Memory Files (projects/*/memory/*.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MemoryFile {
    pub project: String,
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub est_tokens: u64,
}

pub fn read_memory_files() -> Result<Vec<MemoryFile>> {
    let projects_dir = claude_home()?.join("projects");
    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]),
    };

    let mut results = Vec::new();
    for proj_entry in entries.flatten() {
        let proj_path = proj_entry.path();
        if !proj_path.is_dir() {
            continue;
        }
        let memory_dir = proj_path.join("memory");
        if !memory_dir.is_dir() {
            continue;
        }
        // Decode project name from directory: -Users-foo--projects-bar → bar
        let proj_dir_name = proj_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let project_name = proj_dir_name.rsplit('-').next().unwrap_or(&proj_dir_name);

        if let Ok(mem_entries) = std::fs::read_dir(&memory_dir) {
            for mem_entry in mem_entries.flatten() {
                let mem_path = mem_entry.path();
                if mem_path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                if let Ok(metadata) = std::fs::metadata(&mem_path) {
                    let size = metadata.len();
                    results.push(MemoryFile {
                        project: project_name.to_string(),
                        name: mem_path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        path: mem_path.display().to_string(),
                        size_bytes: size,
                        est_tokens: size / 4,
                    });
                }
            }
        }
    }
    results.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    Ok(results)
}

// ---------------------------------------------------------------------------
// 6. Permissions (settings.json + settings.local.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PermissionsSummary {
    pub default_mode: String,
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionRule {
    pub rule: String,
    pub action: String, // "allow" or "deny"
    pub scope: String,  // "global", "local", or project name
}

fn extract_rules(
    raw: &serde_json::Value,
    scope: &str,
    results: &mut Vec<PermissionRule>,
) -> String {
    let perms = raw.get("permissions");
    let mode = perms
        .and_then(|p| p.get("defaultMode"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    for action in &["allow", "deny"] {
        if let Some(arr) = perms
            .and_then(|p| p.get(*action))
            .and_then(|v| v.as_array())
        {
            for v in arr {
                if let Some(s) = v.as_str() {
                    results.push(PermissionRule {
                        rule: s.to_string(),
                        action: action.to_string(),
                        scope: scope.to_string(),
                    });
                }
            }
        }
    }
    mode
}

pub fn read_permissions() -> Result<PermissionsSummary> {
    let home = claude_home()?;
    let mut rules = Vec::new();

    // Global settings
    let mode = if let Ok(content) = std::fs::read_to_string(home.join("settings.json")) {
        let raw: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
        extract_rules(&raw, "global", &mut rules)
    } else {
        "default".to_string()
    };

    // Local settings
    if let Ok(content) = std::fs::read_to_string(home.join("settings.local.json")) {
        let raw: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
        extract_rules(&raw, "local", &mut rules);
    }

    // Per-project settings
    let projects_dir = home.join("projects");
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let proj_path = entry.path();
            if !proj_path.is_dir() {
                continue;
            }
            let proj_name = proj_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            // Decode: -Users-foo--projects-bar → last segment
            let short_name = proj_name
                .rsplit('-')
                .next()
                .unwrap_or(&proj_name)
                .to_string();

            for settings_file in &["settings.json", "settings.local.json"] {
                let path = proj_path.join(settings_file);
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let raw: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
                    extract_rules(&raw, &short_name, &mut rules);
                }
            }
        }
    }

    Ok(PermissionsSummary {
        default_mode: mode,
        rules,
    })
}

// ---------------------------------------------------------------------------
// 7. Prompt History (history.jsonl)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PromptHistory {
    pub total_count: u64,
    pub entries: Vec<PromptEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromptEntry {
    pub display: String,
    pub timestamp: u64,
    pub project: Option<String>,
}

pub fn read_prompt_history(limit: usize) -> Result<PromptHistory> {
    let path = claude_home()?.join("history.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return Ok(PromptHistory {
                total_count: 0,
                entries: vec![],
            });
        }
    };

    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_count = lines.len() as u64;

    let start = if lines.len() > limit {
        lines.len() - limit
    } else {
        0
    };

    let entries: Vec<PromptEntry> = lines[start..]
        .iter()
        .rev()
        .filter_map(|line| {
            let raw: serde_json::Value = serde_json::from_str(line).ok()?;
            let display = raw.get("display")?.as_str()?.to_string();
            // Skip slash commands and very short entries
            if display.starts_with('/') || display.len() < 3 {
                return None;
            }
            let timestamp = raw.get("timestamp")?.as_u64()?;
            let project = raw
                .get("project")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(PromptEntry {
                display,
                timestamp,
                project,
            })
        })
        .collect();

    Ok(PromptHistory {
        total_count,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_home_resolves() {
        let home = claude_home();
        assert!(home.is_ok());
        assert!(home.unwrap().ends_with(".claude"));
    }

    #[test]
    fn read_activity_handles_missing() {
        // If stats-cache.json is missing on CI, should still return empty
        let result = read_activity_timeline();
        assert!(result.is_ok());
    }

    #[test]
    fn read_plugins_handles_missing() {
        let result = read_installed_plugins();
        assert!(result.is_ok());
    }

    #[test]
    fn read_sessions_handles_missing() {
        let result = read_active_sessions();
        assert!(result.is_ok());
    }

    #[test]
    fn read_plans_handles_missing() {
        let result = read_plans();
        assert!(result.is_ok());
    }

    #[test]
    fn read_memory_handles_missing() {
        let result = read_memory_files();
        assert!(result.is_ok());
    }

    #[test]
    fn read_permissions_handles_missing() {
        let result = read_permissions();
        assert!(result.is_ok());
    }

    #[test]
    fn read_history_handles_missing() {
        let result = read_prompt_history(10);
        assert!(result.is_ok());
    }
}
