//! `debug-mcp` — the published binary (task 5.5, Spec FR-1).
//!
//! Constructs the neutral [`SessionManager`] and the lldb [`LldbFactory`], wires them into
//! the [`ToolServer`], and serves the 21 MCP tools over stdio via rmcp. The factory is
//! **not** invoked at startup — lldb-dap is spawned lazily on the first `launch`/`attach`
//! (Spec FR-1.6). On a fatal server error the process prints `Server error: <e>` to stderr
//! and exits with code 1 (Spec FR-1.4).

use std::process::ExitCode;
use std::sync::Arc;

use lldb_backend::LldbFactory;
use mcp_session::SessionManager;
use mcp_tools::ToolServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> ExitCode {
    let session = Arc::new(SessionManager::new());
    let factory = Arc::new(LldbFactory::new());
    let server = ToolServer::new(session, factory);

    // serve over stdio; on a fatal error print to stderr + exit 1 (Go main.go:25-26).
    let running = match server.serve(stdio()).await {
        Ok(running) => running,
        Err(e) => {
            eprintln!("Server error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = running.waiting().await {
        eprintln!("Server error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
