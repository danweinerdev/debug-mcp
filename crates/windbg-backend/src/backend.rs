//! [`WinDbgBackend`] — the DbgEng implementation of [`DebuggerBackend`], plus the [`call`] /
//! marshaling primitive every op is built on.
//!
//! [`WinDbgBackend::call`] is the heart of the backend: it builds an [`EngineCmd`] (with a fresh
//! reply `oneshot`), sends it over the command channel to the engine thread, and awaits the
//! reply — mapping a closed channel / dropped reply onto [`BackendError::Closed`] and an
//! [`EngineError`] onto a neutral [`BackendError`]. Tasks 3.2–3.4 build their lifecycle /
//! execution / inspection ops on this one helper.
//!
//! No `unsafe`: the backend only ever holds channel endpoints and a `Send`
//! [`InterruptHandle`](dbgeng_sys::InterruptHandle); all DbgEng/COM work is on the engine thread.

use async_trait::async_trait;
use dbgeng_sys::InterruptHandle;
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BreakpointResult, DebuggerBackend, EvalMode,
    EvalResult, Frame, FunctionBp, Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead,
    ModuleInfo, Scope, SourceBp, StepKind, StopOutcome, ThreadInfo, Variable,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::EngineError;
use crate::thread::EngineCmd;

/// The WinDbg backend handle held above the seam as `Arc<dyn DebuggerBackend>`.
///
/// It owns no DbgEng state — only the marshaling [`mpsc::Sender<EngineCmd>`] to the engine
/// thread, a `Send` [`InterruptHandle`] for [`pause`](DebuggerBackend::pause), and the optional
/// debugger pid. Every op flows through [`WinDbgBackend::call`].
pub struct WinDbgBackend {
    /// The command channel to the engine thread. Dropping the last clone of this closes the
    /// channel, which ends the engine thread (its `blocking_recv` returns `None`) — the
    /// teardown path.
    cmd_tx: mpsc::Sender<EngineCmd>,
    /// A `Send` interrupt token minted on the engine thread at readiness. `pause` sets its flag,
    /// which the engine thread's in-flight `go` poll loop observes and turns into a real break
    /// (the flag-only R4 design).
    interrupt: InterruptHandle,
    /// The debugger pid, when one is known. DbgEng runs in-process (no debugger subprocess), so
    /// this is `None` in the normal case; kept on the struct so a future kernel/remote mode can
    /// populate it and [`DebuggerBackend::debugger_pid`] can surface it.
    debugger_pid: Option<i64>,
    /// The engine thread's join handle, owned here so the thread is not silently detached: this
    /// makes the thread's lifetime observable and gives Phase 5 a hook for the R2 orphan-thread
    /// teardown. We deliberately do NOT join it on `Drop` — a thread blocked in a long `go`
    /// (or the uncancellable kernel-attach wait, R2) would hang `drop`. Teardown instead relies on
    /// closing the command channel (dropping `cmd_tx`), which ends the loop in the normal path; a
    /// genuinely-stuck thread is left to exit at process end (R2, Phase 5).
    _engine_thread: std::thread::JoinHandle<()>,
}

impl WinDbgBackend {
    /// Assemble a backend over an already-spawned engine thread's channel + readiness handle.
    /// The factory (and the test spawn helper) call this after awaiting the readiness `oneshot`.
    pub fn new(
        cmd_tx: mpsc::Sender<EngineCmd>,
        interrupt: InterruptHandle,
        debugger_pid: Option<i64>,
        engine_thread: std::thread::JoinHandle<()>,
    ) -> WinDbgBackend {
        WinDbgBackend {
            cmd_tx,
            interrupt,
            debugger_pid,
            _engine_thread: engine_thread,
        }
    }

    /// The marshaling primitive: build an [`EngineCmd`] carrying a fresh reply `oneshot`, send it
    /// to the engine thread, and await the reply.
    ///
    /// `make_cmd` receives the reply `Sender` and returns the fully-built command (each op's
    /// closure plugs the `Sender` into the right variant). Errors map as:
    /// - send fails (engine thread gone / channel closed) → [`BackendError::Closed`];
    /// - the reply `Sender` is dropped without a value (the thread died mid-op) →
    ///   [`BackendError::Closed`];
    /// - the engine returned an [`EngineError`] → mapped via [`map_engine_err`].
    ///
    /// Holds no lock across the `.await` (it owns only channel endpoints), satisfying the
    /// never-hold-a-lock-across-await rule.
    pub(crate) async fn call<T>(
        &self,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> EngineCmd,
    ) -> Result<T, BackendError> {
        let (reply_tx, reply_rx) = oneshot::channel::<Result<T, EngineError>>();
        let cmd = make_cmd(reply_tx);
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| BackendError::Closed)?;
        match reply_rx.await {
            Ok(result) => result.map_err(map_engine_err),
            // The engine thread dropped the reply sender without answering (it exited mid-op).
            Err(_) => Err(BackendError::Closed),
        }
    }
}

/// Map a `dbgeng-sys` [`EngineError`] onto the neutral [`BackendError`]. The full per-op Go-string
/// wording is owned by the tool layer / later tasks; here we carry the engine's verbatim message
/// in the closest neutral variant so nothing is lost: a COM/engine failure becomes
/// [`BackendError::Dap`] (the "the debugger reported an error" channel) carrying the `Display`
/// text. Tasks 3.2–3.4 refine the per-op mapping where a different variant fits better.
pub(crate) fn map_engine_err(err: EngineError) -> BackendError {
    BackendError::Dap {
        message: err.to_string(),
    }
}

#[async_trait]
impl DebuggerBackend for WinDbgBackend {
    // --- lifecycle ---

    async fn launch(&self, _spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        // TODO(3.2): translate LaunchSpec → dbgeng_sys::LaunchReq, flush breakpoints, marshal
        // EngineCmd::Launch, and map StopOutcome → LaunchOutcome.
        Err(BackendError::Send(
            "launch: not yet implemented (phase 3.2)".into(),
        ))
    }

    async fn attach(&self, _spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        // TODO(3.2): marshal EngineCmd::AttachPid (pid path) and map StopOutcome → AttachOutcome.
        Err(BackendError::Send(
            "attach: not yet implemented (phase 3.2)".into(),
        ))
    }

    async fn disconnect(&self, _terminate: bool) {
        // Best-effort detach (never errors, Spec FR-6). The full terminate-vs-detach semantics
        // are 3.2; here we marshal a plain detach so the engine releases the target, and discard
        // any error. Dropping the backend afterwards closes the channel and ends the thread.
        // TODO(3.2): honor `terminate` (kill vs detach) once the engine exposes the distinction.
        let _ = self.call(|reply| EngineCmd::Detach { reply }).await;
    }

    // --- breakpoints ---

    async fn set_source_breakpoints(
        &self,
        _file: &str,
        _bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        // TODO(3.3): parse each SourceBp into a BpLoc::FileLine and marshal EngineCmd::SetBreakpoint
        // per line (DbgEng has no batch set-for-file; the per-file list is reconciled here).
        Err(BackendError::Send(
            "set_source_breakpoints: not yet implemented (phase 3.3)".into(),
        ))
    }

    async fn set_function_breakpoints(
        &self,
        _bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        // TODO(3.3): parse each FunctionBp into a BpLoc::Function and marshal EngineCmd::SetBreakpoint.
        Err(BackendError::Send(
            "set_function_breakpoints: not yet implemented (phase 3.3)".into(),
        ))
    }

    // --- execution ---

    async fn cont(&self, _thread_id: i64) -> Result<StopOutcome, BackendError> {
        // TODO(3.3): marshal EngineCmd::Go with the continue timeout budget and map the
        // still-running (Ok(None)) case onto the neutral continue-timed-out path.
        Err(BackendError::Send(
            "cont: not yet implemented (phase 3.3)".into(),
        ))
    }

    async fn step(
        &self,
        _kind: StepKind,
        _thread_id: i64,
        _gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError> {
        // TODO(3.3): map neutral StepKind → dbgeng_sys::StepKind and marshal EngineCmd::Step.
        Err(BackendError::Send(
            "step: not yet implemented (phase 3.3)".into(),
        ))
    }

    async fn pause(&self) -> Result<(), BackendError> {
        // Set the cooperative interrupt flag (the flag-only R4 design): the engine thread's
        // in-flight `go` poll loop observes it within one ~200 ms slice and converts it into a
        // real break. `pause` returns immediately and never marshals a blocking command — the
        // unblocking happens on the engine thread, not here. A request while nothing is running
        // is harmlessly consumed (`go` resets the flag at entry).
        self.interrupt.interrupt();
        Ok(())
    }

    // --- inspection ---

    async fn threads(&self) -> Result<Vec<ThreadInfo>, BackendError> {
        // Direct marshal: the engine returns neutral ThreadInfo already. (3.4 may refine error
        // wording, but the round-trip is correct now.)
        self.call(|reply| EngineCmd::Threads { reply }).await
    }

    async fn stack_trace(
        &self,
        _thread_id: i64,
        _start: i64,
        _levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError> {
        // TODO(3.4): marshal EngineCmd::StackTrace (thread_id, max=levels) and compute total_frames.
        Err(BackendError::Send(
            "stack_trace: not yet implemented (phase 3.4)".into(),
        ))
    }

    async fn scopes(&self, _frame_id: i64) -> Result<Vec<Scope>, BackendError> {
        // TODO(3.4): DbgEng has no DAP scopes; synthesize a Locals scope whose variables_reference
        // encodes the frame so `variables` can fetch locals via EngineCmd::Locals.
        Err(BackendError::Send(
            "scopes: not yet implemented (phase 3.4)".into(),
        ))
    }

    async fn variables(&self, _variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        // TODO(3.4): decode the frame from variables_reference and marshal EngineCmd::Locals.
        Err(BackendError::Send(
            "variables: not yet implemented (phase 3.4)".into(),
        ))
    }

    async fn evaluate(
        &self,
        _expr: &str,
        _frame_id: Option<i64>,
        _mode: EvalMode,
    ) -> Result<EvalResult, BackendError> {
        // TODO(3.4): Expression → EngineCmd::Evaluate; Repl → EngineCmd::Execute (raw command,
        // no backtick — supports_command_repl_mode() is true).
        Err(BackendError::Send(
            "evaluate: not yet implemented (phase 3.4)".into(),
        ))
    }

    async fn read_memory(&self, _address: &str, _count: i64) -> Result<MemoryRead, BackendError> {
        // TODO(3.4): parse the address string → u64 and marshal EngineCmd::ReadMemory.
        Err(BackendError::Send(
            "read_memory: not yet implemented (phase 3.4)".into(),
        ))
    }

    async fn disassemble(
        &self,
        _address: &str,
        _count: i64,
    ) -> Result<Vec<Instruction>, BackendError> {
        // TODO(3.4): parse the address string → u64 and marshal EngineCmd::Disassemble.
        Err(BackendError::Send(
            "disassemble: not yet implemented (phase 3.4)".into(),
        ))
    }

    // --- capability ---

    fn supports_command_repl_mode(&self) -> bool {
        // WinDbg drives raw commands through `Execute` (no backtick prefix), so repl-mode is on.
        true
    }

    fn debugger_pid(&self) -> Option<i64> {
        self.debugger_pid
    }

    // --- capability-gated (WinDbg-only verbs) ---

    async fn modules(&self) -> Result<Vec<ModuleInfo>, BackendError> {
        // Direct marshal: the engine returns neutral ModuleInfo already.
        self.call(|reply| EngineCmd::Modules { reply }).await
    }

    // open_dump / attach_kernel are Phase-4 engine stubs; analyze is 3.x. They inherit the trait's
    // default `Unsupported` bodies for now and are overridden in their respective tasks.
    // TODO(3.x/Phase 4): override open_dump, attach_kernel, analyze.
}
