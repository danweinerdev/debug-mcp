//! `mcp-tools` — the helper layer the 21 MCP tool handlers build on.
//!
//! This crate is split into an **rmcp-free, pure-logic foundation** (Phase 5.1/5.2)
//! and the rmcp-aware handlers + server wiring (Phase 5.3–5.5, added later). The
//! foundation here is deliberately free of any rmcp type so it is trivially
//! unit-testable:
//!
//! - [`args`]: the [`Args`] accessor reproducing mcp-go's permissive argument
//!   handling and the exact Go validation strings (Spec FR-3).
//! - [`response`]: [`ToolOutcome`] (the rmcp-free response intent) + [`RespBuilder`]
//!   for conditional/omit-empty payload assembly.
//! - [`format`]: [`format_hex_dump`] (Spec FR-13.1) and [`format_output_entries`]
//!   (Spec FR-12.5).
//! - [`flatten`]: [`flatten_variables`] (Spec FR-11) + [`FlatVariable`] +
//!   [`VariableFetcher`].
//!
//! SEAM: no `dap-client`/`lldb-backend` dependency. The handler/server layer adds the
//! rmcp glue on top of these helpers:
//!
//! - [`errors`]: per-call-site mapping of `BackendError` → the exact Go tool-error strings.
//! - [`frame`]: `resolve_frame_id` (implicit `stack_trace(levels=20)` on a frame-map miss).
//! - [`schema`]: the 21 hand-built tool definitions (Decision 3 / R2).
//! - [`server`]: [`ToolServer`] — shared state + the rmcp `ServerHandler` (Decision 7 / R1).
//! - [`handlers`]: the 21 tool handlers (`impl ToolServer`).

mod args;
mod errors;
mod flatten;
mod format;
mod frame;
mod handlers;
mod response;
mod schema;
mod server;

pub use args::Args;
pub use flatten::{flatten_variables, FlatVariable, VariableFetcher};
pub use format::{format_hex_dump, format_output_entries};
pub use response::{RespBuilder, ToolOutcome};
pub use server::ToolServer;

#[cfg(test)]
mod tests;
