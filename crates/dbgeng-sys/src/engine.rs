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

use std::sync::{Arc, Mutex};

#[cfg(test)]
use windows::core::PCSTR;
use windows::core::{s, Interface};
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DebugCreate, IDebugClient5, IDebugControl4, IDebugDataSpaces4, IDebugEventCallbacks,
    IDebugOutputCallbacks, IDebugRegisters2, IDebugSymbols3, IDebugSystemObjects4,
};
#[cfg(test)]
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DEBUG_EXECUTE_DEFAULT, DEBUG_OUTCTL_THIS_CLIENT,
};
use windows::Win32::System::Diagnostics::Debug::SYMOPT_NO_IMAGE_SEARCH;

use crate::callbacks::{self, CallbackState, OutputSink};
use crate::error::EngineError;

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

    // The remaining five interfaces back the operations added in tasks 2.2–2.5. They are
    // exposed as borrowing accessors so the scaffolding compiles warning-clean (without a
    // consumer the fields would be `dead_code` under `-D warnings`; `pub` fns are exempt while
    // `pub(crate)` ones are not, which is why these are `pub` for now). They are SCAFFOLDING:
    // as 2.2–2.5 add the real `&mut self` neutral-typed methods (which touch the fields
    // directly), these accessors are narrowed to `pub(crate)` or removed so the raw COM
    // interfaces never form part of the crate's public surface (the "safe Engine" contract).

    /// The root `IDebugClient5` (lifecycle: launch/attach/dump/EndSession — task 2.3).
    pub fn client(&self) -> &IDebugClient5 {
        &self.client
    }

    /// The `IDebugControl4` (execution, `Execute`, `Evaluate`, `WaitForEvent` — tasks 2.4/2.5).
    pub fn control(&self) -> &IDebugControl4 {
        &self.control
    }

    /// `IDebugSymbols3` (symbol/module queries — tasks 2.4/2.5).
    pub fn symbols(&self) -> &IDebugSymbols3 {
        &self.symbols
    }

    /// `IDebugDataSpaces4` (memory reads — task 2.5).
    pub fn data_spaces(&self) -> &IDebugDataSpaces4 {
        &self.data_spaces
    }

    /// `IDebugRegisters2` (register reads — task 2.5).
    pub fn registers(&self) -> &IDebugRegisters2 {
        &self.registers
    }

    /// `IDebugSystemObjects4` (process/thread enumeration — task 2.4).
    pub fn system_objects(&self) -> &IDebugSystemObjects4 {
        &self.system_objects
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
