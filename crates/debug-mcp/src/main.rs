//! `debug-mcp` — the published binary (task 5.5, Spec FR-1).
//!
//! Constructs the neutral [`SessionManager`] and a [`BackendRegistry`], wires them into the
//! [`ToolServer`], and serves the MCP tools over stdio via rmcp. **Backend registration is
//! platform-exclusive:** the lldb backend (lldb-dap) is registered on macOS/Linux only; the
//! WinDbg backend (DbgEng) is registered on Windows only. The runtime `backend`-arg switcher
//! is retained so lldb-on-Windows can be added later (it would register `LldbFactory` under
//! `cfg(windows)` too). No factory is invoked at startup — the backend is spawned lazily on
//! the first connect (Spec FR-1.6). On a fatal server error the process prints
//! `Server error: <e>` to stderr and exits with code 1 (Spec FR-1.4).
//!
//! No `unsafe`: all COM/FFI `unsafe` is confined to `dbgeng-sys` (design Decision 1).
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

#[cfg(not(windows))]
use lldb_backend::LldbFactory;
use mcp_session::SessionManager;
use mcp_tools::{default_backend_for_os, BackendRegistry, ToolServer};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
#[cfg(windows)]
use windbg_backend::WinDbgFactory;

#[tokio::main]
async fn main() -> ExitCode {
    let session = Arc::new(SessionManager::new());

    // Platform-exclusive backend registration. The per-OS default (`windbg` on Windows, `lldb`
    // elsewhere) is set on the registry; only the native backend is registered. (Bound without
    // `mut` on platforms with no registration so there is no unused-`mut` warning.)
    #[cfg(not(windows))]
    let registry = {
        let mut registry = BackendRegistry::new(default_backend_for_os());
        registry.register(Arc::new(LldbFactory::new()));
        registry
    };
    #[cfg(windows)]
    let registry = {
        let mut registry = BackendRegistry::new(default_backend_for_os());
        registry.register(Arc::new(WinDbgFactory::new()));
        registry
    };

    let server = ToolServer::new(session, registry);

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
