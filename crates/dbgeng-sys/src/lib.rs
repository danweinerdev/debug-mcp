//! `dbgeng-sys` — the confined DbgEng (WinDbg) COM/FFI layer.
//!
//! This is the **one crate in the workspace permitted to contain `unsafe`** (design Decision 1).
//! It wraps Microsoft's official [`windows`] crate to drive the DbgEng engine
//! (`dbgeng.dll`: `IDebugClient5`/`IDebugControl4`/`IDebugSymbols3`/`IDebugDataSpaces4`/
//! `IDebugRegisters2`/`IDebugSystemObjects4`) and exposes a **safe, synchronous** [`Engine`]
//! handle. Everything above it (`windbg-backend` and up) stays `#![forbid(unsafe_code)]`.
//!
//! ## The confined-unsafe contract
//!
//! - All `unsafe` in the workspace lives under `crates/dbgeng-sys/src/`; the `unsafe-gate`
//!   Makefile target enforces this with a source scan, and every other crate root carries
//!   `#![forbid(unsafe_code)]` as belt-and-suspenders.
//! - Every `unsafe` block in this crate carries a `// SAFETY:` comment justifying the COM
//!   call's preconditions (apartment/thread ownership, pointer validity, refcount).
//! - The COM interfaces are held as `windows`-crate smart pointers, so `AddRef`/`Release` are
//!   automatic via `Clone`/`Drop` — the C++ manual-`Release` footgun does not apply (see
//!   [`engine`]).
//!
//! The crate is **Windows-only**: the library is `#![cfg(windows)]` and the `windows`
//! dependency sits under `[target.'cfg(windows)'.dependencies]`, so off Windows the crate
//! compiles to nothing.
//!
//! Failures surface as [`EngineError`] (an `HRESULT` + operation context); mapping to the
//! neutral `debugger_core::BackendError` happens upstream in `windbg-backend` (Phase 3).
#![cfg(windows)]

mod callbacks;
mod com;
mod engine;
mod error;
mod process;

#[cfg(test)]
mod tests;

pub use callbacks::{exception_breaks, OutputKind, OutputSink};
pub use com::ComApartment;
pub use engine::{BpLoc, Engine, InterruptHandle, LaunchReq};
pub use error::EngineError;
pub use process::find_process_by_name;
