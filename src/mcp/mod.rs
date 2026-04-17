//! MCP server: rmcp stdio JSON-RPC 2.0 transport exposing the
//! `textlog__*` tool family. Spawned by Claude Code via
//! `claude mcp add textlog -- tl mcp`.

use std::sync::Arc;

use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use crate::error::{Error, Result};
use crate::storage::Storage;

pub mod schema;
pub mod tools;

pub use tools::McpServer;

/// Run the MCP server on stdin/stdout until the peer disconnects.
/// Used by `tl mcp`.
pub async fn run_stdio(storage: Arc<Storage>) -> Result<()> {
    let server = McpServer::new(storage);
    let running = server
        .serve(stdio())
        .await
        .map_err(|e| Error::Mcp(format!("stdio serve init failed: {e}")))?;
    running
        .waiting()
        .await
        .map_err(|e| Error::Mcp(format!("stdio serve loop failed: {e}")))?;
    Ok(())
}
