//! DAP request builders (the `arguments` shapes for each command, task 3.3/3.4).
//!
//! Each builder returns a `dap_client::Request` with `type:"request"`, the command
//! name, and the raw-JSON `arguments` lldb-dap expects. The arg field names and the
//! initialize argument set reproduce what `internal/tools/*.go` assembles via go-dap
//! (`clientID="lldb-debug-mcp"`, `adapterID="lldb-dap"`, the breakpoint/step/memory
//! shapes). Breakpoint `condition` is omitted when empty (go-dap `omitempty`).

use dap_client::Request;
use debugger_core::{FunctionBp, SourceBp};
use serde_json::{json, Value};

/// The `initialize` request (Spec FR-4.4.8). `clientID` stays `"lldb-debug-mcp"` —
/// below the seam, lldb-dap-facing (Spec FR-1.1).
pub fn initialize() -> Request {
    Request::new(
        "initialize",
        Some(json!({
            "clientID": "lldb-debug-mcp",
            "adapterID": "lldb-dap",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "supportsVariableType": true,
            "supportsRunInTerminalRequest": false,
        })),
    )
}

/// The `launch` request carrying the pre-built lldb launch args object.
pub fn launch(args: Value) -> Request {
    Request::new("launch", Some(args))
}

/// The `attach` request carrying the pre-built lldb attach args object.
pub fn attach(args: Value) -> Request {
    Request::new("attach", Some(args))
}

/// `setBreakpoints` for one file with its full breakpoint list (Go
/// `SetBreakpointsArguments{Source{Path}, Breakpoints}`).
pub fn set_breakpoints(file: &str, bps: &[SourceBp]) -> Request {
    let breakpoints: Vec<Value> = bps
        .iter()
        .map(|b| {
            let mut m = serde_json::Map::new();
            m.insert("line".to_string(), b.line.into());
            if !b.condition.is_empty() {
                m.insert("condition".to_string(), b.condition.clone().into());
            }
            Value::Object(m)
        })
        .collect();
    Request::new(
        "setBreakpoints",
        Some(json!({
            "source": { "path": file },
            "breakpoints": breakpoints,
        })),
    )
}

/// `setFunctionBreakpoints` with the full function-breakpoint list (Go
/// `SetFunctionBreakpointsArguments{Breakpoints}`).
pub fn set_function_breakpoints(bps: &[FunctionBp]) -> Request {
    let breakpoints: Vec<Value> = bps
        .iter()
        .map(|b| {
            let mut m = serde_json::Map::new();
            m.insert("name".to_string(), b.name.clone().into());
            if !b.condition.is_empty() {
                m.insert("condition".to_string(), b.condition.clone().into());
            }
            Value::Object(m)
        })
        .collect();
    Request::new(
        "setFunctionBreakpoints",
        Some(json!({ "breakpoints": breakpoints })),
    )
}

/// `setExceptionBreakpoints` with empty filters (Spec FR-4.4.11 — sent even when there
/// are none).
pub fn set_exception_breakpoints() -> Request {
    Request::new("setExceptionBreakpoints", Some(json!({ "filters": [] })))
}

/// `configurationDone` (no arguments).
pub fn configuration_done() -> Request {
    Request::new("configurationDone", None)
}

/// `continue` for a thread (Go `ContinueArguments{ThreadId}`).
pub fn cont(thread_id: i64) -> Request {
    Request::new("continue", Some(json!({ "threadId": thread_id })))
}

/// A step request (`next`/`stepIn`/`stepOut`) for a thread, with an optional
/// granularity applied only when present (Go's "set granularity only when non-empty").
pub fn step(command: &str, thread_id: i64, granularity: Option<&str>) -> Request {
    let mut args = serde_json::Map::new();
    args.insert("threadId".to_string(), thread_id.into());
    if let Some(g) = granularity {
        args.insert("granularity".to_string(), g.into());
    }
    Request::new(command, Some(Value::Object(args)))
}

/// `pause` for all threads (Go uses thread id `0`).
pub fn pause() -> Request {
    Request::new("pause", Some(json!({ "threadId": 0 })))
}

/// `threads` (no arguments).
pub fn threads() -> Request {
    Request::new("threads", None)
}

/// `stackTrace` (Go `StackTraceArguments{ThreadId, StartFrame, Levels}`).
pub fn stack_trace(thread_id: i64, start: i64, levels: i64) -> Request {
    Request::new(
        "stackTrace",
        Some(json!({
            "threadId": thread_id,
            "startFrame": start,
            "levels": levels,
        })),
    )
}

/// `scopes` (Go `ScopesArguments{FrameId}`).
pub fn scopes(frame_id: i64) -> Request {
    Request::new("scopes", Some(json!({ "frameId": frame_id })))
}

/// `variables` (Go `VariablesArguments{VariablesReference}`).
pub fn variables(variables_reference: i64) -> Request {
    Request::new(
        "variables",
        Some(json!({ "variablesReference": variables_reference })),
    )
}

/// `evaluate`. `frame_id` is included only when present (the repl path sends no frame);
/// `context` is `"variables"` or `"repl"`.
pub fn evaluate(expression: &str, frame_id: Option<i64>, context: &str) -> Request {
    let mut args = serde_json::Map::new();
    args.insert("expression".to_string(), expression.into());
    if let Some(fid) = frame_id {
        args.insert("frameId".to_string(), fid.into());
    }
    args.insert("context".to_string(), context.into());
    Request::new("evaluate", Some(Value::Object(args)))
}

/// `readMemory` (Go `ReadMemoryArguments{MemoryReference, Count}`).
pub fn read_memory(address: &str, count: i64) -> Request {
    Request::new(
        "readMemory",
        Some(json!({ "memoryReference": address, "count": count })),
    )
}

/// `disassemble` (Go `DisassembleArguments{MemoryReference, InstructionCount}`).
pub fn disassemble(address: &str, count: i64) -> Request {
    Request::new(
        "disassemble",
        Some(json!({ "memoryReference": address, "instructionCount": count })),
    )
}

/// `disconnect` (Go `DisconnectArguments{TerminateDebuggee}`).
pub fn disconnect(terminate: bool) -> Request {
    Request::new(
        "disconnect",
        Some(json!({ "terminateDebuggee": terminate })),
    )
}
