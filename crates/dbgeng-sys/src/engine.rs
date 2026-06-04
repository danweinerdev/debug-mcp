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

use windows::core::{s, Interface};
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DebugCreate, IDebugClient5, IDebugControl4, IDebugDataSpaces4, IDebugRegisters2,
    IDebugSymbols3, IDebugSystemObjects4,
};
use windows::Win32::System::Diagnostics::Debug::SYMOPT_NO_IMAGE_SEARCH;

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

        // task 2.2: register callbacks (SetEventCallbacks/SetOutputCallbacks) — the callback
        // types do not exist yet.

        Ok(Engine {
            client,
            control,
            symbols,
            data_spaces,
            registers,
            system_objects,
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
