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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dbgeng_sys::{find_process_by_name, BpLoc, InterruptHandle, LaunchReq};
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BreakpointResult, DebuggerBackend, DumpOutcome,
    EvalMode, EvalResult, Frame, FunctionBp, Granularity, Instruction, LaunchOutcome, LaunchSpec,
    MemoryRead, ModuleInfo, Scope, SourceBp, StepKind, StopOutcome, ThreadInfo, Variable,
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
    /// Per-category breakpoint tracking for the **replace-all** reconciliation the runtime setters
    /// implement (see [`set_source_breakpoints`](DebuggerBackend::set_source_breakpoints)).
    ///
    /// The tool layer drives breakpoints declaratively, exactly like lldb's DAP: each
    /// `setBreakpoints(file, …)` call carries the FULL desired list for that file, and
    /// `setFunctionBreakpoints(…)` the FULL desired function list — the backend must make the
    /// engine match, preserving the engine id of an already-set location (so a stop reports the same
    /// id the `set_breakpoint` tool returned) and removing ones no longer requested. DbgEng has a
    /// single flat breakpoint pool with no file/category tag, so the backend tracks the mapping
    /// itself: `location key → cached BreakpointResult` per category. The `Mutex` is only ever
    /// locked for a synchronous snapshot/commit that drops the guard before any `.await` (the engine
    /// round-trips happen between, lock-free).
    breakpoints: Mutex<BreakpointTable>,
    /// The engine thread's join handle, owned here so the thread is not silently detached: this
    /// makes the thread's lifetime observable and gives Phase 5 a hook for the R2 orphan-thread
    /// teardown. We deliberately do NOT join it on `Drop` — a thread blocked in a long `go`
    /// (or the uncancellable kernel-attach wait, R2) would hang `drop`. Teardown instead relies on
    /// closing the command channel (dropping `cmd_tx`), which ends the loop in the normal path; a
    /// genuinely-stuck thread is left to exit at process end (R2, Phase 5).
    _engine_thread: std::thread::JoinHandle<()>,
}

/// The backend's per-category breakpoint tracking, keyed by source location so the runtime setters
/// can do replace-all reconciliation while preserving engine ids (see [`WinDbgBackend::breakpoints`]).
///
/// `source` maps a file path → (line → the engine's [`BreakpointResult`] for that file:line);
/// `function` maps a function name → its [`BreakpointResult`]. The cached result carries the engine
/// id minted by `set_breakpoint`, so a re-send of an unchanged location reuses the same id rather
/// than creating a duplicate (DbgEng `AddBreakpoint` would otherwise leak a second breakpoint at the
/// same address each call). Source and function breakpoints are tracked separately because the tool
/// layer reconciles them through separate DAP-style requests.
#[derive(Default)]
struct BreakpointTable {
    /// file path → (line → cached result). One inner map per source file (DAP `setBreakpoints` is
    /// per-file, so a reconcile only touches the addressed file's lines).
    source: HashMap<String, HashMap<u32, BreakpointResult>>,
    /// function name → cached result. A single category (DAP `setFunctionBreakpoints` is global).
    function: HashMap<String, BreakpointResult>,
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
            breakpoints: Mutex::new(BreakpointTable::default()),
            _engine_thread: engine_thread,
        }
    }

    /// Test-only: build a backend whose command channel is ALREADY closed (its receiver is
    /// dropped) over a benign, already-finished engine thread. Any marshaled op's `cmd_tx.send`
    /// fails immediately → [`call`](WinDbgBackend::call) maps it to [`BackendError::Closed`] — the
    /// deterministic "dead engine thread" path. Used by the breakpoint Closed-propagation test
    /// without racing a real teardown.
    #[cfg(test)]
    pub(crate) fn with_closed_channel_for_test() -> WinDbgBackend {
        // A channel whose receiver is dropped: every send returns `Err` (the engine thread gone).
        let (cmd_tx, rx) = mpsc::channel::<EngineCmd>(1);
        // Close the channel immediately, before constructing the backend, so every `cmd_tx.send`
        // fails from the outset.
        drop(rx);
        let interrupt = InterruptHandle::from_flag(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        ));
        // A trivially-finished thread so the JoinHandle field is populated without a live engine.
        let engine_thread = std::thread::spawn(|| {});
        WinDbgBackend {
            cmd_tx,
            interrupt,
            target_pid: Mutex::new(None),
            breakpoints: Mutex::new(BreakpointTable::default()),
            _engine_thread: engine_thread,
        }
    }

    /// Record the target pid (set by `attach`). Locks the pid cell, writes, and drops the guard
    /// before returning — never held across an `.await`.
    fn set_target_pid(&self, pid: Option<i64>) {
        let mut guard = self.target_pid.lock().unwrap_or_else(|p| p.into_inner());
        *guard = pid;
    }

    /// Remove one engine breakpoint by id during a reconcile, best-effort. A non-transport failure
    /// (e.g. the id was already gone) is swallowed — the worst case is a leftover engine breakpoint,
    /// never a wrong result — but a transport `Closed` (the engine thread died) is propagated so the
    /// caller aborts the whole reconcile (nothing further can succeed). An id `<= 0` is an unverified
    /// sentinel (never a real engine breakpoint) and is skipped without a round-trip.
    async fn remove_engine_breakpoint(&self, id: i64) -> Result<(), BackendError> {
        if id <= 0 {
            return Ok(());
        }
        match self
            .call(|reply| EngineCmd::RemoveBreakpoint { id, reply })
            .await
        {
            Ok(()) => Ok(()),
            Err(BackendError::Closed) => Err(BackendError::Closed),
            // A non-transport remove failure (stale id, already removed) is best-effort: swallow it.
            Err(_) => Ok(()),
        }
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

/// Parse a neutral address string (`read_memory`/`disassemble`'s `address` arg) into a `u64`.
/// Accepts a `0x`/`0X`-prefixed hex address or a plain decimal address (the two forms the tool
/// layer can carry — an IP reference like `Frame::instruction_pointer` is `0x{:016X}`). A
/// malformed address is a user-facing failure carried in [`BackendError::Dap`] (the neutral
/// "the debugger reported an error" channel), not a transport error.
pub(crate) fn parse_address(address: &str) -> Result<u64, BackendError> {
    let trimmed = address.trim();
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u64>()
    };
    parsed.map_err(|_| BackendError::Dap {
        message: format!("invalid address: '{address}'"),
    })
}

/// Map an engine [`StopOutcome`] onto the neutral [`AttachOutcome`] both `attach` and
/// `attach_kernel` return. The two outcome enums are structurally parallel (Stopped/Exited/
/// Terminated), so this is a 1:1 translation: `Stopped`→`Stopped`, `Exited{code}`→`Exited{code}`,
/// `Terminated`→`Terminated`. Factored out so `attach` (attach-by-pid / `wait_for`) and
/// `attach_kernel` (KDNET kernel attach) share one mapping rather than each inlining it — they
/// differ only in WHICH engine command produces the `StopOutcome`, not in how it maps to the
/// neutral attach result.
fn stop_to_attach(outcome: StopOutcome) -> AttachOutcome {
    match outcome {
        StopOutcome::Stopped(info) => AttachOutcome::Stopped(info),
        StopOutcome::Exited { code } => AttachOutcome::Exited { code },
        StopOutcome::Terminated => AttachOutcome::Terminated,
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

/// Resolve one breakpoint's [`SetBreakpoint`](EngineCmd::SetBreakpoint) call into a per-bp result,
/// applying the lldb-parity per-bp-failure rule used by both runtime breakpoint setters.
///
/// `Engine::set_breakpoint` (dbgeng-sys) returns `Err(EngineError)` for an UNRESOLVABLE location
/// (no symbols / unknown line / unknown function — `GetOffsetByLine`/`GetOffsetByName` fails),
/// which [`WinDbgBackend::call`] maps through [`map_engine_err`] to [`BackendError::Dap`]. lldb's
/// DAP `setBreakpoints`/`setFunctionBreakpoints`, by contrast, returns a *per-bp* `verified:false`
/// entry for an unresolvable bp and NEVER a request-level error. To match that parity we convert an
/// unresolvable-bp `Err` into a `verified:false` `BreakpointResult` (id 0 — the lldb "unverified"
/// sentinel from `DapBreakpoint`'s `#[serde(default)]`, carrying the engine's message) and let the
/// batch continue — so one bad line/function does not fail the others.
///
/// The lone exception is [`BackendError::Closed`]: that is a TRANSPORT failure (the engine thread
/// died), so nothing further can succeed — it is propagated (`Err`) and aborts the batch. Any other
/// backend error (the engine's "could not resolve", surfaced via `map_engine_err` as
/// [`BackendError::Dap`]) is treated as a per-bp failure, not a transport failure.
///
/// `unverified_line` is the `line` to record on the unverified-error fallback: a source bp echoes
/// its requested line (so the handler's line-match finds the entry); a function bp passes 0 (the
/// "no line" sentinel).
fn bp_result_or_continue(
    outcome: Result<BreakpointResult, BackendError>,
    unverified_line: i64,
) -> Result<BreakpointResult, BackendError> {
    match outcome {
        Ok(result) => Ok(result),
        // Transport failure: the engine thread is gone — propagate and abort the batch.
        Err(BackendError::Closed) => Err(BackendError::Closed),
        // An unresolvable bp (or any non-transport engine error): mirror lldb's per-bp
        // `verified:false` result and continue the batch rather than failing the whole call.
        Err(other) => Ok(BreakpointResult {
            id: 0,
            verified: false,
            line: unverified_line,
            message: other.to_string(),
        }),
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
        // breakpoint that fails to resolve (e.g. no symbols yet) must NOT abort the launch.
        //
        // A successful (verified) flush is RECORDED in `self.breakpoints` so that a later runtime
        // `set_source_breakpoints`/`set_function_breakpoints` re-send of the same location reuses the
        // flushed engine breakpoint (preserving its id) instead of adding a duplicate — keeping the
        // flush path and the runtime reconcile path consistent.
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
                    let result = self
                        .call(|reply| EngineCmd::SetBreakpoint {
                            loc,
                            condition,
                            reply,
                        })
                        .await;
                    if let Ok(result) = result {
                        if result.verified {
                            let mut table =
                                self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
                            table
                                .source
                                .entry(file.clone())
                                .or_default()
                                .insert(line, result);
                        }
                    }
                }
            }
            for bp in &spec.function_breakpoints {
                let loc = BpLoc::Function(bp.name.clone());
                let condition = bp.condition.clone();
                let result = self
                    .call(|reply| EngineCmd::SetBreakpoint {
                        loc,
                        condition,
                        reply,
                    })
                    .await;
                if let Ok(result) = result {
                    if result.verified {
                        let mut table = self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
                        table.function.insert(bp.name.clone(), result);
                    }
                }
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
        Ok(stop_to_attach(outcome))
    }

    async fn disconnect(&self, terminate: bool) {
        // Set the interrupt flag FIRST, then marshal the detach. This matters because handlers are
        // concurrent: a `disconnect` can land while a free-running `cont`'s `go` is still blocking
        // the engine thread. `go` only checks the cooperative interrupt flag (not the command
        // channel), so a `Detach` queued behind it would never run until `go` returned on its own.
        // Tripping the flag here makes the in-flight `go` break on its next ~200 ms poll slice and
        // return, so the engine thread loops back to `blocking_recv` and processes the queued
        // `Detach`. With nothing running, the flag is harmlessly consumed (the next `go` resets it
        // at entry). This makes the common free-running-cont disconnect clean; it does NOT solve
        // the uncancellable kernel-wait orphan case (R2 / Phase 5).
        self.interrupt.interrupt();

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
        file: &str,
        bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        // REPLACE-ALL reconciliation (lldb/DAP parity): the tool layer passes the FULL desired list
        // for this file each call. The engine must end up with exactly these file:line breakpoints —
        // preserving the engine id of a line already set (so a stop reports the same id the
        // `set_breakpoint` tool returned), removing lines no longer requested, and adding new ones.
        // DbgEng `AddBreakpoint` always creates a NEW breakpoint, so without this reconcile a re-send
        // (e.g. after a sibling `remove_breakpoint` re-sends the file's remaining list) would leak a
        // duplicate at the same address and the stop id would drift. We track the per-file mapping in
        // `self.breakpoints.source[file]`.
        //
        // Snapshot the file's prior tracked lines under the lock, drop the lock, do the engine
        // round-trips lock-free, then commit the new map under the lock — never holding the lock
        // across an `.await`.
        let prior: HashMap<u32, BreakpointResult> = {
            let table = self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
            table.source.get(file).cloned().unwrap_or_default()
        };

        // The set of valid (u32) lines requested this call — what should remain after reconcile.
        let mut desired_lines: std::collections::HashSet<u32> = std::collections::HashSet::new();

        // Per-bp failure semantics (lldb/DAP parity): an unresolvable line yields a `verified:false`
        // per-bp result and the batch continues; only a transport failure (`Closed`) is fatal (see
        // `bp_result_or_continue`). The result vector is positional with `bps` (the handler matches
        // by line or takes the last).
        let mut results = Vec::with_capacity(bps.len());
        let mut new_map: HashMap<u32, BreakpointResult> = HashMap::new();
        for bp in bps {
            // A line outside u32 range is malformed and cannot be a real source line. Like lldb's
            // per-bp unresolvable handling, do NOT abort the batch — push an unverified result
            // echoing the requested line and DO NOT ask the engine to set it (skip the marshal).
            let line = match u32::try_from(bp.line) {
                Ok(line) => line,
                Err(_) => {
                    results.push(BreakpointResult {
                        id: 0,
                        verified: false,
                        line: bp.line,
                        message: format!("{file}:{}: line out of range", bp.line),
                    });
                    continue;
                }
            };
            desired_lines.insert(line);

            // Already set at this file:line — reuse the cached result (and its engine id) rather than
            // adding a duplicate. INTENTIONAL/DEFERRED: a re-sent location keeps its ORIGINAL
            // condition — a changed `condition` on a re-send is NOT re-applied to the engine. The
            // condition is stored engine-side at first-set time, and conditional breakpoint
            // evaluation is a deferred Phase-5 feature, so a bare re-send of the same line is
            // (deliberately) idempotent here. If conditional-eval lands, this reuse branch must
            // diff the condition and re-set the engine breakpoint when it changed.
            if let Some(existing) = prior.get(&line) {
                results.push(existing.clone());
                new_map.insert(line, existing.clone());
                continue;
            }

            // A new line: ask the engine to set it.
            let loc = BpLoc::FileLine {
                file: file.to_string(),
                line,
            };
            let condition = bp.condition.clone();
            let outcome = self
                .call(|reply| EngineCmd::SetBreakpoint {
                    loc,
                    condition,
                    reply,
                })
                .await;
            // A source bp's unverified-error fallback echoes the requested line; a transport failure
            // propagates (aborting before any commit, so the tracked map is unchanged).
            let result = bp_result_or_continue(outcome, bp.line)?;
            // Only a verified (real engine id) result is tracked for reuse; an unverified one carries
            // no engine breakpoint, so it must not be cached as one.
            if result.verified {
                new_map.insert(line, result.clone());
            }
            results.push(result);
        }

        // Remove the engine breakpoints for lines that were tracked before but are no longer
        // requested (the lines dropped by this declarative re-send). Best-effort per id — a remove
        // failure for one stale line must not fail the whole reconcile (the worst case is a leftover
        // breakpoint, not a wrong result); a transport `Closed` aborts.
        //
        // We capture (rather than `?`-propagate) a `Closed` from the remove pass so the commit below
        // still runs: `new_map` reflects what was actually ADDED this call, and committing it keeps
        // the tracked table consistent with the engine's real state (the just-added entries are kept,
        // and the stale entries — which we were trying to remove — are dropped) before we surface the
        // transport error. `Closed` means the engine thread is permanently dead, so this is largely
        // theoretical, but it leaves no silently-wrong table.
        let remove_result: Result<(), BackendError> = async {
            for (line, old) in &prior {
                if !desired_lines.contains(line) {
                    self.remove_engine_breakpoint(old.id).await?;
                }
            }
            Ok(())
        }
        .await;

        // Commit the new per-file map under the lock (even on a `Closed` remove failure — see above).
        {
            let mut table = self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
            if new_map.is_empty() {
                table.source.remove(file);
            } else {
                table.source.insert(file.to_string(), new_map);
            }
        }
        remove_result?;
        Ok(results)
    }

    async fn set_function_breakpoints(
        &self,
        bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        // REPLACE-ALL reconciliation, same as `set_source_breakpoints` but for the single global
        // function category (DAP `setFunctionBreakpoints` is not per-file). The tool layer passes the
        // FULL desired function list; we preserve the engine id of a name already set, remove names
        // no longer requested, and add new ones. The result vector is positional with `bps` (the
        // handler takes the last). Per-bp-error-vs-`Closed` handling matches `bp_result_or_continue`:
        // an unresolvable function (the `err_bad_bp` scenario — e.g. `nonexistent_xyz`) is a
        // `verified:false` result, NOT a request error; only a dead engine thread (`Closed`) is fatal.
        let prior: HashMap<String, BreakpointResult> = {
            let table = self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
            table.function.clone()
        };

        let mut desired_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut results = Vec::with_capacity(bps.len());
        let mut new_map: HashMap<String, BreakpointResult> = HashMap::new();
        for bp in bps {
            desired_names.insert(bp.name.clone());

            // Already set for this function name — reuse the cached result (preserving the engine id).
            if let Some(existing) = prior.get(&bp.name) {
                results.push(existing.clone());
                new_map.insert(bp.name.clone(), existing.clone());
                continue;
            }

            let loc = BpLoc::Function(bp.name.clone());
            let condition = bp.condition.clone();
            let outcome = self
                .call(|reply| EngineCmd::SetBreakpoint {
                    loc,
                    condition,
                    reply,
                })
                .await;
            // A function bp has no input line; the unverified-error fallback uses line 0 (the
            // "no line" sentinel — matches lldb's function-bp default and `list_breakpoints`'
            // `line == 0` suppression).
            let result = bp_result_or_continue(outcome, 0)?;
            if result.verified {
                new_map.insert(bp.name.clone(), result.clone());
            }
            results.push(result);
        }

        // Remove the engine breakpoints for functions tracked before but no longer requested.
        // As in `set_source_breakpoints`, capture (rather than `?`-propagate) a `Closed` from the
        // remove pass so the commit below still runs — keeping the tracked table consistent with what
        // was actually added before surfacing the transport error (`Closed` = engine thread dead).
        let remove_result: Result<(), BackendError> = async {
            for (name, old) in &prior {
                if !desired_names.contains(name) {
                    self.remove_engine_breakpoint(old.id).await?;
                }
            }
            Ok(())
        }
        .await;

        {
            let mut table = self.breakpoints.lock().unwrap_or_else(|p| p.into_inner());
            table.function = new_map;
        }
        remove_result?;
        Ok(results)
    }

    // --- execution ---

    async fn cont(&self, _thread_id: i64) -> Result<StopOutcome, BackendError> {
        // LIMITATION (debuggee stdout): running the target here does NOT surface the debuggee's own
        // stdout/stderr (`printf`) through `BackendEvent::Output`. The only output the sink sees is
        // DbgEng's `IDebugOutputCallbacks` — *engine* output (ModLoad, break notices, symbol
        // diagnostics), not the child's console writes. `dbgeng-sys` launches the child
        // `CREATE_NO_WINDOW`, so its console writes go nowhere observable. Consequently the
        // `read_output` MCP tool returns engine output, not program output, under WinDbg — a known
        // difference from the lldb/DAP backend, where the launched process's stdout is piped through
        // the event stream. Surfacing real debuggee stdout needs a `dbgeng-sys` launch-path change
        // (stdout pipe redirection / console handling), tracked for a later phase.
        //
        // WinDbg/lldb `continue` semantics: block until the next stop. We marshal a `Go` with an
        // effectively-infinite budget (`u32::MAX` ms): the engine's `go` polls (~200 ms) until the
        // target stops/exits OR the cooperative interrupt flag fires (`pause`/`disconnect` trip it),
        // so an unbounded budget is the correct "block until stop" — the tool layer owns user
        // cancellation (its request token), and `pause` is what actually breaks a running target.
        //
        // `_thread_id` is intentionally ignored: WinDbg `g` resumes the WHOLE target, not a single
        // thread (DbgEng has no per-thread continue — unlike DAP's per-thread `continue`). This
        // matches the C++ windbg plugin, which likewise has no per-thread continue.
        let outcome = self
            .call(|reply| EngineCmd::Go {
                timeout_ms: u32::MAX,
                reply,
            })
            .await?;
        match outcome {
            // The normal case: the target hit a stop / exited within the budget.
            Some(stop) => Ok(stop),
            // Safety net for the (practically unreachable) ~u32::MAX-ms deadline: `pause` trips the
            // interrupt flag long before this budget could ever elapse, so the only way to land here
            // is the deadline expiring with the target still running and no interrupt — essentially
            // impossible in practice. If it ever does, fall back to `break_in()` to regain a real
            // stop-with-context (a still-running `go` returns no context) and return that.
            //
            // If `break_in` itself times out here (it carries its own ~10 s polling bound), treat
            // it like `Ok(None)`: the target is still running and needs an external `pause`/restart
            // to regain control — same recovery guidance as a cancelled cont. (No logic change; this
            // whole arm is practically unreachable.)
            None => self.call(|reply| EngineCmd::BreakIn { reply }).await,
        }
    }

    async fn step(
        &self,
        kind: StepKind,
        _thread_id: i64,
        _gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError> {
        // Map the neutral StepKind onto the engine step and marshal it. `dbgeng_sys`'s step takes
        // the same `debugger_core::StepKind` (Over/Into/Out), so this is a direct pass-through — no
        // separate dbgeng_sys::StepKind to translate.
        //
        // `_thread_id` and `_gran` are intentionally ignored, both deliberate WinDbg behavior notes:
        // - DbgEng steps the CURRENT thread (no per-thread step target; the C++ plugin stepped the
        //   current thread too), so `_thread_id` does not apply.
        // - DbgEng step is source/line-oriented (`p`/`t`/`gu`) with no separate
        //   instruction-granularity knob, so `_gran` (the DAP statement-vs-instruction hint) has no
        //   WinDbg analog and is dropped.
        self.call(|reply| EngineCmd::Step { kind, reply }).await
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
        thread_id: i64,
        start: i64,
        levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError> {
        // Marshal the call-stack walk to the engine, which produces neutral `Frame`s already
        // (index/id = frame index, symbolicated `name`, `source_path`/`line` via GetLineByOffset,
        // and the IP as the `0x{:016X}` `instruction_pointer`) — the same shape lldb's DAP mapping
        // emits, so the `backtrace` handler formats them byte-identically.
        //
        // Frame request semantics: the engine has no separate "start frame" parameter (DbgEng's
        // GetStackTrace always walks from the top), so we ask it for the frames we need —
        // `start + levels` from the top — and apply the `start`/`levels` window here, mirroring the
        // DAP `startFrame`/`levels` slicing the handler expects. `levels <= 0` means "all frames"
        // in the neutral contract (the handler only passes a positive `levels` or its 20 default,
        // but `resolve_frame_id` always passes 20); we then clamp the engine `max` to a sane bound.
        let start = start.max(0);
        // How many frames to fetch from the top: `start + levels` when a positive window is asked
        // for, else a generous cap (the engine clamps to [1, 1024]).
        let fetch_max = if levels > 0 {
            start.saturating_add(levels)
        } else {
            1024
        };

        let frames = self
            .call(|reply| EngineCmd::StackTrace {
                thread_id,
                max: fetch_max,
                reply,
            })
            .await?;

        // `total_frames` is the count of frames FETCHED in THIS request (the DAP `totalFrames`
        // analog the handler echoes), computed BEFORE the `start` slice. It is NOT the true stack
        // depth: the engine returns at most `fetch_max` (= `start + levels`) frames, so this count
        // is bounded by the fetch window, not the whole stack. Getting the true depth would cost an
        // extra unconditional walk to the engine frame cap (a separate `GetStackTrace` round-trip)
        // on every request — `Engine::stack_trace` exposes no cheaper depth — so we deliberately do
        // not pay it.
        //
        // Why this is correct for every CURRENT caller: each tool-layer caller passes `start = 0`
        // (`handle_backtrace`, `resolve_frame_id`, and the `read_memory` IP lookup all pass 0), so
        // `fetch_max == levels` and the fetched window IS the full stack (up to `levels`/the engine
        // cap) — `total_frames` equals the true depth for them.
        //
        // CAVEAT for a future `start > 0` paginating caller: with `start > 0` the engine still walks
        // from the top and caps at `start + levels`, so `total_frames` reflects only that fetched
        // prefix, NOT the full stack depth. Such a caller must not treat `total_frames` as the total
        // stack size for pagination; closing that gap needs a true-depth walk here (or an engine
        // depth query) before the `start` slice.
        let total_frames = frames.len() as i64;

        // Apply the `start` window. The engine already capped the count at `fetch_max`, so a
        // positive `levels` is honored by the fetch bound; here we only drop the leading `start`
        // frames. The engine numbers frames by their absolute index (0 = innermost), which we keep
        // as `Frame::index`/`Frame::id` so the frame-map the handler builds (index → id) stays
        // consistent across a windowed request.
        let windowed: Vec<Frame> = frames.into_iter().skip(start as usize).collect();
        Ok((windowed, total_frames))
    }

    async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, BackendError> {
        // DbgEng has no DAP `scopes` request: locals are read per frame via the symbol-group API
        // (`Engine::locals(frame_index)`). We synthesize the single scope the inspection handler
        // needs — a "Locals" group (the case-insensitive `local` scope match in
        // `inspection.rs::scope_matches`) — whose `variables_reference` ENCODES the frame so a
        // later `variables(reference)` can recover the frame index and fetch its locals.
        //
        // Encoding: `variables_reference = frame_id + 1`. The `+1` offset is load-bearing — the
        // flatten algorithm (`flatten_variables`) only treats a reference as expandable when it is
        // `> 0`, so frame 0 must not map to reference 0. `variables()` decodes it back with `- 1`.
        // `frame_id` is the engine frame index (`Frame::id == Frame::index` from `stack_trace`).
        //
        // Saturation note: `frame_id` is always a value that came from `stack_trace` via the tool
        // layer's `resolve_frame_id` (the only path that supplies a `frame_id`), so it is bounded by
        // the stack depth (≤ the engine's 1024-frame cap — `Engine::stack_trace` clamps `max` to
        // `[1, 1024]` and numbers frames `0..filled`). The `saturating_add(1)` therefore never
        // saturates in practice (frame_id ≪ i64::MAX); the saturating form is just a belt-and-braces
        // guard against an arithmetic overflow rather than a reachable path. `variables` also rejects
        // an implausibly large reference (see there), so a saturated reference would round-trip to a
        // rejected, non-expandable value rather than silently breaking the `+1`/`-1` round-trip.
        //
        // This mirrors lldb's structure: lldb returns a `Locals` `Scope` with a DAP
        // variablesReference; we return a `Locals` `Scope` with our frame-encoded reference. The
        // handler's scope match and flatten are identical for both. (lldb also surfaces Globals /
        // Registers scopes; WinDbg's locals path covers only Locals — the C++ windbg-mcp plugin
        // likewise exposed only `get_locals`, so a Globals/Registers scope has no WinDbg analog and
        // is intentionally absent.)
        let reference = frame_id.saturating_add(1);
        Ok(vec![Scope {
            name: "Locals".to_string(),
            variables_reference: reference,
        }])
    }

    async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        // Decode the frame index from the reference produced by `scopes` (`frame_id + 1`). A
        // reference `<= 0` is not one we minted (the handler only ever passes a reference it got
        // from our `scopes`, but guard anyway) — there are no nested child references under WinDbg
        // (see the LIMITATION below), so anything we did not mint has no locals to fetch.
        //
        // We also reject an implausibly large reference. A reference we mint is `frame_id + 1`, and
        // `frame_id` is bounded by the engine's frame cap (≤ 1024 — see `scopes`), so a legitimate
        // reference is ≤ 1025. `MAX_FRAME_REFERENCE` is a generous bound well above that; a reference
        // beyond it cannot be one we minted (it would imply a frame index past the cap, or a
        // saturated `frame_id + 1`), so we treat it as "no locals to fetch" rather than passing a
        // bogus frame index to the engine. This makes the `+1`/`-1` round-trip self-documenting and
        // robust even at the `i64::MAX` corner without over-engineering.
        const MAX_FRAME_REFERENCE: i64 = 1 << 20;
        if variables_reference <= 0 || variables_reference > MAX_FRAME_REFERENCE {
            return Ok(Vec::new());
        }
        let frame_index = variables_reference - 1;

        // Marshal the per-frame locals read. The engine returns neutral `Variable`s already
        // (name/value/type from the DEBUG_SCOPE_GROUP_LOCALS symbol group), with
        // `variables_reference`/`named`/`indexed` all 0.
        //
        // LIMITATION (nested expansion): WinDbg locals come back as a FLAT, top-level-only list.
        // The `dbgeng-sys` symbol-group surface (`read_scope_locals`) does not expand child members
        // (struct fields / array elements / pointee), so every `Variable` carries
        // `variables_reference = 0` and `named = indexed = 0`. This is a deliberate, design-deferred
        // gap (Plan 03 §3.4 — deep nested expansion is not in scope): the `flatten_variables`
        // algorithm sees a leaf for each local (reference 0 → no recursion, `has_children` stays
        // false), so the top level formats correctly and nothing is silently dropped — a struct
        // local simply renders with its summary `value` text and no expandable children. lldb, by
        // contrast, returns a positive `variables_reference` + a `named`/`indexed` child count for a
        // container, which the flatten then expands. Closing this gap needs a `dbgeng-sys`
        // child-symbol-expansion path (ExpandSymbol / GetSymbolEntryInformation), tracked for a
        // later phase; until then WinDbg `variables` is intentionally one level deep.
        self.call(|reply| EngineCmd::Locals { frame_index, reply })
            .await
    }

    async fn evaluate(
        &self,
        expr: &str,
        _frame_id: Option<i64>,
        mode: EvalMode,
    ) -> Result<EvalResult, BackendError> {
        // `_frame_id` is intentionally ignored: the engine evaluates in the CURRENT scope (the C++
        // windbg-mcp plugin evaluated in the current frame too — there is no per-call frame-scoped
        // evaluate on this surface; the active frame is set by the last stop/stack walk).
        //
        // OBSERVABLE PARITY DIFFERENCE (deliberate): the lldb/DAP backend HONORS `frameId` —
        // `evaluate` there runs in the requested frame's scope, so the same expression can yield a
        // different value per frame. WinDbg here always evaluates in the innermost (current) stopped
        // frame regardless of `_frame_id`, so a non-current `frame_id` is silently evaluated in the
        // current scope, not the requested one. This is a real behavior difference a future reader
        // must know is intentional, not an oversight. Closing it needs DbgEng frame-scope switching
        // around the evaluate (`IDebugSymbols::SetScope` / `SetScopeFrameByIndex` to the requested
        // frame, evaluate, then `ResetScope` — the same scope dance `Engine::locals` already does
        // for per-frame locals), tracked for a later phase.
        match mode {
            // Expression evaluation: the C++ plugin runs `?? <expr>` (the readable C++-expression
            // evaluator) through Execute. `Engine::evaluate` already wraps `expr` as `?? expr` and
            // returns the trimmed output as the `result` (type/var_reference left empty/0 — the
            // `??` text carries the type inline, and there is no structured child reference). The
            // `evaluate` handler renders `result`/`type` and only adds children when
            // `variables_reference > 0`, so a 0 reference formats identically to an lldb scalar.
            EvalMode::Expression => {
                self.call(|reply| EngineCmd::Evaluate {
                    expr: expr.to_string(),
                    reply,
                })
                .await
            }
            // Repl / raw-command mode (the `run_command` tool's escape hatch): run the command
            // verbatim through `Engine::execute`. `supports_command_repl_mode()` is `true`, so the
            // handler passes the RAW command with NO backtick prefix (the backtick is only for
            // legacy lldb-vscode). We map the captured command output into the `result` field and
            // leave type/var_reference empty/0 (a raw command has no typed value or children).
            EvalMode::Repl => {
                let output = self
                    .call(|reply| EngineCmd::Execute {
                        command: expr.to_string(),
                        reply,
                    })
                    .await?;
                Ok(EvalResult {
                    result: output,
                    ty: String::new(),
                    variables_reference: 0,
                })
            }
        }
    }

    async fn read_memory(&self, address: &str, count: i64) -> Result<MemoryRead, BackendError> {
        // Parse the neutral address string → u64. The tool layer passes a hex (`0x…`) or decimal
        // address; accept both. A malformed address is a user-facing error (carried in the closest
        // neutral variant), not a transport failure.
        let parsed = parse_address(address)?;

        // A non-positive count reads nothing (the tool layer clamps to a positive size; guard the
        // i64→usize conversion so a negative value cannot wrap to a huge allocation).
        let size = usize::try_from(count.max(0)).unwrap_or(0);

        // The engine reads up to `size` bytes (truncating to the bytes actually read at a page
        // boundary) and echoes the address as `0x{:016X}`, exactly the `MemoryRead` shape the
        // `read_memory` handler formats (it base64-encodes `data` and reports `bytes_read`).
        self.call(|reply| EngineCmd::ReadMemory {
            address: parsed,
            size,
            reply,
        })
        .await
    }

    async fn disassemble(
        &self,
        address: &str,
        count: i64,
    ) -> Result<Vec<Instruction>, BackendError> {
        // Parse the neutral address string → u64 (hex or decimal), same as `read_memory`.
        let parsed = parse_address(address)?;

        // Honor the `count` passed by the tool layer verbatim (the project-wide
        // `instruction_count = 20` default is a tool-layer concern; the backend disassembles
        // exactly what it is asked for). The engine renders each instruction's address
        // (`0x{:016X}`) and mnemonic text into neutral `Instruction`s (bytes/symbol left empty —
        // the text line carries the mnemonic), the shape the `disassemble` handler formats.
        self.call(|reply| EngineCmd::Disassemble {
            address: parsed,
            count,
            reply,
        })
        .await
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

    async fn open_dump(&self, path: &str) -> Result<DumpOutcome, BackendError> {
        // Direct marshal: the engine opens the dump and returns the neutral `DumpOutcome` already
        // (its `stop`/`crash_location` come back unchanged); `call` maps an `EngineError` →
        // `BackendError`.
        //
        // The connect-point flow is the TOOL layer's job, NOT this method's: the `open_crash_dump`
        // handler (task 4.3) `connect()`s a FRESH WinDbg engine thread for the dump session and
        // then calls `open_dump` on that backend — exactly like `launch` runs on an
        // already-connected backend's engine thread. This method only marshals the `OpenDump` op
        // onto the (already-connected) engine thread; it neither spawns nor selects a backend.
        self.call(|reply| EngineCmd::OpenDump {
            path: path.to_string(),
            reply,
        })
        .await
    }

    async fn attach_kernel(&self, connection: &str) -> Result<AttachOutcome, BackendError> {
        // Marshal the kernel attach (the engine returns a `StopOutcome`) and map it onto the
        // neutral `AttachOutcome` via the same `stop_to_attach` helper `attach` uses — kernel
        // attach is just another path that produces a first-stop outcome.
        //
        // The INFINITE-wait caveat lives in the ENGINE (task 4.1): `Engine::attach_kernel` blocks
        // (uncancellably) until the kernel connection produces its first break, since DbgEng's
        // kernel-attach has no bounded wait. This method only marshals + maps; it adds no timeout
        // of its own (the engine thread is occupied for the duration, and teardown for a stuck
        // kernel wait is the R2 orphan-thread case, Phase 5).
        let outcome = self
            .call(|reply| EngineCmd::AttachKernel {
                connection: connection.to_string(),
                reply,
            })
            .await?;
        Ok(stop_to_attach(outcome))
    }

    async fn analyze(&self) -> Result<String, BackendError> {
        // Direct marshal: the engine runs `!analyze -v` and returns its raw text; `call` maps an
        // `EngineError` → `BackendError`.
        //
        // State guarding is the TOOL layer's job, NOT this method's: the `analyze_crash` handler
        // (`mcp-tools::handlers::windbg::handle_analyze_crash`) owns the
        // `check_state(&[State::Stopped])` guard (confirmed present in task 4.1's review), so
        // `analyze` itself does not re-check state — it only marshals the `Analyze` op.
        self.call(|reply| EngineCmd::Analyze { reply }).await
    }
}
