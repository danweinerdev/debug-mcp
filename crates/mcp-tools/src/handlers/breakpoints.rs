//! Breakpoint handlers: `set_breakpoint`, `set_function_breakpoint`, `remove_breakpoint`,
//! `list_breakpoints` (Spec FR-7, task 5.3).
//!
//! Pending-vs-stopped: in `idle` the breakpoint is buffered (pending JSON, no DAP); in
//! `stopped` the backend is called and the matched response breakpoint recorded.
//! `list_breakpoints` has no guard and id-sorts (`[]`, never `null`).

use debugger_core::SourceBp;
use mcp_session::{BreakpointInfo, State};
use serde_json::{Map, Value};

use crate::errors;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

impl ToolServer {
    /// `set_breakpoint` (Spec FR-7.1). Guard idle|stopped. Idle → buffer pending; stopped →
    /// send the file's full list, select the matched breakpoint (exact line, else last),
    /// record it.
    pub(crate) async fn handle_set_breakpoint(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle, State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let file = match args.require_string("file") {
            Ok(f) => f,
            Err(e) => return ToolOutcome::error(e),
        };
        let line = match args.require_int("line") {
            Ok(l) => l,
            Err(e) => return ToolOutcome::error(e),
        };
        let condition = args.get_string("condition", "");

        // Idle: buffer as a pending breakpoint, no DAP.
        if self.session.state() == State::Idle {
            self.session
                .add_pending_source_breakpoint(&file, line, &condition);
            return ToolOutcome::Json(
                RespBuilder::new()
                    .set("status", "pending")
                    .set("file", file)
                    .set("line", line)
                    .set("condition", condition)
                    .set("message", "Breakpoint will be set when program is launched")
                    .build(),
            );
        }

        // Stopped: append to the file's tracked list, send the full list.
        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::SET_BREAKPOINTS.request_failed.to_string()),
        };
        self.session.add_source_breakpoint(&file, line, &condition);
        let bps = self.session.source_breakpoints_for_file(&file);

        let results = match backend.set_source_breakpoints(&file, &bps).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::SET_BREAKPOINTS.render(e)),
        };

        // Select the response breakpoint matching the requested line; else the last; else
        // error (no breakpoints).
        let matched = results
            .iter()
            .find(|bp| bp.line == line)
            .or_else(|| results.last());
        let matched = match matched {
            Some(bp) => bp,
            None => return ToolOutcome::error("setBreakpoints response contained no breakpoints"),
        };

        self.session.add_breakpoint_response(BreakpointInfo {
            id: matched.id,
            ty: "source".to_string(),
            file: file.clone(),
            line: matched.line,
            function: String::new(),
            condition,
            verified: matched.verified,
        });

        ToolOutcome::Json(
            RespBuilder::new()
                .set("breakpoint_id", matched.id)
                .set("verified", matched.verified)
                .set("file", file)
                .set("line", matched.line)
                .set_if(
                    !matched.message.is_empty(),
                    "message",
                    matched.message.clone(),
                )
                .build(),
        )
    }

    /// `set_function_breakpoint` (Spec FR-7.2). Guard idle|stopped. Idle → buffer pending;
    /// stopped → send the full function list, take the **last** response breakpoint, record
    /// it, synthesize the message when empty.
    pub(crate) async fn handle_set_function_breakpoint(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle, State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let name = match args.require_string("name") {
            Ok(n) => n,
            Err(e) => return ToolOutcome::error(e),
        };
        let condition = args.get_string("condition", "");

        if self.session.state() == State::Idle {
            self.session
                .add_pending_function_breakpoint(&name, &condition);
            return ToolOutcome::Json(
                RespBuilder::new()
                    .set("status", "pending")
                    .set("function", name)
                    .set("condition", condition)
                    .set(
                        "message",
                        "Function breakpoint will be set when program is launched",
                    )
                    .build(),
            );
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => {
                return ToolOutcome::error(
                    errors::SET_FUNCTION_BREAKPOINTS.request_failed.to_string(),
                )
            }
        };
        self.session.add_function_breakpoint(&name, &condition);
        let bps = self.session.all_function_breakpoints();

        let results = match backend.set_function_breakpoints(&bps).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::SET_FUNCTION_BREAKPOINTS.render(e)),
        };

        // The new breakpoint is the last in the response (positional with the request).
        let (id, verified, mut message) = match results.last() {
            Some(bp) => {
                self.session.add_breakpoint_response(BreakpointInfo {
                    id: bp.id,
                    ty: "function".to_string(),
                    file: String::new(),
                    line: 0,
                    function: name.clone(),
                    condition,
                    verified: bp.verified,
                });
                (bp.id, bp.verified, bp.message.clone())
            }
            None => (0, false, String::new()),
        };

        if message.is_empty() {
            message = if verified {
                format!("Breakpoint set on function '{name}'")
            } else {
                format!("Breakpoint on function '{name}' pending verification")
            };
        }

        ToolOutcome::Json(
            RespBuilder::new()
                .set("breakpoint_id", id)
                .set("verified", verified)
                .set("function", name)
                .set("message", message)
                .build(),
        )
    }

    /// `remove_breakpoint` (Spec FR-7.3). Guard stopped. Remove from session tracking, then
    /// re-send the remaining breakpoints (function list, or the file's source list).
    pub(crate) async fn handle_remove_breakpoint(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let id = match args.require_int("breakpoint_id") {
            Ok(i) => i,
            Err(e) => return ToolOutcome::error(e),
        };

        let (file_path, was_function) = match self.session.remove_breakpoint_by_id(id) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("failed to remove breakpoint: {e}")),
        };

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(remove_send_error(was_function)),
        };

        if was_function {
            let bps = self.session.all_function_breakpoints();
            if let Err(e) = backend.set_function_breakpoints(&bps).await {
                return ToolOutcome::error(errors::SET_FUNCTION_BREAKPOINTS.render(e));
            }
        } else {
            let bps: Vec<SourceBp> = self.session.source_breakpoints_for_file(&file_path);
            if let Err(e) = backend.set_source_breakpoints(&file_path, &bps).await {
                return ToolOutcome::error(errors::SET_BREAKPOINTS.render(e));
            }
        }

        ToolOutcome::Json(
            RespBuilder::new()
                .set("removed", true)
                .set("breakpoint_id", id)
                .build(),
        )
    }

    /// `list_breakpoints` (Spec FR-7.4). No guard, no DAP. Id-sorted; empty list serializes
    /// as `[]`, not `null`; each entry conditionally includes file/line/function/condition.
    pub(crate) fn handle_list_breakpoints(&self) -> ToolOutcome {
        let breakpoints = self.session.list_breakpoints();

        let items: Vec<Value> = breakpoints
            .iter()
            .map(|bp| {
                let mut entry = Map::new();
                entry.insert("id".to_string(), Value::from(bp.id));
                entry.insert("type".to_string(), Value::from(bp.ty.clone()));
                entry.insert("verified".to_string(), Value::from(bp.verified));
                if !bp.file.is_empty() {
                    entry.insert("file".to_string(), Value::from(bp.file.clone()));
                }
                if bp.line > 0 {
                    entry.insert("line".to_string(), Value::from(bp.line));
                }
                if !bp.function.is_empty() {
                    entry.insert("function".to_string(), Value::from(bp.function.clone()));
                }
                if !bp.condition.is_empty() {
                    entry.insert("condition".to_string(), Value::from(bp.condition.clone()));
                }
                Value::Object(entry)
            })
            .collect();

        let count = items.len();
        ToolOutcome::Json(
            RespBuilder::new()
                .set("breakpoints", Value::Array(items))
                .set("count", count)
                .build(),
        )
    }
}

/// The send-error wording for the missing-backend case in `remove_breakpoint` (matches the
/// op whose remaining list would have been re-sent).
fn remove_send_error(was_function: bool) -> String {
    if was_function {
        errors::SET_FUNCTION_BREAKPOINTS.request_failed.to_string()
    } else {
        errors::SET_BREAKPOINTS.request_failed.to_string()
    }
}
