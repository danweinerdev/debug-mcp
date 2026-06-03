//! `lldb-backend` — the lldb-dap backend (Phase 3).
//!
//! Turns the generic [`dap_client`] transport into a [`debugger_core::DebuggerBackend`]
//! by owning everything lldb-specific below the seam:
//!
//! - [`detect`] — lldb-dap detection (Spec FR-15): the env/PATH/versioned/`xcrun`
//!   fallback chain + the repl-mode capability flag.
//! - [`subprocess`] — `tokio::process` spawn + the stderr keep-last-N ring (Spec FR-16).
//! - [`args`] — the lldb-dap launch/attach argument shapes (Spec FR-17.9 — exact JSON
//!   field names + `omitempty`).
//! - [`requests`] — the per-command DAP `arguments` builders.
//! - [`body`] — typed decode of DAP response bodies → neutral types.
//! - [`backend`] — [`LldbBackend`]: the launch/attach handshake (incl. the stop-waiter
//!   placement asymmetry + the order-independent `InitializedEvent` wait) and every
//!   `DebuggerBackend` op, translated to neutral types. No response *formatting* here —
//!   hex dump / variable flatten / JSON shaping are Phase 5.
//! - [`factory`] — [`LldbFactory`]: detect → spawn → client → read loop → event stream →
//!   [`debugger_core::Connection`].
//!
//! Built on `dap-client`; the launch/attach handshake uses its `send_and_await_both`
//! (order-independent response + Initialized) and its stop waiter (register-before-resume).

mod args;
mod backend;
mod body;
mod detect;
mod factory;
mod requests;
mod subprocess;

pub use backend::LldbBackend;
pub use detect::{find_lldb_dap, Detected};
pub use factory::LldbFactory;
pub use subprocess::{spawn, StderrBuffer, Subprocess};

#[cfg(test)]
mod tests;
