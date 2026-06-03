//! Mapping [`BackendError`] back into the exact Go tool-error strings.
//!
//! The coarse [`DebuggerBackend`](debugger_core::DebuggerBackend) trait returns a single
//! neutral [`BackendError`]; the per-op error wording (`continue request failed: <e>`,
//! `unexpected stackTrace response type: <type>`, `setBreakpoints failed: <message>`)
//! lives here, at the handler call site, exactly as Go renders it in
//! `internal/tools/*.go`.
//!
//! Most ops go through the backend's `send_checked(op, …)`, which builds:
//! - `Send(inner)` — never (send failures are folded into `Dap` for `send_checked` ops);
//! - `Dap{message}` carrying the **already-Go-shaped** `"<op> request failed: <e>"`
//!   (send failure) or `"<op> failed: <msg>"` (`success=false`);
//! - `Protocol{ty: "<op>:<label>"}` for a wrong-typed/non-response message.
//!
//! The exceptions are `cont`/`step` (which surface a raw `Send(inner)` on a write
//! failure, mapped to `<op> request failed: <inner>`) and `run_command`, whose Go verbs
//! diverge from the backend's `evaluate` verb. [`OpError`] captures the three Go
//! templates per call site so the rendering substitutes the right verb regardless of how
//! the backend phrased it.

use debugger_core::BackendError;

/// The three Go error templates for one tool call site (Go origin: the `<op> request
/// failed:` / `unexpected <op> response type:` / `<op> failed:` triad each handler uses).
/// `unexpected` is the full prefix up to and including the trailing space before the
/// type, since a few ops omit the verb (`set_function_breakpoint` → `unexpected response
/// type: `).
pub struct OpError {
    /// The send-failure prefix, e.g. `continue request failed: `.
    pub request_failed: &'static str,
    /// The wrong-response-type prefix, e.g. `unexpected stackTrace response type: `.
    pub unexpected: &'static str,
    /// The `success=false` prefix, e.g. `stackTrace failed: `.
    pub failed: &'static str,
}

impl OpError {
    /// Render a [`BackendError`] into this call site's Go string.
    pub fn render(&self, err: BackendError) -> String {
        match err {
            // A raw send/transport failure (cont/step write error, or a closed transport
            // surfaced before any response): the Go `<op> request failed: <inner>`.
            BackendError::Send(inner) => format!("{}{inner}", self.request_failed),
            BackendError::Closed => format!("{}{}", self.request_failed, "connection closed"),
            BackendError::Timeout => format!("{}{}", self.request_failed, "operation timed out"),
            BackendError::Detect(m) => format!("{}{m}", self.request_failed),
            BackendError::Spawn(m) => format!("{}{m}", self.request_failed),

            // The backend built `"<op> request failed: <e>"` or `"<op> failed: <msg>"`.
            // Re-emit under THIS call site's verb so run_command's diverging verbs
            // (`run_command request failed:` / `command failed:`) render correctly while
            // the common ops (where the verbs already match) round-trip unchanged.
            BackendError::Dap { message } => self.render_dap(&message),

            // A wrong-typed/non-response message. The backend stamped `"<op>:<label>"`;
            // strip the op prefix to recover the `<label>` standing in for Go's `%T`.
            BackendError::Protocol { ty } => {
                let label = ty.split_once(':').map(|(_, rest)| rest).unwrap_or(&ty);
                format!("{}{label}", self.unexpected)
            }
        }
    }

    /// Re-shape a backend `Dap{message}` under this call site's verbs. The backend phrases
    /// it as `"<backend_op> request failed: <e>"` or `"<backend_op> failed: <msg>"`; we
    /// detect which and re-emit with `request_failed`/`failed`, preserving the inner
    /// detail. A message that fits neither shape (cannot happen for `send_checked` ops) is
    /// passed through verbatim.
    fn render_dap(&self, message: &str) -> String {
        if let Some(inner) = strip_after(message, " request failed: ") {
            format!("{}{inner}", self.request_failed)
        } else if let Some(inner) = strip_after(message, " failed: ") {
            format!("{}{inner}", self.failed)
        } else {
            message.to_string()
        }
    }
}

/// Return the substring after the first occurrence of `sep`, or `None` if absent.
fn strip_after<'a>(s: &'a str, sep: &str) -> Option<&'a str> {
    s.find(sep).map(|i| &s[i + sep.len()..])
}

/// `continue`.
pub const CONTINUE: OpError = OpError {
    request_failed: "continue request failed: ",
    unexpected: "unexpected continue response type: ",
    failed: "continue failed: ",
};

/// `step_over` (DAP `next`).
pub const STEP_OVER: OpError = OpError {
    request_failed: "step over request failed: ",
    unexpected: "unexpected step over response type: ",
    failed: "step over failed: ",
};

/// `step_into` (DAP `stepIn`).
pub const STEP_INTO: OpError = OpError {
    request_failed: "step into request failed: ",
    unexpected: "unexpected step into response type: ",
    failed: "step into failed: ",
};

/// `step_out` (DAP `stepOut`).
pub const STEP_OUT: OpError = OpError {
    request_failed: "step out request failed: ",
    unexpected: "unexpected step out response type: ",
    failed: "step out failed: ",
};

/// `pause`.
pub const PAUSE: OpError = OpError {
    request_failed: "pause request failed: ",
    unexpected: "unexpected pause response type: ",
    failed: "pause failed: ",
};

/// `set_breakpoint` (stopped mode) and the source path of `remove_breakpoint`.
pub const SET_BREAKPOINTS: OpError = OpError {
    request_failed: "setBreakpoints request failed: ",
    unexpected: "unexpected setBreakpoints response type: ",
    failed: "setBreakpoints failed: ",
};

/// `set_function_breakpoint` (stopped) and the function path of `remove_breakpoint`. Go
/// uses the bare `unexpected response type:` here (no verb).
pub const SET_FUNCTION_BREAKPOINTS: OpError = OpError {
    request_failed: "setFunctionBreakpoints request failed: ",
    unexpected: "unexpected response type: ",
    failed: "setFunctionBreakpoints failed: ",
};

/// `backtrace` (and the `disassemble` current-PC stackTrace).
pub const STACK_TRACE: OpError = OpError {
    request_failed: "stackTrace request failed: ",
    unexpected: "unexpected stackTrace response type: ",
    failed: "stackTrace failed: ",
};

/// `threads`.
pub const THREADS: OpError = OpError {
    request_failed: "threads request failed: ",
    unexpected: "unexpected threads response type: ",
    failed: "threads failed: ",
};

/// `variables` — the `scopes` request.
pub const SCOPES: OpError = OpError {
    request_failed: "scopes request failed: ",
    unexpected: "unexpected scopes response type: ",
    failed: "scopes failed: ",
};

/// `evaluate`.
pub const EVALUATE: OpError = OpError {
    request_failed: "evaluate request failed: ",
    unexpected: "unexpected evaluate response type: ",
    failed: "evaluate failed: ",
};

/// `read_memory`.
pub const READ_MEMORY: OpError = OpError {
    request_failed: "readMemory request failed: ",
    unexpected: "unexpected readMemory response type: ",
    failed: "readMemory failed: ",
};

/// `disassemble`.
pub const DISASSEMBLE: OpError = OpError {
    request_failed: "disassemble request failed: ",
    unexpected: "unexpected disassemble response type: ",
    failed: "disassemble failed: ",
};

/// `run_command` — Go phrases the send failure as `run_command request failed:`, the
/// `success=false` as `command failed:`, and the wrong type as `unexpected evaluate
/// response type:` (the backend issues an `evaluate`).
pub const RUN_COMMAND: OpError = OpError {
    request_failed: "run_command request failed: ",
    unexpected: "unexpected evaluate response type: ",
    failed: "command failed: ",
};

/// The `variables` flatten path: the inner `flatten_variables` error (already a full Go
/// string from [`OpError::render`] with [`VARIABLES`]) is wrapped as `failed to fetch
/// variables: <err>` by the handler (Go `inspection.go`).
pub const VARIABLES: OpError = OpError {
    request_failed: "variables request failed: ",
    unexpected: "unexpected variables response type: ",
    // Go's FlattenVariables uses `variables request failed: <message>` for a
    // success=false response too (not `variables failed:`).
    failed: "variables request failed: ",
};
