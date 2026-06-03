//! `dap-client` — the generic, debugger-agnostic DAP transport (Phase 2).
//!
//! Below the seam: this crate owns the DAP wire framing, request/response correlation,
//! the read loop with the full event-dispatch table, the single-slot stop waiter, and
//! EOF/crash recovery. It knows nothing about lldb — request `arguments` are raw JSON
//! (the lldb arg shapes live in Phase 3's `lldb-backend`).
//!
//! Layout (Go → Rust module map, design Appendix):
//! - [`wire`] — Content-Length framing + the ~20 DAP message types (`internal/dap/types.go`).
//! - [`client`] — seq, pending map, `send`/`send_async`/`send_and_await_both`, the
//!   `AbortGuard` (`internal/dap/client.go`).
//! - [`stop_waiter`] — the single-slot stop waiter (`internal/dap/stopwaiter.go`).
//! - [`read_loop`] — the read-loop task, event dispatch, EOF recovery (`client.go` ReadLoop).
//!
//! Wiring (what `lldb-backend` does in Phase 3):
//! ```ignore
//! let client = Client::new(child_stdin);
//! let (read_loop, channels) = ReadLoop::new(child_stdout_buf, client.shared_for_read_loop());
//! tokio::spawn(read_loop.run());
//! // drive `channels.output` / `channels.terminated` into a `BackendEvent` stream.
//! ```

mod client;
mod error;
mod read_loop;
mod stop_waiter;
mod wire;

pub use client::{Client, Shared};
pub use error::WireError;
pub use read_loop::{OutputChunk, ReadLoop, ReadLoopChannels};
pub use stop_waiter::StopWaiter;
pub use wire::{
    read_message, write_message, DapMessage, Envelope, Event, ExitedBody, ExitedEvent, OutputBody,
    OutputEvent, Request, Response, StoppedBody, StoppedEvent,
};
