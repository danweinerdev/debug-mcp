//! Resolved-breakpoint metadata tracked by the session (Spec FR-7).
//!
//! Go origin: `internal/session/session.go` `BreakpointInfo`. IDs are assigned by the
//! debugger (DAP responses), never by the session.

/// Metadata about a resolved breakpoint, keyed in the session by its debugger-assigned
/// `id`. `ty` is `"source"` or `"function"` and selects the removal-matching rule
/// (source by line, function by name — Spec FR-7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointInfo {
    pub id: i64,
    pub ty: String,
    pub file: String,
    pub line: i64,
    pub function: String,
    pub condition: String,
    pub verified: bool,
}
