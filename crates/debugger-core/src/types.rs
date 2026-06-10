//! Neutral, debugger-agnostic data types — the parity vocabulary.
//!
//! These types are the seam contract: the tool/session layers speak only these,
//! and each backend translates to/from them. Per Spec OQ-2 (FR-18.6) the payloads
//! are **opaque pass-through** — stop `reason`, instruction-pointer references,
//! etc. stay free-form strings/ints rather than normalized enums, so the tool
//! layer's output stays byte-identical to the Go server.
//!
//! Reproduced from design §Interfaces. Go origins are noted per type so Phase 3/5
//! implementers can cross-check field-by-field against the Go tree.

use serde::{Deserialize, Serialize};

/// Step/disassemble granularity. Mirrors the DAP `SteppingGranularity` notion the
/// Go execution handlers pass through (`internal/tools/execution.go`); kept neutral
/// so the backend owns the DAP mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Granularity {
    Line,
    Instruction,
}

/// Evaluation mode. `Repl` ⇒ the backtick-prefix decision (Spec FR-14.2) lives in
/// the backend, not the tool layer. Go origin: the `context` argument split between
/// `internal/tools/inspection.go` (`"variables"`) and `internal/tools/run_command.go`
/// (`"repl"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvalMode {
    Expression,
    Repl,
}

/// Which execution step to perform. Go origin: the distinct `step_over`/`step_into`/
/// `step_out` handlers in `internal/tools/execution.go`, collapsed onto one neutral
/// method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepKind {
    Over,
    Into,
    Out,
}

/// A stopped-event snapshot. Go origin: the cached `lastStoppedEvent` fields read by
/// `handleStopResult` / `status` (`internal/session/session.go`, `internal/dap`).
/// `reason` is opaque pass-through (Spec FR-18.6): `"breakpoint"`, `"exception"`,
/// `"signal"`, `"step"`, …
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopInfo {
    pub reason: String,
    pub thread_id: i64,
    pub description: String,
    pub hit_breakpoint_ids: Vec<i64>,
}

/// Outcome of any operation that resumes the target (`cont`, `step`). Go origin:
/// the `handleStopResult` switch over stopped/exited/terminated
/// (`internal/tools/execution.go`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    Stopped(StopInfo),
    Exited { code: Option<i64> },
    Terminated,
}

/// Source-line breakpoint request. Go origin: the per-file breakpoint entries the
/// session tracks and flushes (`internal/session/session.go`,
/// `internal/tools/breakpoints.go`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBp {
    pub line: i64,
    pub condition: String,
}

/// Function breakpoint request. Go origin: the function-breakpoint list the session
/// tracks (`internal/session/session.go`, `internal/tools/breakpoints.go`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionBp {
    pub name: String,
    pub condition: String,
}

/// Everything `launch` needs, gathered above the seam (incl. the flushed pending
/// breakpoints — Spec FR-4.4.10). Go origin: the launch arguments assembled in
/// `internal/tools/launch.go` plus the pending-breakpoint flush from the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub stop_on_entry: bool,
    /// Per-file source breakpoints flushed during configuration.
    pub source_breakpoints: Vec<(String, Vec<SourceBp>)>,
    pub function_breakpoints: Vec<FunctionBp>,
}

/// Outcome of `launch`. `Running` covers `stop_on_entry=false` (Spec FR-4.6). Go
/// origin: the launch success branches in `internal/tools/launch.go`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchOutcome {
    Stopped(StopInfo),
    Running,
    Exited { code: Option<i64> },
}

/// Everything `attach` needs. `pid` takes precedence over `wait_for` (Spec FR-5.1).
/// Go origin: the attach arguments in `internal/tools/attach.go`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachSpec {
    pub pid: Option<i64>,
    pub wait_for: Option<String>,
}

/// Outcome of `attach`. Attach is always stop-on-entry, so there is **no** `Running`
/// variant; it can produce "Process exited during attach" (Go `attach.go:220`), hence
/// `Exited`/`Terminated`. Go origin: the attach success branches in
/// `internal/tools/attach.go`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachOutcome {
    Stopped(StopInfo),
    Exited { code: Option<i64> },
    Terminated,
}

/// One stack frame. Go origin: the per-frame fields built in
/// `internal/tools/inspection.go` (`backtrace`); `instruction_pointer` is the opaque
/// IP reference string (Spec FR-18.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub index: i64,
    pub id: i64,
    pub name: String,
    pub source_path: Option<String>,
    pub line: i64,
    pub instruction_pointer: Option<String>,
}

/// One thread. Go origin: the thread entries built in `internal/tools/inspection.go`
/// (`threads`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: i64,
    pub name: String,
}

/// A variable scope (Locals/Globals/Registers). Go origin: the DAP `ScopesRequest`
/// result matched case-insensitively in `internal/tools/inspection.go` (`variables`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub name: String,
    pub variables_reference: i64,
}

/// One variable. `named`/`indexed` are the child counts used by the flattening
/// algorithm's `children_count` (Spec FR-11.6). Go origin: the DAP `Variable` fields
/// consumed by `FlattenVariables` (`internal/tools/variables_util.go`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub ty: String,
    pub variables_reference: i64,
    pub named: i64,
    pub indexed: i64,
}

/// Result of setting a breakpoint. Go origin: the breakpoint-response selection in
/// `internal/tools/breakpoints.go`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakpointResult {
    pub id: i64,
    pub verified: bool,
    pub line: i64,
    pub message: String,
    /// The backend DECLINED to register this breakpoint (it will never resolve and must NOT be
    /// tracked/re-flushed) — distinct from an unverified-but-pending breakpoint that may resolve
    /// later. Set only by a backend that rejects a location it cannot safely track (the WinDbg
    /// ASLR-unsafe bare-address rejection, R6); lldb leaves it false (every lldb result, verified
    /// or pending, is trackable). Default false ⇒ a strict no-op for existing backends.
    #[serde(default)]
    pub rejected: bool,
}

/// Result of an `evaluate`. Go origin: the `EvaluateResponse` fields surfaced by
/// `internal/tools/inspection.go` (`evaluate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalResult {
    pub result: String,
    pub ty: String,
    pub variables_reference: i64,
}

/// Raw memory bytes plus the backend's echoed address. Go origin: the
/// `ReadMemoryResponse` consumed by `internal/tools/memory.go` (`read_memory`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRead {
    /// The backend's echoed address (used verbatim in the response).
    pub address: String,
    pub data: Vec<u8>,
}

/// One loaded module. C++ origin: the `get_modules` tool / `DebugEngine::GetModules`
/// in the `windbg-mcp` plugin (no lldb analog — surfaced only by the WinDbg backend).
/// Opaque pass-through (Spec FR-18.6): every field is a free-form string carried
/// verbatim to the tool layer.
///
/// - `base` is a hex string formatted `"0x{:016X}"` (the parity convention for
///   IP/address fields, matching [`Frame::instruction_pointer`]).
/// - `size` is the module's size in bytes as a decimal string.
/// - `symbol_status` ∈ {`"pdb"`, `"export"`, `"deferred"`, `"none"`} — the C++
///   `SymbolType` map, passed through unmodified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub name: String,
    pub base: String,
    pub size: String,
    pub symbol_status: String,
}

/// Outcome of `open_dump`. C++ origin: `DebugEngine::OpenDumpFile` in the `windbg-mcp`
/// plugin, followed by `GetCurrentSourceLocation` (no lldb analog).
///
/// - `stop` is the stop snapshot produced when the dump's `WaitForEvent` returns, or
///   `None` if the dump did not yield a stopped context.
/// - `crash_location` is `"<file>:<line>"` sourced from the backend's
///   `current_source_location()` called *inside* `open_dump` after the dump loads;
///   `None` when no source line maps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpOutcome {
    pub stop: Option<StopInfo>,
    pub crash_location: Option<String>,
}

/// One disassembled instruction. Go origin: the per-instruction fields built in
/// `internal/tools/memory.go` (`disassemble`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub address: String,
    pub instruction: String,
    pub bytes: String,
    pub symbol: String,
    pub source_path: Option<String>,
    pub line: i64,
}
