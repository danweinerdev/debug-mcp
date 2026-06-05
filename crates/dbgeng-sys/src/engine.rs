//! `Engine` — the owned, safe handle over the six DbgEng COM interfaces.
//!
//! [`Engine::create`] performs the C++ `DebugEngine` constructor's prologue: `DebugCreate`
//! yields an `IDebugClient5`, the other five interfaces are obtained from it via
//! `QueryInterface` (`Interface::cast`), and the symbol path/options are configured for fast
//! module loads. The remaining engine operations (launch/go/step/breakpoints/inspection) are
//! added in tasks 2.2–2.5.
//!
//! ## Refcount safety
//!
//! Each field is a `windows`-crate smart pointer (e.g. [`IDebugClient5`]), **not** a raw COM
//! pointer. These types `AddRef` on `Clone` and `Release` on `Drop` automatically, so the
//! C++ raw-pointer/`unique_ptr` manual-`Release` footgun (see `engine.cpp`'s destructor, which
//! has to `Release()` all six by hand) does not apply here: dropping an `Engine` releases all
//! six interfaces in field order with no hand-written cleanup.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use debugger_core::{DumpOutcome, StepKind, StopInfo, StopOutcome};
use windows::core::{s, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
};
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DebugCreate, IDebugClient5, IDebugControl, IDebugControl4, IDebugDataSpaces4,
    IDebugEventCallbacks, IDebugOutputCallbacks, IDebugRegisters2, IDebugSymbols3,
    IDebugSystemObjects4, DEBUG_ATTACH_DEFAULT, DEBUG_CREATE_PROCESS_OPTIONS,
    DEBUG_END_ACTIVE_DETACH, DEBUG_END_PASSIVE, DEBUG_ENGOPT_INITIAL_BREAK, DEBUG_EXECUTE_DEFAULT,
    DEBUG_INTERRUPT_ACTIVE, DEBUG_MODNAME_MODULE, DEBUG_OUTCTL_THIS_CLIENT, DEBUG_STATUS_BREAK,
    DEBUG_STATUS_GO, DEBUG_STATUS_GO_HANDLED, DEBUG_STATUS_GO_NOT_HANDLED,
    DEBUG_STATUS_NO_DEBUGGEE, DEBUG_STATUS_STEP_BRANCH, DEBUG_STATUS_STEP_INTO,
    DEBUG_STATUS_STEP_OVER,
};
use windows::Win32::System::Diagnostics::Debug::SYMOPT_NO_IMAGE_SEARCH;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, CREATE_NO_WINDOW, DEBUG_ONLY_THIS_PROCESS,
};

use crate::callbacks::{self, CallbackState, OutputSink};
use crate::error::EngineError;

/// A minimal launch request (dbgeng-sys-local). The rich `debugger_core::LaunchSpec` → this
/// mapping (breakpoints, env, stop-on-entry) lives in `windbg-backend` (Phase 3); here we only
/// need the program, its arguments, and an optional working directory.
pub struct LaunchReq {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

/// The outcome of one `wait_for_event`: either an event fired (with the neutral stop outcome) or
/// the wait hit its deadline (`S_FALSE`) with no event — the distinction `WaitForEvent`'s
/// `Result<()>` wrapper collapses, recovered here from the raw `HRESULT`.
pub(crate) enum WaitResult {
    Event(StopOutcome),
    TimedOut,
}

/// An owned DbgEng engine: the six COM interfaces queried off a single `IDebugClient5`.
///
/// The fields are `windows`-crate smart pointers, so refcounting is automatic (see the module
/// docs). The engine is single-thread-confined by design (every method that drives DbgEng will
/// take `&mut self`); this scaffolding only exposes [`Engine::create`].
pub struct Engine {
    /// The root client interface returned by `DebugCreate`.
    client: IDebugClient5,
    /// Execution control: `WaitForEvent`, `SetExecutionStatus`, `Execute`, `Evaluate`, …
    control: IDebugControl4,
    /// Symbol/module queries and symbol-path configuration.
    symbols: IDebugSymbols3,
    /// Virtual/physical memory access (`ReadVirtual`, …).
    data_spaces: IDebugDataSpaces4,
    /// Register access.
    registers: IDebugRegisters2,
    /// Process/thread/system enumeration and context switching.
    system_objects: IDebugSystemObjects4,
    /// The shared state the registered callbacks (below) write and this `Engine` reads:
    /// captured output, the output sink, and the last-stop info. `Arc<Mutex<…>>` because DbgEng
    /// invokes the callbacks from its own internal threads (see `callbacks` module docs).
    callback_state: Arc<Mutex<CallbackState>>,
    /// The registered output/event callback interfaces, retained for the engine's lifetime.
    ///
    /// `SetOutputCallbacks`/`SetEventCallbacks` `AddRef` these, so DbgEng holds its own reference
    /// while they are registered; we keep ours too so the objects (and the shared `Arc` they
    /// carry) cannot be released out from under an in-flight callback, and so a future teardown
    /// can re-clear them. They are `Drop`-released with the rest of the interfaces in field
    /// order — no manual `Release` (unlike the C++ destructor's hand-written cleanup).
    _output_callbacks: IDebugOutputCallbacks,
    _event_callbacks: IDebugEventCallbacks,
    /// Whether the active session is a crash dump (set by `open_dump` in Phase 4). A dump cannot
    /// be resumed; `ensure_runnable` (called by go/step in 2.4) refuses when this is set.
    is_dump: bool,
    /// The cooperative interrupt flag shared with every [`InterruptHandle`] minted by
    /// [`Engine::interrupt_handle`]. `go` resets it to `false` at entry (it is the only writer at
    /// that point), then its 200 ms poll loop observes a later `true` written off-thread by a
    /// handle's `interrupt()` and converts it into a real `SetInterrupt`-driven break. This is the
    /// crate's *only* shared mutable state reachable from another thread; all access is through a
    /// single `AtomicBool` with `Acquire`/`Release` ordering (see [`Engine::go`] and
    /// [`InterruptHandle::interrupt`]) — no DbgEng COM call ever crosses a thread boundary.
    interrupt_flag: Arc<AtomicBool>,
}

/// A `Send` interrupt token for one [`Engine`], the *only* part of this crate that crosses a
/// thread boundary. It carries a clone of the engine's `interrupt_flag` `Arc<AtomicBool>` and
/// nothing else: calling [`InterruptHandle::interrupt`] from any thread sets the flag, which the
/// engine's own [`Engine::go`] poll loop (running on the engine-owning thread) observes within one
/// 200 ms poll and turns into a `SetInterrupt`-driven break.
///
/// ## Why flag-only (the R4 decision)
///
/// `IDebugControl::SetInterrupt` is in fact documented as callable from a thread other than the
/// one that owns the DbgEng session — it is the one DbgEng method intended for that — so a
/// cross-thread `SetInterrupt` would be defensible. We deliberately do **not** take that path:
/// this handle holds no COM interface at all, so *no* DbgEng pointer is ever touched off the
/// engine-owning thread, and the crate's confinement invariant ("every DbgEng call runs on the
/// engine's thread") is preserved with zero exceptions to reason about. The cost is bounded
/// latency: the engine's `go` loop polls every 200 ms, so an off-thread `interrupt()` is acted on
/// within ≤200 ms — well inside any human/agent interaction budget. Because the handle is pure
/// atomic state it is trivially `Send` with no `unsafe`.
#[derive(Clone)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
}

impl InterruptHandle {
    /// Request that the engine's in-flight `go` break in. Sets the shared flag with `Release`
    /// ordering so the engine thread's `Acquire` load in `go`'s poll loop observes it. Safe to
    /// call from any thread and at any time (a request while nothing is running is simply consumed
    /// — `go` resets the flag at entry, closing the spurious-interrupt race).
    pub fn interrupt(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

impl Engine {
    /// Create the engine: `DebugCreate` → five `QueryInterface`s → symbol-path/options setup.
    ///
    /// Mirrors the prologue of the C++ `DebugEngine` constructor (`engine.cpp`): the symbol
    /// path is set to a server cache (`"srv*"`) and `SYMOPT_NO_IMAGE_SEARCH` is OR'd into the
    /// symbol options so DbgEng does not recursively walk directory trees searching for image
    /// files during module load (R5). Callbacks are intentionally *not* registered here — they
    /// arrive in task 2.2.
    pub fn create() -> Result<Engine, EngineError> {
        // SAFETY: `DebugCreate` is the documented DbgEng entry point. It allocates a fresh COM
        // object and returns it through the requested interface; the `windows` crate wraps the
        // out-pointer into a ref-counted `IDebugClient5` (one reference owned by us), so there
        // is no aliasing and `Drop` will `Release` it.
        //
        // Thread-confinement invariant (load-bearing for the whole crate): every subsequent
        // operation on this client — and on all five interfaces QI'd from it below — must run on
        // the SAME OS thread for the engine's lifetime. DbgEng does not synchronize its internal
        // state across threads. `Engine` is `!Send`/`!Sync` (the `IDebug*` smart pointers wrap
        // `NonNull`), which enforces single-thread ownership at the type level; the Phase-3
        // `windbg-backend` runs the engine on one dedicated thread accordingly.
        let client: IDebugClient5 = unsafe { DebugCreate::<IDebugClient5>() }
            .map_err(|e| EngineError::op("DebugCreate", e))?;

        // `Interface::cast` is `QueryInterface`: it `AddRef`s the returned interface, so each of
        // the six fields owns an independent reference to the same underlying engine object.
        let control: IDebugControl4 = client
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugControl4)", e))?;
        let symbols: IDebugSymbols3 = client
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugSymbols3)", e))?;
        let data_spaces: IDebugDataSpaces4 = client
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugDataSpaces4)", e))?;
        let registers: IDebugRegisters2 = client
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugRegisters2)", e))?;
        let system_objects: IDebugSystemObjects4 = client
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugSystemObjects4)", e))?;

        // Symbol path: a Microsoft-symbol-server-style cache. `IDebugSymbols3::SetSymbolPath`
        // takes a `PCSTR` (ANSI), so we pass an `s!` literal (the wide variant is
        // `SetSymbolPathWide`/`PCWSTR`).
        // SAFETY: `symbols` is a live `IDebugSymbols3` from the QI above. `s!("srv*")` is a
        // 'static NUL-terminated ANSI string, so the `PCSTR` is valid for the duration of the
        // call (the engine copies the path); no ownership transfers.
        unsafe { symbols.SetSymbolPath(s!("srv*")) }
            .map_err(|e| EngineError::op("SetSymbolPath", e))?;

        // Avoid slow recursive image search (FindExecutableImageExW / EnumDirTreeW) during
        // module load: read the current options and OR in SYMOPT_NO_IMAGE_SEARCH (R5).
        // SAFETY: both calls operate on the live `symbols` interface; `GetSymbolOptions` writes
        // a `u32` out-param the crate returns by value, and `SetSymbolOptions` takes that `u32`
        // by value — no pointers escape.
        let opts = unsafe { symbols.GetSymbolOptions() }
            .map_err(|e| EngineError::op("GetSymbolOptions", e))?;
        unsafe { symbols.SetSymbolOptions(opts | SYMOPT_NO_IMAGE_SEARCH) }
            .map_err(|e| EngineError::op("SetSymbolOptions", e))?;

        // Build and register the output + event callbacks (port of the C++ constructor's
        // `SetEventCallbacks`/`SetOutputCallbacks`). `build` creates the shared `CallbackState`
        // and both COM objects over it; we register the interfaces and retain everything.
        //
        // Ownership/refcount: `SetOutputCallbacks`/`SetEventCallbacks` `AddRef` the interface
        // they receive, so DbgEng holds its own reference for as long as they are registered.
        // We pass `&output_cb`/`&event_cb` (a borrowing `Param`) and store the owned interfaces
        // in `Engine` below, so our reference outlives the registration and the shared `Arc`
        // cannot be released while a callback is in flight. (The C++ had to balance this by hand
        // in its destructor; here `Drop` releases both interfaces automatically.)
        let (callback_state, output_cb, event_cb) = callbacks::build();
        // SAFETY: `client` is the live `IDebugClient5` from `DebugCreate`. The setters take a
        // borrowed COM interface (`&output_cb`/`&event_cb`) and `AddRef` it; both objects are
        // moved into the returned `Engine` immediately below, so they outlive the call and the
        // registration. No raw pointers escape; the registration is on the engine-owning thread.
        unsafe { client.SetOutputCallbacks(&output_cb) }
            .map_err(|e| EngineError::op("SetOutputCallbacks", e))?;
        // SAFETY: same invariants as `SetOutputCallbacks` above; `event_cb` is a live
        // `IDebugEventCallbacks` retained in `Engine` for the engine's lifetime.
        unsafe { client.SetEventCallbacks(&event_cb) }
            .map_err(|e| EngineError::op("SetEventCallbacks", e))?;

        Ok(Engine {
            client,
            control,
            symbols,
            data_spaces,
            registers,
            system_objects,
            callback_state,
            _output_callbacks: output_cb,
            _event_callbacks: event_cb,
            is_dump: false,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The current DbgEng execution status (`IDebugControl::GetExecutionStatus`, one of the
    /// `DEBUG_STATUS_*` values). A trivial read used to prove the control interface is wired;
    /// the full execution surface (`go`/`step`/…) lands in task 2.4. Takes `&mut self` like
    /// every operation that drives DbgEng (the engine is not re-entrant — the `&mut` discipline
    /// serializes calls on the owning thread).
    pub fn execution_status(&mut self) -> Result<u32, EngineError> {
        // SAFETY: `self.control` is a live `IDebugControl4` obtained at `create`. The call only
        // reads a `u32` out-param (returned by value); no pointers cross the FFI boundary.
        unsafe { self.control.GetExecutionStatus() }
            .map_err(|e| EngineError::op("GetExecutionStatus", e))
    }

    /// Helper: lock the shared callback state, recovering from a poisoned mutex (a callback
    /// panicked while holding it) rather than re-panicking on the engine thread.
    fn callback_state(&self) -> std::sync::MutexGuard<'_, CallbackState> {
        self.callback_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Install (or replace) the output sink: a closure invoked for every output line DbgEng
    /// emits, with the line's `OutputKind` and text, *in addition to* the captured buffer.
    /// The closure runs on whatever thread DbgEng dispatches output from, so it must be `Send`.
    /// Phase 3 wires this onto a `BackendEvent` channel; here it backs the live output-capture
    /// test.
    pub fn set_output_sink(&mut self, sink: OutputSink) {
        self.callback_state().set_sink(sink);
    }

    /// Drain and return the captured output accumulated since the last drain (the `GetAndClear`
    /// analog). Leaves the buffer empty.
    pub fn take_output(&mut self) -> String {
        self.callback_state().take_output()
    }

    /// The human-readable reason for the most recent stop (empty until the debuggee first
    /// stops). Mirrors the C++ `GetLastStopReason`.
    pub fn last_stop_reason(&self) -> String {
        self.callback_state().last_stop_reason()
    }

    /// The id of the most recently hit breakpoint, if any (set by the `Breakpoint` event).
    pub fn last_breakpoint_id(&self) -> Option<u32> {
        self.callback_state().last_breakpoint_id()
    }

    /// The offset (address) of the most recently hit breakpoint, if any.
    pub fn last_breakpoint_offset(&self) -> Option<u64> {
        self.callback_state().last_breakpoint_offset()
    }

    /// The code of the most recently recorded exception, if any (set by the `Exception` event).
    pub fn last_exception_code(&self) -> Option<u32> {
        self.callback_state().last_exception_code()
    }

    /// The faulting address of the most recently recorded exception, if any.
    pub fn last_exception_address(&self) -> Option<u64> {
        self.callback_state().last_exception_address()
    }

    /// The debuggee's exit code, once it has exited (set by the `ExitProcess` event).
    pub fn last_exit_code(&self) -> Option<u32> {
        self.callback_state().last_exit_code()
    }

    /// Run a raw DbgEng command line through `IDebugControl::Execute`, routed to *this client*
    /// so its output flows through our registered [`callbacks::OutputCallbacks`]. This is the
    /// minimal command path the task-2.2 output-capture test needs (no-target commands such as
    /// `version`/`.echo`); the full neutral `execute()` surface lands in task 2.5, which will
    /// supersede this. `#[cfg(test)]` + `pub(crate)`: it exists only to drive the 2.2 test and
    /// never forms part of the crate's public (or even non-test) surface.
    #[cfg(test)]
    pub(crate) fn execute_raw(&mut self, command: PCSTR) -> Result<(), EngineError> {
        // SAFETY: `self.control` is the live `IDebugControl4` from `create`. `command` is a
        // NUL-terminated ANSI `PCSTR` owned by the caller for the duration of the call (the
        // engine copies it). `DEBUG_OUTCTL_THIS_CLIENT` routes output to this client's output
        // callbacks; no pointers are retained past the call.
        unsafe {
            self.control
                .Execute(DEBUG_OUTCTL_THIS_CLIENT, command, DEBUG_EXECUTE_DEFAULT)
        }
        .map_err(|e| EngineError::op("Execute", e))
    }

    // --- lifecycle (task 2.3) ---

    /// Launch `req.program` (with `args`/`cwd`) under the debugger and stop at the initial loader
    /// breakpoint. Ports the C++ `LaunchProcess`: `AddEngineOptions(INITIAL_BREAK)` →
    /// `CreateProcess2(DEBUG_ONLY_THIS_PROCESS | CREATE_NO_WINDOW)` → wait for the loader break →
    /// `RemoveEngineOptions(INITIAL_BREAK)` (mandatory — leaving it set re-breaks every `go`) →
    /// force-load the exe's symbols (`Reload "/f <module>"`). Returns the initial-break stop.
    pub fn launch(&mut self, req: &LaunchReq) -> Result<StopOutcome, EngineError> {
        // SAFETY: `self.control` is the live control interface; `AddEngineOptions` takes a u32 flag.
        unsafe { self.control.AddEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) }
            .map_err(|e| EngineError::op("AddEngineOptions(INITIAL_BREAK)", e))?;

        // Quote the program so a path containing spaces (e.g. `C:\Program Files\...`) is parsed
        // as a single token by CreateProcess2 (the C++ oracle did not quote — this fixes that).
        let mut cmdline = format!("\"{}\"", req.program);
        for arg in &req.args {
            cmdline.push(' ');
            cmdline.push_str(arg);
        }
        let cmdline = CString::new(cmdline).map_err(|_| {
            EngineError::engine("launch: program/args contain an interior NUL byte")
        })?;
        let cwd = match &req.cwd {
            Some(d) => Some(CString::new(d.clone()).map_err(|_| {
                EngineError::engine("launch: working directory contains an interior NUL byte")
            })?),
            None => None,
        };
        let opts = DEBUG_CREATE_PROCESS_OPTIONS {
            CreateFlags: DEBUG_ONLY_THIS_PROCESS.0 | CREATE_NO_WINDOW.0,
            ..Default::default()
        };

        // SAFETY: `self.client` is live. `cmdline`/`cwd` are NUL-terminated and outlive the call
        // (DbgEng copies them); `&opts` is a valid `DEBUG_CREATE_PROCESS_OPTIONS` for its declared
        // size; a null environment `PCSTR` inherits ours; a null working dir uses the default. No
        // pointers are retained past the call.
        unsafe {
            self.client.CreateProcess2(
                0,
                PCSTR(cmdline.as_ptr().cast()),
                &opts as *const DEBUG_CREATE_PROCESS_OPTIONS as *const core::ffi::c_void,
                core::mem::size_of::<DEBUG_CREATE_PROCESS_OPTIONS>() as u32,
                cwd.as_ref()
                    .map(|c| PCSTR(c.as_ptr().cast()))
                    .unwrap_or(PCSTR::null()),
                PCSTR::null(),
            )
        }
        .map_err(|e| EngineError::op("CreateProcess2", e))?;

        let wait = self.wait_for_event(10_000);

        // Clear INITIAL_BREAK regardless of the wait result so later `go`/`step` don't re-break.
        // SAFETY: live control interface; u32 flag.
        let removed = unsafe { self.control.RemoveEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) };

        // Surface a wait failure first (most important), then a RemoveEngineOptions failure
        // (leaving INITIAL_BREAK set would re-break every `go`) — *before* the timeout branch, so
        // neither error is dropped on any path.
        let outcome = wait?;
        removed.map_err(|e| EngineError::op("RemoveEngineOptions(INITIAL_BREAK)", e))?;
        let outcome = match outcome {
            WaitResult::Event(o) => o,
            WaitResult::TimedOut => {
                return Err(EngineError::engine(
                    "launch: timed out waiting for the initial breakpoint",
                ))
            }
        };

        // Force-load the exe's symbols so breakpoints resolve (best-effort; mirrors C++ Reload /f).
        self.reload_main_module();

        // The loader break arrives as the 0x80000003 exception; relabel it "Initial breakpoint"
        // for parity with the C++ server's stop reason.
        Ok(with_reason(outcome, "Initial breakpoint"))
    }

    /// Attach to an already-running process by pid and stop at the initial break. Ports the C++
    /// `AttachToProcess`: best-effort `SeDebugPrivilege` → `AddEngineOptions(INITIAL_BREAK)` →
    /// `AttachProcess(DEBUG_ATTACH_DEFAULT)` → wait → `RemoveEngineOptions`.
    pub fn attach_pid(&mut self, pid: u32) -> Result<StopOutcome, EngineError> {
        // Needed only to attach to processes owned by another user / elevated; best-effort
        // (attaching to our own child does not require it), so its failure is not fatal.
        let _ = enable_debug_privilege();

        // SAFETY: live control interface; u32 flag.
        unsafe { self.control.AddEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) }
            .map_err(|e| EngineError::op("AddEngineOptions(INITIAL_BREAK)", e))?;
        // SAFETY: live client; attaches the local engine (`server = 0`) to `pid`.
        unsafe { self.client.AttachProcess(0, pid, DEBUG_ATTACH_DEFAULT) }
            .map_err(|e| EngineError::op("AttachProcess", e))?;

        let wait = self.wait_for_event(10_000);
        // SAFETY: live control interface; u32 flag.
        let removed = unsafe { self.control.RemoveEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) };

        // Surface the wait error, then the RemoveEngineOptions error, before the timeout branch
        // (see `launch`) so neither is dropped.
        let outcome = wait?;
        removed.map_err(|e| EngineError::op("RemoveEngineOptions(INITIAL_BREAK)", e))?;
        match outcome {
            WaitResult::Event(o) => Ok(o),
            WaitResult::TimedOut => Err(EngineError::engine(
                "attach: timed out waiting for the initial break",
            )),
        }
    }

    /// End the session. Ports the C++ `Detach`: a live session uses
    /// `EndSession(DEBUG_END_ACTIVE_DETACH)` (detaches AND releases module file mappings, so the
    /// target's image is not left locked — `DetachProcesses` would leave locks); a dump uses
    /// `DEBUG_END_PASSIVE`. The engine is normally dropped right after detach (Phase 3 model), so
    /// no in-place state reset beyond clearing `is_dump` is required.
    ///
    /// The dump-vs-live choice reads the engine's own `is_dump` (set by `open_dump` in Phase 4),
    /// not a caller parameter, so the flag can never disagree with the actual session kind.
    pub fn detach(&mut self) -> Result<(), EngineError> {
        let flags = if self.is_dump {
            DEBUG_END_PASSIVE
        } else {
            DEBUG_END_ACTIVE_DETACH
        };
        // SAFETY: live client; `flags` is a documented DEBUG_END_* constant.
        unsafe { self.client.EndSession(flags) }.map_err(|e| EngineError::op("EndSession", e))?;
        self.is_dump = false;
        Ok(())
    }

    // --- execution control (task 2.4) ---

    /// Mint a `Send` [`InterruptHandle`] for this engine. The handle shares this engine's
    /// `interrupt_flag` (a clone of the same `Arc<AtomicBool>`), so a `handle.interrupt()` on any
    /// thread is the exact flag `go`'s poll loop reads. See [`InterruptHandle`] for the R4 (flag-
    /// only, no cross-thread COM) rationale. Multiple handles may be minted; they all share the
    /// one flag.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle {
            flag: Arc::clone(&self.interrupt_flag),
        }
    }

    /// Resume the target and run until it stops, is interrupted, or `timeout_ms` elapses. Ports
    /// the C++ `Go`: reset the interrupt flag, `SetExecutionStatus(DEBUG_STATUS_GO)`, then poll
    /// `wait_for_event(200)` so the loop stays responsive to an off-thread interrupt.
    ///
    /// Return contract (the R3 still-running case): `Ok(Some(stop))` when the target stopped or
    /// exited; `Ok(None)` when `timeout_ms` elapsed with the target **still running**. There is no
    /// "running" variant of the neutral `StopOutcome`, so `None` carries that state — `Option` is
    /// chosen over a bespoke enum because it adds no type and reads naturally ("maybe a stop").
    ///
    /// After a `None` (still-running) return the engine's `WaitForEvent` loop has stopped, so it
    /// holds **no valid stop context** (R3 — the C++ `S_FALSE` limitation): the target is running
    /// freely and the engine cannot answer stack/register/locals queries. The caller must call
    /// [`Engine::break_in`] (or `interrupt()` a handle and re-`go`) to regain a real break with
    /// context before inspecting. Phase 3's `windbg-backend` maps `None` onto the neutral
    /// "continue timed out" path.
    ///
    /// The interrupt flag is reset to `false` at entry — `go` is the sole writer of `false`, and
    /// resetting here (rather than after the loop) closes the spurious-interrupt race: a stale
    /// `true` left by a previous run cannot break this run before it starts.
    ///
    /// Timing/contract details:
    /// - `timeout_ms` is a *budget*, not a hard wall-clock deadline: the 200 ms poll granularity
    ///   means the actual elapsed time may exceed `timeout_ms` by up to one ~200 ms slice plus OS
    ///   scheduling jitter. `timeout_ms = 0` means "return almost immediately if nothing is ready"
    ///   (still servicing a pending interrupt); it does NOT mean "run forever" — pass a large
    ///   value for a long budget.
    /// - When an off-thread interrupt fires, `go` drives the break in-thread via the same loop as
    ///   [`Engine::break_in`], so it may block for up to an additional ~10 s (50 × 200 ms) before
    ///   returning the regained stop.
    /// - An `Err` from the interrupt-break path (break-in exhausted its ~10 s budget) leaves the
    ///   target **still running**, just like `Ok(None)`: treat it the same for recovery (the
    ///   engine holds no valid context; `break_in` again or restart). A COM `Err` likewise leaves
    ///   engine state undefined from the caller's view.
    pub fn go(&mut self, timeout_ms: u32) -> Result<Option<StopOutcome>, EngineError> {
        self.ensure_runnable()?;

        // Reset the shared flag before resuming: any interrupt request that arrives from here on
        // is for *this* run. `Relaxed` is sufficient for the reset (the engine thread is the only
        // writer of `false`, and the subsequent `Acquire` loads below establish the ordering with
        // off-thread `Release` stores); we use `Release` for symmetry and to flush eagerly.
        self.interrupt_flag.store(false, Ordering::Release);

        // SAFETY: `self.control` is a live `IDebugControl4` from `create`; `SetExecutionStatus`
        // takes the documented `DEBUG_STATUS_GO` u32 by value. No pointers cross the boundary.
        unsafe { self.control.SetExecutionStatus(DEBUG_STATUS_GO) }
            .map_err(|e| EngineError::op("SetExecutionStatus(GO)", e))?;

        // Poll with short timeouts so an off-thread interrupt is acted on within one 200 ms slice.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        while Instant::now() < deadline && !self.interrupt_flag.load(Ordering::Acquire) {
            match self.wait_for_event(200)? {
                WaitResult::Event(o) => return Ok(Some(o)),
                WaitResult::TimedOut => {} // keep polling
            }
        }

        if self.interrupt_flag.load(Ordering::Acquire) {
            // An interrupt was requested off-thread. The flag carried the cross-thread signal;
            // now drive the real DbgEng interrupt from THIS (engine-owning) thread, where every
            // COM call stays confined. We use the same robust re-issue loop as `break_in` (a
            // single `SetInterrupt` + one wait can be raced — the interrupt is consumed without
            // surfacing a break — so re-issuing per poll is the reliable recovery; the C++ `Go`
            // used a single wait and deferred the loop to its caller's `Break`, we fold that loop
            // in here so an interrupted `go` returns a real stop directly).
            return self.break_loop().map(Some);
        }

        // Overall deadline exceeded with no stop and no interrupt: the target is still running.
        // The engine now holds no valid stop context (R3); the caller must `break_in` to regain it.
        Ok(None)
    }

    /// Step the target one source step and return the resulting stop. Ports the C++
    /// `StepOver`/`StepInto`/`StepOut`: Over/Into set the matching `DEBUG_STATUS_STEP_*` execution
    /// status; Out has no DbgEng status, so it runs the `gu` ("go up") command — exactly the C++
    /// approach. All three then wait (a generous 10 s) for the step to land.
    pub fn step(&mut self, kind: StepKind) -> Result<StopOutcome, EngineError> {
        self.ensure_runnable()?;

        match kind {
            StepKind::Over => {
                // SAFETY: live control interface; documented `DEBUG_STATUS_STEP_OVER` u32 flag.
                unsafe { self.control.SetExecutionStatus(DEBUG_STATUS_STEP_OVER) }
                    .map_err(|e| EngineError::op("SetExecutionStatus(STEP_OVER)", e))?;
            }
            StepKind::Into => {
                // SAFETY: live control interface; documented `DEBUG_STATUS_STEP_INTO` u32 flag.
                unsafe { self.control.SetExecutionStatus(DEBUG_STATUS_STEP_INTO) }
                    .map_err(|e| EngineError::op("SetExecutionStatus(STEP_INTO)", e))?;
            }
            StepKind::Out => {
                // DbgEng has no `DEBUG_STATUS_STEP_OUT`; the C++ oracle runs the `gu` command to
                // step out of the current frame. Route output to this client's callbacks.
                // SAFETY: live control interface; `s!("gu")` is a 'static NUL-terminated ANSI
                // string valid for the call (the engine copies it); `DEBUG_OUTCTL_THIS_CLIENT`
                // and `DEBUG_EXECUTE_DEFAULT` are documented u32 flags. No pointers are retained.
                unsafe {
                    self.control
                        .Execute(DEBUG_OUTCTL_THIS_CLIENT, s!("gu"), DEBUG_EXECUTE_DEFAULT)
                }
                .map_err(|e| EngineError::op("Execute(gu)", e))?;
            }
        }

        match self.wait_for_event(10_000)? {
            WaitResult::Event(o) => Ok(o),
            WaitResult::TimedOut => Err(EngineError::engine("step: timed out")),
        }
    }

    /// Forcibly break a running target back into a state with a valid stop context — the recovery
    /// the agent runs after a `go` still-running timeout (R3). Ports the C++ `Break`:
    /// `SetInterrupt(DEBUG_INTERRUPT_ACTIVE)` then up to ~50 × `wait_for_event(200)`, re-issuing
    /// the interrupt each poll (it can be consumed without surfacing a break) until the execution
    /// status reaches `DEBUG_STATUS_BREAK`/`NO_DEBUGGEE` or the polls are exhausted.
    ///
    /// Returns the regained stop (`Ok(StopOutcome)`); a target that never breaks within the poll
    /// budget yields an `EngineError::engine("break: timed out")`.
    pub fn break_in(&mut self) -> Result<StopOutcome, EngineError> {
        // Consistent with `go`/`step`: refuse on a dump session (which has no running target to
        // break into) with the frozen literal rather than letting SetInterrupt fail opaquely.
        self.ensure_runnable()?;
        self.break_loop()
    }

    /// The shared `SetInterrupt` + re-issue poll loop behind both [`Engine::break_in`] and `go`'s
    /// flag-interrupt branch. Issues `SetInterrupt(DEBUG_INTERRUPT_ACTIVE)`, then up to ~50 ×
    /// `wait_for_event(200)`, re-issuing the interrupt each poll (a single interrupt can be
    /// consumed without surfacing a break) until a break surfaces — as a `WaitForEvent` event, or
    /// as a `DEBUG_STATUS_BREAK`/`NO_DEBUGGEE` status `WaitForEvent` did not surface (the C++
    /// `Break` check). The regained stop is relabeled "Execution paused".
    fn break_loop(&mut self) -> Result<StopOutcome, EngineError> {
        // SAFETY: live control interface; documented `DEBUG_INTERRUPT_ACTIVE` u32 flag.
        unsafe { self.control.SetInterrupt(DEBUG_INTERRUPT_ACTIVE) }
            .map_err(|e| EngineError::op("SetInterrupt(ACTIVE)", e))?;

        for _ in 0..50 {
            // Drain one pending event slice. A real break may surface here as an Event…
            match self.wait_for_event(200)? {
                WaitResult::Event(o) => return Ok(with_reason(o, "Execution paused")),
                WaitResult::TimedOut => {}
            }

            // …or only as a status change WaitForEvent did not surface (the C++ check). Read it.
            // SAFETY: live control interface; returns the execution status u32 by value.
            let status = unsafe { self.control.GetExecutionStatus() }
                .map_err(|e| EngineError::op("GetExecutionStatus", e))?;
            if status == DEBUG_STATUS_BREAK || status == DEBUG_STATUS_NO_DEBUGGEE {
                let outcome = self.build_stop_outcome(status)?;
                return Ok(with_reason(outcome, "Execution paused"));
            }

            // Still running — re-issue the interrupt in case the previous one was consumed.
            // SAFETY: live control interface; documented `DEBUG_INTERRUPT_ACTIVE` u32 flag.
            unsafe { self.control.SetInterrupt(DEBUG_INTERRUPT_ACTIVE) }
                .map_err(|e| EngineError::op("SetInterrupt(ACTIVE)", e))?;
        }

        Err(EngineError::engine("break: timed out"))
    }

    /// Open a crash/minidump. STUB until Phase 4 — present now so the `Engine` surface (and the
    /// Phase-3 `EngineCmd` enum built over it) is a complete, closed set.
    pub fn open_dump(&mut self, _path: &str) -> Result<DumpOutcome, EngineError> {
        Err(EngineError::engine(
            "open_dump is not implemented until Phase 4",
        ))
    }

    /// Attach to a kernel target (KDNET). STUB until Phase 4 — see `open_dump`.
    pub fn attach_kernel(&mut self, _connection: &str) -> Result<StopOutcome, EngineError> {
        Err(EngineError::engine(
            "attach_kernel is not implemented until Phase 4",
        ))
    }

    /// Guard the execution path: a crash-dump session cannot be resumed. Called by go/step
    /// (task 2.4). The message is the frozen contract literal (the same string the tool layer
    /// surfaces). Today `is_dump` is always false (open_dump is a Phase-4 stub), so this is a
    /// belt-and-suspenders engine-level guard behind the mcp-tools `resume()` guard.
    /// `pub` (not `pub(crate)`) so it is exempt from `dead_code` until task 2.4's go/step call it.
    pub fn ensure_runnable(&self) -> Result<(), EngineError> {
        if self.is_dump {
            Err(EngineError::engine("cannot continue a crash-dump session"))
        } else {
            Ok(())
        }
    }

    /// Wait for the next debug event, distinguishing a real event (`S_OK`) from a timeout
    /// (`S_FALSE`). Ports the C++ `WaitForEvent` helper, incl. the "resumed by a BP command →
    /// keep waiting" loop. Shared by launch/attach (here) and go/step (task 2.4).
    ///
    /// `WaitForEvent`'s `Result<()>` wrapper collapses `S_OK`/`S_FALSE` (both are success
    /// HRESULTs), so we call it through the base `IDebugControl` vtable to recover the exact code.
    pub(crate) fn wait_for_event(&mut self, timeout_ms: u32) -> Result<WaitResult, EngineError> {
        // `WaitForEvent` lives on the base `IDebugControl`; QI to it once (a cheap AddRef on the
        // same object) so we can call the raw vtable entry for the precise HRESULT.
        let base: IDebugControl = self
            .control
            .cast()
            .map_err(|e| EngineError::op("QueryInterface(IDebugControl)", e))?;
        loop {
            // SAFETY: `base` is a live `IDebugControl` (a base view of our `IDebugControl4`).
            // `vtable`/`as_raw` are the same calls the windows-crate's own `WaitForEvent` wrapper
            // makes; we only skip its `.ok()` so `S_FALSE` is not collapsed to `Ok`. No pointers
            // escape; the call blocks up to `timeout_ms` on the engine-owning thread.
            let hr = unsafe {
                (windows::core::Interface::vtable(&base).WaitForEvent)(
                    windows::core::Interface::as_raw(&base),
                    0,
                    timeout_ms,
                )
            };
            if hr == windows::core::HRESULT(0) {
                // S_OK: an event fired. Read the execution status.
                // SAFETY: live control interface; returns a u32 by value.
                let status = unsafe { self.control.GetExecutionStatus() }
                    .map_err(|e| EngineError::op("GetExecutionStatus", e))?;
                // If a breakpoint command resumed execution, keep waiting (C++ behavior). The
                // conditional-breakpoint re-loop (GetLastEventInformation + Evaluate) is Phase 5.
                if matches!(
                    status,
                    DEBUG_STATUS_GO
                        | DEBUG_STATUS_GO_HANDLED
                        | DEBUG_STATUS_GO_NOT_HANDLED
                        | DEBUG_STATUS_STEP_OVER
                        | DEBUG_STATUS_STEP_INTO
                        | DEBUG_STATUS_STEP_BRANCH
                ) {
                    continue;
                }
                return Ok(WaitResult::Event(self.build_stop_outcome(status)?));
            } else if hr == windows::core::HRESULT(1) {
                // S_FALSE: the wait reached its deadline with no event.
                return Ok(WaitResult::TimedOut);
            } else {
                return Err(EngineError::op(
                    "WaitForEvent",
                    windows::core::Error::from_hresult(hr),
                ));
            }
        }
    }

    /// Build the neutral [`StopOutcome`] for a stop, from the execution status plus the
    /// callback-recorded state (stop reason, exit code, last breakpoint id).
    fn build_stop_outcome(&mut self, status: u32) -> Result<StopOutcome, EngineError> {
        // Snapshot the shared state under the lock, then drop the guard before any COM call.
        let (reason, exit_code, bp_id) = {
            let st = self.callback_state();
            (
                st.last_stop_reason(),
                st.last_exit_code(),
                st.last_breakpoint_id(),
            )
        };

        // The process exited (the ExitProcess callback recorded a code, or the engine has no
        // debuggee left) → Exited.
        if exit_code.is_some() || status == DEBUG_STATUS_NO_DEBUGGEE {
            return Ok(StopOutcome::Exited {
                code: exit_code.map(|c| c as i64),
            });
        }

        // Otherwise treat it as a real stop (breakpoint/exception/step). `wait_for_event` already
        // filtered out the resume statuses, and exit is handled above, so the remaining status is
        // `DEBUG_STATUS_BREAK` in practice; mapping any other residual status to `Stopped` is the
        // safe default. Capture the current thread and any hit-breakpoint id.
        // SAFETY: live system-objects interface; returns the engine thread id (u32) by value.
        let thread_id = unsafe { self.system_objects.GetCurrentThreadId() }
            .map(|t| t as i64)
            .unwrap_or(0);
        let hit_breakpoint_ids = bp_id.map(|id| vec![id as i64]).unwrap_or_default();
        Ok(StopOutcome::Stopped(StopInfo {
            reason,
            thread_id,
            description: String::new(),
            hit_breakpoint_ids,
        }))
    }

    /// Force-load symbols for the main module (module 0): `Reload("/f <module>")`. Best-effort —
    /// any failure is swallowed (the launch already succeeded; missing symbols only degrade later
    /// breakpoint resolution, which surfaces its own error). Mirrors the C++ launch tail.
    fn reload_main_module(&mut self) {
        // SAFETY: live symbols interface; returns the module-0 base by value.
        let Ok(base) = (unsafe { self.symbols.GetModuleByIndex(0) }) else {
            return;
        };
        let mut name = [0u8; 256];
        // SAFETY: live symbols interface; `name` is a valid &mut buffer it fills with a
        // NUL-terminated ANSI module name; `used` receives the length.
        let mut used = 0u32;
        if unsafe {
            self.symbols.GetModuleNameString(
                DEBUG_MODNAME_MODULE,
                0,
                base,
                Some(&mut name),
                Some(&mut used),
            )
        }
        .is_err()
        {
            return;
        }
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let module = String::from_utf8_lossy(&name[..end]).into_owned();
        let Ok(reload) = CString::new(format!("/f {module}")) else {
            return;
        };
        // SAFETY: live symbols interface; `reload` is NUL-terminated and outlives the call.
        let _ = unsafe { self.symbols.Reload(PCSTR(reload.as_ptr().cast())) };
    }

    // --- raw interface accessors for the not-yet-wired surface (task 2.5) ---
    //
    // `data_spaces`/`registers` are not yet touched by any method, so they are exposed as `pub`
    // borrowing accessors purely to keep the fields from tripping `dead_code` under `-D warnings`
    // (`pub` fns are exempt). Task 2.5 (memory/register reads) consumes the fields directly, at
    // which point these accessors are removed. The other four interfaces are already used
    // directly by the methods above, so they need no accessor.

    /// `IDebugDataSpaces4` (memory reads — task 2.5).
    pub fn data_spaces(&self) -> &IDebugDataSpaces4 {
        &self.data_spaces
    }

    /// `IDebugRegisters2` (register reads — task 2.5).
    pub fn registers(&self) -> &IDebugRegisters2 {
        &self.registers
    }
}

/// Enable `SeDebugPrivilege` on the current process token (best-effort; ports the C++
/// `EnableDebugPrivilege`). Needed to attach to processes owned by another user or running
/// elevated. Returns `true` when the calls *succeeded* — note that `AdjustTokenPrivileges`
/// reports success (returns `Ok`) even when policy denied the privilege with
/// `ERROR_NOT_ALL_ASSIGNED`, so a `true` here does not guarantee the privilege is actually held.
/// Callers treat the whole thing as best-effort (failure non-fatal).
fn enable_debug_privilege() -> bool {
    // SAFETY: a standard Win32 token-privilege adjustment. `GetCurrentProcess` returns a pseudo
    // handle (no close needed); `token` is an out-param we close before returning; `luid`/`tp`
    // are stack locals passed by pointer for the duration of the calls only. All fallible calls
    // are checked; nothing panics. Only `TOKEN_ADJUST_PRIVILEGES` is requested (the minimal right
    // `AdjustTokenPrivileges` needs — no `TOKEN_QUERY`).
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token).is_err() {
            return false;
        }
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid).is_err() {
            let _ = CloseHandle(token);
            return false;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let ok = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_ok();
        let _ = CloseHandle(token);
        ok
    }
}

/// Override the `reason` of a `Stopped` outcome (used by `launch` to label the loader break
/// "Initial breakpoint"); other outcomes pass through unchanged.
fn with_reason(outcome: StopOutcome, reason: &str) -> StopOutcome {
    match outcome {
        StopOutcome::Stopped(mut info) => {
            info.reason = reason.to_string();
            StopOutcome::Stopped(info)
        }
        other => other,
    }
}

impl Drop for Engine {
    /// Explicitly unregister the event/output callbacks before the interfaces are released, so
    /// DbgEng stops dispatching into our callback objects the moment the engine is torn down.
    /// This mirrors the C++ destructor (`engine.cpp`: `SetEventCallbacks(nullptr)` +
    /// `SetOutputCallbacks(nullptr)` before `Release`). Field drops alone do NOT guarantee this:
    /// the six interfaces and DbgEng's own AddRef keep the underlying object alive, so without an
    /// explicit unregister DbgEng could still invoke a released callback — a real race once task
    /// 2.4 runs a `WaitForEvent` loop and a teardown can overlap an in-flight event.
    fn drop(&mut self) {
        // SAFETY: `self.client` is the live `IDebugClient5`. Passing `None` is the documented
        // way to clear a callback registration (the `nullptr` the C++ passes). Errors are
        // ignored — there is nothing to do in `drop` if the engine is already tearing down — and
        // we never panic out of `drop` (no `unwrap`). After this returns, the field drops
        // `Release` every interface (incl. the callback objects) in declaration order.
        unsafe {
            let _ = self.client.SetEventCallbacks(None);
            let _ = self.client.SetOutputCallbacks(None);
        }
    }
}
