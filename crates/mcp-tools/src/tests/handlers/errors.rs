//! `OpError` rendering: each `BackendError` variant → the exact Go tool-error string,
//! including the diverging verbs of `run_command`, the bare `unexpected response type:`
//! of `set_function_breakpoint`, and the `variables request failed:` success=false form.

use debugger_core::BackendError;

use crate::errors;

#[test]
fn continue_send_error() {
    let s = errors::CONTINUE.render(BackendError::Send("broken pipe".to_string()));
    assert_eq!(s, "continue request failed: broken pipe");
}

#[test]
fn stack_trace_dap_failed_and_protocol() {
    // The backend phrases a success=false as `stackTrace failed: <msg>` in Dap.
    let s = errors::STACK_TRACE.render(BackendError::Dap {
        message: "stackTrace failed: bad thread".to_string(),
    });
    assert_eq!(s, "stackTrace failed: bad thread");

    // Wrong type → `unexpected stackTrace response type: <label>` (op prefix stripped).
    let s = errors::STACK_TRACE.render(BackendError::Protocol {
        ty: "stackTrace:response:threads".to_string(),
    });
    assert_eq!(s, "unexpected stackTrace response type: response:threads");
}

#[test]
fn set_breakpoints_send_failure_in_dap_round_trips() {
    // send failure folded into Dap as `setBreakpoints request failed: <e>`.
    let s = errors::SET_BREAKPOINTS.render(BackendError::Dap {
        message: "setBreakpoints request failed: closed".to_string(),
    });
    assert_eq!(s, "setBreakpoints request failed: closed");
}

#[test]
fn set_function_breakpoints_uses_bare_unexpected_verb() {
    // Go uses `unexpected response type:` (no verb) for set_function_breakpoint.
    let s = errors::SET_FUNCTION_BREAKPOINTS.render(BackendError::Protocol {
        ty: "setFunctionBreakpoints:response:foo".to_string(),
    });
    assert_eq!(s, "unexpected response type: response:foo");
}

#[test]
fn run_command_diverging_verbs() {
    // request-failed → `run_command request failed: <e>`.
    let s = errors::RUN_COMMAND.render(BackendError::Send("write error".to_string()));
    assert_eq!(s, "run_command request failed: write error");

    // The backend phrases a success=false evaluate as `evaluate failed: <msg>`; Go's
    // run_command wants `command failed: <msg>`.
    let s = errors::RUN_COMMAND.render(BackendError::Dap {
        message: "evaluate failed: no symbol".to_string(),
    });
    assert_eq!(s, "command failed: no symbol");

    // Wrong type → `unexpected evaluate response type: <label>`.
    let s = errors::RUN_COMMAND.render(BackendError::Protocol {
        ty: "evaluate:response:launch".to_string(),
    });
    assert_eq!(s, "unexpected evaluate response type: response:launch");
}

#[test]
fn variables_success_false_uses_request_failed_form() {
    // Go's FlattenVariables uses `variables request failed: <msg>` for a success=false
    // response; the backend phrases it as `variables failed: <msg>` in Dap.
    let s = errors::VARIABLES.render(BackendError::Dap {
        message: "variables failed: bad ref".to_string(),
    });
    assert_eq!(s, "variables request failed: bad ref");
}

#[test]
fn read_memory_and_disassemble_verbs() {
    let s = errors::READ_MEMORY.render(BackendError::Dap {
        message: "readMemory failed: oob".to_string(),
    });
    assert_eq!(s, "readMemory failed: oob");

    let s = errors::DISASSEMBLE.render(BackendError::Send("eof".to_string()));
    assert_eq!(s, "disassemble request failed: eof");
}
