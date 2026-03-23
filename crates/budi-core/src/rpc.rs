use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequest {
    pub repo_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub daemon_version: String,
    pub repo_root: String,
    pub hooks_detected: bool,
}
