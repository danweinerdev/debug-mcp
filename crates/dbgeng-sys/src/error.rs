//! `EngineError` — the `dbgeng-sys`-local error type.
//!
//! Every fallible COM call in this crate returns `Result<_, EngineError>`. An `EngineError`
//! pairs the underlying [`windows::core::Error`] (which carries the failing `HRESULT` and the
//! OS-formatted message) with a `&'static str` *context* label naming the operation that failed
//! (e.g. `"DebugCreate"`, `"QueryInterface(IDebugControl4)"`). Mapping this onto the neutral
//! `debugger_core::BackendError` happens upstream in `windbg-backend` (Phase 3); this crate only
//! ever returns `EngineError`.

use windows::core::HRESULT;
use windows::Win32::Foundation::E_FAIL;

/// A `dbgeng-sys` failure. Either a COM call returned a failing `HRESULT` ([`EngineError::Com`]),
/// or the engine itself refused/aborted with a free-form reason ([`EngineError::Engine`] — e.g.
/// a wait timeout, a not-yet-implemented stub, or the crash-dump execution guard).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A DbgEng COM operation failed: the failing `HRESULT` (wrapped in a
    /// [`windows::core::Error`]) plus a static label naming the operation/context.
    #[error("{context} failed: {hr:#010x} ({message})", hr = source.code().0, message = source.message())]
    Com {
        /// The operation/context that failed (e.g. `"DebugCreate"`).
        context: &'static str,
        /// The underlying windows-crate error, carrying the `HRESULT` and its message.
        #[source]
        source: windows::core::Error,
    },
    /// An engine-logic failure with a free-form message (no single owning `HRESULT`): a wait
    /// timeout, an unimplemented stub, or the dump-session guard. The message is surfaced
    /// verbatim by `Display`.
    #[error("{0}")]
    Engine(String),
}

impl EngineError {
    /// Build an `EngineError` from a failing COM operation: the static `context` label and the
    /// `windows::core::Error` returned by the call.
    pub fn op(context: &'static str, source: windows::core::Error) -> EngineError {
        EngineError::Com { context, source }
    }

    /// Build an engine-logic error with a free-form, verbatim message (timeout / stub / guard).
    pub fn engine(message: impl Into<String>) -> EngineError {
        EngineError::Engine(message.into())
    }

    /// The static operation/context label, or `"engine"` for a free-form engine error.
    pub fn context(&self) -> &'static str {
        match self {
            EngineError::Com { context, .. } => context,
            EngineError::Engine(_) => "engine",
        }
    }

    /// The failing `HRESULT` for a COM error, or `E_FAIL` for a free-form engine error.
    pub fn hresult(&self) -> HRESULT {
        match self {
            EngineError::Com { source, .. } => source.code(),
            EngineError::Engine(_) => E_FAIL,
        }
    }
}
