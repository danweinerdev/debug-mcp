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

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbgeng_sys::{find_process_by_name, BpLoc, InterruptHandle, LaunchReq};
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BreakpointResult, DebuggerBackend, EvalMode,
    EvalResult, Frame, FunctionBp, Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead,
    ModuleInfo, Scope, SourceBp, StepKind, StopOutcome, ThreadInfo, Variable,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::EngineError;
use crate::thread::EngineCmd;

/// The `wait_for` poll interval — how often `attach`'s `wait_for` path snapshots the process list
/// looking for the named process to appear.
const WAIT_FOR_POLL: Duration = Duration::from_millis(75);

/// The `wait_for` overall bound — how long `attach` waits for the named process to appear before
/// giving up with a clear error.
const WAIT_FOR_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The target pid, when one is known, behind a `Mutex` for interior mutability: `attach` sets
    /// it (to the supplied pid, or the pid resolved from `wait_for`) *after* construction, and
    /// [`DebuggerBackend::debugger_pid`] reads it. DbgEng runs in-process with no debugger
    /// subprocess, and the engine surface does not readily expose a launched target's own pid, so
    /// `launch` leaves it `None`; it is meaningful mainly for attach-by-`wait_for` (Spec FR-5.6 —
    /// attach-by-pid already reports the supplied pid at the tool layer). The `Mutex` is only ever
    /// locked for a synchronous read/write that drops the guard before any `.await`.
    target_pid: Mutex<Option<i64>>,
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
            target_pid: Mutex::new(debugger_pid),
            _engine_thread: engine_thread,
        }
    }

    /// Record the target pid (set by `attach`). Locks the pid cell, writes, and drops the guard
    /// before returning — never held across an `.await`.
    fn set_target_pid(&self, pid: Option<i64>) {
        let mut guard = self.target_pid.lock().unwrap_or_else(|p| p.into_inner());
        *guard = pid;
    }

    /// Resolve `attach`'s `wait_for` mode: poll the process list for a process named `name` to
    /// appear, returning its pid once found. Delegates to [`poll_for_process`] with the real
    /// `dbgeng_sys::find_process_by_name` lookup — that lookup does only a Toolhelp32 snapshot (no
    /// DbgEng/COM), so it is safe to call directly from this async side off the engine thread.
    async fn resolve_wait_for(&self, name: &str) -> Result<u32, BackendError> {
        poll_for_process(name, find_process_by_name, WAIT_FOR_POLL, WAIT_FOR_TIMEOUT).await
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

/// Poll `lookup(name)` every `interval` until it returns `Some(pid)` or `timeout` elapses, mapping
/// a never-appears to a clear [`BackendError`]. Factored out (with the lookup injected) so the
/// `wait_for` polling logic is unit-testable with a scripted lookup — production passes
/// [`find_process_by_name`].
///
/// The lookup runs directly on the async side (it is a plain Toolhelp32 snapshot, no DbgEng/COM),
/// and the wait is broken into `interval` `tokio::time::sleep`s so the task yields between snapshots
/// rather than busy-spinning. A Toolhelp32 snapshot is typically sub-millisecond, so we accept it on
/// the worker thread rather than paying `spawn_blocking` overhead on each of the (up to ~400) polls;
/// if profiling ever shows it stalling the runtime, wrap `lookup` in `tokio::task::spawn_blocking`.
pub(crate) async fn poll_for_process(
    name: &str,
    lookup: impl Fn(&str) -> Option<u32>,
    interval: Duration,
    timeout: Duration,
) -> Result<u32, BackendError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = lookup(name) {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            return Err(BackendError::Dap {
                message: format!("wait_for: no process named '{name}' appeared"),
            });
        }
        tokio::time::sleep(interval).await;
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

    async fn launch(&self, spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        // Map the neutral LaunchSpec onto the engine's minimal LaunchReq. The engine's LaunchReq
        // carries no environment — WinDbg env injection is NOT wired (DbgEng's CreateProcess2 path
        // here inherits our environment). A non-empty `spec.env` is therefore a known gap: it is
        // silently ignored rather than failing the launch (the agent's launch still succeeds; only
        // custom env vars are not applied). Documented here per the task; revisit if a backend-level
        // env channel is added.
        let req = LaunchReq {
            program: spec.program,
            args: spec.args,
            cwd: spec.cwd,
        };

        // Marshal the launch. The engine always stops at the loader (INITIAL_BREAK) break, so a
        // successful launch yields StopOutcome::Stopped even when stop_on_entry is false.
        let outcome = self.call(|reply| EngineCmd::Launch { req, reply }).await?;

        // On a stop (the normal launch result), flush the pending breakpoints the session tracked
        // when the tool set them: launch flushes, attach does not. Each set is best-effort — a
        // breakpoint that fails to resolve (e.g. no symbols yet) must NOT abort the launch; the
        // per-bp result is discarded here and the session's own tracking carries verification.
        if matches!(outcome, StopOutcome::Stopped(_)) {
            for (file, bps) in &spec.source_breakpoints {
                for bp in bps {
                    // `SourceBp::line` is i64; a line outside u32 range is malformed — skip it
                    // (best-effort flush) rather than wrap-casting to a bogus line.
                    let Ok(line) = u32::try_from(bp.line) else {
                        continue;
                    };
                    let loc = BpLoc::FileLine {
                        file: file.clone(),
                        line,
                    };
                    let condition = bp.condition.clone();
                    let _ = self
                        .call(|reply| EngineCmd::SetBreakpoint {
                            loc,
                            condition,
                            reply,
                        })
                        .await;
                }
            }
            for bp in &spec.function_breakpoints {
                let loc = BpLoc::Function(bp.name.clone());
                let condition = bp.condition.clone();
                let _ = self
                    .call(|reply| EngineCmd::SetBreakpoint {
                        loc,
                        condition,
                        reply,
                    })
                    .await;
            }
        }

        // Map the engine StopOutcome onto the neutral LaunchOutcome. WinDbg always stops at the
        // loader break (DbgEng INITIAL_BREAK) — a deliberate behavior difference from lldb's
        // stop_on_entry: launch returns Stopped even when stop_on_entry is false, and we do NOT
        // auto-continue (that would block the engine thread). The agent issues `continue` next; the
        // breakpoints are already set. A `Terminated` (no live target) maps to Exited{code:None}.
        // `LaunchOutcome::Running` is unreachable from WinDbg by design (it would require
        // auto-continuing, which would block the engine thread) — not an omission.
        Ok(match outcome {
            StopOutcome::Stopped(info) => LaunchOutcome::Stopped(info),
            StopOutcome::Exited { code } => LaunchOutcome::Exited { code },
            StopOutcome::Terminated => LaunchOutcome::Exited { code: None },
        })
    }

    async fn attach(&self, spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        // Resolve the pid to attach to: an explicit pid takes precedence (the tool layer already
        // enforces pid XOR wait_for); otherwise poll for the named process to appear (wait_for).
        let pid = match spec.pid {
            // The tool layer validates `pid > 0`; guard the i64→u32 conversion anyway so a
            // malformed value surfaces a clear error instead of wrap-casting to a bogus pid.
            Some(pid) => u32::try_from(pid).map_err(|_| BackendError::Dap {
                message: format!("attach: pid {pid} is out of range"),
            })?,
            None => match &spec.wait_for {
                Some(name) => self.resolve_wait_for(name).await?,
                None => {
                    return Err(BackendError::Dap {
                        message: "attach: neither pid nor wait_for was supplied".to_string(),
                    })
                }
            },
        };

        // Record the resolved target pid for `debugger_pid` (lock + write + drop before any await).
        self.set_target_pid(Some(pid as i64));

        // Marshal the attach and map the engine StopOutcome onto the neutral AttachOutcome.
        let outcome = self
            .call(|reply| EngineCmd::AttachPid { pid, reply })
            .await?;
        Ok(match outcome {
            StopOutcome::Stopped(info) => AttachOutcome::Stopped(info),
            StopOutcome::Exited { code } => AttachOutcome::Exited { code },
            StopOutcome::Terminated => AttachOutcome::Terminated,
        })
    }

    async fn disconnect(&self, terminate: bool) {
        // Best-effort detach (never errors, Spec FR-6): marshal Detach carrying `terminate` (kill
        // vs. plain detach, honored by the engine), and discard any error. Dropping the backend
        // afterwards closes the channel and ends the thread (whose own teardown never kills).
        let _ = self
            .call(|reply| EngineCmd::Detach { terminate, reply })
            .await;
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
        // Read the recorded target pid (set by `attach`). Lock + read + drop the guard — no await.
        *self.target_pid.lock().unwrap_or_else(|p| p.into_inner())
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
