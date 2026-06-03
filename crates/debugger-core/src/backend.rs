//! The `DebuggerBackend` + `BackendFactory` traits and the `Connection` they hand
//! back — the seam every neutral crate is written against (Spec FR-18, design
//! §Interfaces).
//!
//! The trait is **coarse-grained and blocking** for execution/lifecycle: methods
//! that resume the target return the **next stop**. This keeps the DAP handshake and
//! the `InitializedEvent`-ordering quirk entirely below the seam (Spec FR-18.4,
//! design Decision 2). This concrete trait supersedes the *indicative* (non-normative)
//! sketch in Spec FR-18.7.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::BackendError;
use crate::event::BackendEvent;
use crate::types::{
    AttachOutcome, AttachSpec, BreakpointResult, EvalMode, EvalResult, Frame, FunctionBp,
    Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead, Scope, SourceBp, StepKind,
    StopOutcome, ThreadInfo, Variable,
};

/// A debugger-neutral backend. Every concrete debugger (lldb-dap today, WinDbg later)
/// implements this; everything above the seam holds only `Arc<dyn DebuggerBackend>`.
///
/// **No `CancellationToken` in any signature.** Cancellation is applied at the tool
/// layer via `tokio::select!` against the request token (design §Interfaces); this
/// keeps `debugger-core` free of any `tokio`/`tokio-util` dependency. On cancel the
/// in-flight backend future is simply dropped (the DAP backend's drop cleanup makes
/// that cancel-safe).
///
/// **No `set_exception_breakpoints`.** Each backend's `launch`/`attach` sends empty
/// exception filters internally (matches Go); promote it to the trait only when a
/// backend needs caller-controlled exception filters (e.g. WinDbg).
///
/// **Launch-vs-attach stop-waiter asymmetry.** `launch` registers the stop waiter
/// *before* `configurationDone` (Go `launch.go:304`); `attach` registers it *after*
/// (Go `attach.go:219`). That asymmetry is internal to each backend's `launch`/`attach`,
/// below the seam — the trait surface is symmetric.
#[async_trait]
pub trait DebuggerBackend: Send + Sync {
    // --- lifecycle (own the full handshake below the seam) ---

    /// Run the full launch handshake and block until the first outcome. Go origin:
    /// `internal/tools/launch.go`.
    async fn launch(&self, spec: LaunchSpec) -> Result<LaunchOutcome, BackendError>;

    /// Run the full attach handshake and block until the first outcome. Go origin:
    /// `internal/tools/attach.go`.
    async fn attach(&self, spec: AttachSpec) -> Result<AttachOutcome, BackendError>;

    /// Best-effort teardown; never errors (Spec FR-6). Go origin:
    /// `internal/tools/disconnect.go`.
    async fn disconnect(&self, terminate: bool);

    // --- breakpoints (used while stopped) ---

    /// Set the full source-breakpoint list for one file. Go origin:
    /// `internal/tools/breakpoints.go` (`set_breakpoint`).
    async fn set_source_breakpoints(
        &self,
        file: &str,
        bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError>;

    /// Set the full function-breakpoint list. Go origin:
    /// `internal/tools/breakpoints.go` (`set_function_breakpoint`).
    async fn set_function_breakpoints(
        &self,
        bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError>;

    // --- execution (block until the next stop) ---

    /// Resume the target; block until the next stop. Go origin:
    /// `internal/tools/execution.go` (`continue`).
    async fn cont(&self, thread_id: i64) -> Result<StopOutcome, BackendError>;

    /// Step over/into/out; block until the next stop. Go origin:
    /// `internal/tools/execution.go` (`step_over`/`step_into`/`step_out`).
    async fn step(
        &self,
        kind: StepKind,
        thread_id: i64,
        gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError>;

    /// Pause all threads; returns immediately, unblocking an in-flight `cont`/`step`.
    /// Go origin: `internal/tools/execution.go` (`pause`).
    async fn pause(&self) -> Result<(), BackendError>;

    // --- inspection ---

    /// List threads. Go origin: `internal/tools/inspection.go` (`threads`).
    async fn threads(&self) -> Result<Vec<ThreadInfo>, BackendError>;

    /// Fetch a stack trace; returns `(frames, total_frames)`. `start` is always `0`
    /// from current callers (documented for future backends). Go origin:
    /// `internal/tools/inspection.go` (`backtrace`).
    async fn stack_trace(
        &self,
        thread_id: i64,
        start: i64,
        levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError>;

    /// List scopes for a frame. Go origin: `internal/tools/inspection.go`
    /// (`variables`).
    async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, BackendError>;

    /// Fetch the children of a variables reference. Go origin:
    /// `internal/tools/variables_util.go`.
    async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError>;

    /// Evaluate an expression (or raw repl command, via `EvalMode::Repl`). The
    /// backtick-prefix decision (Spec FR-14.2) lives in the backend. Go origin:
    /// `internal/tools/inspection.go` (`evaluate`) + `internal/tools/run_command.go`.
    async fn evaluate(
        &self,
        expr: &str,
        frame_id: Option<i64>,
        mode: EvalMode,
    ) -> Result<EvalResult, BackendError>;

    /// Read raw memory. Go origin: `internal/tools/memory.go` (`read_memory`).
    async fn read_memory(&self, address: &str, count: i64) -> Result<MemoryRead, BackendError>;

    /// Disassemble. Go origin: `internal/tools/memory.go` (`disassemble`).
    async fn disassemble(
        &self,
        address: &str,
        count: i64,
    ) -> Result<Vec<Instruction>, BackendError>;

    // --- capability ---

    /// Whether the backend supports raw command mode without backtick-prefixing (the
    /// repl-mode flag, Spec FR-14.2/FR-15). Go origin: the `replModeCommand` flag.
    fn supports_command_repl_mode(&self) -> bool;

    /// The OS pid of the backend's debugger subprocess, when one was spawned. Go origin:
    /// the lldb-dap subprocess pid recorded via `session.SetPID(sub.Cmd.Process.Pid)` in
    /// `launch.go`/`attach.go` — the value `launch`/`status` report (`"pid"`). For a
    /// scripted/peer backend with no real child this is `None`. Attach-by-pid uses the
    /// *target* pid supplied by the caller instead of this (Spec FR-5.6), so the handler
    /// only consults this when no explicit pid is available.
    fn debugger_pid(&self) -> Option<i64> {
        None
    }
}

/// A connected-but-not-yet-launched backend plus its async event stream. Returned by
/// [`BackendFactory::connect`]; the session stores `backend`, bumps its generation,
/// and spawns an event-pump task draining `events` (design §Interfaces).
pub struct Connection {
    pub backend: Arc<dyn DebuggerBackend>,
    /// Runtime-neutral event stream (`futures::Stream`, not a `tokio` channel) so the
    /// contract crate stays runtime-agnostic; `lldb-backend` builds it from a tokio
    /// `mpsc` internally and boxes it (design Decision 5).
    pub events: BoxStream<'static, BackendEvent>,
}

/// The single injection point the binary registers; the session asks one to connect.
/// Maps to Go's lazy lldb-dap spawn at launch/attach time. Adding WinDbg = a new
/// factory + one registration line in `bin`, with zero changes above the seam.
#[async_trait]
pub trait BackendFactory: Send + Sync {
    /// A short backend name, e.g. `"lldb"`.
    fn name(&self) -> &'static str;

    /// Detect + spawn + start the transport; returns a *not-yet-launched* backend plus
    /// its event stream. Go origin: the lazy lldb-dap spawn in
    /// `internal/tools/launch.go`/`attach.go`.
    async fn connect(&self) -> Result<Connection, BackendError>;
}
