//! `EngineError` — the `dbgeng-sys`-local error type.
//!
//! Every fallible COM call in this crate returns `Result<_, EngineError>`. An `EngineError`
//! pairs the underlying [`windows::core::Error`] (which carries the failing `HRESULT` and the
//! OS-formatted message) with a `&'static str` *context* label naming the operation that failed
//! (e.g. `"DebugCreate"`, `"QueryInterface(IDebugControl4)"`). Mapping this onto the neutral
//! `debugger_core::BackendError` happens upstream in `windbg-backend` (Phase 3); this crate only
//! ever returns `EngineError`.

use windows::core::HRESULT;

/// A DbgEng COM operation failure: the failing `HRESULT` (wrapped in a [`windows::core::Error`])
/// plus a static label naming the operation/context.
#[derive(Debug, thiserror::Error)]
#[error("{context} failed: {hr:#010x} ({message})", hr = self.source.code().0, message = self.source.message())]
pub struct EngineError {
    /// The operation/context that failed (e.g. `"DebugCreate"`).
    context: &'static str,
    /// The underlying windows-crate error, carrying the `HRESULT` and its message.
    #[source]
    source: windows::core::Error,
}

impl EngineError {
    /// Build an `EngineError` from a failing COM operation: the static `context` label and the
    /// `windows::core::Error` returned by the call.
    pub fn op(context: &'static str, source: windows::core::Error) -> EngineError {
        EngineError { context, source }
    }

    /// The static operation/context label this error was tagged with.
    pub fn context(&self) -> &'static str {
        self.context
    }

    /// The failing `HRESULT`.
    pub fn hresult(&self) -> HRESULT {
        self.source.code()
    }
}
