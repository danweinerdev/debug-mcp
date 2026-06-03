//! The neutral, runtime-agnostic event stream carried on a [`Connection`].
//!
//! Only two events cross the seam asynchronously, because the coarse blocking trait
//! (design Decision 2) handles stopped/initialized/exited **synchronously** as the
//! return value of `launch`/`attach`/`cont`/`step`. What remains async is (a) output
//! arriving at unpredictable times during a run and (b) crash/EOF death that must
//! flip session state to `terminated` even with no execution call in flight.
//!
//! This concrete two-variant shape supersedes the *indicative* (non-normative)
//! `BackendEvent` sketch in Spec FR-18.7. Go origin: `SetOutputHandler` (output) and
//! `onExit`/`onTerminated` (async death), expressed as one neutral stream
//! (design Decision 5).
//!
//! [`Connection`]: crate::Connection

use serde::{Deserialize, Serialize};

/// An asynchronous backend event drained by the session's event-pump task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendEvent {
    /// Program output. `category` is opaque pass-through (`"stdout"`, `"stderr"`,
    /// `"console"`, …) and `text` is appended to the session's `OutputBuffer`.
    Output { category: String, text: String },

    /// Async death (crash / EOF) — drives session state → `terminated` (Spec FR-17.7).
    Terminated { code: Option<i64> },
}
