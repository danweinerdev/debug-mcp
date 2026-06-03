//! The session state machine (Spec FR-4.1). Five states with exact lowercase string
//! renderings; an out-of-range numeric repr renders as `unknown(<n>)`.
//!
//! Go origin: `internal/session/session.go` `State` + its `String()` method.

use std::fmt;

/// The current state of a debug session. Transitions are unconditional — guarding is a
/// read-only check (`SessionManager::check_state`), never transition validation
/// (Spec FR-4.2/FR-4.4, Go parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Configuring,
    Stopped,
    Running,
    Terminated,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            State::Idle => "idle",
            State::Configuring => "configuring",
            State::Stopped => "stopped",
            State::Running => "running",
            State::Terminated => "terminated",
        };
        f.write_str(s)
    }
}
