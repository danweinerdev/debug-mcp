//! Breakpoint handlers: `set_breakpoint`, `set_function_breakpoint`, `remove_breakpoint`,
//! `list_breakpoints` (Spec FR-7, task 5.3).
//!
//! Pending-vs-stopped: in `idle` the breakpoint is buffered (pending JSON, no DAP); in
//! `stopped` the backend is called and the matched response breakpoint recorded.
//! `list_breakpoints` has no guard and id-sorts (`[]`, never `null`).
//!
//! The stopped-path mutations are **transactional** (review finding 4): each handler builds
//! the proposed breakpoint list locally, sends it to DAP, and commits the session mutation
//! only after the DAP call succeeds — so a backend rejection (or a no-breakpoints response)
//! leaves the tracked lists unchanged. The success-path behavior is identical to the prior
//! mutate-then-send order; only the failure path differs (robustness improvement).

use debugger_core::{FunctionBp, SourceBp};
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
        // `line` must be a positive integer after truncation (Rust numeric-validation
        // policy — rejects zero/negative/fractional-to-zero lines at the boundary).
        let line = match args.require_positive_int("line") {
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

        // Stopped: build the proposed list locally (current tracked + the new bp) and send
        // it WITHOUT mutating the session first. The session is committed only after DAP
        // confirms, so a backend rejection leaves the tracked list unchanged (transactional
        // — review finding 4; happy-path behavior is identical to mutate-then-send).
        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::SET_BREAKPOINTS.request_failed.to_string()),
        };
        let mut bps = self.session.source_breakpoints_for_file(&file);
        bps.push(SourceBp {
            line,
            condition: condition.clone(),
        });

        let results = match backend.set_source_breakpoints(&file, &bps).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::SET_BREAKPOINTS.render(e)),
        };

        // Select the response breakpoint matching the requested line; else the last; else
        // error (no breakpoints). Done before committing, so a no-breakpoints response also
        // leaves the session unchanged.
        let matched = results
            .iter()
            .find(|bp| bp.line == line)
            .or_else(|| results.last());
        let matched = match matched {
            Some(bp) => bp,
            None => return ToolOutcome::error("setBreakpoints response contained no breakpoints"),
        };

        // DAP succeeded — commit the session mutation now.
        self.session.add_source_breakpoint(&file, line, &condition);
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

        // Build the proposed full function list locally (current tracked + the new one) and
        // send it WITHOUT mutating the session first; commit only after DAP success
        // (transactional — review finding 4).
        let backend = match self.current_backend().await {
            Some(b) => b,
            None => {
                return ToolOutcome::error(
                    errors::SET_FUNCTION_BREAKPOINTS.request_failed.to_string(),
                )
            }
        };
        let mut bps = self.session.all_function_breakpoints();
        bps.push(FunctionBp {
            name: name.clone(),
            condition: condition.clone(),
        });

        let results = match backend.set_function_breakpoints(&bps).await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::SET_FUNCTION_BREAKPOINTS.render(e)),
        };

        // DAP succeeded — commit the session mutation. The new breakpoint is the last in
        // the response (positional with the request).
        self.session.add_function_breakpoint(&name, &condition);
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

    /// `remove_breakpoint` (Spec FR-7.3). Guard stopped. Peek the tracked breakpoint, send
    /// the proposed remaining list (function list, or the file's source list), and commit
    /// the session removal only on DAP success (transactional — review finding 4).
    pub(crate) async fn handle_remove_breakpoint(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let id = match args.require_int("breakpoint_id") {
            Ok(i) => i,
            Err(e) => return ToolOutcome::error(e),
        };

        // Peek (read-only) at the tracked breakpoint, then compute the proposed remaining
        // list and send it to DAP BEFORE committing the session removal. The session is
        // mutated only after DAP confirms, so a backend rejection leaves the breakpoint
        // tracked (transactional — review finding 4). Removal matching mirrors
        // `remove_breakpoint_by_id`: source by line, function by name, first match.
        let info = match self.session.breakpoint_info(id) {
            Some(info) => info,
            None => {
                return ToolOutcome::error(format!(
                    "failed to remove breakpoint: breakpoint ID {id} not found"
                ))
            }
        };
        let was_function = info.ty == "function";

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(remove_send_error(was_function)),
        };

        if was_function {
            let bps =
                remaining_function_breakpoints(&self.session.all_function_breakpoints(), &info);
            if let Err(e) = backend.set_function_breakpoints(&bps).await {
                return ToolOutcome::error(errors::SET_FUNCTION_BREAKPOINTS.render(e));
            }
        } else {
            let bps = remaining_source_breakpoints(
                &self.session.source_breakpoints_for_file(&info.file),
                &info,
            );
            if let Err(e) = backend.set_source_breakpoints(&info.file, &bps).await {
                return ToolOutcome::error(errors::SET_BREAKPOINTS.render(e));
            }
        }

        // DAP succeeded — now commit the session removal.
        if let Err(e) = self.session.remove_breakpoint_by_id(id) {
            return ToolOutcome::error(format!("failed to remove breakpoint: {e}"));
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

/// The proposed source-breakpoint list for a file after removing `info` — the current list
/// with the **first** entry whose line matches dropped (mirrors `remove_breakpoint_by_id`'s
/// line-only, first-match rule). Computed without mutating the session, for the
/// transactional remove path.
fn remaining_source_breakpoints(current: &[SourceBp], info: &BreakpointInfo) -> Vec<SourceBp> {
    let mut bps = current.to_vec();
    if let Some(pos) = bps.iter().position(|bp| bp.line == info.line) {
        bps.remove(pos);
    }
    bps
}

/// The proposed function-breakpoint list after removing `info` — the current list with the
/// **first** entry whose name matches dropped (function breakpoints are matched by name
/// only). Computed without mutating the session.
fn remaining_function_breakpoints(
    current: &[FunctionBp],
    info: &BreakpointInfo,
) -> Vec<FunctionBp> {
    let mut bps = current.to_vec();
    if let Some(pos) = bps.iter().position(|bp| bp.name == info.function) {
        bps.remove(pos);
    }
    bps
}
