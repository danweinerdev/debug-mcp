//! The `ToolServer` — shared state for the 21 handlers plus the rmcp `ServerHandler`
//! wiring (Spec FR-1, design §"MCP tool surface", Decisions 3/7).
//!
//! It owns `Arc<SessionManager>`, the `Arc<dyn BackendFactory>` (lazily `connect()`-ed on
//! the first `launch`/`attach`, never at startup — Spec FR-1.6), and the *connected*
//! `Arc<dyn DebuggerBackend>` for the active session (set on connect, cleared on
//! disconnect). The backend slot is a `tokio::sync::RwLock` so handlers can read it across
//! an `.await` without ever holding the session lock there (Decision 7) — this is what
//! lets a concurrent `pause` interrupt a blocked `continue`.
//!
//! Dispatch is by tool name; the input schemas are hand-built (Decision 3 / R2) and
//! handlers parse the raw arguments object via [`Args`](crate::Args). rmcp runs each
//! `tools/call` on its own task with a per-request `CancellationToken` (R1 resolved: see
//! the module-level note), so no spawn-and-await workaround is needed.

use std::sync::Arc;

use debugger_core::{BackendFactory, DebuggerBackend};
use mcp_session::SessionManager;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolsCapability,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use serde_json::Map;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::response::ToolOutcome;
use crate::schema;
use crate::Args;

/// The shared state every handler operates on: the neutral session, the backend factory,
/// and the currently-connected backend (if any).
pub struct ToolServer {
    pub(crate) session: Arc<SessionManager>,
    pub(crate) factory: Arc<dyn BackendFactory>,
    pub(crate) backend: RwLock<Option<Arc<dyn DebuggerBackend>>>,
}

impl ToolServer {
    /// Build the server over a session and a backend factory. The factory is **not**
    /// invoked here (lazy spawn, Spec FR-1.6).
    pub fn new(session: Arc<SessionManager>, factory: Arc<dyn BackendFactory>) -> Self {
        ToolServer {
            session,
            factory,
            backend: RwLock::new(None),
        }
    }

    /// A clone of the connected backend, or `None` when no session is active. Held only
    /// briefly (never across the session lock), so a concurrent `pause` can read it while
    /// a `continue` is blocked awaiting its backend call.
    pub(crate) async fn current_backend(&self) -> Option<Arc<dyn DebuggerBackend>> {
        self.backend.read().await.clone()
    }

    /// Store the connected backend (set during launch/attach connect).
    pub(crate) async fn set_backend(&self, backend: Arc<dyn DebuggerBackend>) {
        *self.backend.write().await = Some(backend);
    }

    /// Drop the connected backend (disconnect / connect-failure cleanup). Dropping the
    /// last `Arc` tears down the subprocess and ends the event-pump stream.
    pub(crate) async fn clear_backend(&self) {
        *self.backend.write().await = None;
    }

    /// Dispatch a parsed tool call to its handler. Returns the neutral [`ToolOutcome`];
    /// the rmcp glue ([`ToolServer::call_tool`]) maps it to a `CallToolResult`. An unknown
    /// tool name yields an error outcome (rmcp validates names up front, so this is a
    /// belt-and-suspenders guard).
    async fn dispatch(&self, name: &str, args: &Args<'_>, ct: &CancellationToken) -> ToolOutcome {
        match name {
            "launch" => self.handle_launch(args, ct).await,
            "attach" => self.handle_attach(args, ct).await,
            "disconnect" => self.handle_disconnect(args).await,
            "set_breakpoint" => self.handle_set_breakpoint(args).await,
            "set_function_breakpoint" => self.handle_set_function_breakpoint(args).await,
            "remove_breakpoint" => self.handle_remove_breakpoint(args).await,
            "list_breakpoints" => self.handle_list_breakpoints(),
            "continue" => self.handle_continue(args, ct).await,
            "step_over" => self.handle_step_over(args, ct).await,
            "step_into" => self.handle_step_into(args, ct).await,
            "step_out" => self.handle_step_out(args, ct).await,
            "pause" => self.handle_pause(args).await,
            "status" => self.handle_status(),
            "backtrace" => self.handle_backtrace(args).await,
            "threads" => self.handle_threads(args).await,
            "variables" => self.handle_variables(args).await,
            "evaluate" => self.handle_evaluate(args).await,
            "read_output" => self.handle_read_output(),
            "read_memory" => self.handle_read_memory(args).await,
            "disassemble" => self.handle_disassemble(args).await,
            "run_command" => self.handle_run_command(args).await,
            other => ToolOutcome::error(format!("unknown tool: {other}")),
        }
    }

    /// In-process tool invocation, bypassing the rmcp transport: dispatch `name` over the
    /// argument map and return the neutral [`ToolOutcome`]. This is the integration-test
    /// driver analog of calling Go's `tools.handleX(ctx, req)` directly — it exercises the
    /// real handlers, session, and (live or fake) backend without an stdio round-trip.
    /// `ct` carries cancellation/timeout exactly as the rmcp `call_tool` path does.
    pub async fn call(
        &self,
        name: &str,
        args: &Map<String, serde_json::Value>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        let args = Args::new(args);
        self.dispatch(name, &args, ct).await
    }

    /// The active session manager (so an in-process integration driver can assert state and
    /// read the recorded subprocess pid — e.g. to kill lldb-dap in a crash-recovery test).
    pub fn session(&self) -> &Arc<SessionManager> {
        &self.session
    }

    /// The 21 tool definitions (verbatim names/descriptions + hand-built schemas).
    pub fn tools() -> Vec<Tool> {
        schema::all_tools()
    }
}

/// Map a neutral [`ToolOutcome`] to an rmcp `CallToolResult` (design §"MCP tool surface"):
/// `Json` → success, one text content holding the serialized JSON object (Go
/// `NewToolResultText` over a marshaled map); `Text` → success, plain text (the
/// `Program exited during launch` / `Process exited during attach` early-exits); `Error`
/// → an error result (`is_error=true`, Go `NewToolResultError`).
fn outcome_to_result(outcome: ToolOutcome) -> CallToolResult {
    match outcome {
        ToolOutcome::Json(value) => {
            // serde_json::to_string over an object never fails; fall back defensively.
            let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
            CallToolResult::success(vec![Content::text(text)])
        }
        ToolOutcome::Text(text) => CallToolResult::success(vec![Content::text(text)]),
        ToolOutcome::Error(message) => CallToolResult::error(vec![Content::text(message)]),
    }
}

impl ServerHandler for ToolServer {
    fn get_info(&self) -> ServerInfo {
        // WithToolCapabilities(false): advertise tools with listChanged=false.
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(ToolsCapability {
            list_changed: Some(false),
        });

        let mut info = ServerInfo::default();
        info.capabilities = capabilities;
        // Server name "debug" (Spec FR-1.1, intentional rename), version "1.0.0".
        info.server_info = Implementation::new("debug", "1.0.0");
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: Self::tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Domain errors are NEVER protocol errors (Spec FR-3.2): everything is a
        // CallToolResult, error or not. The transport `Result` stays `Ok`.
        let empty = Map::new();
        let arg_map = request.arguments.as_ref().unwrap_or(&empty);
        let args = Args::new(arg_map);
        let outcome = self.dispatch(&request.name, &args, &context.ct).await;
        Ok(outcome_to_result(outcome))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tools().into_iter().find(|t| t.name == name)
    }
}
