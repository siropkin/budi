use std::env;

use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::ServerCapabilities,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7878";

// ── Tool input schemas ──────────────────────────────────────────────────────

/// Search the indexed codebase.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchInput {
    /// The user's question or prompt to search the codebase for
    pub prompt: String,
    /// Absolute path to the repo root (defaults to cwd if omitted)
    #[serde(default)]
    pub repo_root: Option<String>,
}

/// Check repository index status.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusInput {
    /// Absolute path to the repo root
    pub repo_root: String,
}

// ── Daemon response types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DaemonQueryResponse {
    context: String,
    #[serde(default)]
    snippets: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct DaemonQueryRequest {
    repo_root: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct DaemonStatusResponse {
    #[serde(default)]
    indexed_chunks: Option<u64>,
    #[serde(default)]
    embedded_chunks: Option<u64>,
}

// ── MCP Server ──────────────────────────────────────────────────────────────

/// Budi MCP server — exposes local RAG retrieval over stdio transport.
#[derive(Debug, Clone)]
pub struct BudiMcpServer {
    /// Tool router for MCP tool dispatch.
    tool_router: ToolRouter<Self>,
    /// Base URL of the budi daemon.
    daemon_url: String,
    /// HTTP client for daemon communication.
    client: reqwest::Client,
}

impl BudiMcpServer {
    /// Create a new server with the given daemon URL.
    pub fn new(daemon_url: String) -> Self {
        Self {
            tool_router: Self::tool_router(),
            daemon_url,
            client: reqwest::Client::new(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BudiMcpServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        let mut info = rmcp::model::ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Budi is a local RAG retrieval system for codebases. \
             Use budi_search to find relevant code for any question about the repository."
                .to_string(),
        );
        info
    }
}

#[tool_router(router = tool_router)]
impl BudiMcpServer {
    /// Search the indexed codebase for code relevant to a question.
    /// Returns evidence cards with file paths, line ranges, and proof lines.
    #[tool(
        name = "budi_search",
        description = "Search the indexed codebase for code relevant to a question. Returns evidence cards with file paths, line ranges, and proof lines."
    )]
    async fn search(&self, input: Parameters<SearchInput>) -> String {
        let Parameters(input) = input;
        let repo_root = input.repo_root.unwrap_or_else(|| {
            env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

        let req = DaemonQueryRequest {
            repo_root,
            prompt: input.prompt,
        };

        match self
            .client
            .post(format!("{}/query", self.daemon_url))
            .json(&req)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<DaemonQueryResponse>().await {
                    Ok(data) => {
                        if data.context.is_empty() || data.snippets.is_empty() {
                            "No relevant code found in the index for this query.".to_string()
                        } else {
                            data.context
                        }
                    }
                    Err(e) => format!("Failed to parse daemon response: {e}"),
                }
            }
            Ok(resp) => format!("Daemon returned error: HTTP {}", resp.status()),
            Err(e) => format!(
                "Could not reach budi daemon at {}. Is it running? Error: {e}",
                self.daemon_url
            ),
        }
    }

    /// Check if a repository is indexed and ready for search.
    #[tool(
        name = "budi_status",
        description = "Check if a repository is indexed and ready for search."
    )]
    async fn status(&self, input: Parameters<StatusInput>) -> String {
        let Parameters(input) = input;

        #[derive(Serialize)]
        struct Req {
            repo_root: String,
        }
        let req = Req {
            repo_root: input.repo_root.clone(),
        };

        match self
            .client
            .post(format!("{}/status", self.daemon_url))
            .json(&req)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<DaemonStatusResponse>().await {
                    Ok(data) => {
                        let chunks = data.indexed_chunks.unwrap_or(0);
                        let embedded = data.embedded_chunks.unwrap_or(0);
                        if chunks == 0 {
                            format!(
                                "Repository {} is not indexed. Run `budi init --index` in the repo.",
                                input.repo_root
                            )
                        } else {
                            format!(
                                "Repository {} is indexed: {chunks} chunks, {embedded} embedded.",
                                input.repo_root
                            )
                        }
                    }
                    Err(e) => format!("Failed to parse status response: {e}"),
                }
            }
            Ok(resp) => format!("Daemon returned error: HTTP {}", resp.status()),
            Err(e) => format!(
                "Could not reach budi daemon at {}. Is it running? Error: {e}",
                self.daemon_url
            ),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Handle --version before anything else (no tracing, no tokio needed).
    if env::args().any(|a| a == "--version") {
        println!("budi-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let daemon_url = env::var("BUDI_DAEMON_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string());

    let server = BudiMcpServer::new(daemon_url);

    tracing::info!("Starting budi MCP server (stdio transport)");

    let transport = rmcp::transport::io::stdio();
    let server = server.serve(transport).await?;
    server.waiting().await?;

    Ok(())
}
