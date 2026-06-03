//! `mcp-session` — the neutral `SessionManager`.
//!
//! Everything Go's `internal/session/session.go` owned, minus the DAP specifics: the
//! state machine + read-only guards + the generation epoch (Spec FR-4), breakpoint
//! tracking (FR-7), the [`OutputBuffer`] (FR-12), the frame-map + last-stop caches, and
//! the [`spawn_event_pump`] task that drains a backend's [`BackendEvent`] stream into the
//! buffer/state.
//!
//! It depends only on `debugger-core` (the seam) + `tokio`/`futures` for the pump — it
//! cannot name a DAP or lldb type. The session API is consumed by the Phase 5 tool
//! handlers.
//!
//! [`BackendEvent`]: debugger_core::BackendEvent

mod breakpoint;
mod manager;
mod output_buffer;
mod pump;
mod state;

pub use breakpoint::BreakpointInfo;
pub use manager::SessionManager;
pub use output_buffer::{OutputBuffer, OutputEntry};
pub use pump::spawn_event_pump;
pub use state::State;

#[cfg(test)]
mod tests;
