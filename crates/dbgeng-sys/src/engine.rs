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
use std::sync::{Arc, Mutex};

use debugger_core::{DumpOutcome, StopInfo, StopOutcome};
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
    DEBUG_END_ACTIVE_DETACH, DEBUG_END_PASSIVE, DEBUG_ENGOPT_INITIAL_BREAK, DEBUG_MODNAME_MODULE,
    DEBUG_STATUS_GO, DEBUG_STATUS_GO_HANDLED, DEBUG_STATUS_GO_NOT_HANDLED,
    DEBUG_STATUS_NO_DEBUGGEE, DEBUG_STATUS_STEP_BRANCH, DEBUG_STATUS_STEP_INTO,
    DEBUG_STATUS_STEP_OVER,
};
#[cfg(test)]
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DEBUG_EXECUTE_DEFAULT, DEBUG_OUTCTL_THIS_CLIENT,
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
