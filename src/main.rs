// ABOUTME: Main entry point for seren-acp-codex binary
// ABOUTME: Runs the ACP server on stdio

use seren_acp_codex::acp::run_acp_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_acp_server("stdio").await?;
    Ok(())
}
