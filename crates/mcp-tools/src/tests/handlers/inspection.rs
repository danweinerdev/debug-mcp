//! Inspection handler tests: `status`, `backtrace`, `threads`, `variables`, `evaluate`
//! (Spec FR-9/FR-10), including the `resolve_frame_id` implicit-stackTrace error strings.

use debugger_core::{
    BackendError, EvalMode, EvalResult, Frame, Scope, StopInfo, ThreadInfo, Variable,
};
use mcp_session::State;
use serde_json::json;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{args, expect_error, expect_json, Harness};

fn frame(
    index: i64,
    id: i64,
    name: &str,
    path: Option<&str>,
    line: i64,
    ip: Option<&str>,
) -> Frame {
    Frame {
        index,
        id,
        name: name.to_string(),
        source_path: path.map(str::to_string),
        line,
        instruction_pointer: ip.map(str::to_string),
    }
}

// ---- status (cache-only) ----

#[tokio::test]
async fn status_idle() {
    let h = Harness::new();
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["state"], json!("idle"));
    assert_eq!(v["message"], json!("No active debug session"));
}

#[tokio::test]
async fn status_configuring() {
    let h = Harness::new();
    h.set_state(State::Configuring);
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["message"], json!("Debug session is being configured"));
}

#[tokio::test]
async fn status_stopped_full_event() {
    let h = Harness::new();
    h.set_state(State::Stopped);
    h.session.set_program("/usr/bin/test".to_string());
    h.session.set_pid(12345);
    h.session.set_last_stopped(StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: "stopped at breakpoint 3".to_string(),
        hit_breakpoint_ids: vec![3],
    });
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["state"], json!("stopped"));
    assert_eq!(v["program"], json!("/usr/bin/test"));
    assert_eq!(v["pid"], json!(12345));
    assert_eq!(v["stop_reason"], json!("breakpoint"));
    assert_eq!(v["stopped_thread_id"], json!(1));
    assert_eq!(v["stop_description"], json!("stopped at breakpoint 3"));
    assert_eq!(v["hit_breakpoint_ids"], json!([3]));
}

#[tokio::test]
async fn status_stopped_no_event_omits_stop_fields() {
    let h = Harness::new();
    h.set_state(State::Stopped);
    h.session.set_program("/usr/bin/test".to_string());
    h.session.set_pid(42);
    let v = expect_json(&h.server.handle_status()).clone();
    assert!(v.get("stop_reason").is_none());
    assert!(v.get("stopped_thread_id").is_none());
}

#[tokio::test]
async fn status_stopped_minimal_event_omits_description_and_bps() {
    let h = Harness::new();
    h.set_state(State::Stopped);
    h.session.set_last_stopped(StopInfo {
        reason: "step".to_string(),
        thread_id: 2,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    });
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["stop_reason"], json!("step"));
    assert!(v.get("stop_description").is_none());
    assert!(v.get("hit_breakpoint_ids").is_none());
}

#[tokio::test]
async fn status_running() {
    let h = Harness::new();
    h.set_state(State::Running);
    h.session.set_program("/bin/x".to_string());
    h.session.set_pid(7);
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["state"], json!("running"));
    assert_eq!(v["program"], json!("/bin/x"));
    assert_eq!(v["pid"], json!(7));
}

#[tokio::test]
async fn status_terminated_with_and_without_exit_code() {
    let h = Harness::new();
    h.set_state(State::Terminated);
    h.session.set_program("/bin/x".to_string());
    h.session.set_exit_code(0);
    let v = expect_json(&h.server.handle_status()).clone();
    assert_eq!(v["state"], json!("terminated"));
    assert_eq!(v["exit_code"], json!(0));

    let h = Harness::new();
    h.set_state(State::Terminated);
    h.session.set_program("/bin/x".to_string());
    let v = expect_json(&h.server.handle_status()).clone();
    assert!(v.get("exit_code").is_none());
}

// ---- backtrace ----

#[tokio::test]
async fn backtrace_formats_frames_and_stores_mapping() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().stack_trace_result = Some(Ok((
        vec![
            frame(0, 100, "main", Some("/loop.c"), 6, Some("0xdead")),
            frame(1, 101, "_start", None, 0, None),
        ],
        2,
    )));
    let empty = args(&[]);
    let out = h.server.handle_backtrace(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["total_frames"], json!(2));
    assert_eq!(v["thread_id"], json!(1));
    let frames = v["frames"].as_array().unwrap();
    assert_eq!(frames[0]["index"], json!(0));
    assert_eq!(frames[0]["id"], json!(100));
    assert_eq!(frames[0]["name"], json!("main"));
    assert_eq!(frames[0]["file"], json!("/loop.c"));
    assert_eq!(frames[0]["line"], json!(6));
    assert_eq!(frames[0]["address"], json!("0xdead"));
    // Second frame: no source/ip → no file/line/address.
    assert!(frames[1].get("file").is_none());
    assert!(frames[1].get("address").is_none());
    // Frame mapping stored.
    assert_eq!(h.session.frame_mapping().get(&0), Some(&100));
    assert_eq!(h.session.frame_mapping().get(&1), Some(&101));
    // levels default 20.
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::StackTrace { levels: 20, .. })));
}

#[tokio::test]
async fn backtrace_levels_override_only_when_positive() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().stack_trace_result = Some(Ok((Vec::new(), 0)));
    let a = args(&[("levels", json!(5))]);
    let _ = h.server.handle_backtrace(&crate::Args::new(&a)).await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::StackTrace { levels: 5, .. })));
}

// ---- threads ----

#[tokio::test]
async fn threads_marks_stopped_and_current() {
    let h = Harness::connected(State::Stopped).await;
    h.session.set_last_stopped(StopInfo {
        reason: "x".to_string(),
        thread_id: 2,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    });
    h.state.lock().unwrap().threads_result = Some(Ok(vec![
        ThreadInfo {
            id: 1,
            name: "main".to_string(),
        },
        ThreadInfo {
            id: 2,
            name: "worker".to_string(),
        },
    ]));
    let empty = args(&[]);
    let out = h.server.handle_threads(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["count"], json!(2));
    assert_eq!(v["stopped_thread_id"], json!(2));
    let threads = v["threads"].as_array().unwrap();
    assert!(threads[0].get("is_stopped").is_none());
    assert_eq!(threads[1]["is_stopped"], json!(true));
    assert_eq!(threads[1]["is_current"], json!(true));
}

#[tokio::test]
async fn threads_no_stopped_match_omits_stopped_thread_id() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().threads_result = Some(Ok(vec![ThreadInfo {
        id: 1,
        name: "main".to_string(),
    }]));
    let empty = args(&[]);
    let out = h.server.handle_threads(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert!(v.get("stopped_thread_id").is_none());
}

// ---- variables ----

#[tokio::test]
async fn variables_resolves_frame_scope_and_flattens() {
    let h = Harness::connected(State::Stopped).await;
    // Pre-seed the frame map so resolve hits (no implicit stackTrace).
    let mut mapping = std::collections::HashMap::new();
    mapping.insert(0, 100);
    h.session.set_frame_mapping(mapping);
    h.state.lock().unwrap().scopes_result = Some(Ok(vec![Scope {
        name: "Locals".to_string(),
        variables_reference: 1,
    }]));
    h.state.lock().unwrap().variables_result = Some(Ok(vec![
        Variable {
            name: "i".to_string(),
            value: "0".to_string(),
            ty: "int".to_string(),
            variables_reference: 0,
            named: 0,
            indexed: 0,
        },
        Variable {
            name: "sum".to_string(),
            value: "10".to_string(),
            ty: "int".to_string(),
            variables_reference: 0,
            named: 0,
            indexed: 0,
        },
    ]));
    let empty = args(&[]);
    let out = h.server.handle_variables(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["scope"], json!("local"));
    assert_eq!(v["count"], json!(2));
    assert_eq!(v["truncated"], json!(false));
    let vars = v["variables"].as_array().unwrap();
    assert_eq!(vars[0]["name"], json!("i"));
    assert_eq!(vars[1]["name"], json!("sum"));
    // scopes used frame id 100.
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Scopes { frame_id: 100 })));
}

#[tokio::test]
async fn variables_scope_not_found() {
    let h = Harness::connected(State::Stopped).await;
    let mut mapping = std::collections::HashMap::new();
    mapping.insert(0, 100);
    h.session.set_frame_mapping(mapping);
    h.state.lock().unwrap().scopes_result = Some(Ok(vec![Scope {
        name: "Registers".to_string(),
        variables_reference: 2,
    }]));
    let empty = args(&[]);
    let out = h.server.handle_variables(&crate::Args::new(&empty)).await;
    assert_eq!(expect_error(&out), "scope 'local' not found in frame 0");
}

#[tokio::test]
async fn variables_global_scope_case_insensitive() {
    let h = Harness::connected(State::Stopped).await;
    let mut mapping = std::collections::HashMap::new();
    mapping.insert(0, 100);
    h.session.set_frame_mapping(mapping);
    h.state.lock().unwrap().scopes_result = Some(Ok(vec![Scope {
        name: "Globals".to_string(),
        variables_reference: 3,
    }]));
    h.state.lock().unwrap().variables_result = Some(Ok(Vec::new()));
    let a = args(&[("scope", json!("global"))]);
    let out = h.server.handle_variables(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["scope"], json!("global"));
}

#[tokio::test]
async fn variables_implicit_frame_resolution_uses_levels_20() {
    // Frame-map miss → implicit stackTrace(levels=20) rebuilds the mapping.
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().stack_trace_result =
        Some(Ok((vec![frame(0, 100, "main", None, 0, None)], 1)));
    h.state.lock().unwrap().scopes_result = Some(Ok(vec![Scope {
        name: "Locals".to_string(),
        variables_reference: 1,
    }]));
    h.state.lock().unwrap().variables_result = Some(Ok(Vec::new()));
    let empty = args(&[]);
    let _ = h.server.handle_variables(&crate::Args::new(&empty)).await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::StackTrace { levels: 20, .. })));
}

#[tokio::test]
async fn variables_frame_resolution_error_uses_combined_string() {
    // The implicit stackTrace fails (success=false) → the inner `implicit stackTrace
    // failed: …` wrapped as `failed to resolve frame: …` (Spec FR-10.3 / task 5.4).
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().stack_trace_result = Some(Err(BackendError::Dap {
        message: "stackTrace failed: boom".to_string(),
    }));
    let empty = args(&[]);
    let out = h.server.handle_variables(&crate::Args::new(&empty)).await;
    assert_eq!(
        expect_error(&out),
        "failed to resolve frame: implicit stackTrace failed: boom"
    );
}

#[tokio::test]
async fn variables_frame_out_of_range_error() {
    let h = Harness::connected(State::Stopped).await;
    // Implicit stackTrace returns 1 frame; request frame_index 3 → out of range.
    h.state.lock().unwrap().stack_trace_result =
        Some(Ok((vec![frame(0, 100, "main", None, 0, None)], 1)));
    let a = args(&[("frame_index", json!(3))]);
    let out = h.server.handle_variables(&crate::Args::new(&a)).await;
    assert_eq!(
        expect_error(&out),
        "failed to resolve frame: frame index 3 out of range (stack has 1 frames)"
    );
}

// ---- evaluate ----

#[tokio::test]
async fn evaluate_returns_result_and_type() {
    let h = Harness::connected(State::Stopped).await;
    let mut mapping = std::collections::HashMap::new();
    mapping.insert(0, 100);
    h.session.set_frame_mapping(mapping);
    h.state.lock().unwrap().evaluate_result = Some(Ok(EvalResult {
        result: "1".to_string(),
        ty: "int".to_string(),
        variables_reference: 0,
    }));
    let a = args(&[("expression", json!("i + 1"))]);
    let out = h.server.handle_evaluate(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["result"], json!("1"));
    assert_eq!(v["type"], json!("int"));
    assert!(v.get("has_children").is_none());
    // context=variables with the resolved frame id.
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::Evaluate {
            frame_id: Some(100),
            mode: EvalMode::Expression,
            ..
        }
    )));
}

#[tokio::test]
async fn evaluate_with_children_adds_has_children() {
    let h = Harness::connected(State::Stopped).await;
    let mut mapping = std::collections::HashMap::new();
    mapping.insert(0, 100);
    h.session.set_frame_mapping(mapping);
    h.state.lock().unwrap().evaluate_result = Some(Ok(EvalResult {
        result: "Point".to_string(),
        ty: "Point".to_string(),
        variables_reference: 7,
    }));
    let a = args(&[("expression", json!("p"))]);
    let out = h.server.handle_evaluate(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["has_children"], json!(true));
    assert_eq!(v["variables_reference"], json!(7));
}

#[tokio::test]
async fn evaluate_missing_expression() {
    let h = Harness::connected(State::Stopped).await;
    let empty = args(&[]);
    let out = h.server.handle_evaluate(&crate::Args::new(&empty)).await;
    assert!(expect_error(&out).starts_with("missing required parameter:"));
}
