//! Per-tool state-guard rejections (Spec FR-4.3) — mirrors the Go `*_test.go` guard tests
//! (`TestHandle*StateGuardRejects*`). Each disallowed state must yield an error outcome.

use mcp_session::State;
use serde_json::{json, Value};

use crate::tests::handlers::support::{args, expect_error, token, Harness};

const ALL: [State; 5] = [
    State::Idle,
    State::Configuring,
    State::Stopped,
    State::Running,
    State::Terminated,
];

/// States in `ALL` not in `allowed`.
fn disallowed(allowed: &[State]) -> Vec<State> {
    ALL.iter()
        .copied()
        .filter(|s| !allowed.contains(s))
        .collect()
}

#[tokio::test]
async fn launch_rejects_non_idle() {
    for state in disallowed(&[State::Idle]) {
        let h = Harness::new();
        h.set_state(state);
        let a = args(&[("program", json!("/bin/x"))]);
        let out = h
            .server
            .handle_launch(&crate::Args::new(&a), &token())
            .await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn attach_rejects_non_idle() {
    for state in disallowed(&[State::Idle]) {
        let h = Harness::new();
        h.set_state(state);
        let a = args(&[("pid", json!(123))]);
        let out = h
            .server
            .handle_attach(&crate::Args::new(&a), &token())
            .await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn disconnect_rejects_idle_only() {
    // idle → error; every other state past the guard returns disconnected.
    let h = Harness::new();
    h.set_state(State::Idle);
    let a = Value::Object(args(&[]));
    let out = h
        .server
        .handle_disconnect(&crate::Args::new(a.as_object().unwrap()))
        .await;
    let msg = expect_error(&out);
    assert!(
        msg.contains("no debug session active"),
        "idle guard message, got {msg}"
    );

    for state in [
        State::Configuring,
        State::Stopped,
        State::Running,
        State::Terminated,
    ] {
        let h = Harness::new();
        h.set_state(state);
        let empty = args(&[]);
        let out = h.server.handle_disconnect(&crate::Args::new(&empty)).await;
        assert!(!out.is_error(), "disconnect should succeed in {state:?}");
    }
}

#[tokio::test]
async fn set_breakpoint_rejects_running_terminated_configuring() {
    for state in disallowed(&[State::Idle, State::Stopped]) {
        let h = Harness::new();
        h.set_state(state);
        let a = args(&[("file", json!("/f.c")), ("line", json!(10))]);
        let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn set_function_breakpoint_rejects_running_terminated_configuring() {
    for state in disallowed(&[State::Idle, State::Stopped]) {
        let h = Harness::new();
        h.set_state(state);
        let a = args(&[("name", json!("main"))]);
        let out = h
            .server
            .handle_set_function_breakpoint(&crate::Args::new(&a))
            .await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn remove_breakpoint_rejects_non_stopped() {
    for state in disallowed(&[State::Stopped]) {
        let h = Harness::new();
        h.set_state(state);
        let a = args(&[("breakpoint_id", json!(1))]);
        let out = h
            .server
            .handle_remove_breakpoint(&crate::Args::new(&a))
            .await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn continue_rejects_non_stopped() {
    for state in disallowed(&[State::Stopped]) {
        let h = Harness::new();
        h.set_state(state);
        let empty = args(&[]);
        let out = h
            .server
            .handle_continue(&crate::Args::new(&empty), &token())
            .await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn steps_reject_non_stopped() {
    for state in disallowed(&[State::Stopped]) {
        let empty = args(&[]);

        let h = Harness::new();
        h.set_state(state);
        expect_error(
            &h.server
                .handle_step_over(&crate::Args::new(&empty), &token())
                .await,
        );

        let h = Harness::new();
        h.set_state(state);
        expect_error(
            &h.server
                .handle_step_into(&crate::Args::new(&empty), &token())
                .await,
        );

        let h = Harness::new();
        h.set_state(state);
        expect_error(
            &h.server
                .handle_step_out(&crate::Args::new(&empty), &token())
                .await,
        );
    }
}

#[tokio::test]
async fn pause_rejects_non_running() {
    for state in disallowed(&[State::Running]) {
        let h = Harness::new();
        h.set_state(state);
        let empty = args(&[]);
        let out = h.server.handle_pause(&crate::Args::new(&empty)).await;
        expect_error(&out);
    }
}

#[tokio::test]
async fn inspection_tools_reject_non_stopped() {
    for state in disallowed(&[State::Stopped]) {
        let empty = args(&[]);

        let h = Harness::new();
        h.set_state(state);
        expect_error(&h.server.handle_backtrace(&crate::Args::new(&empty)).await);

        let h = Harness::new();
        h.set_state(state);
        expect_error(&h.server.handle_threads(&crate::Args::new(&empty)).await);

        let h = Harness::new();
        h.set_state(state);
        expect_error(&h.server.handle_variables(&crate::Args::new(&empty)).await);

        let h = Harness::new();
        h.set_state(state);
        let eval = args(&[("expression", json!("x"))]);
        expect_error(&h.server.handle_evaluate(&crate::Args::new(&eval)).await);
    }
}

#[tokio::test]
async fn memory_and_run_command_reject_non_stopped() {
    for state in disallowed(&[State::Stopped]) {
        let h = Harness::new();
        h.set_state(state);
        let mem = args(&[("address", json!("0x1000")), ("count", json!(4))]);
        expect_error(&h.server.handle_read_memory(&crate::Args::new(&mem)).await);

        let h = Harness::new();
        h.set_state(state);
        let empty = args(&[]);
        expect_error(&h.server.handle_disassemble(&crate::Args::new(&empty)).await);

        let h = Harness::new();
        h.set_state(state);
        let cmd = args(&[("command", json!("bt"))]);
        expect_error(&h.server.handle_run_command(&crate::Args::new(&cmd)).await);
    }
}

#[tokio::test]
async fn read_output_rejects_idle_only() {
    let h = Harness::new();
    h.set_state(State::Idle);
    expect_error(&h.server.handle_read_output());

    for state in [
        State::Configuring,
        State::Stopped,
        State::Running,
        State::Terminated,
    ] {
        let h = Harness::new();
        h.set_state(state);
        assert!(!h.server.handle_read_output().is_error());
    }
}

#[tokio::test]
async fn status_and_list_breakpoints_have_no_guard() {
    for state in ALL {
        let h = Harness::new();
        h.set_state(state);
        assert!(!h.server.handle_status().is_error());
        assert!(!h.server.handle_list_breakpoints().is_error());
    }
}
