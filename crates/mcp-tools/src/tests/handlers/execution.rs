//! Execution handler tests: stop/exit/terminate formatting, output merge, thread
//! resolution, granularity, send-error revert, the generation guard, and the
//! pause-during-continue concurrency (Spec FR-8, R1).

use std::sync::Arc;

use debugger_core::{BackendError, Granularity, StepKind, StopInfo, StopOutcome};
use mcp_session::State;
use serde_json::json;
use tokio::sync::oneshot;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{args, expect_error, expect_json, token, Harness};

fn stop(reason: &str, thread_id: i64) -> StopInfo {
    StopInfo {
        reason: reason.to_string(),
        thread_id,
        description: "at line 6".to_string(),
        hit_breakpoint_ids: vec![3],
    }
}

#[tokio::test]
async fn continue_stopped_formats_stop_and_caches_last() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Stopped(stop("breakpoint", 1))));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("stopped"));
    assert_eq!(v["reason"], json!("breakpoint"));
    assert_eq!(v["thread_id"], json!(1));
    assert_eq!(v["description"], json!("at line 6"));
    assert_eq!(v["hit_breakpoint_ids"], json!([3]));
    assert_eq!(h.session.state(), State::Stopped);
    assert_eq!(h.session.last_stopped().unwrap().reason, "breakpoint");
}

#[tokio::test]
async fn continue_exited_sets_terminated_with_exit_code() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Exited { code: Some(0) }));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("exited"));
    assert_eq!(v["exit_code"], json!(0));
    assert_eq!(h.session.state(), State::Terminated);
}

#[tokio::test]
async fn continue_exited_caches_exit_code_for_immediate_status() {
    // Review finding 1: an exited `continue` must cache the exit code on the session so an
    // immediate `status` (which reads only cached data) reports it — without waiting for the
    // async Terminated event.
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Exited { code: Some(42) }));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("exited"));
    assert_eq!(v["exit_code"], json!(42));
    assert_eq!(h.session.state(), State::Terminated);
    // The session cached the exit code, so a status now reports it.
    assert_eq!(h.session.exit_code(), Some(42));
    let status = expect_json(&h.server.handle_status()).clone();
    assert_eq!(status["state"], json!("terminated"));
    assert_eq!(status["exit_code"], json!(42));
}

#[tokio::test]
async fn continue_stopped_after_disconnect_does_not_repopulate_last_stopped() {
    // Review finding 2: a Stopped outcome produced after a concurrent disconnect (which bumps
    // the generation + resets) must NOT write last_stopped onto the fresh idle session.
    let h = Harness::connected(State::Stopped).await;
    let (tx, rx) = oneshot::channel::<StopOutcome>();
    h.state.lock().unwrap().cont_gate = Some(rx);

    let session = Arc::clone(&h.session);
    let server = Arc::new(h.server);
    let s2 = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        let a = args(&[]);
        let ct = token();
        s2.handle_continue(&crate::Args::new(&a), &ct).await
    });

    // Let the continue reach its gate (state Running, generation N), then reset (the
    // disconnect equivalent: bumps the generation, clears last_stopped, returns to Idle).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    session.reset();
    // Release the gated continue with a Stopped outcome carrying a stale thread id.
    let _ = tx.send(StopOutcome::Stopped(stop("breakpoint", 9)));
    let _ = handle.await.unwrap();

    // The stale-generation stop must not have repopulated the reset session's stop cache.
    assert_eq!(session.state(), State::Idle);
    assert!(session.last_stopped().is_none());
}

#[tokio::test]
async fn continue_terminated_sets_terminated_message() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Terminated));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("terminated"));
    assert_eq!(v["message"], json!("Debug session ended"));
    assert_eq!(h.session.state(), State::Terminated);
}

#[tokio::test]
async fn continue_merges_output_into_stop_response() {
    let h = Harness::connected(State::Stopped).await;
    h.session.output_buffer().append("stdout", "hello\n");
    h.session.output_buffer().append("stderr", "warn\n");
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Stopped(stop("breakpoint", 1))));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    let v = expect_json(&out);
    // Output merged (count + per-category buckets), and the buffer drained.
    assert_eq!(v["stdout"], json!("hello\n"));
    assert_eq!(v["stderr"], json!("warn\n"));
    assert_eq!(v["count"], json!(2));
    assert!(h.session.output_buffer().drain().is_empty());
}

#[tokio::test]
async fn continue_thread_resolution_explicit_arg_wins() {
    let h = Harness::connected(State::Stopped).await;
    // last-stopped says thread 9, but the explicit arg is 5.
    h.session.set_last_stopped(stop("x", 9));
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Stopped(stop("breakpoint", 5))));
    let a = args(&[("thread_id", json!(5))]);
    let _ = h
        .server
        .handle_continue(&crate::Args::new(&a), &token())
        .await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Cont { thread_id: 5 })));
}

#[tokio::test]
async fn continue_thread_resolution_last_stopped() {
    let h = Harness::connected(State::Stopped).await;
    h.session.set_last_stopped(stop("x", 7));
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Stopped(stop("breakpoint", 7))));
    let empty = args(&[]);
    let _ = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Cont { thread_id: 7 })));
}

#[tokio::test]
async fn continue_thread_resolution_default_one() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Ok(StopOutcome::Stopped(stop("breakpoint", 1))));
    let empty = args(&[]);
    let _ = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Cont { thread_id: 1 })));
}

#[tokio::test]
async fn continue_rejects_non_positive_explicit_thread_id() {
    // Rust numeric-validation policy: an explicit, numeric, non-positive thread_id is
    // rejected at the boundary and no Cont call is made.
    let h = Harness::connected(State::Stopped).await;
    for bad in [json!(0), json!(-1), json!(-2.5)] {
        let a = args(&[("thread_id", bad)]);
        let out = h
            .server
            .handle_continue(&crate::Args::new(&a), &token())
            .await;
        assert_eq!(expect_error(&out), "'thread_id' must be a positive integer");
    }
    assert!(!h.calls().iter().any(|c| matches!(c, Call::Cont { .. })));
    // State unchanged (the guard/validation ran before set_state(Running)).
    assert_eq!(h.session.state(), State::Stopped);
}

#[tokio::test]
async fn step_over_rejects_non_positive_explicit_thread_id() {
    let h = Harness::connected(State::Stopped).await;
    let a = args(&[("thread_id", json!(-1))]);
    let out = h
        .server
        .handle_step_over(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_error(&out), "'thread_id' must be a positive integer");
    assert!(!h.calls().iter().any(|c| matches!(c, Call::Step { .. })));
}

#[tokio::test]
async fn step_over_uses_over_with_granularity() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().step_result = Some(Ok(StopOutcome::Stopped(stop("step", 1))));
    let a = args(&[("granularity", json!("instruction"))]);
    let _ = h
        .server
        .handle_step_over(&crate::Args::new(&a), &token())
        .await;
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::Step {
            kind: StepKind::Over,
            gran: Some(Granularity::Instruction),
            ..
        }
    )));
}

#[tokio::test]
async fn step_into_uses_into_with_line_granularity() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().step_result = Some(Ok(StopOutcome::Stopped(stop("step", 1))));
    let a = args(&[("granularity", json!("line"))]);
    let _ = h
        .server
        .handle_step_into(&crate::Args::new(&a), &token())
        .await;
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::Step {
            kind: StepKind::Into,
            gran: Some(Granularity::Line),
            ..
        }
    )));
}

#[tokio::test]
async fn step_out_drops_granularity() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().step_result = Some(Ok(StopOutcome::Stopped(stop("step", 1))));
    // step_out has no granularity param; the handler never parses one.
    let empty = args(&[]);
    let _ = h
        .server
        .handle_step_out(&crate::Args::new(&empty), &token())
        .await;
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::Step {
            kind: StepKind::Out,
            gran: None,
            ..
        }
    )));
}

#[tokio::test]
async fn continue_send_error_reverts_to_stopped() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().cont_result = Some(Err(BackendError::Send("broken pipe".to_string())));
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &token())
        .await;
    assert_eq!(expect_error(&out), "continue request failed: broken pipe");
    // State reverted to stopped (not left running).
    assert_eq!(h.session.state(), State::Stopped);
}

#[tokio::test]
async fn continue_cancellation_returns_timeout_and_leaves_running() {
    // A backend whose cont awaits a gate we never fire; the cancelled token wins.
    let h = Harness::connected(State::Stopped).await;
    let (_tx, rx) = oneshot::channel::<StopOutcome>();
    h.state.lock().unwrap().cont_gate = Some(rx);
    let ct = token();
    ct.cancel();
    let empty = args(&[]);
    let out = h
        .server
        .handle_continue(&crate::Args::new(&empty), &ct)
        .await;
    assert_eq!(
        expect_error(&out),
        "continue timed out; process still running, use 'pause' to stop it"
    );
    // State left running (Go parity — recover with pause).
    assert_eq!(h.session.state(), State::Running);
}

#[tokio::test]
async fn continue_after_disconnect_does_not_clobber_idle() {
    // Generation guard: a continue that returns Stopped AFTER a concurrent reset (which
    // bumps the generation) must not write Stopped over the reset Idle state.
    let h = Harness::connected(State::Stopped).await;
    let (tx, rx) = oneshot::channel::<StopOutcome>();
    h.state.lock().unwrap().cont_gate = Some(rx);

    let session = Arc::clone(&h.session);
    let server = Arc::new(h.server);
    let s2 = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        let a = args(&[]);
        let ct = token();
        s2.handle_continue(&crate::Args::new(&a), &ct).await
    });

    // Let the continue reach its gate (state is now Running, generation N).
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    // Concurrent disconnect-equivalent: reset bumps the generation, sets Idle.
    session.reset();
    // Now release the gated continue with a Stopped outcome.
    let _ = tx.send(StopOutcome::Stopped(stop("breakpoint", 1)));
    let _ = handle.await.unwrap();

    // The stale-generation continue must NOT have written Stopped — state stays Idle.
    assert_eq!(session.state(), State::Idle);
}

#[tokio::test]
async fn pause_returns_pause_requested_without_state_change() {
    let h = Harness::connected(State::Running).await;
    let empty = args(&[]);
    let out = h.server.handle_pause(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("pause_requested"));
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("Pause request sent"));
    // State unchanged.
    assert_eq!(h.session.state(), State::Running);
    assert!(h.calls().iter().any(|c| matches!(c, Call::Pause)));
}

#[tokio::test]
async fn pause_interrupts_a_blocked_continue() {
    // R1 concurrency: a continue is blocked on its gate (state Running); a concurrent pause
    // runs and fires the gate, unblocking the continue with a Stopped outcome.
    let h = Harness::connected(State::Stopped).await;
    let (tx, rx) = oneshot::channel::<StopOutcome>();
    h.state.lock().unwrap().cont_gate = Some(rx);

    let session = Arc::clone(&h.session);
    let server = Arc::new(h.server);
    let s2 = Arc::clone(&server);

    // Start the continue; it sets Running and blocks on the gate.
    let cont = tokio::spawn(async move {
        let a = args(&[]);
        let ct = token();
        s2.handle_continue(&crate::Args::new(&a), &ct).await
    });

    // Wait for the continue to reach Running.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(session.state(), State::Running);

    // A concurrent pause runs while the continue is blocked (proves rmcp-style concurrent
    // dispatch is safe — no session lock held across the await).
    let pause_args = args(&[]);
    let pause_out = server.handle_pause(&crate::Args::new(&pause_args)).await;
    assert!(
        !pause_out.is_error(),
        "pause must run during a blocked continue"
    );

    // The pause's effect (in real lldb-dap a StoppedEvent) is modeled by firing the gate.
    let _ = tx.send(StopOutcome::Stopped(stop("breakpoint", 1)));
    let out = cont.await.unwrap();
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("stopped"));
    assert_eq!(session.state(), State::Stopped);
}
