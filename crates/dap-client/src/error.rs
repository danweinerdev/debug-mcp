//! Transport-layer errors for the wire/framing layer.
//!
//! The wire layer reports [`WireError`]; the client maps the transport-failure cases
//! into [`debugger_core::BackendError`] when it unblocks waiters (the pending map and
//! the stop waiter speak the neutral `BackendError`, per design §"DAP Client
//! internals"). Go origin: the `fmt.Errorf` wraps in `internal/dap/client.go`
//! (`SendAsync: write failed`, `read loop terminated`) and the framing errors
//! go-dap's `ReadProtocolMessage`/`WriteProtocolMessage` return.

use std::io;

use thiserror::Error;

/// A failure in the DAP wire layer (framing, IO, or JSON).
#[derive(Debug, Error)]
pub enum WireError {
    /// A clean end-of-stream before any byte of a new frame — the peer closed the
    /// connection between messages (the normal shutdown / crash-recovery path).
    #[error("connection closed (EOF)")]
    Eof,

    /// An underlying IO error, including a truncated frame (EOF mid-header/mid-body,
    /// surfaced as `UnexpectedEof`).
    #[error("io error: {0}")]
    Io(#[source] io::Error),

    /// The header block ended without a `Content-Length` field.
    #[error("missing Content-Length header")]
    MissingContentLength,

    /// `Content-Length` was present but not a valid byte count.
    #[error("invalid Content-Length header: {0}")]
    InvalidContentLength(String),

    /// The framed body was not valid JSON / did not match the message schema.
    #[error("malformed DAP message body: {0}")]
    Json(#[source] serde_json::Error),
}

impl WireError {
    /// True when this error means the transport is gone (EOF or any IO error),
    /// i.e. the read loop must run its EOF-recovery sequence. A malformed-but-framed
    /// message (`MissingContentLength`/`InvalidContentLength`/`Json`) is *not* terminal
    /// here — but the read loop, like go-dap's, treats **any** read error as terminal
    /// (see [`crate::read_loop`]); this predicate is for callers that distinguish.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, WireError::Eof | WireError::Io(_))
    }
}
