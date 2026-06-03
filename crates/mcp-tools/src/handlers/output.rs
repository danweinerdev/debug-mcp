//! `read_output` handler (Spec FR-12.4, task 5.4).
//!
//! Guard any-state-but-idle → drain the output buffer → format the entries (FR-12.5). The
//! drain surfaces the `[output truncated]` marker and clears the buffer (idempotent).

use mcp_session::State;

use crate::format::format_output_entries;
use crate::response::ToolOutcome;
use crate::server::ToolServer;

impl ToolServer {
    /// `read_output` (Spec FR-12.4). No live DAP — drains the cached output buffer.
    pub(crate) fn handle_read_output(&self) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[
            State::Configuring,
            State::Stopped,
            State::Running,
            State::Terminated,
        ]) {
            return ToolOutcome::error(e);
        }

        let entries = self.session.output_buffer().drain();
        // format_output_entries returns a JSON object; the marshal-error path
        // (`failed to marshal output: <err>`) is effectively unreachable with serde.
        ToolOutcome::Json(format_output_entries(&entries))
    }
}
