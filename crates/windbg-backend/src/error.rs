//! The `dbgeng-sys` error type, re-exported for the backend's internal signatures.
//!
//! `EngineOps`/`EngineCmd` carry `Result<_, EngineError>` end-to-end (the `Engine` already
//! returns the neutral `debugger_core` *value* types; only its *error* is crate-local), and
//! [`WinDbgBackend::call`](crate::backend::WinDbgBackend) maps an `EngineError` onto the neutral
//! `debugger_core::BackendError` at the seam. Re-exporting here keeps those internal paths short
//! and gives the mapping one obvious home.

pub use dbgeng_sys::EngineError;
