//! WinDbg capability-gated handler tests: `open_crash_dump`, `attach_kernel`,
//! `analyze_crash`, `get_modules` (task 1.4).
//!
//! In Phase 1 no windbg factory is registered, so the two connect-point tools resolve to
//! the "not available" tool-error, and the two active-backend tools map the trait-default
//! `Unsupported` (the FakeBackend does not override the four new methods) to the standard
//! `"<tool> is not supported by the <backend> backend"` string. The dispatch test closes
//! the advertised-but-not-dispatchable gap.

use debugger_core::{AttachOutcome, DumpOutcome, ModuleInfo, StopInfo};
use mcp_session::State;
use serde_json::json;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{args, expect_error, expect_json, token, Harness};

// ---- open_crash_dump (connect point) ----

#[tokio::test]
async fn open_crash_dump_not_available_when_windbg_unregistered() {
    // The default harness registers only the "fake" factory; force-selecting "windbg" fails
    // and is wrapped as the not-available string (NOT the raw registry "unknown backend").
    let h = Harness::new();
    let a = args(&[("dump_path", json!("/tmp/core.dmp"))]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "open_crash_dump is not available: the windbg backend is not registered on this platform"
    );
    // The session is reset back to idle (connect-failure cleanup).
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn open_crash_dump_missing_dump_path_errors() {
    let h = Harness::new();
    let a = args(&[]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "missing required parameter: required argument \"dump_path\" not found"
    );
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn open_crash_dump_wrong_state_errors_with_idle_guard() {
    // Not idle (a live session) → the Idle state guard string, before any dump_path parse.
    let h = Harness::new();
    h.set_state(State::Stopped);
    let a = args(&[("dump_path", json!("/tmp/core.dmp"))]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;
    // The Idle guard string is parity-exact (a Stopped session is not idle).
    assert_eq!(
        expect_error(&out),
        "invalid state: stopped, expected one of: idle"
    );
    // Unchanged state (the guard fires before set_state).
    assert_eq!(h.session.state(), State::Stopped);
}

// ---- attach_kernel (connect point) ----

#[tokio::test]
async fn attach_kernel_rejects_non_net_connection() {
    let h = Harness::new();
    let a = args(&[("connection", json!("tcp:port=50000"))]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "'connection' must be a KDNET connection string starting with 'net:'"
    );
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn attach_kernel_not_available_when_windbg_unregistered() {
    let h = Harness::new();
    let a = args(&[("connection", json!("net:port=50000,key=1.2.3.4"))]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "attach_kernel is not available: the windbg backend is not registered on this platform"
    );
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn attach_kernel_missing_connection_errors() {
    let h = Harness::new();
    let a = args(&[]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "missing required parameter: required argument \"connection\" not found"
    );
    assert_eq!(h.session.state(), State::Idle);
}

// ---- analyze_crash (active backend) ----

#[tokio::test]
async fn analyze_crash_unsupported_on_connected_backend() {
    // The FakeBackend's analyze() override returns Unsupported("analyze_crash") when
    // analyze_result is None (scripted None = the lldb/default path); the handler maps it to
    // the standard string keyed on the active backend name ("fake" in the harness).
    let h = Harness::connected(State::Stopped).await;
    let out = h.server.handle_analyze_crash().await;
    assert_eq!(
        expect_error(&out),
        "analyze_crash is not supported by the fake backend"
    );
}

#[tokio::test]
async fn analyze_crash_requires_stopped_state() {
    let h = Harness::connected(State::Running).await;
    let out = h.server.handle_analyze_crash().await;
    // The Stopped guard fires before the backend call (Running → the "running" guard).
    assert_eq!(expect_error(&out), "process is running. Use 'pause' first.");
}

// ---- get_modules (active backend) ----

#[tokio::test]
async fn get_modules_unsupported_on_connected_backend() {
    let h = Harness::connected(State::Stopped).await;
    let out = h.server.handle_get_modules().await;
    assert_eq!(
        expect_error(&out),
        "get_modules is not supported by the fake backend"
    );
}

#[tokio::test]
async fn get_modules_requires_stopped_state() {
    let h = Harness::connected(State::Running).await;
    let out = h.server.handle_get_modules().await;
    assert_eq!(expect_error(&out), "process is running. Use 'pause' first.");
}

// ---- dispatch: the four tool NAMES reach their handlers (not "unknown tool") ----

#[tokio::test]
async fn four_windbg_tools_dispatch_to_handlers() {
    // Calling each tool by name through `dispatch` (via `server.call`) must reach the
    // handler — proven by getting the handler's guard/not-available/unsupported error,
    // NEVER "unknown tool: <name>". This closes the advertised-but-not-dispatchable gap.
    let h = Harness::connected(State::Stopped).await;
    let ct = token();

    let empty = args(&[]);
    let dump_args = args(&[("dump_path", json!("/tmp/core.dmp"))]);
    let conn_args = args(&[("connection", json!("net:port=1,key=2"))]);

    // open_crash_dump / attach_kernel go through the connected-state harness, so they fail
    // the Idle guard (still NOT "unknown tool").
    for (name, a) in [
        ("open_crash_dump", &dump_args),
        ("attach_kernel", &conn_args),
        ("analyze_crash", &empty),
        ("get_modules", &empty),
    ] {
        let out = h.server.call(name, a, &ct).await;
        let msg = expect_error(&out);
        assert!(
            !msg.starts_with("unknown tool"),
            "tool '{name}' did not dispatch: {msg}"
        );
    }
}

// ---- response-shape tests over a windbg-like factory (task 4.3) ------------------------
//
// These pin the JSON contract the agent sees for the four tools' SUCCESS paths, end-to-end
// through the real handler/session/registry, with a scripted fake backend standing in for
// the live DbgEng engine. The live engine + fixture proof of the same shapes is task 4.4's
// Dump/Crash integration group on Windows; these run cross-platform (no DbgEng).

fn stop_info(reason: &str, thread_id: i64) -> StopInfo {
    StopInfo {
        reason: reason.to_string(),
        thread_id,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    }
}

#[tokio::test]
async fn open_crash_dump_success_response_shape() {
    let h = Harness::new_windbg();
    h.state.lock().unwrap().open_dump_result = Some(Ok(DumpOutcome {
        stop: Some(stop_info("exception", 7)),
        crash_location: Some("crash.c:42".to_string()),
    }));

    let a = args(&[("dump_path", json!("C:\\dumps\\app.dmp"))]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);

    assert_eq!(v["status"], json!("dump_loaded"));
    assert_eq!(v["program"], json!("C:\\dumps\\app.dmp"));
    assert_eq!(v["stop_reason"], json!("exception"));
    assert_eq!(v["stopped_thread_id"], json!(7));
    assert_eq!(v["crash_location"], json!("crash.c:42"));

    // A dump session lands Stopped + is_dump (so continue/step_* are later rejected).
    assert_eq!(h.session.state(), State::Stopped);
    assert!(h.session.is_dump());

    // The handler drove `open_dump` with the supplied path.
    assert_eq!(
        h.state.lock().unwrap().calls,
        vec![Call::OpenDump {
            path: "C:\\dumps\\app.dmp".to_string()
        }]
    );
}

#[tokio::test]
async fn open_crash_dump_success_omits_optional_fields_when_absent() {
    // A dump with no faulting-thread context / no source line: the response omits
    // stop_reason/stopped_thread_id/crash_location but still loads (Stopped + is_dump).
    let h = Harness::new_windbg();
    h.state.lock().unwrap().open_dump_result = Some(Ok(DumpOutcome {
        stop: None,
        crash_location: None,
    }));

    let a = args(&[("dump_path", json!("/tmp/core.dmp"))]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);

    assert_eq!(v["status"], json!("dump_loaded"));
    assert_eq!(v["program"], json!("/tmp/core.dmp"));
    assert!(v.get("stop_reason").is_none());
    assert!(v.get("stopped_thread_id").is_none());
    assert!(v.get("crash_location").is_none());
    assert_eq!(h.session.state(), State::Stopped);
    assert!(h.session.is_dump());

    // The handler still drove `open_dump` with the supplied path.
    assert_eq!(
        h.state.lock().unwrap().calls,
        vec![Call::OpenDump {
            path: "/tmp/core.dmp".to_string()
        }]
    );
}

#[tokio::test]
async fn attach_kernel_success_response_shape() {
    let h = Harness::new_windbg();
    h.state.lock().unwrap().attach_kernel_result =
        Some(Ok(AttachOutcome::Stopped(stop_info("kernel-break", 0))));

    let a = args(&[("connection", json!("net:port=50000,key=1.2.3.4"))]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);

    assert_eq!(v["status"], json!("kernel_attached"));
    assert_eq!(v["connection"], json!("net:port=50000,key=1.2.3.4"));
    assert_eq!(v["state"], json!("stopped"));
    assert_eq!(v["stop_reason"], json!("kernel-break"));
    assert_eq!(v["stopped_thread_id"], json!(0));
    assert_eq!(h.session.state(), State::Stopped);

    assert_eq!(
        h.state.lock().unwrap().calls,
        vec![Call::AttachKernel {
            connection: "net:port=50000,key=1.2.3.4".to_string()
        }]
    );
}

#[tokio::test]
async fn attach_kernel_terminated_outcome_ends_session() {
    // The kernel target died/was unreachable mid-handshake → Terminated, text result, the
    // backend stays in the slot (the agent disconnects to recover). This is the clean
    // (non-hanging) error-path return; the unreachable-KDNET INFINITE block is the
    // `#[ignore]` live test, never unit-tested.
    let h = Harness::new_windbg();
    h.state.lock().unwrap().attach_kernel_result = Some(Ok(AttachOutcome::Terminated));

    let a = args(&[("connection", json!("net:port=50000,key=1.2.3.4"))]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;
    // A Text outcome (not an error, not a panic) and the session is Terminated.
    match &out {
        crate::ToolOutcome::Text(t) => assert!(t.contains("Kernel session ended")),
        other => panic!("expected Text outcome, got {other:?}"),
    }
    assert_eq!(h.session.state(), State::Terminated);

    // The handler routed to `attach_kernel` (not another method that also yields Terminated).
    assert_eq!(
        h.state.lock().unwrap().calls,
        vec![Call::AttachKernel {
            connection: "net:port=50000,key=1.2.3.4".to_string()
        }]
    );
}

#[tokio::test]
async fn open_crash_dump_connect_failure_resets_and_reports_windbg_wording() {
    // The windbg-named factory's connect() fails: the force-select succeeds (name-based), so
    // the failure lands at the connect phase → the handler resets the session to Idle, clears
    // the backend, and surfaces the windbg-aware connect_error wording. Mirrors the launch/
    // attach connect-failure tests, but through the open_crash_dump connect point.
    let h = Harness::new_windbg_connect_error(debugger_core::BackendError::Detect(
        "no dbgeng".to_string(),
    ));

    let a = args(&[("dump_path", json!("/tmp/core.dmp"))]);
    let out = h
        .server
        .handle_open_crash_dump(&crate::Args::new(&a), &token())
        .await;

    // connect_error("windbg", Detect(..)) → the reserved WinDbg "not found" wording.
    assert_eq!(
        expect_error(&out),
        "Debugging Tools for Windows not found: no dbgeng"
    );
    // The connect-failure cleanup reset the session back to Idle.
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn attach_kernel_connect_failure_resets_and_reports_windbg_wording() {
    // Same connect-failure cleanup path, but through the attach_kernel connect point with a
    // valid net: connection (so the failure is at connect, not the connection-string guard).
    // A Spawn error here exercises the other connect_error branch.
    let h = Harness::new_windbg_connect_error(debugger_core::BackendError::Spawn(
        "init failed".to_string(),
    ));

    let a = args(&[("connection", json!("net:port=50000,key=1.2.3.4"))]);
    let out = h
        .server
        .handle_attach_kernel(&crate::Args::new(&a), &token())
        .await;

    // connect_error("windbg", Spawn(..)) → the reserved DbgEng-init wording.
    assert_eq!(
        expect_error(&out),
        "failed to initialize DbgEng: init failed"
    );
    assert_eq!(h.session.state(), State::Idle);
}

#[tokio::test]
async fn analyze_crash_success_response_shape() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().analyze_result = Some(Ok(
        "*** Bugcheck Analysis ***\nFAILURE_BUCKET_ID: FOO".to_string(),
    ));

    let out = h.server.handle_analyze_crash().await;
    let v = expect_json(&out);
    assert_eq!(
        v["analysis"],
        json!("*** Bugcheck Analysis ***\nFAILURE_BUCKET_ID: FOO")
    );
    assert_eq!(h.state.lock().unwrap().calls, vec![Call::Analyze]);
}

#[tokio::test]
async fn get_modules_success_response_shape() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().modules_result = Some(Ok(vec![
        ModuleInfo {
            name: "test_target".to_string(),
            base: "0x0000000140000000".to_string(),
            size: "65536".to_string(),
            symbol_status: "pdb".to_string(),
        },
        ModuleInfo {
            name: "ntdll".to_string(),
            base: "0x00007FFAB0000000".to_string(),
            size: "2031616".to_string(),
            symbol_status: "export".to_string(),
        },
    ]));

    let out = h.server.handle_get_modules().await;
    let v = expect_json(&out);
    let modules = v["modules"].as_array().expect("modules array");
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0]["name"], json!("test_target"));
    assert_eq!(modules[0]["base"], json!("0x0000000140000000"));
    assert_eq!(modules[0]["size"], json!("65536"));
    assert_eq!(modules[0]["symbol_status"], json!("pdb"));
    assert_eq!(modules[1]["name"], json!("ntdll"));
    assert_eq!(h.state.lock().unwrap().calls, vec![Call::Modules]);
}
