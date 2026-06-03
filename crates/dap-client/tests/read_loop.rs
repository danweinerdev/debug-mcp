//! Read-loop + event-dispatch tests — mirrors Go `internal/dap/client_test.go`'s
//! `TestReadLoop*` set, driven over the scripted `tokio::io::duplex` peer.
//!
//! Covers the per-event dispatch table (Stopped/Initialized/Output/Exited/Terminated +
//! informational log-only), interleaved responses-and-events, the dual-await
//! (`send_and_await_both`, order-independent), and EOF recovery (all pending fail + stop
//! waiter terminated + terminated signal fired).

mod common;

use common::Harness;
use dap_client::{
    DapMessage, ExitedBody, ExitedEvent, OutputBody, OutputEvent, Request, Response, StoppedBody,
    StoppedEvent,
};
use debugger_core::{BackendError, StopOutcome};
use serde_json::json;

fn stopped_event(reason: &str, thread_id: i64) -> StoppedEvent {
    StoppedEvent {
        seq: 1,
        ty: "event".to_string(),
        event: "stopped".to_string(),
        body: StoppedBody {
            reason: reason.to_string(),
            thread_id,
            ..Default::default()
        },
    }
}

fn output_event(category: &str, output: &str) -> OutputEvent {
    OutputEvent {
        seq: 1,
        ty: "event".to_string(),
        event: "output".to_string(),
        body: OutputBody {
            category: category.to_string(),
            output: output.to_string(),
        },
    }
}

fn initialize_response(request_seq: i64) -> Response {
    Response {
        seq: 10,
        ty: "response".to_string(),
        request_seq,
        success: true,
        command: "initialize".to_string(),
        message: String::new(),
        body: None,
    }
}

fn request_seq(msg: &DapMessage) -> i64 {
    match msg {
        DapMessage::Other(env) => env.seq,
        other => panic!("expected request envelope, got {other:?}"),
    }
}

#[tokio::test]
async fn stopped_event_delivers_to_stop_waiter() {
    // Go `TestReadLoopStoppedEvent`: a StoppedEvent reaches the registered stop waiter
    // (and, above the seam, the onStopped cache hook — here the StopInfo carries the
    // reason/thread).
    let mut h = Harness::new();
    let rx = h.client.stop_waiter().register();

    h.inject(&stopped_event("breakpoint", 1)).await;

    let outcome = rx.await.expect("stop outcome");
    match outcome {
        StopOutcome::Stopped(info) => {
            assert_eq!(info.reason, "breakpoint");
            assert_eq!(info.thread_id, 1);
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn exited_event_delivers_exit_code() {
    // Go `TestReadLoopExitedEvent`: an ExitedEvent delivers Exited{code} to the waiter.
    let mut h = Harness::new();
    let rx = h.client.stop_waiter().register();

    h.inject(&ExitedEvent {
        seq: 1,
        ty: "event".to_string(),
        event: "exited".to_string(),
        body: ExitedBody { exit_code: 42 },
    })
    .await;

    match rx.await.expect("outcome") {
        StopOutcome::Exited { code } => assert_eq!(code, Some(42)),
        other => panic!("expected Exited, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn terminated_event_cancels_stop_waiter_and_fires_signal() {
    // Go `TestReadLoopTerminatedEvent`: a TerminatedEvent cancels the stop waiter
    // (Terminated) and fires the terminated lifecycle signal (the onTerminated analog).
    let mut h = Harness::new();
    let rx = h.client.stop_waiter().register();

    let terminated = json!({"seq": 1, "type": "event", "event": "terminated"});
    h.inject(&terminated).await;

    assert!(matches!(
        rx.await.expect("outcome"),
        StopOutcome::Terminated
    ));

    // The terminated signal carries the last-known exit code (None here, no prior exit).
    let code = (&mut h.channels.terminated)
        .await
        .expect("terminated fired");
    assert_eq!(code, None);
    h.close_and_join().await;
}

#[tokio::test]
async fn exited_then_terminated_carries_exit_code_in_signal() {
    // An ExitedEvent records the code; a following TerminatedEvent fires the terminated
    // signal carrying that code (the lldb-dap exit→terminate sequence).
    let mut h = Harness::new();
    let stop_rx = h.client.stop_waiter().register();

    h.inject(&ExitedEvent {
        seq: 1,
        ty: "event".to_string(),
        event: "exited".to_string(),
        body: ExitedBody { exit_code: 7 },
    })
    .await;
    // The exit delivers to the (single-shot) stop waiter.
    assert!(matches!(
        stop_rx.await.expect("exit"),
        StopOutcome::Exited { code: Some(7) }
    ));

    h.inject(&json!({"seq": 2, "type": "event", "event": "terminated"}))
        .await;
    let code = (&mut h.channels.terminated).await.expect("terminated");
    assert_eq!(
        code,
        Some(7),
        "terminated signal carries the recorded exit code"
    );
    h.close_and_join().await;
}

#[tokio::test]
async fn initialized_event_signals() {
    // Go `TestReadLoopInitializedEvent`: an InitializedEvent unblocks the dual-await's
    // initialized latch. Use send_and_await_both: it needs both the response AND
    // initialized, in any order.
    let mut h = Harness::new();
    let client = h.client.clone();

    let dual = tokio::spawn(async move {
        client
            .send_and_await_both(Request::new("initialize", Some(json!({}))))
            .await
    });

    // Read the request, then inject initialized FIRST and the response SECOND.
    let req = h.next_request().await;
    let seq = request_seq(&req);
    h.inject(&json!({"seq": 1, "type": "event", "event": "initialized"}))
        .await;
    h.inject(&initialize_response(seq)).await;

    let result = dual.await.expect("join").expect("dual-await ok");
    assert!(matches!(result, DapMessage::Response(r) if r.success));
    h.close_and_join().await;
}

#[tokio::test]
async fn send_and_await_both_response_first() {
    // The dual-await is order-independent: inject the response FIRST, then initialized.
    let mut h = Harness::new();
    let client = h.client.clone();

    let dual = tokio::spawn(async move {
        client
            .send_and_await_both(Request::new(
                "launch",
                Some(json!({"program": "/bin/true"})),
            ))
            .await
    });

    let req = h.next_request().await;
    let seq = request_seq(&req);
    h.inject(&Response {
        seq: 11,
        ty: "response".to_string(),
        request_seq: seq,
        success: true,
        command: "launch".to_string(),
        message: String::new(),
        body: None,
    })
    .await;
    h.inject(&json!({"seq": 1, "type": "event", "event": "initialized"}))
        .await;

    let result = dual.await.expect("join").expect("dual-await ok");
    assert!(matches!(result, DapMessage::Response(r) if r.command == "launch"));
    h.close_and_join().await;
}

#[tokio::test]
async fn output_event_forwarded_to_sink() {
    // Go `TestReadLoopOutputEvent`: an OutputEvent reaches the output sink.
    let mut h = Harness::new();
    h.inject(&output_event("stderr", "error message\n")).await;

    let chunk = h.channels.output.recv().await.expect("output chunk");
    assert_eq!(chunk.category, "stderr");
    assert_eq!(chunk.text, "error message\n");
    h.close_and_join().await;
}

#[tokio::test]
async fn interleaved_responses_and_events() {
    // Go `TestReadLoopInterleavedResponsesAndEvents`: an InitializedEvent, an
    // OutputEvent, and a response interleave; each lands in the right place.
    let mut h = Harness::new();

    let rx = h
        .client
        .send_async(Request::new("initialize", Some(json!({}))))
        .await
        .expect("send_async");
    let req = h.next_request().await;
    let seq = request_seq(&req);

    h.inject(&json!({"seq": 1, "type": "event", "event": "initialized"}))
        .await;
    h.inject(&output_event("stdout", "hello world\n")).await;
    h.inject(&initialize_response(seq)).await;

    // Output reaches the sink.
    let chunk = h.channels.output.recv().await.expect("output");
    assert_eq!(chunk.text, "hello world\n");
    assert_eq!(chunk.category, "stdout");

    // The response reaches the pending waiter.
    let msg = rx.await.expect("receiver").expect("ok");
    assert!(matches!(msg, DapMessage::Response(r) if r.success));
    h.close_and_join().await;
}

#[tokio::test]
async fn informational_events_are_log_only() {
    // Thread/Breakpoint/Process/Continued/Module/Capabilities have no callback and no
    // stop-waiter effect (Go `client.go:218`). A registered waiter is NOT delivered to by
    // them. `module`/`capabilities` are modeled as informational so lldb-dap's per-library
    // module events do not flood stderr as "unhandled event".
    let mut h = Harness::new();
    let rx = h.client.stop_waiter().register();

    for name in [
        "thread",
        "breakpoint",
        "process",
        "continued",
        "module",
        "capabilities",
    ] {
        h.inject(&json!({"seq": 1, "type": "event", "event": name}))
            .await;
    }
    // A stopped event after them still delivers — proving the loop kept running and the
    // informational events did not consume the waiter.
    h.inject(&stopped_event("step", 2)).await;

    match rx.await.expect("outcome") {
        StopOutcome::Stopped(info) => assert_eq!(info.reason, "step"),
        other => panic!("expected Stopped after informational events, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn response_to_no_waiter_does_not_stop_the_loop() {
    // A response with no pending waiter is logged + discarded, never panics, and the
    // loop keeps running (Go `dispatchResponse` no-waiter path).
    let mut h = Harness::new();

    // Inject a response for a seq nobody is waiting on.
    h.inject(&initialize_response(9999)).await;
    // The loop survives: a subsequent stopped event still delivers.
    let rx = h.client.stop_waiter().register();
    h.inject(&stopped_event("breakpoint", 1)).await;
    assert!(matches!(
        rx.await.expect("outcome"),
        StopOutcome::Stopped(_)
    ));
    h.close_and_join().await;
}

#[tokio::test]
async fn eof_recovery_unblocks_pending_and_stop_waiter_and_signal() {
    // Go `TestReadLoopEOF`: on EOF, every pending request fails, the stop waiter is
    // cancelled (Terminated), the client is closed, and the terminated signal fires.
    let mut h = Harness::new();

    // A registered stop waiter.
    let stop_rx = h.client.stop_waiter().register();

    // Two pending requests.
    let mut receivers = Vec::new();
    for _ in 0..2 {
        let rx = h
            .client
            .send_async(Request::new("initialize", Some(json!({}))))
            .await
            .expect("send_async");
        receivers.push(rx);
        let _ = h.next_request().await;
    }

    // Trigger EOF by dropping the peer's response writer.
    let Harness {
        peer_writes,
        read_loop,
        mut channels,
        ..
    } = h;
    drop(peer_writes);
    read_loop.await.expect("read loop joins");

    // Every pending request unblocks with an error (Closed).
    for rx in receivers {
        let result = rx.await.expect("receiver");
        assert!(
            matches!(result, Err(BackendError::Closed)),
            "got {result:?}"
        );
    }

    // The stop waiter is cancelled → Terminated.
    assert!(matches!(
        stop_rx.await.expect("stop outcome"),
        StopOutcome::Terminated
    ));

    // The terminated signal fired (no prior exit code → None).
    let code = channels.terminated.await.expect("terminated fired on EOF");
    assert_eq!(code, None);

    // No further output.
    assert!(
        channels.output.recv().await.is_none(),
        "output channel closed"
    );
}
