use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct HookInputCommon {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub permission_mode: String,
    pub hook_event_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserPromptSubmitInput {
    #[serde(flatten)]
    pub common: HookInputCommon,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostToolUseInput {
    #[serde(flatten)]
    pub common: HookInputCommon,
    pub tool_name: String,
    pub tool_input: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserPromptSubmitOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

impl UserPromptSubmitOutput {
    pub fn allow_with_context(context: String) -> Self {
        Self {
            decision: None,
            reason: None,
            hook_specific_output: HookSpecificOutput {
                hook_event_name: "UserPromptSubmit".to_string(),
                additional_context: context,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AsyncSystemMessageOutput {
    #[serde(rename = "systemMessage")]
    pub system_message: String,
}
