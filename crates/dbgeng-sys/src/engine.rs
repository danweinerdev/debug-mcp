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

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use debugger_core::{
    BreakpointResult, DumpOutcome, EvalResult, Frame, Instruction, MemoryRead, ModuleInfo,
    StepKind, StopInfo, StopOutcome, ThreadInfo, Variable,
};
use windows::core::{s, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
};
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DebugCreate, IDebugBreakpoint, IDebugClient5, IDebugControl, IDebugControl4, IDebugDataSpaces4,
    IDebugEventCallbacks, IDebugOutputCallbacks, IDebugRegisters2, IDebugSymbols3,
    IDebugSystemObjects4, DEBUG_ANY_ID, DEBUG_ATTACH_DEFAULT, DEBUG_ATTACH_KERNEL_CONNECTION,
    DEBUG_BREAKPOINT_CODE, DEBUG_BREAKPOINT_ENABLED, DEBUG_CREATE_PROCESS_OPTIONS,
    DEBUG_END_ACTIVE_DETACH, DEBUG_END_ACTIVE_TERMINATE, DEBUG_END_PASSIVE,
    DEBUG_ENGOPT_INITIAL_BREAK, DEBUG_EVENT_BREAKPOINT, DEBUG_EXECUTE_DEFAULT,
    DEBUG_EXECUTE_NOT_LOGGED, DEBUG_INTERRUPT_ACTIVE, DEBUG_LAST_EVENT_INFO_BREAKPOINT,
    DEBUG_MODNAME_MODULE, DEBUG_MODULE_PARAMETERS, DEBUG_OUTCTL_IGNORE, DEBUG_OUTCTL_THIS_CLIENT,
    DEBUG_SCOPE_GROUP_LOCALS, DEBUG_STACK_FRAME, DEBUG_STATUS_BREAK, DEBUG_STATUS_GO,
    DEBUG_STATUS_GO_HANDLED, DEBUG_STATUS_GO_NOT_HANDLED, DEBUG_STATUS_NO_DEBUGGEE,
    DEBUG_STATUS_STEP_BRANCH, DEBUG_STATUS_STEP_INTO, DEBUG_STATUS_STEP_OVER,
    DEBUG_SYMTYPE_CODEVIEW, DEBUG_SYMTYPE_COFF, DEBUG_SYMTYPE_DEFERRED, DEBUG_SYMTYPE_DIA,
    DEBUG_SYMTYPE_EXPORT, DEBUG_SYMTYPE_PDB, DEBUG_SYMTYPE_SYM, DEBUG_VALUE, DEBUG_VALUE_INT64,
};
use windows::Win32::System::Diagnostics::Debug::SYMOPT_NO_IMAGE_SEARCH;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
    RRF_SUBKEY_WOW6464KEY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, CREATE_NO_WINDOW, DEBUG_ONLY_THIS_PROCESS, INFINITE,
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

/// Where a breakpoint should be placed. The pre-parsed form `set_breakpoint` accepts so the
/// engine does not have to guess between an address literal, a `file:line`, and a function name
/// (the three location kinds the C++ `SetBreakpointBy{Address,Line,Function}` overloads cover).
/// The `windbg-backend` (Phase 3) parses the neutral tool arguments into this; the resolution
/// logic (symbol lookup, the module-qualify fallback) stays here next to the COM calls.
pub enum BpLoc {
    /// An absolute code address (the C++ `SetBreakpointByAddress`). Used verbatim.
    Address(u64),
    /// A `file:line` source location (the C++ `SetBreakpointByLine`): resolved via
    /// `GetOffsetByLine`.
    FileLine { file: String, line: u32 },
    /// A function name, optionally module-qualified `module!func` (the C++
    /// `SetBreakpointByFunction`): resolved via `GetOffsetByName`, with the per-module
    /// `module!func` fallback when an unqualified name does not resolve.
    Function(String),
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
    /// Engine-side breakpoint conditions, keyed by DbgEng breakpoint id. Ports the C++
    /// `breakpointConditions_` map: DbgEng's command-string conditions (`bp /c`) cannot work with
    /// our `WaitForEvent` poll loop (`g`/`gc` can't re-enter the event loop), so the condition is
    /// stored here and the conditional-break re-loop is deferred to Phase 5 (the `wait_for_event`
    /// hook). `set_breakpoint` inserts, `remove_breakpoint` drops, and `detach` clears the map.
    breakpoint_conditions: HashMap<u32, String>,
    /// Whether the per-session debugger-extension load (`.extpath` + `.load ext.dll`) has run.
    /// Ports the C++ `extensionsLoaded_` guard: `ensure_extensions_loaded` performs the load once
    /// and sets this; `detach` resets it to `false` so a fresh session reloads (mirrors the C++
    /// `Detach` clearing `extensionsLoaded_`). `execute`/`analyze` call `ensure_extensions_loaded`
    /// first so `!`-extension commands resolve (`!analyze`, `!heap`, …).
    extensions_loaded: bool,
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
    /// Build an [`InterruptHandle`] over a caller-supplied flag, **not** tied to any live engine.
    ///
    /// Used by `windbg-backend`'s test `FakeEngine` (which has no real engine to mint a handle
    /// from) so its `interrupt_handle()` can return a usable, inspectable handle sharing the same
    /// `Arc<AtomicBool>` the fake holds. In production the flag is always the engine's own
    /// `interrupt_flag` (see [`Engine::interrupt_handle`]); this constructor just lets a test
    /// observe an `interrupt()` without standing up DbgEng.
    pub fn from_flag(flag: Arc<AtomicBool>) -> InterruptHandle {
        InterruptHandle { flag }
    }

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
            breakpoint_conditions: HashMap::new(),
            extensions_loaded: false,
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

    /// Run a raw DbgEng command line through `IDebugControl::Execute`, routed to *this client* so
    /// its output flows through our registered [`callbacks::OutputCallbacks`], and return the
    /// captured output. Ports the C++ `ExecuteCommand`: drain any stale output, `Execute(
    /// DEBUG_OUTCTL_THIS_CLIENT, command, DEBUG_EXECUTE_DEFAULT)`, then drain and return what the
    /// command emitted.
    ///
    /// `!analyze`/extension commands need the runtime extension-path discovery (R8) the C++
    /// `EnsureExtensionsLoaded` performs first; like the C++ `ExecuteCommand`, `execute` calls
    /// [`Engine::ensure_extensions_loaded`] before running the command (once per session), so the
    /// `run_command` escape hatch can use `!`-extensions (`!heap`, `!analyze`, …) too. The load is
    /// best-effort (a missing `ext.dll` does not fail the command); ordinary commands (`r`, `dd`,
    /// `version`, `.echo`, …) work whether or not the extension load succeeded.
    ///
    /// Mirrors the C++ tolerance for a command that fails but still produced output: a failing
    /// `HRESULT` is only surfaced as an error when nothing was captured; otherwise the captured
    /// text is returned (some DbgEng commands return a non-`S_OK` status yet print useful output).
    pub fn execute(&mut self, command: &str) -> Result<String, EngineError> {
        let cmd = CString::new(command)
            .map_err(|_| EngineError::engine("execute: command contains an interior NUL byte"))?;
        // Load debugger extensions once per session (C++ `ExecuteCommand` calls this first), so an
        // `!`-extension command run through the raw escape hatch resolves. Best-effort.
        self.ensure_extensions_loaded();
        // Drop any output buffered before this command so the returned text is just this command's.
        let _ = self.take_output();
        // SAFETY: `self.control` is the live `IDebugControl4` from `create`. `cmd` is a
        // NUL-terminated ANSI string owned here for the duration of the call (the engine copies
        // it); the `PCSTR` borrows it and does not outlive this statement.
        // `DEBUG_OUTCTL_THIS_CLIENT` routes output to this client's output callbacks; no pointers
        // are retained past the call.
        let hr = unsafe {
            self.control.Execute(
                DEBUG_OUTCTL_THIS_CLIENT,
                PCSTR(cmd.as_ptr().cast()),
                DEBUG_EXECUTE_DEFAULT,
            )
        };
        let output = self.take_output();
        if let Err(e) = hr {
            if output.is_empty() {
                return Err(EngineError::op("Execute", e));
            }
        }
        Ok(output)
    }

    // --- breakpoints (task 2.5) ---

    /// Set a code breakpoint at `loc` and store its `condition` (if any) engine-side. Ports the
    /// C++ `SetBreakpointBy{Address,Line,Function}` + `SetBreakpointByAddress`: resolve `loc` to an
    /// absolute offset, then `AddBreakpoint(DEBUG_BREAKPOINT_CODE, DEBUG_ANY_ID)` →
    /// `SetOffset(addr)` → `AddFlags(DEBUG_BREAKPOINT_ENABLED)` → `GetId()`.
    ///
    /// The `condition` is *stored* (keyed by breakpoint id) and evaluated engine-side in the
    /// `wait_for_event` conditional re-loop (DbgEng's command-string conditions cannot drive our
    /// `WaitForEvent` poll loop). An empty `condition` stores nothing.
    ///
    /// # Condition evaluation
    ///
    /// A `condition` that fails to evaluate at the stop (out-of-scope variable, typo, or partial
    /// parse) is treated as `false` — the breakpoint is silently SKIPPED, matching C++ DbgEng
    /// behavior. There is no API-level notification; a condition referencing a not-yet-loaded symbol
    /// will never fire.
    pub fn set_breakpoint(
        &mut self,
        loc: &BpLoc,
        condition: &str,
    ) -> Result<BreakpointResult, EngineError> {
        let (addr, line) = self.resolve_bp_loc(loc)?;

        // SAFETY: live control interface; `DEBUG_BREAKPOINT_CODE`/`DEBUG_ANY_ID` are documented
        // u32 flags. Returns an `IDebugBreakpoint`. DbgEng breakpoint objects are owned by the
        // engine (their lifetime is `AddBreakpoint`..`RemoveBreakpoint`); their `IUnknown::Release`
        // must NOT be called — so the smart pointer is wrapped in [`Bp`] (a `ManuallyDrop` guard)
        // immediately so its `Drop`/`Release` never fires (see [`Bp`]). Calling `Release` on a
        // DbgEng breakpoint is an access violation.
        let bp = Bp::new(
            unsafe {
                self.control
                    .AddBreakpoint(DEBUG_BREAKPOINT_CODE, DEBUG_ANY_ID)
            }
            .map_err(|e| EngineError::op("AddBreakpoint", e))?,
        );

        // SAFETY: `bp` is the live breakpoint just created; `SetOffset` takes the address by value.
        unsafe { bp.get().SetOffset(addr) }.map_err(|e| EngineError::op("SetOffset", e))?;
        // SAFETY: live breakpoint; `DEBUG_BREAKPOINT_ENABLED` is a documented u32 flag.
        unsafe { bp.get().AddFlags(DEBUG_BREAKPOINT_ENABLED) }
            .map_err(|e| EngineError::op("AddFlags(ENABLED)", e))?;
        // SAFETY: live breakpoint; returns the id (u32) by value.
        let id = unsafe { bp.get().GetId() }.map_err(|e| EngineError::op("GetId", e))?;

        if !condition.is_empty() {
            self.breakpoint_conditions.insert(id, condition.to_string());
        }

        Ok(BreakpointResult {
            id: id as i64,
            verified: true,
            line: line.unwrap_or(0) as i64,
            message: String::new(),
            // A real engine set result is trackable.
            rejected: false,
        })
    }

    /// Resolve a [`BpLoc`] to an absolute code offset (and, for a `file:line`, the line number).
    /// Splits out the C++ resolution paths: address → verbatim; `file:line` → `GetOffsetByLine`;
    /// function → `GetOffsetByName` with the per-module `module!func` fallback.
    fn resolve_bp_loc(&mut self, loc: &BpLoc) -> Result<(u64, Option<u32>), EngineError> {
        match loc {
            BpLoc::Address(addr) => Ok((*addr, None)),
            BpLoc::FileLine { file, line } => {
                let file_c = CString::new(file.as_str()).map_err(|_| {
                    EngineError::engine("set_breakpoint: file path contains an interior NUL byte")
                })?;
                // SAFETY: live symbols interface; `file_c` is NUL-terminated and outlives the call
                // (the engine copies it). Returns the resolved offset (u64) by value.
                let offset = unsafe {
                    self.symbols
                        .GetOffsetByLine(*line, PCSTR(file_c.as_ptr().cast()))
                }
                .map_err(|e| EngineError::op("GetOffsetByLine", e))?;
                Ok((offset, Some(*line)))
            }
            BpLoc::Function(name) => Ok((self.resolve_function(name)?, None)),
        }
    }

    /// Resolve a function name to an offset via `GetOffsetByName`, falling back to trying each
    /// loaded module as `module!func` when an unqualified name (no `!`) does not resolve. Ports the
    /// C++ `SetBreakpointByFunction` module-qualify loop.
    fn resolve_function(&mut self, name: &str) -> Result<u64, EngineError> {
        let name_c = CString::new(name).map_err(|_| {
            EngineError::engine("set_breakpoint: function name contains an interior NUL byte")
        })?;
        // SAFETY: live symbols interface; `name_c` is NUL-terminated and outlives the call.
        let direct = unsafe { self.symbols.GetOffsetByName(PCSTR(name_c.as_ptr().cast())) };
        if let Ok(offset) = direct {
            return Ok(offset);
        }

        // Unqualified name failed: try `<module>!<func>` for each loaded module (C++ fallback).
        if !name.contains('!') {
            let mut loaded = 0u32;
            let mut unloaded = 0u32;
            // SAFETY: live symbols interface; both out-params are valid &mut u32 locals.
            if unsafe { self.symbols.GetNumberModules(&mut loaded, &mut unloaded) }.is_ok() {
                for i in 0..loaded {
                    let Some(module) = self.module_name(i) else {
                        continue;
                    };
                    let Ok(qualified) = CString::new(format!("{module}!{name}")) else {
                        continue;
                    };
                    // SAFETY: live symbols interface; `qualified` is NUL-terminated and outlives
                    // the call. Returns the resolved offset (u64) by value on success.
                    if let Ok(offset) = unsafe {
                        self.symbols
                            .GetOffsetByName(PCSTR(qualified.as_ptr().cast()))
                    } {
                        return Ok(offset);
                    }
                }
            }
        }

        // Re-issue the direct lookup so the caller gets the real resolution HRESULT.
        direct.map_err(|e| EngineError::op("GetOffsetByName", e))?;
        unreachable!("direct was Err in this branch")
    }

    /// Remove the breakpoint with the given engine id and drop its stored condition. Ports the C++
    /// `RemoveBreakpoint`: `GetBreakpointById` then `RemoveBreakpoint`.
    pub fn remove_breakpoint(&mut self, id: i64) -> Result<(), EngineError> {
        let bp_id = id as u32;
        // SAFETY: live control interface; `bp_id` is a u32. Returns an engine-owned
        // `IDebugBreakpoint` whose `Release` must not run (see [`Bp`]), so it is wrapped at once.
        let bp = Bp::new(
            unsafe { self.control.GetBreakpointById(bp_id) }
                .map_err(|e| EngineError::op("GetBreakpointById", e))?,
        );
        self.breakpoint_conditions.remove(&bp_id);
        // SAFETY: live control interface; `bp` is the live breakpoint just fetched.
        // `RemoveBreakpoint` consumes the engine's ownership of the object (after this the pointer
        // is dangling), which is exactly why `bp` is never `Release`d.
        unsafe { self.control.RemoveBreakpoint(bp.get()) }
            .map_err(|e| EngineError::op("RemoveBreakpoint", e))
    }

    /// List the currently set breakpoints. Ports the C++ `ListBreakpoints`: `GetNumberBreakpoints`
    /// then per-index `GetBreakpointByIndex` → id/offset, symbolicating the offset via
    /// `GetNameByOffset` (with a `+0x…` displacement suffix) for the `message` field; an
    /// unresolved offset falls back to its `0x…` hex address.
    pub fn list_breakpoints(&mut self) -> Result<Vec<BreakpointResult>, EngineError> {
        // SAFETY: live control interface; returns the breakpoint count (u32) by value.
        let count = unsafe { self.control.GetNumberBreakpoints() }
            .map_err(|e| EngineError::op("GetNumberBreakpoints", e))?;

        let mut result = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: live control interface; `i` is a valid index `< count`. A failure for one
            // index is skipped (the breakpoint set can change between calls); ports the C++ skip.
            // The returned object is engine-owned (its `Release` must not run — see [`Bp`]), so it
            // is wrapped immediately.
            let Ok(bp) = (unsafe { self.control.GetBreakpointByIndex(i) }) else {
                continue;
            };
            let bp = Bp::new(bp);
            // SAFETY: `bp` is the live breakpoint; both reads return their value by value.
            let id = unsafe { bp.get().GetId() }.unwrap_or(0);
            // SAFETY: live breakpoint; returns the offset (u64) by value.
            let offset = unsafe { bp.get().GetOffset() }.unwrap_or(0);
            let message = self
                .symbolicate(offset)
                .unwrap_or_else(|| format!("0x{offset:016X}"));
            result.push(BreakpointResult {
                id: id as i64,
                verified: true,
                line: 0,
                message,
                // A real engine list result is trackable.
                rejected: false,
            });
        }
        Ok(result)
    }

    // --- inspection (task 2.5) ---

    /// Enumerate the target's threads. Ports the C++ `GetThreads`: `GetNumberThreads` +
    /// `GetThreadIdsByIndex` for the engine and system thread ids. `id` is the *engine* thread id
    /// (what `SetCurrentThreadId`/the stop outcome use); `name` carries the system (OS) thread id
    /// as `"sys=<systemId>"` (the C++ surfaced both; we keep the engine id authoritative and fold
    /// the OS id into the neutral `name` rather than walking each thread's stack here).
    pub fn threads(&mut self) -> Result<Vec<ThreadInfo>, EngineError> {
        // SAFETY: live system-objects interface; returns the thread count (u32) by value.
        let count = unsafe { self.system_objects.GetNumberThreads() }
            .map_err(|e| EngineError::op("GetNumberThreads", e))?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut engine_ids = vec![0u32; count as usize];
        let mut system_ids = vec![0u32; count as usize];
        // SAFETY: live system-objects interface; both buffers are valid `&mut [u32]` of length
        // `count`, passed as the documented out-arrays for `[0, count)`.
        unsafe {
            self.system_objects.GetThreadIdsByIndex(
                0,
                count,
                Some(engine_ids.as_mut_ptr()),
                Some(system_ids.as_mut_ptr()),
            )
        }
        .map_err(|e| EngineError::op("GetThreadIdsByIndex", e))?;

        Ok(engine_ids
            .iter()
            .zip(system_ids.iter())
            .map(|(&eng, &sys)| ThreadInfo {
                id: eng as i64,
                name: format!("sys={sys}"),
            })
            .collect())
    }

    /// Walk the call stack of `thread_id` (engine thread id), up to `max` frames. Ports the C++
    /// `GetCallStack`: switch to the thread (`SetCurrentThreadId`), `GetStackTrace` into a frame
    /// buffer, then per frame fill `index`, `instruction_pointer` (`0x{:016X}` of the IP), the
    /// symbolicated `name` (`GetNameByOffset` + `+0x…` displacement, else the hex address), and the
    /// source `source_path`/`line` (`GetLineByOffset`, left `None`/0 when no line maps).
    pub fn stack_trace(&mut self, thread_id: i64, max: i64) -> Result<Vec<Frame>, EngineError> {
        let max_frames = max.clamp(1, 1024) as usize;

        // Save the engine's current thread so we can restore it: switching here would otherwise
        // leave the *wrong* current thread for a later `locals`/`current_source_location`/stop
        // query (the C++ thread-switching helpers restore it too). SAFETY: live system-objects
        // interface; returns a u32 by value.
        let prev_thread =
            unsafe { self.system_objects.GetCurrentThreadId() }.unwrap_or(thread_id as u32);

        // SAFETY: live system-objects interface; `thread_id as u32` selects the engine thread.
        unsafe { self.system_objects.SetCurrentThreadId(thread_id as u32) }
            .map_err(|e| EngineError::op("SetCurrentThreadId", e))?;

        let mut frames = vec![DEBUG_STACK_FRAME::default(); max_frames];
        let mut filled = 0u32;
        // SAFETY: live control interface; `frames` is a valid `&mut [DEBUG_STACK_FRAME]` the engine
        // fills (default frame/stack/instruction offsets = "current context"); `filled` is a valid
        // &mut u32 out-param. No pointers are retained past the call.
        let trace = unsafe {
            self.control
                .GetStackTrace(0, 0, 0, &mut frames, Some(&mut filled))
        };

        // Restore the previous current thread before surfacing any GetStackTrace error.
        // SAFETY: live system-objects interface; `prev_thread` is the id we read above.
        let _ = unsafe { self.system_objects.SetCurrentThreadId(prev_thread) };

        trace.map_err(|e| EngineError::op("GetStackTrace", e))?;

        let mut result = Vec::with_capacity(filled as usize);
        for (i, frame) in frames.iter().take(filled as usize).enumerate() {
            let ip = frame.InstructionOffset;
            let name = self
                .symbolicate(ip)
                .unwrap_or_else(|| format!("0x{ip:016X}"));
            let (source_path, line) = self.line_at(ip);
            result.push(Frame {
                index: i as i64,
                id: i as i64,
                name,
                source_path,
                line,
                instruction_pointer: Some(format!("0x{ip:016X}")),
            });
        }
        Ok(result)
    }

    /// Read the local variables in scope at `frame_index`. Ports the C++ `GetLocals`: for a frame
    /// above 0, `GetStackTrace` + `SetScope` to that frame; `GetScopeSymbolGroup2(
    /// DEBUG_SCOPE_GROUP_LOCALS)` → per-symbol `GetSymbolName`/`GetSymbolValueText`/
    /// `GetSymbolTypeName`; `ResetScope` after. Nested child expansion is later, so
    /// `variables_reference`/`named`/`indexed` are 0.
    pub fn locals(&mut self, frame_index: i64) -> Result<Vec<Variable>, EngineError> {
        let scoped = frame_index > 0;
        if scoped {
            let want = (frame_index + 1) as usize;
            let mut frames = vec![DEBUG_STACK_FRAME::default(); want];
            let mut filled = 0u32;
            // SAFETY: live control interface; `frames` is a valid `&mut [DEBUG_STACK_FRAME]`; the
            // engine fills the current thread's frames and writes `filled`.
            unsafe {
                self.control
                    .GetStackTrace(0, 0, 0, &mut frames, Some(&mut filled))
            }
            .map_err(|e| EngineError::op("GetStackTrace", e))?;
            if (filled as i64) <= frame_index {
                return Err(EngineError::engine(format!(
                    "locals: frame {frame_index} is out of range ({filled} frames)"
                )));
            }
            let frame = frames[frame_index as usize];
            // SAFETY: live symbols interface; `&frame` points to a valid `DEBUG_STACK_FRAME` for
            // the duration of the call (the engine copies the scope), no context blob is supplied.
            unsafe {
                self.symbols
                    .SetScope(0, Some(&frame as *const DEBUG_STACK_FRAME), None, 0)
            }
            .map_err(|e| EngineError::op("SetScope", e))?;
        }

        let result = self.read_scope_locals();

        if scoped {
            // SAFETY: live symbols interface; restores the default (frame-0) scope. Best-effort —
            // a ResetScope failure does not invalidate the locals already read.
            let _ = unsafe { self.symbols.ResetScope() };
        }
        result
    }

    /// Read the `DEBUG_SCOPE_GROUP_LOCALS` symbol group of the *current* scope into neutral
    /// [`Variable`]s. Factored out of [`Engine::locals`] so the `ResetScope` cleanup runs on every
    /// path (including an error mid-read).
    fn read_scope_locals(&mut self) -> Result<Vec<Variable>, EngineError> {
        // SAFETY: live symbols interface; `DEBUG_SCOPE_GROUP_LOCALS` is a documented u32 flag and
        // `None` requests a fresh group (no group to update). Returns an owned
        // `IDebugSymbolGroup2` smart pointer.
        let group = unsafe {
            self.symbols
                .GetScopeSymbolGroup2(DEBUG_SCOPE_GROUP_LOCALS, None)
        }
        .map_err(|e| EngineError::op("GetScopeSymbolGroup2", e))?;

        // SAFETY: `group` is the live symbol group; returns the symbol count (u32) by value.
        let count = unsafe { group.GetNumberSymbols() }
            .map_err(|e| EngineError::op("GetNumberSymbols", e))?;

        let mut result = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut name_buf = [0u8; 256];
            // SAFETY: live symbol group; `name_buf` is a valid &mut buffer the engine fills with a
            // NUL-terminated ANSI symbol name; `i < count`; the size out-param is omitted.
            let name = if unsafe { group.GetSymbolName(i, Some(&mut name_buf), None) }.is_ok() {
                cstr_from_buf(&name_buf)
            } else {
                String::new()
            };

            let mut value_buf = [0u8; 1024];
            // SAFETY: live symbol group; `value_buf` is a valid &mut buffer the engine fills with
            // the NUL-terminated value text; `i < count`.
            let value =
                if unsafe { group.GetSymbolValueText(i, Some(&mut value_buf), None) }.is_ok() {
                    cstr_from_buf(&value_buf)
                } else {
                    String::new()
                };

            let mut type_buf = [0u8; 256];
            // SAFETY: live symbol group; `type_buf` is a valid &mut buffer the engine fills with
            // the NUL-terminated type name; `i < count`.
            let ty = if unsafe { group.GetSymbolTypeName(i, Some(&mut type_buf), None) }.is_ok() {
                cstr_from_buf(&type_buf)
            } else {
                String::new()
            };

            result.push(Variable {
                name,
                value,
                ty,
                variables_reference: 0,
                named: 0,
                indexed: 0,
            });
        }
        Ok(result)
    }

    /// Evaluate `expr` as a C++ expression and return its rendered value. Ports the C++
    /// `EvaluateExpression`: run `?? <expr>` (the readable C++-expression evaluator) through
    /// [`Engine::execute`] and return the trimmed captured output. `ty`/`variables_reference` are
    /// left empty/0 (the `??` text carries the type inline; structured type extraction is later).
    pub fn evaluate(&mut self, expr: &str) -> Result<EvalResult, EngineError> {
        let output = self.execute(&format!("?? {expr}"))?;
        Ok(EvalResult {
            result: output.trim().to_string(),
            ty: String::new(),
            variables_reference: 0,
        })
    }

    // --- memory / disassembly (task 2.5) ---

    /// Read `size` bytes of the target's virtual memory at `address`. Ports the C++ `ReadMemory`:
    /// `ReadVirtual`, then truncate to the bytes actually read (a short read at a page boundary
    /// yields fewer bytes rather than an error). `size` is capped so the `u32` ReadVirtual length
    /// cannot silently truncate (callers should bound it further; the tool layer clamps small).
    pub fn read_memory(&mut self, address: u64, size: usize) -> Result<MemoryRead, EngineError> {
        // ReadVirtual takes a u32 length; clamp so a huge `size` neither wraps the length nor
        // over-allocates the buffer beyond what the call could ever fill.
        let size = size.min(u32::MAX as usize);
        let mut buf = vec![0u8; size];
        let mut read = 0u32;
        // SAFETY: live data-spaces interface; `buf` is a valid `size`-byte allocation we pass as a
        // raw `*mut c_void` with its true length (`size` fits a u32 after the clamp above); the
        // engine writes at most `size` bytes and reports the count in `read` (a valid &mut u32
        // out-param). `buf` outlives the call.
        unsafe {
            self.data_spaces.ReadVirtual(
                address,
                buf.as_mut_ptr() as *mut c_void,
                size as u32,
                Some(&mut read),
            )
        }
        .map_err(|e| EngineError::op("ReadVirtual", e))?;
        buf.truncate(read as usize);
        Ok(MemoryRead {
            address: format!("0x{address:016X}"),
            data: buf,
        })
    }

    /// Disassemble `count` instructions starting at `address`. The C++ plugin has no disassemble;
    /// this drives `IDebugControl::Disassemble`, which renders one instruction into a text buffer
    /// and returns the offset of the *next* instruction. We loop `count` times, advancing `offset`
    /// to the returned end offset each time, and parse the rendered line for the mnemonic text
    /// (the bytes/symbol fields are best-effort and left empty — the text line carries the
    /// mnemonic). Disassembly stops early (no error) once a step fails or fails to advance.
    pub fn disassemble(
        &mut self,
        address: u64,
        count: i64,
    ) -> Result<Vec<Instruction>, EngineError> {
        let n = count.max(0) as usize;
        let mut result = Vec::with_capacity(n);
        let mut offset = address;
        for _ in 0..n {
            let mut buf = [0u8; 512];
            let mut disasm_size = 0u32;
            let mut end_offset = 0u64;
            // SAFETY: live control interface; `buf` is a valid 512-byte &mut buffer the engine
            // fills with the NUL-terminated disassembly text; `disasm_size`/`end_offset` are valid
            // out-params. Flags 0 = no effective-address annotation. Errors stop the loop.
            let ok = unsafe {
                self.control.Disassemble(
                    offset,
                    0,
                    Some(&mut buf),
                    Some(&mut disasm_size),
                    &mut end_offset,
                )
            }
            .is_ok();
            if !ok {
                break;
            }

            let text = cstr_from_buf(&buf);
            result.push(Instruction {
                address: format!("0x{offset:016X}"),
                instruction: parse_disasm_text(&text),
                bytes: String::new(),
                symbol: String::new(),
                source_path: None,
                line: 0,
            });

            // Guard against a non-advancing step (would otherwise spin emitting the same address).
            if end_offset <= offset {
                break;
            }
            offset = end_offset;
        }
        Ok(result)
    }

    // --- modules / source location (task 2.5) ---

    /// Enumerate the loaded modules. Ports the C++ `GetModules`: `GetNumberModules` +
    /// per-index `GetModuleByIndex` (base) + `GetModuleNameString` (name) + `GetModuleParameters`
    /// (size, `SymbolType`). `base` is `0x{:016X}`, `size` a decimal string, and `symbol_status`
    /// the neutral token mapped from `SymbolType` (PDB→`pdb`, EXPORT→`export`, DEFERRED→`deferred`,
    /// else `none`).
    pub fn modules(&mut self) -> Result<Vec<ModuleInfo>, EngineError> {
        let mut loaded = 0u32;
        let mut unloaded = 0u32;
        // SAFETY: live symbols interface; both out-params are valid &mut u32 locals.
        unsafe { self.symbols.GetNumberModules(&mut loaded, &mut unloaded) }
            .map_err(|e| EngineError::op("GetNumberModules", e))?;

        let mut result = Vec::with_capacity(loaded as usize);
        for i in 0..loaded {
            // SAFETY: live symbols interface; `i < loaded` is a valid module index. Returns the
            // module base (u64) by value.
            let Ok(base) = (unsafe { self.symbols.GetModuleByIndex(i) }) else {
                continue;
            };
            let name = self.module_name(i).unwrap_or_default();

            let mut params = DEBUG_MODULE_PARAMETERS::default();
            // SAFETY: live symbols interface; `bases` points to the single `base` (count 1), and
            // `params` is a valid &mut `DEBUG_MODULE_PARAMETERS` the engine fills. `start` = 0.
            let (size, symbol_status) = if unsafe {
                self.symbols
                    .GetModuleParameters(1, Some(&base as *const u64), 0, &mut params)
            }
            .is_ok()
            {
                (params.Size as u64, symbol_status(params.SymbolType))
            } else {
                (0, "none".to_string())
            };

            result.push(ModuleInfo {
                name,
                base: format!("0x{base:016X}"),
                size: size.to_string(),
                symbol_status,
            });
        }
        Ok(result)
    }

    /// The current instruction's source location, if one maps. Ports the C++
    /// `GetCurrentSourceLocation`: `GetInstructionOffset` (the IP) → `GetLineByOffset` →
    /// `Some((file, line))`, or `None` when no source line is available. Phase 4's `open_dump`
    /// calls this for `crash_location`.
    pub fn current_source_location(&mut self) -> Result<Option<(String, i64)>, EngineError> {
        // SAFETY: live registers interface; returns the instruction pointer (u64) by value.
        let ip = unsafe { self.registers.GetInstructionOffset() }
            .map_err(|e| EngineError::op("GetInstructionOffset", e))?;
        let (file, line) = self.line_at(ip);
        Ok(file.map(|f| (f, line)))
    }

    // --- inspection helpers (task 2.5) ---

    /// The module name at index `i` via `GetModuleNameString(DEBUG_MODNAME_MODULE)`, or `None`.
    fn module_name(&self, i: u32) -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: live symbols interface; `buf` is a valid &mut buffer the engine fills with a
        // NUL-terminated ANSI module name; `base` 0 means "use the index"; size out-param omitted.
        if unsafe {
            self.symbols
                .GetModuleNameString(DEBUG_MODNAME_MODULE, i, 0, Some(&mut buf), None)
        }
        .is_ok()
        {
            Some(cstr_from_buf(&buf))
        } else {
            None
        }
    }

    /// Symbolicate an offset via `GetNameByOffset`, appending a `+0x…` displacement when the offset
    /// is not exactly at a symbol. Returns `None` when no symbol resolves (the caller falls back to
    /// the raw hex address). Ports the C++ `GetNameByOffset` + displacement formatting.
    fn symbolicate(&self, offset: u64) -> Option<String> {
        let mut buf = [0u8; 512];
        let mut displacement = 0u64;
        // SAFETY: live symbols interface; `buf` is a valid &mut buffer the engine fills with the
        // NUL-terminated symbol name; `displacement` is a valid &mut u64 out-param; size omitted.
        if unsafe {
            self.symbols
                .GetNameByOffset(offset, Some(&mut buf), None, Some(&mut displacement))
        }
        .is_ok()
        {
            let mut name = cstr_from_buf(&buf);
            if name.is_empty() {
                return None;
            }
            if displacement > 0 {
                name.push_str(&format!("+0x{displacement:X}"));
            }
            Some(name)
        } else {
            None
        }
    }

    /// The source `(file, line)` for an offset via `GetLineByOffset`: `(Some(file), line)` when a
    /// line maps, else `(None, 0)`. Shared by `stack_trace` and `current_source_location`.
    fn line_at(&self, offset: u64) -> (Option<String>, i64) {
        let mut buf = [0u8; 260]; // MAX_PATH
        let mut line = 0u32;
        // SAFETY: live symbols interface; `line` is a valid &mut u32 out-param; `buf` is a valid
        // &mut file-path buffer the engine fills with a NUL-terminated path; size/displacement
        // out-params omitted.
        if unsafe {
            self.symbols
                .GetLineByOffset(offset, Some(&mut line), Some(&mut buf), None, None)
        }
        .is_ok()
        {
            let file = cstr_from_buf(&buf);
            if file.is_empty() {
                (None, 0)
            } else {
                (Some(file), line as i64)
            }
        } else {
            (None, 0)
        }
    }

    // --- lifecycle (task 2.3) ---

    /// Launch `req.program` (with `args`/`cwd`) under the debugger and stop at the initial loader
    /// breakpoint. Ports the C++ `LaunchProcess`: `AddEngineOptions(INITIAL_BREAK)` →
    /// `CreateProcess2(DEBUG_ONLY_THIS_PROCESS | CREATE_NO_WINDOW)` → wait for the loader break →
    /// `RemoveEngineOptions(INITIAL_BREAK)` (mandatory — leaving it set re-breaks every `go`) →
    /// force-load the exe's symbols (`Reload "/f <module>"`). Returns the initial-break stop.
    pub fn launch(&mut self, req: &LaunchReq) -> Result<StopOutcome, EngineError> {
        // A new session's breakpoint ids start fresh, so any conditions from a prior session
        // (which `detach` clears, but a target that exited on its own does not) must not linger and
        // collide with a reused id.
        self.breakpoint_conditions.clear();

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
        // Fresh session — drop any stale breakpoint conditions (see `launch`).
        self.breakpoint_conditions.clear();

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

    /// End the session. Ports the C++ `Detach`: a live session normally uses
    /// `EndSession(DEBUG_END_ACTIVE_DETACH)` (detaches AND releases module file mappings, so the
    /// target's image is not left locked — `DetachProcesses` would leave locks); a dump uses
    /// `DEBUG_END_PASSIVE`. The engine is normally dropped right after detach (Phase 3 model), so
    /// no in-place state reset beyond clearing `is_dump` is required.
    ///
    /// `terminate` selects kill-vs-detach for a **live** session: when `terminate` is set (and the
    /// session is not a dump), `EndSession(DEBUG_END_ACTIVE_TERMINATE)` kills the debuggee instead
    /// of leaving it running. A dump session ignores `terminate` (there is no live process to kill);
    /// it always ends `DEBUG_END_PASSIVE`. The dump-vs-live choice reads the engine's own `is_dump`
    /// (set by `open_dump` in Phase 4), not a caller parameter, so the flag can never disagree with
    /// the actual session kind.
    pub fn detach(&mut self, terminate: bool) -> Result<(), EngineError> {
        let flags = if self.is_dump {
            DEBUG_END_PASSIVE
        } else if terminate {
            DEBUG_END_ACTIVE_TERMINATE
        } else {
            DEBUG_END_ACTIVE_DETACH
        };
        // SAFETY: live client; `flags` is a documented DEBUG_END_* constant.
        unsafe { self.client.EndSession(flags) }.map_err(|e| EngineError::op("EndSession", e))?;
        self.is_dump = false;
        self.extensions_loaded = false;
        self.breakpoint_conditions.clear();
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

    /// Open a crash/minidump and load it into a (stopped) dump session. Ports the C++
    /// `OpenDumpFile`: guard `state == NoTarget`, `OpenDumpFile(path)`, mark the session a dump,
    /// then `WaitForEvent(30000)` to load the dump's stored state.
    ///
    /// The returned [`DumpOutcome`] carries the dump's stop snapshot (`stop`) when `WaitForEvent`
    /// surfaces one, and the crash source location (`crash_location` = `"<file>:<line>"`) from
    /// [`Engine::current_source_location`]. The session is left `is_dump = true`, so a later
    /// `go`/`step`/`break_in` is refused by [`Engine::ensure_runnable`] with the frozen
    /// `"cannot continue a crash-dump session"` literal.
    pub fn open_dump(&mut self, path: &str) -> Result<DumpOutcome, EngineError> {
        // C++ `OpenDumpFile` guards `state != NoTarget → E_UNEXPECTED`, which rejects opening a
        // dump while ANY session is already active — a live process attach/launch as well as
        // another dump. The Rust engine has no `State` enum, so we mirror the C++ guard exactly by
        // reading DbgEng's execution status: anything other than `DEBUG_STATUS_NO_DEBUGGEE` means a
        // debuggee (live or dump) is bound to this engine, and reusing it for a dump is a
        // programming error. The Phase-3 model creates a fresh `Engine` per session at the connect
        // point, so a just-created engine is `NoTarget` and this guard is rarely hit; mirroring it
        // defensively prevents silently reusing/clobbering an existing session.
        //
        // SAFETY: `self.control` is the live `IDebugControl4` from `create`; `GetExecutionStatus`
        // only reads a `u32` out-param (returned by value) — no pointers cross the FFI boundary.
        let status = unsafe { self.control.GetExecutionStatus() }
            .map_err(|e| EngineError::op("GetExecutionStatus", e))?;
        if status != DEBUG_STATUS_NO_DEBUGGEE {
            return Err(EngineError::engine(
                "open_dump: a session is already active",
            ));
        }

        let path_c = CString::new(path).map_err(|_| {
            EngineError::engine("open_dump: dump path contains an interior NUL byte")
        })?;
        // SAFETY: `self.client` is the live `IDebugClient5` from `create`. `OpenDumpFile` takes a
        // `PCSTR` (ANSI — matching the C++ `OpenDumpFile(path.c_str())`); `path_c` is a
        // NUL-terminated buffer owned here that outlives the call (the engine copies the path). No
        // pointers are retained past the call.
        unsafe { self.client.OpenDumpFile(PCSTR(path_c.as_ptr().cast())) }
            .map_err(|e| EngineError::op("OpenDumpFile", e))?;

        // Mark the session a dump *before* the wait, exactly as the C++ sets `isDumpSession_` right
        // after a successful `OpenDumpFile` — so even an erroring wait leaves the dump flag correct
        // for `detach` (which must end a dump session with `DEBUG_END_PASSIVE`).
        self.is_dump = true;

        // `WaitForEvent` loads the dump's stored state (C++ uses a 30 s budget).
        let stop = match self.wait_for_event(30_000)? {
            WaitResult::Event(StopOutcome::Stopped(info)) => Some(info),
            // The dump loaded but did not surface a `Stopped` snapshot (e.g. an `Exited`-shaped
            // event); the session is still a valid stopped dump — leave `stop` unset.
            WaitResult::Event(_) => None,
            WaitResult::TimedOut => {
                return Err(EngineError::engine("open_dump: timed out loading dump"))
            }
        };

        // `crash_location` is `"<file>:<line>"` when the dump's instruction maps to source; `None`
        // otherwise (no symbols / no line). Best-effort: a failure to read the location is not a
        // dump-open failure (the dump still loaded).
        let crash_location = self
            .current_source_location()
            .ok()
            .flatten()
            .map(|(file, line)| format!("{file}:{line}"));

        Ok(DumpOutcome {
            stop,
            crash_location,
        })
    }

    /// Attach to a kernel target over KDNET. Ports the C++ `AttachKernel`: guard `NoTarget`,
    /// `AddEngineOptions(INITIAL_BREAK)` (without it `WaitForEvent` blocks forever on a running
    /// kernel target), `AttachKernel(DEBUG_ATTACH_KERNEL_CONNECTION, connection)`, then wait for
    /// the initial break and `RemoveEngineOptions(INITIAL_BREAK)`.
    ///
    /// `connection` is passed through verbatim (the only supported form is KDNET
    /// `"net:port=<p>,key=<k>"`, validated by DbgEng itself — an unparseable string surfaces as an
    /// `AttachKernel` error).
    ///
    /// ## R2 decision — the wait is `INFINITE` (and uncancellable)
    ///
    /// We mirror the C++ exactly and wait `INFINITE`: KDNET is the only supported transport and
    /// DbgEng's KDNET wait does not honor a finite timeout there ("INFINITE is sadly the only
    /// supported wait"). The consequence is a **hard caveat**: if the KDNET target is unreachable
    /// (the guest VM is not listening on the port), `WaitForEvent(INFINITE)` blocks the
    /// engine-owning thread *forever* with no cancellation point — DbgEng's KDNET transport retries
    /// indefinitely. There is no way to interrupt it from here. The user-facing recovery is "`/mcp`
    /// disconnect" (tear down the whole server), as documented in the plugin's kernel workflow.
    ///
    /// The proper fix — running the attach on an orphan-able engine thread so an unreachable target
    /// can be abandoned without hanging the server — is **deferred to Phase 5** (the
    /// uncancellable-wait orphan-thread teardown). This method must therefore never be driven with
    /// an unreachable target from a context that cannot itself be torn down (the dbgeng-sys live
    /// test for this path is `#[ignore]`d for exactly this reason; see `tests/lifecycle.rs`).
    pub fn attach_kernel(&mut self, connection: &str) -> Result<StopOutcome, EngineError> {
        // Fresh session — drop any stale breakpoint conditions (see `launch`/`attach_pid`).
        self.breakpoint_conditions.clear();

        let connection_c = CString::new(connection).map_err(|_| {
            EngineError::engine("attach_kernel: connection string contains an interior NUL byte")
        })?;

        // Without INITIAL_BREAK, `WaitForEvent` blocks forever on a running kernel target (C++).
        // SAFETY: live control interface; `DEBUG_ENGOPT_INITIAL_BREAK` is a documented u32 flag.
        unsafe { self.control.AddEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) }
            .map_err(|e| EngineError::op("AddEngineOptions(INITIAL_BREAK)", e))?;

        // SAFETY: live client; `DEBUG_ATTACH_KERNEL_CONNECTION` selects the connection-string
        // (KDNET) transport. `connection_c` is NUL-terminated and outlives the call (the engine
        // copies it); the `PCSTR` borrows it for the duration of the call only.
        if let Err(e) = unsafe {
            self.client.AttachKernel(
                DEBUG_ATTACH_KERNEL_CONNECTION,
                PCSTR(connection_c.as_ptr().cast()),
            )
        } {
            // AttachKernel itself failed (e.g. malformed connection string) before any wait: undo
            // the INITIAL_BREAK option (so it does not leak into a later session on a reused engine)
            // and surface the mapped error. SAFETY: live control interface; documented u32 flag.
            let _ = unsafe { self.control.RemoveEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) };
            return Err(EngineError::op("AttachKernel", e));
        }

        // A live kernel session is never a dump (C++ sets `isDumpSession_ = false` here).
        self.is_dump = false;

        // R2: INFINITE is the only supported KDNET wait — this can block forever on an unreachable
        // target (see the method doc). The Phase-5 orphan-thread teardown is the cancellation fix.
        let wait = self.wait_for_event(INFINITE);

        // Clear INITIAL_BREAK regardless of the wait result so later `go`/`step` don't re-break.
        // SAFETY: live control interface; documented u32 flag.
        let removed = unsafe { self.control.RemoveEngineOptions(DEBUG_ENGOPT_INITIAL_BREAK) };

        // Surface a wait failure first, then a RemoveEngineOptions failure, before the timeout
        // branch — the launch/attach error-ordering. On a wait FAILURE, also `EndSession` so the
        // KDNET port is not leaked (C++ `EndSession(DEBUG_END_ACTIVE_DETACH)` on the failure path).
        let outcome = match wait {
            Ok(o) => o,
            Err(e) => {
                // SAFETY: live client; `DEBUG_END_ACTIVE_DETACH` is a documented u32 flag. Do not
                // leak the KDNET port — end the half-open session before returning the wait error.
                let _ = unsafe { self.client.EndSession(DEBUG_END_ACTIVE_DETACH) };
                self.is_dump = false;
                return Err(e);
            }
        };
        // DELIBERATE, DOCUMENTED deviation from the C++: the C++ `AttachKernel` drops the
        // `RemoveEngineOptions(INITIAL_BREAK)` HRESULT on the success path; we surface it. A
        // lingering INITIAL_BREAK would re-break every subsequent `go`/`step` (corrupting all later
        // execution on this session), so a failure to clear it is a real fault worth raising — and
        // surfacing it here is consistent with `launch`/`attach_pid`, which already propagate this
        // HRESULT in the same ordering (wait error first, then RemoveEngineOptions).
        removed.map_err(|e| EngineError::op("RemoveEngineOptions(INITIAL_BREAK)", e))?;
        match outcome {
            WaitResult::Event(o) => Ok(o),
            // `wait_for_event(INFINITE)` only returns `TimedOut` on an S_FALSE, which an INFINITE
            // wait does not produce in practice; treat it as a clear error rather than a stop.
            WaitResult::TimedOut => Err(EngineError::engine(
                "attach_kernel: kernel wait returned without an event",
            )),
        }
    }

    /// Run `!analyze -v` and return the (truncated) automated crash-analysis report. Ports the C++
    /// `analyze_crash` tool: `ExecuteCommand("!analyze -v")` (which loads extensions first), then
    /// the tool-layer `truncateOutput` cap. The 32 KiB cap + the exact truncation suffix mirror the
    /// C++ `truncateOutput` (`advanced_tools.cpp`); applying it here keeps the neutral `analyze`
    /// return self-contained.
    ///
    /// ## State contract — this method does NOT guard state
    ///
    /// Mirroring the C++ (which guards at the tool layer, not in the engine), `analyze` performs no
    /// state check. Run against a `NoTarget` engine it simply asks DbgEng to `!analyze -v` with no
    /// debuggee, which yields empty/error DbgEng output rather than a meaningful report — it does
    /// not fault, but it is not useful. The **tool layer is responsible for the state guard**: the
    /// `analyze_crash` handler (task 4.3, `crates/mcp-tools/src/handlers/windbg.rs`) calls
    /// `session.check_state(&[State::Stopped])` before dispatching here, which is the contract this
    /// method relies on. (Verified in place as of task 4.1.)
    pub fn analyze(&mut self) -> Result<String, EngineError> {
        // `execute` already calls `ensure_extensions_loaded` (so `!analyze` resolves); call it here
        // too for clarity/parity with the C++ `ExecuteCommand` ordering — the guard makes the
        // second call a no-op.
        self.ensure_extensions_loaded();
        let output = self.execute("!analyze -v")?;
        Ok(truncate_output(output))
    }

    /// Load the WinDbg debugger extensions once per session: `.extpath <discovered x64 dirs>` then
    /// `.load ext.dll`. Ports the C++ `EnsureExtensionsLoaded`, guarded by
    /// [`Engine::extensions_loaded`] so it runs at most once per session (reset by `detach`).
    ///
    /// ## R8 refinement — runtime discovery of the `Debuggers\x64` root (task 5.0)
    ///
    /// The C++ HARDCODED the x64 Windows Kits 10 debugger extension directories
    /// (`C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\{winext,WINXP,base}`), so `!analyze`
    /// and other `!`-extension commands only resolved on a default-path SDK install. This refinement
    /// discovers the `Debuggers\x64` root at RUNTIME so the extensions resolve wherever the SDK is
    /// actually installed:
    ///
    /// 1. Registry `KitsRoot10` (`HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`, REG_SZ),
    ///    native view then the `WOW6432Node` view, joined with `Debuggers\x64`.
    /// 2. The `WindowsSdkDir` env var, joined with `Debuggers\x64`.
    /// 3. The former hardcoded default `C:\Program Files (x86)\Windows Kits\10\Debuggers\x64`.
    ///
    /// The first candidate whose base dir EXISTS wins (see [`discover_debuggers_root`]). From that
    /// root, the extension search path is built from only the EXISTING subdirs (`winext`, `winxp`,
    /// and the root itself; see [`extension_dirs`]) joined with `;`. If discovery finds nothing the
    /// load is SKIPPED (logged to stderr — `!analyze`/`!`-commands are then unavailable) and the
    /// guard is still set so it is not retried every call.
    ///
    /// Both commands run with `DEBUG_OUTCTL_IGNORE` (no output routed to our callbacks) and
    /// `DEBUG_EXECUTE_NOT_LOGGED`, and any `Execute` failure is swallowed — extension load is
    /// best-effort, exactly as the C++ ignored both `Execute` HRESULTs.
    fn ensure_extensions_loaded(&mut self) {
        if self.extensions_loaded {
            return;
        }
        // Set the guard before issuing the commands (matching the C++, which sets
        // `extensionsLoaded_ = true` up front) so a re-entrant call cannot double-load — and so a
        // failed/empty discovery is not retried on every subsequent `execute`/`analyze`.
        self.extensions_loaded = true;

        // Discover the real `Debuggers\x64` root (registry → env → default) and build `.extpath`
        // from only the dirs that exist. Make the result observable so an operator can tell WHY
        // `!analyze` failed (a missing SDK) versus succeeded.
        let dirs = match discover_debuggers_root() {
            Some(root) => extension_dirs(&root, |p| p.exists()),
            None => Vec::new(),
        };
        if dirs.is_empty() {
            eprintln!(
                "dbgeng: no debugger extensions found (KitsRoot10 not located) — \
                 !analyze/!-commands unavailable"
            );
            return;
        }
        let joined = dirs
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join(";");
        eprintln!("dbgeng: extensions search path = {joined}");

        // Build the `.extpath <dirs>` command as a NUL-terminated ANSI string bound to a NAMED
        // LOCAL that outlives the `Execute` call below — the string is now RUNTIME-built (not an
        // `s!` 'static), so it must not be a temporary that drops before the FFI call reads it.
        // Paths cannot contain an interior NUL, so the `Ok` is taken; on the impossible NUL case we
        // skip the `.extpath` (the `.load` below still runs and may resolve via the default path).
        if let Ok(extpath_cmd) = CString::new(format!(".extpath {joined}")) {
            // SAFETY: `self.control` is the live `IDebugControl4` from `create`. `extpath_cmd` is a
            // NUL-terminated ANSI string owned by this stack frame for the whole call (the engine
            // copies it); the `PCSTR` borrows `extpath_cmd.as_ptr()` and does NOT outlive this
            // statement — `extpath_cmd` lives to the end of the enclosing scope. `DEBUG_OUTCTL_IGNORE`
            // / `DEBUG_EXECUTE_NOT_LOGGED` are documented u32 flags. Best-effort: the HRESULT is
            // intentionally ignored (C++ parity).
            let _ = unsafe {
                self.control.Execute(
                    DEBUG_OUTCTL_IGNORE,
                    PCSTR(extpath_cmd.as_ptr().cast()),
                    DEBUG_EXECUTE_NOT_LOGGED,
                )
            };
        }
        // SAFETY: same invariants as `execute`'s `Execute` call; `s!(".load ext.dll")` is a 'static
        // NUL-terminated ANSI string valid for the call. Best-effort: the HRESULT is intentionally
        // ignored (C++ parity).
        let _ = unsafe {
            self.control.Execute(
                DEBUG_OUTCTL_IGNORE,
                s!(".load ext.dll"),
                DEBUG_EXECUTE_NOT_LOGGED,
            )
        };
    }

    /// Guard the execution path: a crash-dump session cannot be resumed. Called by
    /// `go`/`step`/`break_in` (task 2.4). The message is the frozen contract literal (the same
    /// string the tool layer surfaces). `open_dump` (Phase 4) sets `is_dump`, so a real dump
    /// session now reaches this guard; for a live session this is a belt-and-suspenders
    /// engine-level guard behind the mcp-tools `resume()` guard.
    pub fn ensure_runnable(&self) -> Result<(), EngineError> {
        if self.is_dump {
            Err(EngineError::engine("cannot continue a crash-dump session"))
        } else {
            Ok(())
        }
    }

    /// Whether the stored condition for breakpoint `bp_id` is satisfied at the current stop. Ports
    /// the C++ `EvaluateBreakpointCondition`:
    ///
    /// - **No stored condition** → `true` (an unconditional breakpoint always stops). This is the
    ///   common case for the not-found path; the `wait_for_event` hook only calls this when the
    ///   condition map is non-empty, but a hit on a *different* (unconditional) breakpoint still
    ///   lands here and must stop.
    /// - **Has a condition** → evaluate `@@c++( (<cond>) ? 1 : 0 )` as a typed `INT64` via
    ///   `IDebugControl::Evaluate`. A successful eval stops iff the result is non-zero.
    /// - **Eval failed** (`Err`) → `false` (skip). The documented C++ footgun: a condition that
    ///   references a variable not yet in scope fails to evaluate, and we treat that as
    ///   "condition not met" so the breakpoint is silently resumed rather than spuriously stopping.
    fn breakpoint_condition_met(&mut self, bp_id: u32) -> bool {
        let Some(condition) = self.breakpoint_conditions.get(&bp_id) else {
            return true; // No condition — always stop.
        };

        // Wrap the C++ condition in a boolean projection so the typed result is exactly 0 or 1.
        let expr = condition_expr(condition);
        // Bind the NUL-terminated string to a named local so its backing allocation outlives the
        // `Evaluate` call (the runtime-string lifetime rule — a `CString` temporary would be freed
        // before the FFI read otherwise). On an interior NUL the condition is unusable: skip.
        let Ok(expr_c) = CString::new(expr) else {
            return false;
        };

        // The byte length of the expression *excluding* the NUL terminator. `Evaluate` writes
        // `remainder` as the index of the first character it did NOT consume; a fully-parsed
        // expression consumes the whole string, so `remainder` lands at the end (the NUL index,
        // i.e. == `expr_len`). A *partial* parse stops earlier (`remainder < expr_len`).
        let expr_len = expr_c.as_bytes().len() as u32;
        let mut value = DEBUG_VALUE::default();
        let mut remainder = 0u32;
        // SAFETY: `self.control` is a live `IDebugControl4` from `create`. `expr_c` is NUL-terminated
        // and outlives the call (the engine reads it synchronously here); `DEBUG_VALUE_INT64` is a
        // documented desired-type constant; `value`/`remainder` are valid &mut out-params. Evaluate
        // does not retain any pointer past the call.
        let eval = unsafe {
            self.control.Evaluate(
                PCSTR(expr_c.as_ptr().cast()),
                DEBUG_VALUE_INT64,
                &mut value,
                Some(&mut remainder),
            )
        };
        if eval.is_err() {
            // Eval failed — likely the variable isn't in scope yet (the documented footgun). Treat
            // as "condition not met" and skip the stop.
            return false;
        }
        // Partial-parse guard: `remainder` is the index of the first character `Evaluate` did not
        // consume. A complete parse reaches the end (`remainder >= expr_len`); a partial parse stops
        // before the expression's end (`remainder < expr_len`), leaving `value` from a fragment.
        // Treat a partial parse as eval-failure and skip (same policy as the `is_err()` branch
        // above), so a future `condition_expr` bug is a safe skip rather than a silent wrong result.
        if remainder < expr_len {
            return false;
        }
        // SAFETY: `Evaluate` succeeded with `DEBUG_VALUE_INT64` as the desired type, so the engine
        // populated the `I64` arm of the `DEBUG_VALUE` union; reading that arm of the union is the
        // active variant. (The C++ oracle reads `result.I64`; in the windows-crate the field lives
        // at `Anonymous.Anonymous.I64`.) Note the `I64` field is `ULONG64`/`u64` despite its name;
        // the only operation here is `!= 0`, which is correct for either signedness.
        let cond_value: u64 = unsafe { value.Anonymous.Anonymous.I64 };
        cond_value != 0
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
                // If a breakpoint command resumed execution, keep waiting (C++ behavior). This is
                // the *resumed* case (the engine reports a GO/STEP status); the conditional re-loop
                // below is the other case — a genuine `DEBUG_STATUS_BREAK` stop whose stored
                // condition is false.
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

                // Conditional-breakpoint re-loop (ports the C++ `WaitForEvent` tail). DbgEng's
                // command-string conditions can't drive our poll loop, so the condition lives in
                // `breakpoint_conditions` and is evaluated here. Only meaningful for a genuine
                // BREAK stop with at least one stored condition.
                if status == DEBUG_STATUS_BREAK && !self.breakpoint_conditions.is_empty() {
                    let mut event_type = 0u32;
                    let mut process_id = 0u32;
                    let mut thread_id = 0u32;
                    let mut bp_info = DEBUG_LAST_EVENT_INFO_BREAKPOINT::default();
                    let mut extra_used = 0u32;
                    // SAFETY: `self.control` is a live `IDebugControl4`. The three id out-params are
                    // valid `&mut u32`s; `bp_info` is a valid `DEBUG_LAST_EVENT_INFO_BREAKPOINT` we
                    // pass as the extra-information buffer with its true byte size, and `extra_used`
                    // receives the bytes written. We pass `None`/`None` for the description buffer
                    // (the C++ passes `nullptr, 0, nullptr`). The engine writes only into these
                    // locals and retains no pointer past the call.
                    let last_event = unsafe {
                        self.control.GetLastEventInformation(
                            &mut event_type,
                            &mut process_id,
                            &mut thread_id,
                            Some((&mut bp_info as *mut DEBUG_LAST_EVENT_INFO_BREAKPOINT).cast()),
                            std::mem::size_of::<DEBUG_LAST_EVENT_INFO_BREAKPOINT>() as u32,
                            Some(&mut extra_used),
                            None,
                            None,
                        )
                    };

                    if last_event.is_ok()
                        && event_type == DEBUG_EVENT_BREAKPOINT
                        && !self.breakpoint_condition_met(bp_info.Id)
                    {
                        // The stop is a breakpoint whose condition is false: resume and keep
                        // waiting. `continue` re-enters *this* `wait_for_event` call's loop, so a
                        // `go(timeout_ms)` poll slice that resumes a false-condition BP keeps
                        // waiting up to its own 200 ms slice, times out (S_FALSE → `TimedOut`), and
                        // the outer `go` loop re-polls — leaving the interrupt-flag/deadline checks
                        // in `go` fully intact (a resumed-then-still-running slice is just a normal
                        // timeout to `go`). The non-poll callers (launch/attach/break paths) use a
                        // single long `wait_for_event`, where the re-loop simply waits again within
                        // that one call.
                        // SAFETY: live control interface; `DEBUG_STATUS_GO` is the documented resume
                        // status u32. No pointers cross the boundary.
                        unsafe { self.control.SetExecutionStatus(DEBUG_STATUS_GO) }
                            .map_err(|e| EngineError::op("SetExecutionStatus(GO)", e))?;
                        // Bounded per-slice for finite-timeout callers (each `go` slice is 200 ms,
                        // interrupt checked between slices). For `attach_kernel(INFINITE)` a rapid
                        // false-condition BP loop would be unbounded here with no interrupt check —
                        // mitigated in practice by `breakpoint_conditions.clear()` at attach entry
                        // (no conditions exist at the initial kernel break, so this branch can't be
                        // reached on that path).
                        continue;
                    }
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

/// A lifetime guard for an [`IDebugBreakpoint`] obtained from `AddBreakpoint`/`GetBreakpointBy*`.
///
/// DbgEng's breakpoint objects are **owned by the engine**: their lifetime runs from
/// `AddBreakpoint` to `RemoveBreakpoint`, and DbgEng documents that clients must not call their
/// `IUnknown` methods (`AddRef`/`Release`/`QueryInterface`). The `windows`-crate `IDebugBreakpoint`
/// smart pointer would, on `Drop`, call `Release` — which on a DbgEng breakpoint is an access
/// violation (it is not a normally ref-counted object). Wrapping the pointer in `ManuallyDrop`
/// suppresses that `Drop`, so `Release` is never invoked. The wrapped pointer is only ever used to
/// call the breakpoint's *own* methods (`SetOffset`/`AddFlags`/`GetId`/`GetOffset`) and as the
/// argument to `IDebugControl::RemoveBreakpoint`; it never escapes a single engine method.
struct Bp(ManuallyDrop<IDebugBreakpoint>);

impl Bp {
    /// Wrap an engine-owned breakpoint so its `Release` never runs.
    fn new(bp: IDebugBreakpoint) -> Bp {
        Bp(ManuallyDrop::new(bp))
    }

    /// Borrow the underlying interface to call the breakpoint's own (non-`IUnknown`) methods.
    ///
    /// The returned reference is only valid while the breakpoint is registered: after the borrow
    /// is passed to `IDebugControl::RemoveBreakpoint` the underlying pointer is dangling, so the
    /// reference must not be used again (the `remove_breakpoint` call site uses it exactly once and
    /// drops the `Bp` immediately).
    fn get(&self) -> &IDebugBreakpoint {
        &self.0
    }
}

/// Decode a NUL-terminated ANSI buffer (as filled by the DbgEng `*String`/`*Text` calls) into an
/// owned `String`, stopping at the first NUL and lossily decoding any non-UTF-8 bytes. A buffer
/// with no NUL is decoded in full.
fn cstr_from_buf(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Build the typed-evaluator expression for a breakpoint condition. Ports the C++
/// `EvaluateBreakpointCondition` string: wrap the user's C++ condition in `@@c++( (<cond>) ? 1 : 0 )`
/// so `IDebugControl::Evaluate` with `DEBUG_VALUE_INT64` yields exactly `0` or `1`. Pure (FFI-free)
/// so the formatting is unit-testable; the live tests carry the evaluation itself.
pub(crate) fn condition_expr(condition: &str) -> String {
    format!("@@c++( ({condition}) ? 1 : 0 )")
}

/// Read the `KitsRoot10` value (REG_SZ or REG_EXPAND_SZ — see the flags below) from
/// `HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`, returning the install root (e.g.
/// `C:\Program Files (x86)\Windows Kits\10\`) or `None` if the key or value is absent. `wow6432`
/// selects the WOW6432Node view (the 32-bit registry view) so a 64-bit process can still find a
/// value written under `WOW6432Node`; otherwise the native view is read.
///
/// This is the lone live-registry wrapper (the thin FFI boundary) behind the
/// [`discover_debuggers_root_with`] seam; the rest of discovery (its consumption — env + default +
/// existence selection + the `Debuggers\x64` join + dedup) is pure, injectable, and unit-tested via
/// that seam. This function's own FFI correctness is covered incidentally by the live
/// `live_analyze_resolves_on_a_launched_crash` test (which only resolves on a real machine when this
/// read succeeds). A key/value absence is NOT an error here — it returns `None` so discovery can
/// fall through to the next candidate.
fn read_kits_root10(wow6432: bool) -> Option<PathBuf> {
    // The subkey + value are constant UTF-16 literals (NUL-terminated for the wide FFI). `RegGetValueW`
    // wants `PCWSTR`s; we build owned wide buffers so the pointers are valid for the call.
    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "KitsRoot10"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Accept REG_SZ and REG_EXPAND_SZ (a non-standard SDK installer could write `KitsRoot10` as
    // REG_EXPAND_SZ, e.g. `%ProgramFiles(x86)%...`; with `RRF_RT_REG_EXPAND_SZ` set `RegGetValueW`
    // auto-expands it before returning, so we get a usable absolute path either way). Pick the
    // registry view explicitly (native vs WOW6432Node).
    let flags = RRF_RT_REG_SZ
        | RRF_RT_REG_EXPAND_SZ
        | if wow6432 {
            RRF_SUBKEY_WOW6432KEY
        } else {
            RRF_SUBKEY_WOW6464KEY
        };

    // First call with no buffer to learn the byte size, then a second call to read the data.
    let mut cb: u32 = 0;
    // SAFETY: `HKEY_LOCAL_MACHINE` is a documented predefined key handle. `subkey`/`value` are
    // NUL-terminated UTF-16 buffers owned on this stack and outliving the call; the `PCWSTR`s borrow
    // their pointers only for this call. `pvdata = None` with a valid `pcbdata` is the documented
    // "query required size" form. The return is a `WIN32_ERROR`; no out-pointers are read on failure.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            flags,
            None,
            None,
            Some(&mut cb),
        )
    };
    if rc.is_err() || cb == 0 {
        return None;
    }

    // `cb` is a byte count; REG_SZ data is UTF-16, so allocate that many u16 elements (rounded up).
    let mut buf: Vec<u16> = vec![0u16; (cb as usize).div_ceil(2)];
    let mut cb2: u32 = cb;
    // SAFETY: same key/subkey/value invariants as the sizing call above. `buf` has at least `cb2`
    // bytes of backing storage (`div_ceil(cb, 2)` u16 elements ≥ `cb` = `cb2` bytes), so the API can
    // write up to `cb2` bytes safely; `buf` is owned here for the call, `pvdata` points at its bytes,
    // and `pcbdata` (`cb2`) is updated to the bytes written. The `windows` crate copies nothing it
    // retains.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut cb2),
        )
    };
    if rc.is_err() {
        return None;
    }
    // Trim to the returned byte length and drop the trailing NUL(s) REG_SZ includes.
    let n = (cb2 as usize) / 2;
    let slice = &buf[..n.min(buf.len())];
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let s = String::from_utf16_lossy(&slice[..end]);
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Discover the `Debuggers\x64` root of an installed *Debugging Tools for Windows* at RUNTIME, the
/// R8 refinement that replaces the C++'s hardcoded path (task 5.0). Resolution order, returning the
/// FIRST candidate whose base dir EXISTS:
///
/// 1. Registry `KitsRoot10` (native view, then the `WOW6432Node` view) joined with `Debuggers\x64`.
/// 2. The `WindowsSdkDir` env var joined with `Debuggers\x64`.
/// 3. The former hardcoded default `C:\Program Files (x86)\Windows Kits\10\Debuggers\x64`.
///
/// Returns `None` if none of the candidates exist. This is a thin production wrapper: it binds the
/// live registry reader ([`read_kits_root10`]), the `WindowsSdkDir` env var, and `Path::exists` into
/// the pure [`discover_debuggers_root_with`] seam, which holds all the resolution logic and is
/// unit-tested with injected fakes (no registry/COM/FS).
fn discover_debuggers_root() -> Option<PathBuf> {
    discover_debuggers_root_with(read_kits_root10, std::env::var("WindowsSdkDir").ok(), |p| {
        p.exists()
    })
}

/// Pure resolution logic of [`discover_debuggers_root`] with the registry, env, and filesystem
/// dependencies injected so it is unit-testable without a live registry/COM/filesystem:
///
/// - `reg_reader(wow6432)` yields the `KitsRoot10` install root for the requested registry view
///   (native = `false`, WOW6432Node = `true`), or `None` when absent. In production this is
///   [`read_kits_root10`].
/// - `env_sdk_dir` is the `WindowsSdkDir` env var value (`None`/empty = unset).
/// - `exists(path)` is the base-dir existence predicate (`Path::exists` in production).
///
/// It assembles candidates in priority order — registry native view, registry WOW6432Node view,
/// env, then the former hardcoded default — joining each base with `Debuggers\x64`, de-duplicates
/// adjacent/identical candidates (the native and WOW6432 views usually return the SAME path, so the
/// derived `Debuggers\x64` candidate would otherwise appear twice), and returns the first candidate
/// whose base dir satisfies `exists`. The priority order (native registry → WOW6432 → env → default)
/// is preserved: dedup only drops a path already present earlier in the list, so the native view
/// still wins when both views agree.
pub(crate) fn discover_debuggers_root_with(
    reg_reader: impl Fn(bool) -> Option<PathBuf>,
    env_sdk_dir: Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let push_unique = |cand: PathBuf, candidates: &mut Vec<PathBuf>| {
        if !candidates.contains(&cand) {
            candidates.push(cand);
        }
    };
    // 1. Registry KitsRoot10 — native view first, then WOW6432Node.
    for wow in [false, true] {
        if let Some(root) = reg_reader(wow) {
            push_unique(root.join("Debuggers").join("x64"), &mut candidates);
        }
    }
    // 2. WindowsSdkDir env var.
    if let Some(sdk) = env_sdk_dir {
        if !sdk.is_empty() {
            push_unique(
                PathBuf::from(sdk).join("Debuggers").join("x64"),
                &mut candidates,
            );
        }
    }
    // 3. The former hardcoded default, as a last resort.
    push_unique(
        PathBuf::from("C:\\Program Files (x86)\\Windows Kits\\10\\Debuggers\\x64"),
        &mut candidates,
    );

    select_existing_root(candidates, exists)
}

/// Pure selection step of [`discover_debuggers_root`]: return the first candidate whose base dir
/// satisfies the `exists` predicate, or `None`. Factored out so the ordering/selection logic is
/// unit-testable without touching the live filesystem or registry.
pub(crate) fn select_existing_root(
    candidates: impl IntoIterator<Item = PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates.into_iter().find(|p| exists(p))
}

/// Build the debugger-extension search dirs from a resolved `Debuggers\x64` root, filtered to the
/// dirs that actually exist (per the injected `exists` predicate). Mirrors the C++ winext/WINXP/base
/// set: `<root>\winext`, `<root>\winxp`, and `<root>` itself, in that order. The C++ used `WINXP`
/// (uppercase); `Path::exists` is case-insensitive on Windows, so the literal case here does not
/// matter. The returned list (joined with `;`) becomes the `.extpath` argument; an empty list means
/// no extensions are available and `.extpath` is skipped.
pub(crate) fn extension_dirs(
    debuggers_x64_root: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    [
        debuggers_x64_root.join("winext"),
        debuggers_x64_root.join("winxp"),
        debuggers_x64_root.to_path_buf(),
    ]
    .into_iter()
    .filter(|p| exists(p))
    .collect()
}

/// Cap a long command report at 32 KiB, appending the C++ truncation marker. Ports the C++
/// `truncateOutput` (`advanced_tools.cpp`): `MAX_OUTPUT_SIZE = 32768`; output larger than the cap
/// is cut to exactly the cap and the fixed `"\n\n... (output truncated at 32KB)"` suffix appended.
/// Used by [`Engine::analyze`] so `!analyze -v`'s (often huge) report is bounded.
///
/// The cap is applied on a UTF-8 char boundary at or below 32768 bytes (the C++ `resize` cuts at a
/// raw byte index; we round down to the nearest char boundary so the returned `String` stays valid
/// UTF-8 — the captured text is already lossily decoded ANSI, so this only matters for multi-byte
/// replacement chars and never lengthens the output beyond the cap).
pub(crate) fn truncate_output(mut output: String) -> String {
    const MAX_OUTPUT_SIZE: usize = 32768; // 32 KiB, matching the C++ `MAX_OUTPUT_SIZE`.
    if output.len() > MAX_OUTPUT_SIZE {
        let mut cut = MAX_OUTPUT_SIZE;
        while cut > 0 && !output.is_char_boundary(cut) {
            cut -= 1;
        }
        output.truncate(cut);
        output.push_str("\n\n... (output truncated at 32KB)");
    }
    output
}

/// Map a `DEBUG_MODULE_PARAMETERS::SymbolType` to the neutral `ModuleInfo::symbol_status` token
/// (the lowercase set documented on `debugger_core::ModuleInfo`). The full-symbol formats
/// (PDB/CODEVIEW/COFF/SYM/DIA) all collapse to `pdb` ("real symbols loaded — names resolve"),
/// EXPORT→`export` (export-table only), DEFERRED→`deferred` (not loaded yet), and `DEBUG_SYMTYPE_NONE`
/// (or any unknown value)→`none`. Collapsing the loaded formats to `pdb` keeps the token within the
/// documented vocabulary while still telling the agent "symbols are available".
pub(crate) fn symbol_status(symbol_type: u32) -> String {
    let token = if symbol_type == DEBUG_SYMTYPE_PDB
        || symbol_type == DEBUG_SYMTYPE_CODEVIEW
        || symbol_type == DEBUG_SYMTYPE_COFF
        || symbol_type == DEBUG_SYMTYPE_SYM
        || symbol_type == DEBUG_SYMTYPE_DIA
    {
        "pdb"
    } else if symbol_type == DEBUG_SYMTYPE_EXPORT {
        "export"
    } else if symbol_type == DEBUG_SYMTYPE_DEFERRED {
        "deferred"
    } else {
        "none"
    };
    token.to_string()
}

/// Extract the mnemonic+operands from one `IDebugControl::Disassemble` text line. The engine
/// renders a single line shaped `00007ff6`b1657280 894c2408        mov     dword ptr [rsp+8],ecx`
/// — a backtick-formatted address, a single space, the raw instruction bytes (one hex run, no
/// internal spaces), whitespace, then the instruction. The byte run can be long enough that only a
/// *single* space separates it from the mnemonic (e.g. `c744240400000000 mov …`), so a
/// double-space split is unreliable; instead we drop the first two whitespace-delimited tokens
/// (address, bytes) and keep the remainder as the instruction. Falls back to the whole trimmed
/// line if the expected two leading columns are absent (never returns empty-handed when the line
/// has content).
fn parse_disasm_text(line: &str) -> String {
    let trimmed = line.trim_end_matches(['\r', '\n']).trim();
    // Tokenize on whitespace: [0] = address, [1] = raw bytes, [2..] = instruction.
    let mut it = trimmed.split_whitespace();
    let _addr = it.next();
    let _bytes = it.next();
    let rest: Vec<&str> = it.collect();
    if rest.is_empty() {
        // Unexpected shape (fewer than three columns): return the whole trimmed line.
        trimmed.to_string()
    } else {
        rest.join(" ")
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
