//! Typed decode of DAP response bodies → neutral [`debugger_core`] types (task 3.4).
//!
//! `dap-client` keeps each response `body` as raw JSON (it is debugger-agnostic); this
//! module owns the per-command decode and the DAP→neutral translation. Field names are
//! the standard DAP wire names go-dap produces (confirmed against a live lldb-dap
//! session) — `stackFrames`/`totalFrames`, `instructionPointerReference`,
//! `namedVariables`/`indexedVariables`, `instructionBytes`, `location.path`, etc.
//!
//! The translation is opaque pass-through (Spec OQ-2/FR-18.6): reason strings, IP
//! references, and child counts cross the seam unchanged; no hex-dump/flatten/JSON
//! shaping happens here (that is Phase 5).

use base64::Engine;
use debugger_core::{
    BackendError, BreakpointResult, EvalResult, Frame, Instruction, MemoryRead, Scope, ThreadInfo,
    Variable,
};
use serde::Deserialize;
use serde_json::Value;

/// A DAP source reference (`{path}`) carried on frames/instructions.
#[derive(Debug, Clone, Deserialize, Default)]
struct DapSource {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DapStackFrame {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    source: Option<DapSource>,
    #[serde(default)]
    line: i64,
    #[serde(rename = "instructionPointerReference", default)]
    instruction_pointer_reference: String,
}

#[derive(Debug, Clone, Deserialize)]
struct StackTraceBody {
    #[serde(rename = "stackFrames", default)]
    stack_frames: Vec<DapStackFrame>,
    #[serde(rename = "totalFrames", default)]
    total_frames: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct DapThread {
    id: i64,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ThreadsBody {
    #[serde(default)]
    threads: Vec<DapThread>,
}

#[derive(Debug, Clone, Deserialize)]
struct DapScope {
    #[serde(default)]
    name: String,
    #[serde(rename = "variablesReference", default)]
    variables_reference: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ScopesBody {
    #[serde(default)]
    scopes: Vec<DapScope>,
}

#[derive(Debug, Clone, Deserialize)]
struct DapVariable {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(rename = "variablesReference", default)]
    variables_reference: i64,
    #[serde(rename = "namedVariables", default)]
    named_variables: i64,
    #[serde(rename = "indexedVariables", default)]
    indexed_variables: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct VariablesBody {
    #[serde(default)]
    variables: Vec<DapVariable>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct EvaluateBody {
    #[serde(default)]
    result: String,
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(rename = "variablesReference", default)]
    variables_reference: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ReadMemoryBody {
    #[serde(default)]
    address: String,
    #[serde(default)]
    data: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DapInstruction {
    #[serde(default)]
    address: String,
    #[serde(default)]
    instruction: String,
    #[serde(rename = "instructionBytes", default)]
    instruction_bytes: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    location: Option<DapSource>,
    #[serde(default)]
    line: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct DisassembleBody {
    #[serde(default)]
    instructions: Vec<DapInstruction>,
}

#[derive(Debug, Clone, Deserialize)]
struct DapBreakpoint {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    line: i64,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SetBreakpointsBody {
    #[serde(default)]
    breakpoints: Vec<DapBreakpoint>,
}

/// Decode a response `body` into `T`, mapping a decode failure to
/// [`BackendError::Dap`]. A missing body decodes as `T::default()` for the bodies that
/// derive `Default` (evaluate/read-memory), and as an empty list for the collection
/// bodies (handled by `serde(default)` on the fields). `null`/absent → `{}` here.
fn decode<T: for<'de> Deserialize<'de>>(body: &Option<Value>) -> Result<T, BackendError> {
    let value = body.clone().unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(value).map_err(|e| BackendError::Dap {
        message: format!("malformed response body: {e}"),
    })
}

/// `(frames, total_frames)` from a `stackTrace` response body.
pub fn stack_trace(body: &Option<Value>) -> Result<(Vec<Frame>, i64), BackendError> {
    let parsed: StackTraceBody = decode(body)?;
    let frames = parsed
        .stack_frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let source_path = f.source.map(|s| s.path).filter(|p| !p.is_empty());
            let instruction_pointer = (!f.instruction_pointer_reference.is_empty())
                .then_some(f.instruction_pointer_reference);
            Frame {
                index: i as i64,
                id: f.id,
                name: f.name,
                source_path,
                line: f.line,
                instruction_pointer,
            }
        })
        .collect();
    Ok((frames, parsed.total_frames))
}

/// Thread list from a `threads` response body.
pub fn threads(body: &Option<Value>) -> Result<Vec<ThreadInfo>, BackendError> {
    let parsed: ThreadsBody = decode(body)?;
    Ok(parsed
        .threads
        .into_iter()
        .map(|t| ThreadInfo {
            id: t.id,
            name: t.name,
        })
        .collect())
}

/// Scope list from a `scopes` response body.
pub fn scopes(body: &Option<Value>) -> Result<Vec<Scope>, BackendError> {
    let parsed: ScopesBody = decode(body)?;
    Ok(parsed
        .scopes
        .into_iter()
        .map(|s| Scope {
            name: s.name,
            variables_reference: s.variables_reference,
        })
        .collect())
}

/// Variable list from a `variables` response body.
pub fn variables(body: &Option<Value>) -> Result<Vec<Variable>, BackendError> {
    let parsed: VariablesBody = decode(body)?;
    Ok(parsed
        .variables
        .into_iter()
        .map(|v| Variable {
            name: v.name,
            value: v.value,
            ty: v.ty,
            variables_reference: v.variables_reference,
            named: v.named_variables,
            indexed: v.indexed_variables,
        })
        .collect())
}

/// Eval result from an `evaluate` response body.
pub fn evaluate(body: &Option<Value>) -> Result<EvalResult, BackendError> {
    let parsed: EvaluateBody = decode(body)?;
    Ok(EvalResult {
        result: parsed.result,
        ty: parsed.ty,
        variables_reference: parsed.variables_reference,
    })
}

/// Memory read from a `readMemory` response body. The base64 `data` is decoded **here**
/// (DAP delivers base64; the hex-dump formatting is Phase 5). An empty `data` yields an
/// empty byte vector (the handler reports `bytes_read: 0`). The echoed `address` is
/// passed through verbatim. Go origin: `internal/tools/memory.go` base64 decode.
pub fn read_memory(body: &Option<Value>) -> Result<MemoryRead, BackendError> {
    let parsed: ReadMemoryBody = decode(body)?;
    let data = if parsed.data.is_empty() {
        Vec::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(parsed.data.as_bytes())
            .map_err(|e| BackendError::Dap {
                message: format!("failed to decode memory data: {e}"),
            })?
    };
    Ok(MemoryRead {
        address: parsed.address,
        data,
    })
}

/// Instruction list from a `disassemble` response body. Raw fields pass through
/// (`is_current_pc`/`start_address` normalization is Phase 5).
pub fn disassemble(body: &Option<Value>) -> Result<Vec<Instruction>, BackendError> {
    let parsed: DisassembleBody = decode(body)?;
    Ok(parsed
        .instructions
        .into_iter()
        .map(|i| {
            let source_path = i.location.map(|l| l.path).filter(|p| !p.is_empty());
            Instruction {
                address: i.address,
                instruction: i.instruction,
                bytes: i.instruction_bytes,
                symbol: i.symbol,
                source_path,
                line: i.line,
            }
        })
        .collect())
}

/// Breakpoint results from a `setBreakpoints`/`setFunctionBreakpoints` response body.
pub fn breakpoints(body: &Option<Value>) -> Result<Vec<BreakpointResult>, BackendError> {
    let parsed: SetBreakpointsBody = decode(body)?;
    Ok(parsed
        .breakpoints
        .into_iter()
        .map(|b| BreakpointResult {
            id: b.id,
            verified: b.verified,
            line: b.line,
            message: b.message,
        })
        .collect())
}
