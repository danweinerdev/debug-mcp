//! WinDbg capability-gated handler tests: `open_crash_dump`, `attach_kernel`,
//! `analyze_crash`, `get_modules` (task 1.4).
//!
//! In Phase 1 no windbg factory is registered, so the two connect-point tools resolve to
//! the "not available" tool-error, and the two active-backend tools map the trait-default
//! `Unsupported` (the FakeBackend does not override the four new methods) to the standard
//! `"<tool> is not supported by the <backend> backend"` string. The dispatch test closes
//! the advertised-but-not-dispatchable gap.

use mcp_session::State;
use serde_json::json;

use crate::tests::handlers::support::{args, expect_error, token, Harness};

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
    // The FakeBackend inherits the trait-default Unsupported for analyze(); the handler maps
    // it to the standard string keyed on the active backend name ("fake" in the harness).
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
