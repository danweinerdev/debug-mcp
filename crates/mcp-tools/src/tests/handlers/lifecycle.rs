//! Lifecycle handler tests: `launch`, `attach`, `disconnect` (Spec FR-4/FR-5/FR-6).

use std::sync::Arc;

use debugger_core::{
    AttachOutcome, BackendError, BackendEvent, Connection, DebuggerBackend, LaunchOutcome, StopInfo,
};
use futures::stream::{self, StreamExt};
use mcp_session::State;
use serde_json::json;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{
    args, expect_error, expect_json, expect_text, token, Harness,
};

fn stop(reason: &str, thread_id: i64) -> StopInfo {
    StopInfo {
        reason: reason.to_string(),
        thread_id,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    }
}

#[tokio::test]
async fn launch_missing_program() {
    let h = Harness::new();
    let a = args(&[]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    let msg = expect_error(&out);
    assert!(msg.starts_with("missing required parameter:"), "got {msg}");
}

#[tokio::test]
async fn launch_stop_on_entry_returns_stopped_json() {
    let h = Harness::new();
    h.state.lock().unwrap().launch_outcome = Some(Ok(LaunchOutcome::Stopped(stop("entry", 1))));
    let a = args(&[("program", json!("/bin/prog"))]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("launched"));
    assert_eq!(v["program"], json!("/bin/prog"));
    assert_eq!(v["state"], json!("stopped"));
    assert_eq!(v["stop_reason"], json!("entry"));
    assert_eq!(v["stopped_thread_id"], json!(1));
    assert_eq!(h.session.state(), State::Stopped);
    // last_stopped cached.
    assert_eq!(h.session.last_stopped().unwrap().reason, "entry");
}

#[tokio::test]
async fn launch_records_debugger_subprocess_pid() {
    // Go records the lldb-dap subprocess pid (`SetPID(sub.Cmd.Process.Pid)`); the handler
    // pulls it from `backend.debugger_pid()` after connect and reports it in `pid`.
    let h = Harness::new();
    h.state.lock().unwrap().debugger_pid = Some(54321);
    h.state.lock().unwrap().launch_outcome = Some(Ok(LaunchOutcome::Stopped(stop("entry", 1))));
    let a = args(&[("program", json!("/bin/prog"))]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["pid"], json!(54321));
    assert_eq!(h.session.pid(), 54321);
}

#[tokio::test]
async fn launch_running_returns_running_json() {
    let h = Harness::new();
    h.state.lock().unwrap().launch_outcome = Some(Ok(LaunchOutcome::Running));
    let a = args(&[
        ("program", json!("/bin/p")),
        ("stop_on_entry", json!(false)),
    ]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["state"], json!("running"));
    assert!(v.get("stop_reason").is_none());
    assert_eq!(h.session.state(), State::Running);
}

#[tokio::test]
async fn launch_exit_during_returns_plain_text() {
    let h = Harness::new();
    h.state.lock().unwrap().launch_outcome = Some(Ok(LaunchOutcome::Exited { code: Some(3) }));
    let a = args(&[("program", json!("/bin/p"))]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_text(&out), "Program exited during launch");
    assert_eq!(h.session.state(), State::Terminated);
}

#[tokio::test]
async fn launch_connect_failure_maps_to_go_strings_and_resets() {
    // Detect failure → `failed to find lldb-dap: …`; session reset to idle.
    let state = Arc::new(std::sync::Mutex::new(
        crate::tests::fake::FakeState::default(),
    ));
    let session = Arc::new(mcp_session::SessionManager::new());
    let factory = Arc::new(crate::tests::fake::FakeFactory::with_connect_error(
        Arc::clone(&state),
        BackendError::Detect("no binary".to_string()),
    ));
    let server = crate::ToolServer::new(Arc::clone(&session), factory);
    let a = args(&[("program", json!("/bin/p"))]);
    let out = server.handle_launch(&crate::Args::new(&a), &token()).await;
    assert_eq!(expect_error(&out), "failed to find lldb-dap: no binary");
    assert_eq!(session.state(), State::Idle);
}

#[tokio::test]
async fn launch_spawn_failure_maps_to_go_string() {
    let state = Arc::new(std::sync::Mutex::new(
        crate::tests::fake::FakeState::default(),
    ));
    let session = Arc::new(mcp_session::SessionManager::new());
    let factory = Arc::new(crate::tests::fake::FakeFactory::with_connect_error(
        Arc::clone(&state),
        BackendError::Spawn("exec error".to_string()),
    ));
    let server = crate::ToolServer::new(Arc::clone(&session), factory);
    let a = args(&[("program", json!("/bin/p"))]);
    let out = server.handle_launch(&crate::Args::new(&a), &token()).await;
    assert_eq!(expect_error(&out), "failed to spawn lldb-dap: exec error");
}

#[tokio::test]
async fn launch_handshake_failure_renders_backend_message_verbatim() {
    let h = Harness::new();
    h.state.lock().unwrap().launch_outcome = Some(Err(BackendError::Dap {
        message: "launch failed: no such file".to_string(),
    }));
    let a = args(&[("program", json!("/bin/p"))]);
    let out = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_error(&out), "launch failed: no such file");
}

#[tokio::test]
async fn launch_flushes_pending_breakpoints_into_spec() {
    // A pending breakpoint set in idle is flushed; the connect path's fake records nothing
    // about the spec, but flush must clear pending + move it to active tracking.
    let h = Harness::new();
    h.session.add_pending_source_breakpoint("/loop.c", 6, "");
    h.state.lock().unwrap().launch_outcome = Some(Ok(LaunchOutcome::Running));
    let a = args(&[
        ("program", json!("/bin/p")),
        ("stop_on_entry", json!(false)),
    ]);
    let _ = h
        .server
        .handle_launch(&crate::Args::new(&a), &token())
        .await;
    // Active tracking now holds the flushed bp; pending is empty (idempotent re-flush).
    assert_eq!(h.session.source_breakpoints_for_file("/loop.c").len(), 1);
}

#[tokio::test]
async fn launch_event_pump_runs_before_backend_launch() {
    // The pump must be spawned BEFORE backend.launch so a Terminated during the handshake
    // reaches the session. We model this by a factory whose event stream emits Terminated
    // immediately, and a backend whose launch awaits a tick — by the time launch returns
    // Running, the pump has already applied the terminated transition (generation-guarded).
    use std::sync::Mutex;

    struct PumpBackend;
    #[async_trait::async_trait]
    impl DebuggerBackend for PumpBackend {
        async fn launch(
            &self,
            _spec: debugger_core::LaunchSpec,
        ) -> Result<LaunchOutcome, BackendError> {
            // Yield enough for the pump to drain the Terminated event first.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(LaunchOutcome::Running)
        }
        async fn attach(
            &self,
            _spec: debugger_core::AttachSpec,
        ) -> Result<AttachOutcome, BackendError> {
            unreachable!()
        }
        async fn disconnect(&self, _terminate: bool) {}
        async fn set_source_breakpoints(
            &self,
            _f: &str,
            _b: &[debugger_core::SourceBp],
        ) -> Result<Vec<debugger_core::BreakpointResult>, BackendError> {
            Ok(Vec::new())
        }
        async fn set_function_breakpoints(
            &self,
            _b: &[debugger_core::FunctionBp],
        ) -> Result<Vec<debugger_core::BreakpointResult>, BackendError> {
            Ok(Vec::new())
        }
        async fn cont(&self, _t: i64) -> Result<debugger_core::StopOutcome, BackendError> {
            unreachable!()
        }
        async fn step(
            &self,
            _k: debugger_core::StepKind,
            _t: i64,
            _g: Option<debugger_core::Granularity>,
        ) -> Result<debugger_core::StopOutcome, BackendError> {
            unreachable!()
        }
        async fn pause(&self) -> Result<(), BackendError> {
            Ok(())
        }
        async fn threads(&self) -> Result<Vec<debugger_core::ThreadInfo>, BackendError> {
            Ok(Vec::new())
        }
        async fn stack_trace(
            &self,
            _t: i64,
            _s: i64,
            _l: i64,
        ) -> Result<(Vec<debugger_core::Frame>, i64), BackendError> {
            Ok((Vec::new(), 0))
        }
        async fn scopes(&self, _f: i64) -> Result<Vec<debugger_core::Scope>, BackendError> {
            Ok(Vec::new())
        }
        async fn variables(&self, _r: i64) -> Result<Vec<debugger_core::Variable>, BackendError> {
            Ok(Vec::new())
        }
        async fn evaluate(
            &self,
            _e: &str,
            _f: Option<i64>,
            _m: debugger_core::EvalMode,
        ) -> Result<debugger_core::EvalResult, BackendError> {
            unreachable!()
        }
        async fn read_memory(
            &self,
            _a: &str,
            _c: i64,
        ) -> Result<debugger_core::MemoryRead, BackendError> {
            unreachable!()
        }
        async fn disassemble(
            &self,
            _a: &str,
            _c: i64,
        ) -> Result<Vec<debugger_core::Instruction>, BackendError> {
            unreachable!()
        }
        fn supports_command_repl_mode(&self) -> bool {
            true
        }
    }

    struct PumpFactory;
    #[async_trait::async_trait]
    impl debugger_core::BackendFactory for PumpFactory {
        fn name(&self) -> &'static str {
            "pump"
        }
        async fn connect(&self) -> Result<Connection, BackendError> {
            let backend: Arc<dyn DebuggerBackend> = Arc::new(PumpBackend);
            let events = stream::iter(vec![BackendEvent::Terminated { code: Some(9) }]).boxed();
            Ok(Connection { backend, events })
        }
    }

    let session = Arc::new(mcp_session::SessionManager::new());
    let _guard: Mutex<()> = Mutex::new(());
    let server = crate::ToolServer::new(Arc::clone(&session), Arc::new(PumpFactory));
    let a = args(&[
        ("program", json!("/bin/p")),
        ("stop_on_entry", json!(false)),
    ]);
    let _ = server.handle_launch(&crate::Args::new(&a), &token()).await;

    // The pump (spawned before launch) drained the Terminated event during the launch
    // sleep and recorded the exit code — proving it ran before backend.launch returned.
    // Give the pump task a moment to finish in case it raced the handler's final write.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(session.exit_code(), Some(9));
}

#[tokio::test]
async fn launch_cancellation_returns_timeout_string() {
    // A backend whose launch never resolves; the cancelled token wins the select.
    struct HangBackend;
    #[async_trait::async_trait]
    impl DebuggerBackend for HangBackend {
        async fn launch(
            &self,
            _s: debugger_core::LaunchSpec,
        ) -> Result<LaunchOutcome, BackendError> {
            std::future::pending().await
        }
        async fn attach(
            &self,
            _s: debugger_core::AttachSpec,
        ) -> Result<AttachOutcome, BackendError> {
            std::future::pending().await
        }
        async fn disconnect(&self, _t: bool) {}
        async fn set_source_breakpoints(
            &self,
            _f: &str,
            _b: &[debugger_core::SourceBp],
        ) -> Result<Vec<debugger_core::BreakpointResult>, BackendError> {
            Ok(Vec::new())
        }
        async fn set_function_breakpoints(
            &self,
            _b: &[debugger_core::FunctionBp],
        ) -> Result<Vec<debugger_core::BreakpointResult>, BackendError> {
            Ok(Vec::new())
        }
        async fn cont(&self, _t: i64) -> Result<debugger_core::StopOutcome, BackendError> {
            unreachable!()
        }
        async fn step(
            &self,
            _k: debugger_core::StepKind,
            _t: i64,
            _g: Option<debugger_core::Granularity>,
        ) -> Result<debugger_core::StopOutcome, BackendError> {
            unreachable!()
        }
        async fn pause(&self) -> Result<(), BackendError> {
            Ok(())
        }
        async fn threads(&self) -> Result<Vec<debugger_core::ThreadInfo>, BackendError> {
            Ok(Vec::new())
        }
        async fn stack_trace(
            &self,
            _t: i64,
            _s: i64,
            _l: i64,
        ) -> Result<(Vec<debugger_core::Frame>, i64), BackendError> {
            Ok((Vec::new(), 0))
        }
        async fn scopes(&self, _f: i64) -> Result<Vec<debugger_core::Scope>, BackendError> {
            Ok(Vec::new())
        }
        async fn variables(&self, _r: i64) -> Result<Vec<debugger_core::Variable>, BackendError> {
            Ok(Vec::new())
        }
        async fn evaluate(
            &self,
            _e: &str,
            _f: Option<i64>,
            _m: debugger_core::EvalMode,
        ) -> Result<debugger_core::EvalResult, BackendError> {
            unreachable!()
        }
        async fn read_memory(
            &self,
            _a: &str,
            _c: i64,
        ) -> Result<debugger_core::MemoryRead, BackendError> {
            unreachable!()
        }
        async fn disassemble(
            &self,
            _a: &str,
            _c: i64,
        ) -> Result<Vec<debugger_core::Instruction>, BackendError> {
            unreachable!()
        }
        fn supports_command_repl_mode(&self) -> bool {
            true
        }
    }
    struct HangFactory;
    #[async_trait::async_trait]
    impl debugger_core::BackendFactory for HangFactory {
        fn name(&self) -> &'static str {
            "hang"
        }
        async fn connect(&self) -> Result<Connection, BackendError> {
            let backend: Arc<dyn DebuggerBackend> = Arc::new(HangBackend);
            Ok(Connection {
                backend,
                events: stream::empty().boxed(),
            })
        }
    }

    let session = Arc::new(mcp_session::SessionManager::new());
    let server = crate::ToolServer::new(Arc::clone(&session), Arc::new(HangFactory));
    let ct = token();
    ct.cancel();
    let a = args(&[("program", json!("/bin/p"))]);
    let out = server.handle_launch(&crate::Args::new(&a), &ct).await;
    let msg = expect_error(&out);
    assert!(
        msg.contains("timed out waiting for stop on entry"),
        "got {msg}"
    );
    // Cleanup reset the session.
    assert_eq!(session.state(), State::Idle);
}

// ---- attach ----

#[tokio::test]
async fn attach_requires_pid_or_wait_for() {
    let h = Harness::new();
    let a = args(&[]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(
        expect_error(&out),
        "either 'pid' or 'wait_for' must be provided"
    );
}

#[tokio::test]
async fn attach_pid_not_a_number() {
    let h = Harness::new();
    let a = args(&[("pid", json!("oops"))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_error(&out), "'pid' must be a number");
}

#[tokio::test]
async fn attach_pid_must_be_positive() {
    for bad in [0, -5] {
        let h = Harness::new();
        let a = args(&[("pid", json!(bad))]);
        let out = h
            .server
            .handle_attach(&crate::Args::new(&a), &token())
            .await;
        assert_eq!(expect_error(&out), "'pid' must be a positive integer");
    }
}

#[tokio::test]
async fn attach_empty_wait_for() {
    let h = Harness::new();
    let a = args(&[("wait_for", json!(""))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_error(&out), "'wait_for' must be a non-empty string");
}

#[tokio::test]
async fn attach_pid_takes_precedence_and_returns_stopped() {
    let h = Harness::new();
    h.state.lock().unwrap().attach_outcome = Some(Ok(AttachOutcome::Stopped(stop("signal", 2))));
    let a = args(&[("pid", json!(4321)), ("wait_for", json!("ignored"))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("attached"));
    assert_eq!(v["program"], json!("pid:4321"));
    assert_eq!(v["pid"], json!(4321));
    assert_eq!(v["state"], json!("stopped"));
    assert_eq!(v["stop_reason"], json!("signal"));
}

#[tokio::test]
async fn attach_wait_for_label() {
    let h = Harness::new();
    h.state.lock().unwrap().attach_outcome = Some(Ok(AttachOutcome::Stopped(stop("entry", 1))));
    let a = args(&[("wait_for", json!("myproc"))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["program"], json!("myproc"));
}

#[tokio::test]
async fn attach_wait_for_records_subprocess_pid() {
    // Attaching by wait_for has no target pid up front, so Go reports the lldb-dap
    // subprocess pid (Spec FR-5.6); the handler pulls it from `backend.debugger_pid()`.
    let h = Harness::new();
    h.state.lock().unwrap().debugger_pid = Some(7777);
    h.state.lock().unwrap().attach_outcome = Some(Ok(AttachOutcome::Stopped(stop("entry", 1))));
    let a = args(&[("wait_for", json!("myproc"))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["pid"], json!(7777));
    assert_eq!(h.session.pid(), 7777);
}

#[tokio::test]
async fn attach_by_pid_overrides_subprocess_pid() {
    // The supplied target pid takes precedence over the subprocess pid (Spec FR-5.6).
    let h = Harness::new();
    h.state.lock().unwrap().debugger_pid = Some(7777);
    h.state.lock().unwrap().attach_outcome = Some(Ok(AttachOutcome::Stopped(stop("signal", 2))));
    let a = args(&[("pid", json!(4321))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    let v = expect_json(&out);
    assert_eq!(v["pid"], json!(4321));
    assert_eq!(h.session.pid(), 4321);
}

#[tokio::test]
async fn attach_exit_during_returns_plain_text() {
    let h = Harness::new();
    h.state.lock().unwrap().attach_outcome = Some(Ok(AttachOutcome::Terminated));
    let a = args(&[("pid", json!(9))]);
    let out = h
        .server
        .handle_attach(&crate::Args::new(&a), &token())
        .await;
    assert_eq!(expect_text(&out), "Process exited during attach");
    assert_eq!(h.session.state(), State::Terminated);
}

// ---- disconnect ----

#[tokio::test]
async fn disconnect_always_returns_disconnected_and_resets() {
    let h = Harness::connected(State::Stopped).await;
    h.session.set_program("/bin/x".to_string());
    h.session.set_pid(42);
    let empty = args(&[]);
    let out = h.server.handle_disconnect(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("disconnected"));
    assert_eq!(h.session.state(), State::Idle);
    assert_eq!(h.session.program(), "");
    assert_eq!(h.session.pid(), 0);
    // The backend was asked to disconnect with terminate=true (default).
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Disconnect { terminate: true })));
    // The backend slot was cleared.
    assert!(h.server.current_backend().await.is_none());
}

#[tokio::test]
async fn disconnect_terminate_false_passes_through() {
    let h = Harness::connected(State::Running).await;
    let a = args(&[("terminate", json!(false))]);
    let _ = h.server.handle_disconnect(&crate::Args::new(&a)).await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Disconnect { terminate: false })));
}

#[tokio::test]
async fn disconnect_with_no_backend_still_succeeds() {
    // configuring with no backend connected (Go's disconnect-from-configuring path).
    let h = Harness::new();
    h.set_state(State::Configuring);
    let empty = args(&[]);
    let out = h.server.handle_disconnect(&crate::Args::new(&empty)).await;
    assert_eq!(expect_json(&out)["status"], json!("disconnected"));
    assert_eq!(h.session.state(), State::Idle);
}
