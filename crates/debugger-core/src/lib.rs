//! `debugger-core` — the neutral debugger contract.
//!
//! This is the seam every other crate is written against: the `DebuggerBackend` and
//! `BackendFactory` traits, the neutral data types, `BackendError`, `BackendEvent`,
//! and `Connection`. It is a leaf crate — it depends only on `serde`, `serde_json`,
//! `async-trait`, `futures`, and `thiserror`, and deliberately has **no** `tokio`,
//! `rmcp`, or DAP dependency, so DAP/lldb types are *unnameable* above the seam
//! (Spec FR-18, design Decision 1).
//!
//! No `unsafe`: all COM/FFI `unsafe` is confined to `dbgeng-sys` (design Decision 1).
#![forbid(unsafe_code)]

mod backend;
mod capabilities;
mod error;
mod event;
mod types;

pub use backend::{BackendFactory, Connection, DebuggerBackend};
pub use capabilities::BackendCapabilities;
pub use error::BackendError;
pub use event::BackendEvent;
pub use types::{
    AttachOutcome, AttachSpec, BreakpointResult, DumpOutcome, EvalMode, EvalResult, Frame,
    FunctionBp, Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead, ModuleInfo, Scope,
    SourceBp, StepKind, StopInfo, StopOutcome, ThreadInfo, Variable,
};

#[cfg(test)]
mod tests;
