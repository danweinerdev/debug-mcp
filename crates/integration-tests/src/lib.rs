//! `integration-tests` — a test-support crate (Phase 6).
//!
//! It carries no production binary; its library is purely the shared test support the
//! live integration + differential-parity suites build on:
//!
//! - [`harness`] — the in-process driver (a real [`mcp_tools::ToolServer`] over a real
//!   [`mcp_session::SessionManager`] + the real [`lldb_backend::LldbFactory`]), fixture
//!   discovery, per-call timeouts, the disconnect-cleanup helper, and the
//!   `parse_tool_result` / skip-when-absent helpers.
//! - [`stdio`] — a minimal newline-delimited JSON-RPC MCP client for driving the
//!   `debug-mcp` / Go `lldb-debug-mcp` binaries over stdio in the differential harness.
//!
//! The live suites live in `mcp-tools/tests/integration_*.rs` (gated behind mcp-tools'
//! `integration` feature) and consume these as `pub` library API. Keeping the helpers in
//! a library (rather than a `tests/common/` module compiled into every test binary) is
//! what lets them be `pub` without a `#![allow(dead_code)]` — every helper is exported, so
//! the dead-code lint never fires.
//!
//! Why a dedicated crate: it depends on `lldb-backend` (the real factory) and `mcp-tools`
//! (the real handlers), but `mcp-tools` only **dev**-depends on it — so `mcp-tools`' normal
//! dependency edge never names `lldb-backend`, keeping the seam crate's production
//! dependency graph clean (design §Crate layout / Spec FR-18). The dev-dependency cycle
//! (`mcp-tools` →(dev) `integration-tests` →(normal) `mcp-tools`) is allowed by cargo.

pub mod harness;
pub mod stdio;
