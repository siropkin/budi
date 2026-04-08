use serde_json::Value;

// ---------------------------------------------------------------------------
// Hook event constants — single source of truth for CLI, daemon, and tests
// ---------------------------------------------------------------------------

/// Claude Code hook events (PascalCase).
pub const CC_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "PostToolUse",
    "SubagentStop",
    "PreCompact",
    "Stop",
    "UserPromptSubmit",
];

/// Cursor hook events (camelCase).
pub const CURSOR_HOOK_EVENTS: &[&str] = &[
    "sessionStart",
    "sessionEnd",
    "postToolUse",
    "subagentStop",
    "preCompact",
    "stop",
    "afterFileEdit",
    "beforeSubmitPrompt",
];

// ---------------------------------------------------------------------------
// Hook detection helpers
// ---------------------------------------------------------------------------

/// Match any variant of the budi hook command (with or without `|| true` wrapper).
pub fn is_budi_hook_cmd(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed == "budi hook" || trimmed.starts_with("budi hook ")
}

/// Check if a Claude Code hook entry (nested format) contains a budi hook command.
pub fn is_budi_cc_hook_entry(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_budi_hook_cmd)
            })
        })
        .unwrap_or(false)
}

/// Check if a Cursor hook entry (flat format) contains a budi hook command.
pub fn is_budi_cursor_hook_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(is_budi_hook_cmd)
}

// ---------------------------------------------------------------------------
// Integration validation — shared by doctor and /health/integrations
// ---------------------------------------------------------------------------

/// Validate Claude Code hooks in a parsed settings JSON value.
/// Returns `(all_ok, missing_events)`.
pub fn validate_cc_hooks(settings: &Value) -> (bool, Vec<String>) {
    let Some(hooks) = settings.get("hooks").and_then(|v| v.as_object()) else {
        return (false, vec!["no hooks key".into()]);
    };

    let mut missing = Vec::new();
    for event in CC_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|e| is_budi_cc_hook_entry(e)))
            .unwrap_or(false);
        if !ok {
            missing.push((*event).to_string());
        }
    }
    (missing.is_empty(), missing)
}

/// Validate Cursor hooks in a parsed hooks.json value.
/// Returns `(all_ok, missing_events)`.
pub fn validate_cursor_hooks(config: &Value) -> (bool, Vec<String>) {
    let Some(hooks) = config.get("hooks").and_then(|v| v.as_object()) else {
        return (false, vec!["no hooks key".into()]);
    };

    let mut missing = Vec::new();
    for event in CURSOR_HOOK_EVENTS {
        let ok = hooks
            .get(*event)
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|e| is_budi_cursor_hook_entry(e)))
            .unwrap_or(false);
        if !ok {
            missing.push((*event).to_string());
        }
    }
    (missing.is_empty(), missing)
}

/// Check if the budi MCP server is configured in a parsed Claude Code settings value.
pub fn check_mcp_config(settings: &Value) -> bool {
    let Some(budi) = settings.get("mcpServers").and_then(|m| m.get("budi")) else {
        return false;
    };
    budi.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains("budi"))
        && budi
            .get("args")
            .and_then(|a| a.as_array())
            .is_some_and(|args| args.iter().any(|a| a.as_str() == Some("mcp-serve")))
}

/// Check if OTEL env vars are correctly configured in a parsed Claude Code settings value.
///
/// `daemon_port` is used to verify the endpoint points to the expected local daemon.
pub fn check_otel_config(settings: &Value, daemon_port: u16) -> bool {
    let Some(env) = settings.get("env").and_then(|e| e.as_object()) else {
        return false;
    };

    let expected_endpoint = format!("http://127.0.0.1:{}", daemon_port);
    let checks = [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", Some("1")),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            Some(expected_endpoint.as_str()),
        ),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", Some("http/json")),
        ("OTEL_METRICS_EXPORTER", Some("otlp")),
        ("OTEL_LOGS_EXPORTER", Some("otlp")),
    ];

    checks.iter().all(|(key, expected_val)| {
        env.get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|v| expected_val.is_none_or(|exp| v == exp))
    })
}

/// Check if OTEL env vars are configured, using a loose localhost check
/// (for contexts where the exact daemon port is not available).
pub fn check_otel_config_loose(settings: &Value) -> bool {
    let Some(env) = settings.get("env").and_then(|e| e.as_object()) else {
        return false;
    };

    let endpoint_ok = env
        .get("OTEL_EXPORTER_OTLP_ENDPOINT")
        .and_then(|v| v.as_str())
        .is_some_and(|url| {
            let lower = url.to_lowercase();
            lower.contains("127.0.0.1") || lower.contains("localhost")
        });

    endpoint_ok
        && env
            .get("CLAUDE_CODE_ENABLE_TELEMETRY")
            .and_then(|v| v.as_str())
            == Some("1")
        && env
            .get("OTEL_EXPORTER_OTLP_PROTOCOL")
            .and_then(|v| v.as_str())
            == Some("http/json")
        && env.get("OTEL_METRICS_EXPORTER").and_then(|v| v.as_str()) == Some("otlp")
        && env.get("OTEL_LOGS_EXPORTER").and_then(|v| v.as_str()) == Some("otlp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_budi_hook_cmd_matches_variants() {
        assert!(is_budi_hook_cmd("budi hook"));
        assert!(is_budi_hook_cmd("budi hook 2>/dev/null || true"));
        assert!(is_budi_hook_cmd("  budi hook  "));
        assert!(!is_budi_hook_cmd("other command"));
        assert!(!is_budi_hook_cmd("budi hooks"));
    }

    #[test]
    fn is_budi_cc_hook_entry_detects_nested_format() {
        let entry = json!({
            "matcher": "",
            "hooks": [{"type": "command", "command": "budi hook 2>/dev/null || true"}]
        });
        assert!(is_budi_cc_hook_entry(&entry));

        let no_hook = json!({"matcher": "", "hooks": [{"command": "other"}]});
        assert!(!is_budi_cc_hook_entry(&no_hook));
    }

    #[test]
    fn is_budi_cursor_hook_entry_detects_flat_format() {
        let entry = json!({"command": "budi hook 2>/dev/null || true", "type": "command"});
        assert!(is_budi_cursor_hook_entry(&entry));

        let no_hook = json!({"command": "other", "type": "command"});
        assert!(!is_budi_cursor_hook_entry(&no_hook));
    }

    #[test]
    fn validate_cc_hooks_reports_missing_events() {
        let settings = json!({
            "hooks": {
                "SessionStart": [{"matcher": "", "hooks": [{"command": "budi hook"}]}],
                "SessionEnd": [{"matcher": "", "hooks": [{"command": "budi hook"}]}]
            }
        });
        let (ok, missing) = validate_cc_hooks(&settings);
        assert!(!ok);
        assert!(missing.contains(&"PostToolUse".to_string()));
        assert!(missing.contains(&"Stop".to_string()));
    }

    #[test]
    fn validate_cc_hooks_passes_when_all_events_present() {
        let hook_entry = json!({"matcher": "", "hooks": [{"command": "budi hook"}]});
        let mut hooks = serde_json::Map::new();
        for event in CC_HOOK_EVENTS {
            hooks.insert(event.to_string(), json!([hook_entry]));
        }
        let settings = json!({"hooks": hooks});
        let (ok, missing) = validate_cc_hooks(&settings);
        assert!(ok);
        assert!(missing.is_empty());
    }

    #[test]
    fn validate_cursor_hooks_reports_missing_events() {
        let config = json!({
            "hooks": {
                "sessionStart": [{"command": "budi hook", "type": "command"}]
            }
        });
        let (ok, missing) = validate_cursor_hooks(&config);
        assert!(!ok);
        assert!(missing.contains(&"sessionEnd".to_string()));
    }

    #[test]
    fn validate_cursor_hooks_passes_when_all_events_present() {
        let hook_entry = json!({"command": "budi hook", "type": "command"});
        let mut hooks = serde_json::Map::new();
        for event in CURSOR_HOOK_EVENTS {
            hooks.insert(event.to_string(), json!([hook_entry]));
        }
        let config = json!({"hooks": hooks});
        let (ok, missing) = validate_cursor_hooks(&config);
        assert!(ok);
        assert!(missing.is_empty());
    }

    #[test]
    fn validate_cc_hooks_handles_no_hooks_key() {
        let settings = json!({"env": {}});
        let (ok, missing) = validate_cc_hooks(&settings);
        assert!(!ok);
        assert_eq!(missing, vec!["no hooks key"]);
    }

    #[test]
    fn validate_cursor_hooks_handles_no_hooks_key() {
        let config = json!({"version": 1});
        let (ok, missing) = validate_cursor_hooks(&config);
        assert!(!ok);
        assert_eq!(missing, vec!["no hooks key"]);
    }

    #[test]
    fn check_mcp_config_detects_budi_server() {
        let settings = json!({
            "mcpServers": {
                "budi": {
                    "command": "/usr/local/bin/budi",
                    "args": ["mcp-serve"],
                    "type": "stdio"
                }
            }
        });
        assert!(check_mcp_config(&settings));
    }

    #[test]
    fn check_mcp_config_rejects_missing_args() {
        let settings = json!({
            "mcpServers": {
                "budi": {
                    "command": "/usr/local/bin/budi"
                }
            }
        });
        assert!(!check_mcp_config(&settings));
    }

    #[test]
    fn check_mcp_config_rejects_missing_server() {
        let settings = json!({"mcpServers": {}});
        assert!(!check_mcp_config(&settings));
    }

    #[test]
    fn check_otel_config_validates_all_vars() {
        let settings = json!({
            "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:9876",
                "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
                "OTEL_METRICS_EXPORTER": "otlp",
                "OTEL_LOGS_EXPORTER": "otlp"
            }
        });
        assert!(check_otel_config(&settings, 9876));
    }

    #[test]
    fn check_otel_config_rejects_wrong_port() {
        let settings = json!({
            "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:9876",
                "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
                "OTEL_METRICS_EXPORTER": "otlp",
                "OTEL_LOGS_EXPORTER": "otlp"
            }
        });
        assert!(!check_otel_config(&settings, 1234));
    }

    #[test]
    fn check_otel_config_rejects_missing_env() {
        let settings = json!({});
        assert!(!check_otel_config(&settings, 9876));
    }

    #[test]
    fn check_otel_config_loose_accepts_localhost() {
        let settings = json!({
            "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_EXPORTER_OTLP_ENDPOINT": "http://localhost:5555",
                "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
                "OTEL_METRICS_EXPORTER": "otlp",
                "OTEL_LOGS_EXPORTER": "otlp"
            }
        });
        assert!(check_otel_config_loose(&settings));
    }

    #[test]
    fn check_otel_config_loose_rejects_remote_endpoint() {
        let settings = json!({
            "env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_EXPORTER_OTLP_ENDPOINT": "http://remote-host:5555",
                "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
                "OTEL_METRICS_EXPORTER": "otlp",
                "OTEL_LOGS_EXPORTER": "otlp"
            }
        });
        assert!(!check_otel_config_loose(&settings));
    }
}
