use anyhow::Result;
use rmcp::ServiceExt;

/// Run the budi MCP server over stdio.
/// stdout is reserved for JSON-RPC — all logging goes to stderr.
pub async fn run_mcp_server() -> Result<()> {
    let server = crate::mcp::BudiMcpServer::new();
    let service = server
        .serve(rmcp::transport::io::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server failed to start: {e}"))?;
    service.waiting().await?;
    Ok(())
}
