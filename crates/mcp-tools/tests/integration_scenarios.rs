//! Phase 6.2: the ported Go integration scenarios, against real lldb-dap + the C
//! fixtures. Each test mirrors a function in `internal/tools/integration_test.go` and its
//! assertions exactly (stop reasons, locations, exit codes, the within-N-continues /
//! no-hang timing guards). Gated behind the `integration` feature; skips cleanly when
//! lldb-dap or the fixtures are absent.

#![cfg(feature = "integration")]

use std::time::Duration;

use integration_tests::harness::{
    expect_error, expect_json_obj, fixture_path, obj, should_skip, Harness,
};
use mcp_session::State;
use serde_json::{json, Map, Value};

/// Kill an OS process by pid (the lldb-dap subprocess), used by the crash-recovery
/// scenarios. Shells out to `kill -KILL` to stay zero-`unsafe` (Go uses
/// `Process.Kill()`). A non-zero pid is required.
fn kill_pid(pid: i64) {
    assert!(pid > 0, "refusing to kill non-positive pid {pid}");
    let status = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "kill -KILL {pid} failed");
}

// --- Process exit handling (Go TestProcessExitHandling) ---

#[tokio::test]
async fn process_exit_handling() {
    if should_skip("process_exit_handling", &["simple"]) {
        return;
    }
    let h = Harness::new();
    let fixture = fixture_path("simple");

    // 1. Launch simple with stop_on_entry=true → launched/stopped.
    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    // 2-3. Continue → exited, exit_code 0.
    let cont = h.continue_().await;
    assert_eq!(cont["status"], json!("exited"));
    assert_eq!(cont["exit_code"].as_i64(), Some(0), "exit_code 0");

    // 4. Session terminated.
    assert_eq!(h.state(), State::Terminated);

    // 5-6. Inspection tools error in terminated state (variables, backtrace).
    let vars = h.call_default("variables", Map::new()).await;
    let vmsg = expect_error("variables (terminated)", &vars);
    assert!(
        !vmsg.is_empty(),
        "variables error message should not be empty"
    );

    let bt = h.call_default("backtrace", Map::new()).await;
    let bmsg = expect_error("backtrace (terminated)", &bt);
    assert!(
        !bmsg.is_empty(),
        "backtrace error message should not be empty"
    );

    // 7. Disconnect → idle, then relaunch (session reuse).
    let disc = h
        .call(
            "disconnect",
            obj(&[("terminate", json!(true))]),
            Duration::from_secs(10),
        )
        .await;
    let disc = expect_json_obj("disconnect", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
    assert_eq!(h.state(), State::Idle);

    let relaunch = h.launch_fixture(&fixture).await;
    assert_eq!(relaunch["status"], json!("launched"));
    assert_eq!(relaunch["state"], json!("stopped"));

    h.disconnect_cleanup().await;
}

// --- Process exit with output (Go TestProcessExitWithOutput) ---

#[tokio::test]
async fn process_exit_with_output() {
    if should_skip("process_exit_with_output", &["simple"]) {
        return;
    }
    let h = Harness::new();
    let fixture = fixture_path("simple");

    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["status"], json!("launched"));

    let cont = h.continue_().await;
    assert_eq!(cont["status"], json!("exited"));
    assert_eq!(cont["exit_code"].as_i64(), Some(0));

    // stdout may be merged into the continue result; if not, read_output drains it.
    let has_in_continue = cont
        .get("stdout")
        .and_then(Value::as_str)
        .is_some_and(|s| s.contains("hello from simple"));

    if !has_in_continue {
        let out = h.call_default("read_output", Map::new()).await;
        let out = expect_json_obj("read_output", &out);
        let stdout = out
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            stdout.contains("hello from simple"),
            "expected stdout to contain 'hello from simple'; continue={cont:?} output={out:?}"
        );
    }

    h.disconnect_cleanup().await;
}

// --- Crash handling (Go TestCrashHandling) ---

#[tokio::test]
async fn crash_handling() {
    if should_skip("crash_handling", &["crash"]) {
        return;
    }
    let h = Harness::new();
    let fixture = fixture_path("crash");

    // 1. Launch crash → launched/stopped.
    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));

    // 2. Continue → NULL deref → a stop, NOT a tool error.
    let cont = h.continue_().await;

    // 3. Stop reason is "exception" or "signal".
    let reason = cont
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected 'reason' in continue result, got {cont:?}"));
    assert!(
        reason == "exception" || reason == "signal",
        "expected stop reason 'exception' or 'signal', got {reason:?}"
    );

    // 4. status is "stopped" (not exited/terminated).
    assert_eq!(cont["status"], json!("stopped"));

    // 5. Backtrace shows a frame referencing crash.c at line 7.
    let bt = h.call_default("backtrace", Map::new()).await;
    let bt = expect_json_obj("backtrace", &bt);
    let frames = bt["frames"].as_array().expect("frames array");
    assert!(!frames.is_empty(), "expected non-empty frames");

    let mut found_crash_frame = false;
    for frame in frames {
        let file = frame
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if file.ends_with("crash.c") {
            found_crash_frame = true;
            let line = frame
                .get("line")
                .and_then(Value::as_i64)
                .expect("crash frame has a line");
            assert_eq!(line, 7, "expected crash at crash.c:7, got line {line}");
            break;
        }
    }
    assert!(
        found_crash_frame,
        "no frame referencing crash.c found: {frames:?}"
    );

    // 6. run_command "bt" works at the crash site and references "main".
    let rc = h
        .call_default("run_command", obj(&[("command", json!("bt"))]))
        .await;
    let rc = expect_json_obj("run_command bt", &rc);
    let result = rc
        .get("result")
        .and_then(Value::as_str)
        .expect("run_command result string");
    assert!(
        result.contains("main"),
        "expected run_command 'bt' result to contain 'main', got {result:?}"
    );

    h.disconnect_cleanup().await;
}

// --- lldb-dap crash recovery (Go TestLLDBDAPCrashRecovery) ---

#[tokio::test]
async fn lldb_dap_crash_recovery() {
    if should_skip("lldb_dap_crash_recovery", &["loop"]) {
        return;
    }
    let h = Harness::new();
    let fixture = fixture_path("loop");

    // 1. Launch loop → stopped.
    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["state"], json!("stopped"));

    // 2. status reports stopped.
    let status = h.call_default("status", Map::new()).await;
    let status = expect_json_obj("status", &status);
    assert_eq!(status["state"], json!("stopped"));

    // 3. Kill the lldb-dap subprocess (via the recorded pid — the pid fix made this
    // available; previously it was 0).
    let pid = h.pid();
    assert!(
        pid > 0,
        "expected a non-zero lldb-dap subprocess pid to kill"
    );
    kill_pid(pid);

    // Let EOF propagate through the read loop so the terminated transition fires
    // (Go sleeps 200ms).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 4. status now reports terminated.
    let status = h.call_default("status", Map::new()).await;
    let status = expect_json_obj("status after crash", &status);
    assert_eq!(
        status["state"],
        json!("terminated"),
        "expected terminated after killing the subprocess"
    );

    // 5. Disconnect → idle.
    h.disconnect_cleanup().await;
    assert_eq!(h.state(), State::Idle);

    // 6. Relaunch works.
    let relaunch = h.launch_fixture(&fixture).await;
    assert_eq!(relaunch["state"], json!("stopped"));

    let status = h.call_default("status", Map::new()).await;
    let status = expect_json_obj("status after relaunch", &status);
    assert_eq!(status["state"], json!("stopped"));

    h.disconnect_cleanup().await;
}

// --- Crash during a blocked continue (Go TestLLDBDAPCrashDuringContinue) ---

#[tokio::test]
async fn lldb_dap_crash_during_continue() {
    if should_skip("lldb_dap_crash_during_continue", &["loop", "loop.c"]) {
        return;
    }
    let h = std::sync::Arc::new(Harness::new());
    let fixture = fixture_path("loop");
    let loop_src = fixture_path("loop.c");

    // 1. Launch loop → stopped.
    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["state"], json!("stopped"));

    // 2. Set a breakpoint at loop.c:6 (inside the loop).
    let bp = h
        .call_default(
            "set_breakpoint",
            obj(&[
                ("file", json!(loop_src.display().to_string())),
                ("line", json!(6)),
            ]),
        )
        .await;
    let _ = expect_json_obj("set_breakpoint", &bp);

    // 3. Start continue concurrently (it will block waiting for a stop).
    let cont_handle = {
        let h = std::sync::Arc::clone(&h);
        tokio::spawn(async move {
            // Use a 30 s bound like Go; we expect it to return promptly after the kill.
            tokio::time::timeout(
                Duration::from_secs(30),
                h.server.call("continue", &Map::new(), &tokio_token()),
            )
            .await
        })
    };

    // 4. Kill the subprocess after a short delay (let continue start running).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid = h.pid();
    assert!(pid > 0, "expected a non-zero subprocess pid");
    kill_pid(pid);

    // 5. The continue must return within the bound (no hang). The 10 s bound is the Go
    // "did not return within 10 seconds → hanging" guard (the outer timeout is 30 s; we
    // assert the stronger 10 s here).
    let cont_outcome = tokio::time::timeout(Duration::from_secs(10), cont_handle)
        .await
        .expect("continue did not return within 10s after the kill — the server is hanging")
        .expect("continue task did not panic")
        .expect("continue did not time out at the 30s outer bound");

    // After a crash the outcome is a terminated/stopped/exited status, or a tool error —
    // all acceptable; the critical property is that it returned. Only a JSON status (if
    // present) is constrained; an error/plain-text outcome is fine.
    if let mcp_tools::ToolOutcome::Json(Value::Object(map)) = &cont_outcome {
        if let Some(status) = map.get("status").and_then(Value::as_str) {
            assert!(
                matches!(status, "terminated" | "stopped" | "exited"),
                "unexpected status {status:?} in continue result after crash"
            );
        }
    }

    // 6. State is terminated or stopped (both acceptable per Go).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = h.state();
    assert!(
        matches!(state, State::Terminated | State::Stopped),
        "expected terminated or stopped after crash, got {state:?}"
    );

    // 7. Disconnect (force reset if needed) and relaunch.
    let disc = h
        .call(
            "disconnect",
            obj(&[("terminate", json!(true))]),
            Duration::from_secs(10),
        )
        .await;
    if disc.is_error() && h.state() != State::Idle {
        h.session.reset();
    }
    if h.state() != State::Idle {
        h.session.reset();
    }

    let relaunch = h.launch_fixture(&fixture).await;
    assert_eq!(relaunch["state"], json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    h.disconnect_cleanup().await;
}

/// A fresh never-cancelled token for the spawned continue task (the harness's own token is
/// behind a shared `&self`; the spawned call needs its own).
fn tokio_token() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

// --- Full 13-step end-to-end workflow (Go TestEndToEndDebuggingWorkflow) ---

#[tokio::test]
async fn end_to_end_debugging_workflow() {
    if should_skip("end_to_end_debugging_workflow", &["loop", "loop.c"]) {
        return;
    }
    let h = Harness::new();
    let fixture = fixture_path("loop");
    let loop_src = fixture_path("loop.c");
    let loop_src = loop_src.display().to_string();

    // Step 1: Launch loop → launched/stopped.
    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));

    // Step 2: Set breakpoint at loop.c:6 (sum += i).
    let bp = h
        .call_default(
            "set_breakpoint",
            obj(&[("file", json!(loop_src)), ("line", json!(6))]),
        )
        .await;
    let bp = expect_json_obj("set_breakpoint @6", &bp);
    let first_bp_id = bp["breakpoint_id"]
        .as_i64()
        .expect("breakpoint_id at line 6");

    // Step 3: Continue → stopped at the breakpoint.
    let cont = h.continue_().await;
    assert_eq!(cont["status"], json!("stopped"));
    let reason = cont
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        reason.contains("breakpoint"),
        "step 3: expected reason containing 'breakpoint', got {reason:?}"
    );

    // Step 4: Backtrace contains a frame named "main".
    let bt = h.call_default("backtrace", Map::new()).await;
    let bt = expect_json_obj("backtrace", &bt);
    let frames = bt["frames"].as_array().expect("frames");
    assert!(!frames.is_empty(), "step 4: non-empty frames");
    let found_main = frames.iter().any(|f| {
        f.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.contains("main"))
    });
    assert!(
        found_main,
        "step 4: no 'main' frame in backtrace: {frames:?}"
    );

    // Step 5: Variables include "i" and "sum".
    let vars = h.call_default("variables", Map::new()).await;
    let vars = expect_json_obj("variables", &vars);
    let var_list = vars["variables"].as_array().expect("variables array");
    let names: Vec<&str> = var_list
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"i"),
        "step 5: variable 'i' not found: {names:?}"
    );
    assert!(
        names.contains(&"sum"),
        "step 5: variable 'sum' not found: {names:?}"
    );

    // Step 6: Step over 3 times, each landing stopped.
    for n in 1..=3 {
        let step = h.call_default("step_over", Map::new()).await;
        let step = expect_json_obj("step_over", &step);
        assert_eq!(
            step["status"],
            json!("stopped"),
            "step 6.{n}: expected stopped after step_over"
        );
    }

    // Step 7: Evaluate "i + 1" → a numeric-bearing result.
    let eval = h
        .call_default("evaluate", obj(&[("expression", json!("i + 1"))]))
        .await;
    let eval = expect_json_obj("evaluate", &eval);
    let eval_result = eval["result"].as_str().expect("evaluate result string");
    assert!(
        eval_result.chars().any(|c| c.is_ascii_digit()),
        "step 7: expected a numeric value in evaluate result, got {eval_result:?}"
    );

    // Step 8: run_command "register read" → non-empty result.
    let rc = h
        .call_default("run_command", obj(&[("command", json!("register read"))]))
        .await;
    let rc = expect_json_obj("run_command register read", &rc);
    let rc_result = rc["result"].as_str().expect("register read result");
    assert!(
        !rc_result.is_empty(),
        "step 8: expected non-empty register read"
    );

    // Step 9: Set second breakpoint at loop.c:9 (printf final sum).
    let bp2 = h
        .call_default(
            "set_breakpoint",
            obj(&[("file", json!(loop_src)), ("line", json!(9))]),
        )
        .await;
    let bp2 = expect_json_obj("set_breakpoint @9", &bp2);
    let second_bp_id = bp2["breakpoint_id"]
        .as_i64()
        .expect("breakpoint_id at line 9");

    // Step 10: Continue until the second breakpoint is hit, within 20 continues.
    let mut hit_second = false;
    for _ in 0..20 {
        let cont = h.continue_().await;
        assert_eq!(
            cont["status"],
            json!("stopped"),
            "step 10: expected stopped"
        );
        if let Some(hit_ids) = cont.get("hit_breakpoint_ids").and_then(Value::as_array) {
            if hit_ids
                .iter()
                .filter_map(Value::as_i64)
                .any(|id| id == second_bp_id)
            {
                hit_second = true;
                break;
            }
        }
    }
    assert!(
        hit_second,
        "step 10: never hit the second breakpoint within 20 continues"
    );

    // Step 11: Remove the first breakpoint.
    let rm = h
        .call_default(
            "remove_breakpoint",
            obj(&[("breakpoint_id", json!(first_bp_id))]),
        )
        .await;
    let rm = expect_json_obj("remove_breakpoint", &rm);
    assert_eq!(rm["removed"], json!(true), "step 11: expected removed=true");

    // Step 12: List breakpoints → exactly 1 (the second).
    let list = h.call_default("list_breakpoints", Map::new()).await;
    let list = expect_json_obj("list_breakpoints", &list);
    assert_eq!(
        list["count"].as_i64(),
        Some(1),
        "step 12: expected 1 breakpoint"
    );
    let bps = list["breakpoints"].as_array().expect("breakpoints array");
    assert_eq!(bps.len(), 1, "step 12: expected 1 entry");
    assert_eq!(
        bps[0].get("id").and_then(Value::as_i64),
        Some(second_bp_id),
        "step 12: the remaining breakpoint is the second one"
    );

    // Step 13: Continue to exit with code 0.
    let cont = h.continue_().await;
    assert_eq!(cont["status"], json!("exited"), "step 13: expected exited");
    assert_eq!(cont["exit_code"].as_i64(), Some(0), "step 13: exit_code 0");

    h.disconnect_cleanup().await;
}
