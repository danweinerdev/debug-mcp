//! The 21 MCP tool handlers, each an `impl ToolServer` method dispatched by
//! [`ToolServer::dispatch`](crate::server::ToolServer). Grouped to mirror the Go
//! `internal/tools/*.go` files (design Appendix Go→Rust module map):
//!
//! - `lifecycle`: `launch`, `attach`, `disconnect`.
//! - `breakpoints`: `set_breakpoint`, `set_function_breakpoint`, `remove_breakpoint`,
//!   `list_breakpoints`.
//! - `execution`: `continue`, `step_over`, `step_into`, `step_out`, `pause` + the shared
//!   stop-outcome formatter.
//! - `inspection`: `status`, `backtrace`, `threads`, `variables`, `evaluate`.
//! - `memory`: `read_memory`, `disassemble`.
//! - `output`: `read_output`.
//! - `run_command`: `run_command`.
//! - `windbg`: the capability-gated `open_crash_dump`, `attach_kernel`, `analyze_crash`,
//!   `get_modules` (advertised only when a WinDbg-capable factory is registered).

mod breakpoints;
mod execution;
mod inspection;
mod lifecycle;
mod memory;
mod output;
mod run_command;
mod windbg;
