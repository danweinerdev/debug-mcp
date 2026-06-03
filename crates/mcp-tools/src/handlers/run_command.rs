//! `run_command` handler (Spec FR-14, task 5.4) — the LLDB escape hatch.
//!
//! Guard stopped → pass the **raw** command to `evaluate(cmd, None, Repl)` (the backend
//! owns the backtick-prefix decision and `context="repl"`) → `{result,type}`. Unlike
//! `evaluate`, `variables_reference` is **discarded** (no `has_children` key).

use debugger_core::EvalMode;
use mcp_session::State;

use crate::errors;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

impl ToolServer {
    /// `run_command` (Spec FR-14). Sends the command via `EvalMode::Repl`; discards the
    /// variables reference.
    pub(crate) async fn handle_run_command(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let command = match args.require_string("command") {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(e),
        };

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::RUN_COMMAND.request_failed.to_string()),
        };

        // The handler passes the raw command; the backend prepends a backtick iff it does
        // not support command repl mode, and sends context="repl".
        let result = match backend.evaluate(&command, None, EvalMode::Repl).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::RUN_COMMAND.render(e)),
        };

        // {result, type} only — no has_children / variables_reference (Spec FR-14.3).
        ToolOutcome::Json(
            RespBuilder::new()
                .set("result", result.result)
                .set("type", result.ty)
                .build(),
        )
    }
}
