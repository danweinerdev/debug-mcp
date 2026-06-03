//! Unit tests for the neutral `SessionManager`, kept in a dedicated `src/tests/` folder
//! per project convention (not inline `#[cfg(test)]` in source).
//!
//! Each module mirrors the behaviors `internal/session/session_test.go` pins, plus the
//! Rust-only additions the plan calls out (the generation epoch, the oversize-entry
//! output vector, and the event-pump generation guard).

mod breakpoints;
mod output_buffer;
mod pump;
mod state;
