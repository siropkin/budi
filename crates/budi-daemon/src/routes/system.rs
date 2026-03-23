use std::path::PathBuf;

use axum::Json;
use serde_json::json;

pub async fn system_integrations() -> Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(|| {
        let has_starship = std::process::Command::new("which")
            .arg("starship")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        let starship_config_path = std::env::var("STARSHIP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var("XDG_CONFIG_HOME")
                    .map(|x| PathBuf::from(x).join("starship.toml"))
                    .unwrap_or_else(|_| {
                        let home = std::env::var("HOME").unwrap_or_default();
                        PathBuf::from(home).join(".config/starship.toml")
                    })
            });

        let starship_configured = has_starship
            && std::fs::read_to_string(&starship_config_path)
                .unwrap_or_default()
                .contains("[custom.budi]");

        let home = std::env::var("HOME").unwrap_or_default();
        let claude_settings = PathBuf::from(&home).join(".claude/settings.json");
        let claude_statusline = std::fs::read_to_string(&claude_settings)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("statusLine")?
                    .get("command")?
                    .as_str()
                    .map(String::from)
            })
            .is_some_and(|cmd| cmd.contains("budi"));

        json!({
            "claude_code_statusline": claude_statusline,
            "starship": {
                "installed": has_starship,
                "configured": starship_configured,
            }
        })
    })
    .await
    .unwrap_or_else(|_| json!({}));

    Json(result)
}
