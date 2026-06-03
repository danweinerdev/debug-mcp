//! `debugger-core` — the neutral debugger contract.
//!
//! This is the seam every other crate is written against: the `DebuggerBackend` and
//! `BackendFactory` traits, the neutral data types, `BackendError`, `BackendEvent`,
//! and `Connection`. It is a leaf crate — it depends only on `serde`, `serde_json`,
//! `async-trait`, `futures`, and `thiserror`, and deliberately has **no** `tokio`,
//! `rmcp`, or DAP dependency, so DAP/lldb types are *unnameable* above the seam
//! (Spec FR-18, design Decision 1).

mod backend;
mod error;
mod event;
mod types;

pub use backend::{BackendFactory, Connection, DebuggerBackend};
pub use error::BackendError;
pub use event::BackendEvent;
pub use types::{
    AttachOutcome, AttachSpec, BreakpointResult, EvalMode, EvalResult, Frame, FunctionBp,
    Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead, Scope, SourceBp, StepKind,
    StopInfo, StopOutcome, ThreadInfo, Variable,
};

#[cfg(test)]
mod tests;
