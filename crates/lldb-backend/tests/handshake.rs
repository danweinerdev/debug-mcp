//! Launch/attach handshake tests over a scripted DAP peer — mirror the Go
//! `internal/tools/{launch,attach}.go` handshake order and the design's load-bearing
//! details: order-independent `InitializedEvent`, launch-flushes-but-attach-doesn't,
//! exception-bp-before-configurationDone, the **stop-waiter placement asymmetry as a
//! timing test**, the outcomes (stopped/exited/terminated), and arg `omitempty`.

mod common;

use common::Harness;
use debugger_core::{
    AttachOutcome, AttachSpec, DebuggerBackend, LaunchOutcome, LaunchSpec, SourceBp,
};
use serde_json::Value;

/// Inject an `initialized` event into the peer (only the handshake tests need it).
async fn inject_initialized(h: &mut Harness) {
    h.inject_value(&serde_json::json!({
        "seq": 0, "type": "event", "event": "initialized"
    }))
    .await;
}

/// A bare launch spec (no breakpoints), with the given stop-on-entry.
fn launch_spec(stop_on_entry: bool) -> LaunchSpec {
    LaunchSpec {
        program: "/bin/prog".to_string(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        stop_on_entry,
        source_breakpoints: Vec::new(),
        function_breakpoints: Vec::new(),
    }
}

/// Drive the initialize + launch(+initialized) prelude on the peer, returning after the
/// launch response is sent. `inject_initialized_first` controls the order the peer sends
/// the launch response vs the `initialized` event (both orderings must work).
async fn serve_prelude(h: &mut Harness, inject_initialized_first: bool) {
    let (cmd, seq, _args) = h.next_request_full().await;
    assert_eq!(cmd, "initialize");
    h.reply_ok("initialize", seq, None).await;

    let (cmd, seq, _args) = h.next_request_full().await;
    assert_eq!(cmd, "launch");
    if inject_initialized_first {
        inject_initialized(h).await;
        h.reply_ok("launch", seq, None).await;
    } else {
        h.reply_ok("launch", seq, None).await;
        inject_initialized(h).await;
    }
}

/// Serve setExceptionBreakpoints then configurationDone (the common tail).
async fn serve_exc_and_config(h: &mut Harness) {
    let (cmd, seq, args) = h.next_request_full().await;
    assert_eq!(cmd, "setExceptionBreakpoints");
    assert_eq!(args["filters"], Value::Array(vec![]));
    h.reply_ok("setExceptionBreakpoints", seq, None).await;
    let (cmd, seq, _args) = h.next_request_full().await;
    assert_eq!(cmd, "configurationDone");
    h.reply_ok("configurationDone", seq, None).await;
}

#[tokio::test]
async fn launch_initialized_then_response() {
    // `InitializedEvent` arrives BEFORE the launch response — must still complete.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(false);
    let run = async {
        serve_prelude(&mut h, true).await;
        serve_exc_and_config(&mut h).await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert_eq!(result.expect("launch ok"), LaunchOutcome::Running);
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_response_then_initialized() {
    // `InitializedEvent` arrives AFTER the launch response — must still complete.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(false);
    let run = async {
        serve_prelude(&mut h, false).await;
        serve_exc_and_config(&mut h).await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert_eq!(result.expect("launch ok"), LaunchOutcome::Running);
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_stop_on_entry_true_returns_stopped() {
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(true);
    let run = async {
        serve_prelude(&mut h, true).await;
        serve_exc_and_config(&mut h).await;
        h.inject_stopped("entry", 7).await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    match result.expect("ok") {
        LaunchOutcome::Stopped(info) => {
            assert_eq!(info.reason, "entry");
            assert_eq!(info.thread_id, 7);
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_deferred_response_after_config_done() {
    // Real lldb-dap defers the launch RESPONSE until after configurationDone, emitting
    // `initialized` (and the stop) earlier. The handshake awaits `initialized` to gate
    // configuration, sends configurationDone, and only then collects the deferred launch
    // response — this test pins that ordering.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(true);
    let run = async {
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "initialize");
        h.reply_ok("initialize", seq, None).await;

        let (cmd, launch_seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "launch");
        // initialized arrives now; the launch response is withheld until after configDone.
        inject_initialized(&mut h).await;

        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "setExceptionBreakpoints");
        h.reply_ok("setExceptionBreakpoints", seq, None).await;

        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "configurationDone");
        // The stop fires (waiter registered before configDone), then configDone response,
        // then finally the deferred launch response — the real lldb-dap order.
        h.inject_stopped("entry", 1).await;
        h.reply_ok("configurationDone", seq, None).await;
        h.reply_ok("launch", launch_seq, None).await;
        h
    };
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(backend.launch(spec), run)
    })
    .await
    .expect("deferred-response launch must not hang");
    let (outcome, h) = joined;
    assert!(matches!(outcome, Ok(LaunchOutcome::Stopped(_))));
    h.close_and_join().await;
}

/// THE TIMING TEST (3.3 verification). The peer injects the StoppedEvent in the window
/// *right after* configurationDone — specifically, it injects the StoppedEvent BEFORE the
/// configurationDone response. Because launch registers the stop waiter **before** sending
/// configurationDone, the waiter is already in place when the StoppedEvent arrives, so it
/// is captured. With the inverted (attach-style) placement — register AFTER the
/// configurationDone response returns — this StoppedEvent would arrive with no waiter
/// registered and be dropped, so `launch` would never observe the stop (it would hang
/// on the never-resolving waiter). This test therefore pins register-before-configDone.
#[tokio::test]
async fn launch_timing_stop_before_config_done_response_is_captured() {
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(true);
    let run = async {
        serve_prelude(&mut h, true).await;
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "setExceptionBreakpoints");
        h.reply_ok("setExceptionBreakpoints", seq, None).await;
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "configurationDone");
        // The stop fires in the narrow window: BEFORE the configurationDone response.
        // Only register-before-configDone can catch this.
        h.inject_stopped("entry", 1).await;
        h.reply_ok("configurationDone", seq, None).await;
        h
    };
    // Bound the whole join so an inverted placement (which would hang) fails fast.
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(backend.launch(spec), run)
    })
    .await
    .expect("launch must not hang — proves register-before-configurationDone");
    let (outcome, h) = joined;
    match outcome.expect("ok") {
        LaunchOutcome::Stopped(info) => assert_eq!(info.reason, "entry"),
        other => panic!("timing test expected Stopped, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_exit_during_returns_exited() {
    // stop_on_entry=true, but the process exits during launch (ExitedEvent → the stop
    // waiter resolves Exited). Maps to LaunchOutcome::Exited (Go's "Program exited during
    // launch" early-exit).
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(true);
    let run = async {
        serve_prelude(&mut h, true).await;
        serve_exc_and_config(&mut h).await;
        h.inject_value(&serde_json::json!({
            "seq": 0, "type": "event", "event": "exited", "body": {"exitCode": 3}
        }))
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert_eq!(result.expect("ok"), LaunchOutcome::Exited { code: Some(3) });
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_flushes_pending_breakpoints() {
    // Launch flushes source then function breakpoints (attach does not). Verify the
    // setBreakpoints request shape (source.path + breakpoints with conditions omitted
    // when empty) and that it happens before setExceptionBreakpoints/configurationDone.
    let (backend, mut h) = Harness::new(true);
    let mut spec = launch_spec(false);
    spec.source_breakpoints = vec![(
        "/src/loop.c".to_string(),
        vec![
            SourceBp {
                line: 6,
                condition: String::new(),
            },
            SourceBp {
                line: 9,
                condition: "i > 3".to_string(),
            },
        ],
    )];

    let run = async {
        serve_prelude(&mut h, true).await;

        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "setBreakpoints");
        assert_eq!(args["source"]["path"], Value::String("/src/loop.c".into()));
        let bps = args["breakpoints"].as_array().expect("breakpoints array");
        assert_eq!(bps.len(), 2);
        assert_eq!(bps[0]["line"], Value::from(6));
        assert!(bps[0].get("condition").is_none(), "empty condition omitted");
        assert_eq!(bps[1]["line"], Value::from(9));
        assert_eq!(bps[1]["condition"], Value::String("i > 3".into()));
        h.reply_ok(
            "setBreakpoints",
            seq,
            Some(serde_json::json!({"breakpoints": [
                {"id": 1, "line": 6, "verified": true},
                {"id": 2, "line": 9, "verified": true}
            ]})),
        )
        .await;

        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "setExceptionBreakpoints");
        h.reply_ok("setExceptionBreakpoints", seq, None).await;
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "configurationDone");
        h.reply_ok("configurationDone", seq, None).await;
        h
    };

    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert_eq!(result.expect("ok"), LaunchOutcome::Running);
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_args_omit_stop_on_entry_when_false() {
    // The launch arguments omit `stopOnEntry` when false (Go omitempty — behavioral, it
    // reaches lldb-dap). `program` is always present.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(false);
    let run = async {
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "initialize");
        h.reply_ok("initialize", seq, None).await;
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "launch");
        assert_eq!(args["program"], Value::String("/bin/prog".into()));
        assert!(
            args.get("stopOnEntry").is_none(),
            "stopOnEntry omitted when false, got {args}"
        );
        assert!(args.get("args").is_none(), "empty args omitted");
        assert!(args.get("cwd").is_none(), "empty cwd omitted");
        assert!(args.get("env").is_none(), "empty env omitted");
        inject_initialized(&mut h).await;
        h.reply_ok("launch", seq, None).await;
        serve_exc_and_config(&mut h).await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert!(result.is_ok());
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_args_include_stop_on_entry_when_true() {
    // When true, stopOnEntry IS present (value `true`), and args/cwd/env serialize.
    let (backend, mut h) = Harness::new(true);
    let spec = LaunchSpec {
        program: "/bin/p".to_string(),
        args: vec!["--flag".to_string(), "v".to_string()],
        cwd: Some("/work".to_string()),
        env: vec![("KEY".to_string(), "val".to_string())],
        stop_on_entry: true,
        source_breakpoints: Vec::new(),
        function_breakpoints: Vec::new(),
    };
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await;
        h.reply_ok("initialize", seq, None).await;
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "launch");
        assert_eq!(args["stopOnEntry"], Value::Bool(true));
        assert_eq!(args["args"], serde_json::json!(["--flag", "v"]));
        assert_eq!(args["cwd"], Value::String("/work".into()));
        assert_eq!(args["env"], serde_json::json!({"KEY": "val"}));
        inject_initialized(&mut h).await;
        h.reply_ok("launch", seq, None).await;
        serve_exc_and_config(&mut h).await;
        h.inject_stopped("entry", 1).await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    assert!(matches!(result, Ok(LaunchOutcome::Stopped(_))));
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_initialize_failure_maps_to_go_string() {
    // A `success=false` initialize response → `initialize failed: <message>`.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(false);
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await;
        h.reply_err("initialize", seq, "boom").await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    let err = result.expect_err("should fail");
    assert!(
        format!("{err:?}").contains("initialize failed: boom"),
        "got: {err:?}"
    );
    h.close_and_join().await;
}

#[tokio::test]
async fn launch_failed_response_maps_to_go_string() {
    // A `success=false` launch response → `launch failed: <message>`. Real lldb-dap
    // defers the launch response (success OR failure) to after configurationDone and
    // still processes setExceptionBreakpoints/configurationDone for a doomed launch, so
    // the peer scripts the full sequence and delivers the launch error last.
    let (backend, mut h) = Harness::new(true);
    let spec = launch_spec(false);
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await; // initialize
        h.reply_ok("initialize", seq, None).await;
        let (cmd, launch_seq, _a) = h.next_request_full().await; // launch (deferred reply)
        assert_eq!(cmd, "launch");
        inject_initialized(&mut h).await;
        let (_c, seq, _a) = h.next_request_full().await; // setExceptionBreakpoints
        h.reply_ok("setExceptionBreakpoints", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // configurationDone
        h.reply_ok("configurationDone", seq, None).await;
        // The deferred launch error arrives last.
        h.reply_err("launch", launch_seq, "no such file").await;
        h
    };
    let (result, h) = tokio::join!(backend.launch(spec), run);
    let err = result.expect_err("should fail");
    assert!(
        format!("{err:?}").contains("launch failed: no such file"),
        "got: {err:?}"
    );
    h.close_and_join().await;
}

// ---- attach ----

#[tokio::test]
async fn attach_registers_waiter_after_config_done_and_no_flush() {
    // Attach: initialize → attach(+initialized) → setExceptionBreakpoints →
    // configurationDone → (waiter registered AFTER) → stopped. No breakpoint flush.
    let (backend, mut h) = Harness::new(true);
    let spec = AttachSpec {
        pid: Some(4321),
        wait_for: None,
    };
    let run = async {
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "initialize");
        h.reply_ok("initialize", seq, None).await;

        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "attach");
        assert_eq!(args["pid"], Value::from(4321));
        assert_eq!(args["stopOnEntry"], Value::Bool(true));
        assert!(args.get("waitFor").is_none(), "pid path omits waitFor");
        inject_initialized(&mut h).await;
        h.reply_ok("attach", seq, None).await;

        // Directly to setExceptionBreakpoints (NO setBreakpoints flush).
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(
            cmd, "setExceptionBreakpoints",
            "attach does not flush breakpoints"
        );
        h.reply_ok("setExceptionBreakpoints", seq, None).await;

        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "configurationDone");
        h.reply_ok("configurationDone", seq, None).await;

        // The post-configDone stop. Attach registers the waiter only AFTER the
        // configurationDone response resolves, so yield first to let registration happen
        // (real stops arrive well after configurationDone) — the attach side of the
        // asymmetry. A stop delivered before registration is correctly dropped (the
        // launch path avoids this by registering before configurationDone).
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.inject_stopped("signal", 1).await;
        h
    };
    let (result, h) = tokio::join!(backend.attach(spec), run);
    match result.expect("ok") {
        AttachOutcome::Stopped(info) => assert_eq!(info.reason, "signal"),
        other => panic!("expected Stopped, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn attach_wait_for_sets_program_and_wait_for() {
    let (backend, mut h) = Harness::new(true);
    let spec = AttachSpec {
        pid: None,
        wait_for: Some("myproc".to_string()),
    };
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await; // initialize
        h.reply_ok("initialize", seq, None).await;
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "attach");
        assert_eq!(args["waitFor"], Value::Bool(true));
        assert_eq!(args["program"], Value::String("myproc".into()));
        assert!(args.get("pid").is_none(), "wait_for path omits pid");
        inject_initialized(&mut h).await;
        h.reply_ok("attach", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // setExceptionBreakpoints
        h.reply_ok("setExceptionBreakpoints", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // configurationDone
        h.reply_ok("configurationDone", seq, None).await;
        // Attach registers the stop waiter AFTER the configurationDone response resolves;
        // yield so that registration happens before this post-configDone stop is
        // delivered (real lldb-dap stops arrive well after configurationDone). This is
        // the attach-side of the asymmetry the launch path avoids by registering first.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.inject_stopped("entry", 1).await;
        h
    };
    let (result, h) = tokio::join!(backend.attach(spec), run);
    assert!(matches!(result, Ok(AttachOutcome::Stopped(_))));
    h.close_and_join().await;
}

#[tokio::test]
async fn attach_exit_during_returns_terminated() {
    // Process terminates during attach → TerminatedEvent resolves the waiter as
    // Terminated (Go's "Process exited during attach").
    let (backend, mut h) = Harness::new(true);
    let spec = AttachSpec {
        pid: Some(9),
        wait_for: None,
    };
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await; // initialize
        h.reply_ok("initialize", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // attach
        inject_initialized(&mut h).await;
        h.reply_ok("attach", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // setExceptionBreakpoints
        h.reply_ok("setExceptionBreakpoints", seq, None).await;
        let (_c, seq, _a) = h.next_request_full().await; // configurationDone
        h.reply_ok("configurationDone", seq, None).await;
        // Yield so the post-configDone waiter is registered before this terminated event
        // (attach asymmetry — see attach_registers_waiter_after_config_done_and_no_flush).
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.inject_value(&serde_json::json!({
            "seq": 0, "type": "event", "event": "terminated"
        }))
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.attach(spec), run);
    assert_eq!(result.expect("ok"), AttachOutcome::Terminated);
    h.close_and_join().await;
}
