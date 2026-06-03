//! The single error type the `DebuggerBackend` trait returns.
//!
//! Tool handlers map these variants into the **exact Go strings** at each call site
//! (e.g. `continue request failed: <e>`); this crate carries only the neutral cause.
//! Go origin: the failure modes enumerated across `internal/dap` and the
//! `internal/tools/*.go` handlers (design §Error Handling).

use thiserror::Error;

/// A backend operation failure. One enum per cause; the tool layer owns the
/// user-facing wording.
#[derive(Debug, Error)]
pub enum BackendError {
    /// Could not locate a debugger binary (Go: `failed to find lldb-dap`).
    #[error("failed to detect debugger: {0}")]
    Detect(String),

    /// Could not spawn the debugger subprocess (Go: `failed to spawn lldb-dap`).
    #[error("failed to spawn debugger: {0}")]
    Spawn(String),

    /// Failed to write a request to the debugger (Go: the various
    /// `<op> request failed` send errors).
    #[error("failed to send request: {0}")]
    Send(String),

    /// Received a response of an unexpected message type (Go:
    /// `unexpected <op> response type: <type>`). `type` is the offending type name.
    #[error("unexpected response type: {ty}")]
    Protocol { ty: String },

    /// The debugger returned a `success=false` response (Go: `<op> failed: <message>`).
    #[error("debugger error: {message}")]
    Dap { message: String },

    /// The transport closed / read loop terminated (EOF) (Go: `read loop terminated`).
    #[error("connection closed")]
    Closed,

    /// An operation exceeded its bound (Go: the per-tool `<op> timed out` strings).
    #[error("operation timed out")]
    Timeout,
}
