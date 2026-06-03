//! Breakpoint handler tests: pending vs stopped, matched-breakpoint selection, synthesized
//! function messages, removal, id-sorted list (Spec FR-7).

use debugger_core::{BackendError, BreakpointResult};
use mcp_session::{BreakpointInfo, State};
use serde_json::json;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{args, expect_error, expect_json, Harness};

#[tokio::test]
async fn set_breakpoint_pending_mode() {
    let h = Harness::new(); // idle
    let a = args(&[
        ("file", json!("/src/main.c")),
        ("line", json!(42)),
        ("condition", json!("x > 0")),
    ]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("pending"));
    assert_eq!(v["file"], json!("/src/main.c"));
    assert_eq!(v["line"], json!(42));
    assert_eq!(v["condition"], json!("x > 0"));
    assert_eq!(
        v["message"],
        json!("Breakpoint will be set when program is launched")
    );
    // No DAP sent.
    assert!(h.calls().is_empty());
}

#[tokio::test]
async fn set_breakpoint_missing_file_and_line() {
    let h = Harness::new();
    let a = args(&[("line", json!(10))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    assert!(expect_error(&out).starts_with("missing required parameter:"));

    let h = Harness::new();
    let a = args(&[("file", json!("/f.c"))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    assert!(expect_error(&out).starts_with("missing required parameter:"));
}

#[tokio::test]
async fn set_breakpoint_rejects_non_positive_line() {
    // Rust numeric-validation policy: zero/negative/fractional-to-zero line is rejected at
    // the boundary (checked before the pending/stopped split, so no DAP either way).
    let h = Harness::connected(State::Stopped).await;
    for bad in [json!(0), json!(-3), json!(0.4)] {
        let a = args(&[("file", json!("/f.c")), ("line", bad)]);
        let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
        assert_eq!(expect_error(&out), "'line' must be a positive integer");
    }
    assert!(!h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::SetSourceBreakpoints { .. })));
}

#[tokio::test]
async fn set_breakpoint_accepts_fractional_line_truncating() {
    // A valid fractional line truncates toward zero (Go parity preserved): 4.7 → 4.
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Ok(vec![BreakpointResult {
        id: 1,
        verified: true,
        line: 4,
        message: String::new(),
    }]));
    let a = args(&[("file", json!("/f.c")), ("line", json!(4.7))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["line"], json!(4));
}

#[tokio::test]
async fn set_breakpoint_stopped_selects_exact_line_match() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Ok(vec![
        BreakpointResult {
            id: 10,
            verified: true,
            line: 6,
            message: String::new(),
        },
        BreakpointResult {
            id: 11,
            verified: false,
            line: 9,
            message: "pending".to_string(),
        },
    ]));
    let a = args(&[("file", json!("/loop.c")), ("line", json!(9))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    // Exact line-9 match → id 11.
    assert_eq!(v["breakpoint_id"], json!(11));
    assert_eq!(v["verified"], json!(false));
    assert_eq!(v["file"], json!("/loop.c"));
    assert_eq!(v["line"], json!(9));
    assert_eq!(v["message"], json!("pending"));
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::SetSourceBreakpoints { file, .. } if file == "/loop.c")));
}

#[tokio::test]
async fn set_breakpoint_stopped_falls_back_to_last_when_no_line_match() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Ok(vec![
        BreakpointResult {
            id: 1,
            verified: true,
            line: 100,
            message: String::new(),
        },
        BreakpointResult {
            id: 2,
            verified: true,
            line: 200,
            message: String::new(),
        },
    ]));
    let a = args(&[("file", json!("/f.c")), ("line", json!(7))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    // No line-7 match → last (id 2).
    assert_eq!(v["breakpoint_id"], json!(2));
    assert_eq!(v["line"], json!(200));
    // Empty message omitted.
    assert!(v.get("message").is_none());
}

#[tokio::test]
async fn set_breakpoint_stopped_no_breakpoints_errors() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Ok(Vec::new()));
    let a = args(&[("file", json!("/f.c")), ("line", json!(7))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    assert_eq!(
        expect_error(&out),
        "setBreakpoints response contained no breakpoints"
    );
}

#[tokio::test]
async fn set_breakpoint_dap_failure_leaves_session_unchanged() {
    // Review finding 4 (transactional): a backend rejection must NOT add the breakpoint to
    // the tracked source list and must NOT record a bp_response.
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Err(BackendError::Dap {
        message: "denied".to_string(),
    }));
    let a = args(&[("file", json!("/loop.c")), ("line", json!(9))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    assert!(out.is_error());
    // The proposed breakpoint was not committed: the file's tracked list is empty and no
    // bp_response entry exists.
    assert!(h.session.source_breakpoints_for_file("/loop.c").is_empty());
    assert!(h.session.list_breakpoints().is_empty());
}

#[tokio::test]
async fn set_function_breakpoint_dap_failure_leaves_session_unchanged() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().function_bp_result = Some(Err(BackendError::Dap {
        message: "denied".to_string(),
    }));
    let a = args(&[("name", json!("foo"))]);
    let out = h
        .server
        .handle_set_function_breakpoint(&crate::Args::new(&a))
        .await;
    assert!(out.is_error());
    assert!(h.session.all_function_breakpoints().is_empty());
    assert!(h.session.list_breakpoints().is_empty());
}

#[tokio::test]
async fn set_breakpoint_no_breakpoints_response_leaves_session_unchanged() {
    // The no-breakpoints-in-response error must also leave the tracked list unchanged (the
    // commit happens only after a breakpoint is matched).
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().source_bp_result = Some(Ok(Vec::new()));
    let a = args(&[("file", json!("/f.c")), ("line", json!(7))]);
    let out = h.server.handle_set_breakpoint(&crate::Args::new(&a)).await;
    assert_eq!(
        expect_error(&out),
        "setBreakpoints response contained no breakpoints"
    );
    assert!(h.session.source_breakpoints_for_file("/f.c").is_empty());
}

#[tokio::test]
async fn set_function_breakpoint_pending_mode() {
    let h = Harness::new();
    let a = args(&[("name", json!("main"))]);
    let out = h
        .server
        .handle_set_function_breakpoint(&crate::Args::new(&a))
        .await;
    let v = expect_json(&out);
    assert_eq!(v["status"], json!("pending"));
    assert_eq!(v["function"], json!("main"));
    assert_eq!(
        v["message"],
        json!("Function breakpoint will be set when program is launched")
    );
}

#[tokio::test]
async fn set_function_breakpoint_stopped_synthesizes_message_when_verified() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().function_bp_result = Some(Ok(vec![BreakpointResult {
        id: 5,
        verified: true,
        line: 0,
        message: String::new(),
    }]));
    let a = args(&[("name", json!("foo"))]);
    let out = h
        .server
        .handle_set_function_breakpoint(&crate::Args::new(&a))
        .await;
    let v = expect_json(&out);
    assert_eq!(v["breakpoint_id"], json!(5));
    assert_eq!(v["verified"], json!(true));
    assert_eq!(v["function"], json!("foo"));
    assert_eq!(v["message"], json!("Breakpoint set on function 'foo'"));
}

#[tokio::test]
async fn set_function_breakpoint_stopped_synthesizes_unverified_message() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().function_bp_result = Some(Ok(vec![BreakpointResult {
        id: 6,
        verified: false,
        line: 0,
        message: String::new(),
    }]));
    let a = args(&[("name", json!("bar"))]);
    let out = h
        .server
        .handle_set_function_breakpoint(&crate::Args::new(&a))
        .await;
    let v = expect_json(&out);
    assert_eq!(
        v["message"],
        json!("Breakpoint on function 'bar' pending verification")
    );
}

#[tokio::test]
async fn set_function_breakpoint_keeps_nonempty_message() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().function_bp_result = Some(Ok(vec![BreakpointResult {
        id: 7,
        verified: true,
        line: 0,
        message: "resolved at 0x1000".to_string(),
    }]));
    let a = args(&[("name", json!("baz"))]);
    let out = h
        .server
        .handle_set_function_breakpoint(&crate::Args::new(&a))
        .await;
    let v = expect_json(&out);
    assert_eq!(v["message"], json!("resolved at 0x1000"));
}

#[tokio::test]
async fn remove_breakpoint_unknown_id_errors() {
    let h = Harness::connected(State::Stopped).await;
    let a = args(&[("breakpoint_id", json!(99))]);
    let out = h
        .server
        .handle_remove_breakpoint(&crate::Args::new(&a))
        .await;
    assert_eq!(
        expect_error(&out),
        "failed to remove breakpoint: breakpoint ID 99 not found"
    );
}

#[tokio::test]
async fn remove_source_breakpoint_resends_remaining() {
    let h = Harness::connected(State::Stopped).await;
    // Track a source breakpoint via the session.
    h.session.add_source_breakpoint("/f.c", 6, "");
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 10,
        ty: "source".to_string(),
        file: "/f.c".to_string(),
        line: 6,
        function: String::new(),
        condition: String::new(),
        verified: true,
    });
    let a = args(&[("breakpoint_id", json!(10))]);
    let out = h
        .server
        .handle_remove_breakpoint(&crate::Args::new(&a))
        .await;
    let v = expect_json(&out);
    assert_eq!(v["removed"], json!(true));
    assert_eq!(v["breakpoint_id"], json!(10));
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::SetSourceBreakpoints { file, .. } if file == "/f.c")));
}

#[tokio::test]
async fn remove_function_breakpoint_resends_function_list() {
    let h = Harness::connected(State::Stopped).await;
    h.session.add_function_breakpoint("main", "");
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 5,
        ty: "function".to_string(),
        file: String::new(),
        line: 0,
        function: "main".to_string(),
        condition: String::new(),
        verified: true,
    });
    let a = args(&[("breakpoint_id", json!(5))]);
    let out = h
        .server
        .handle_remove_breakpoint(&crate::Args::new(&a))
        .await;
    assert!(!out.is_error());
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::SetFunctionBreakpoints { .. })));
}

#[tokio::test]
async fn remove_source_breakpoint_dap_failure_keeps_it_tracked() {
    // Review finding 4 (transactional remove): a backend rejection of the proposed
    // remaining list must leave the breakpoint tracked (id still removable later).
    let h = Harness::connected(State::Stopped).await;
    h.session.add_source_breakpoint("/f.c", 6, "");
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 10,
        ty: "source".to_string(),
        file: "/f.c".to_string(),
        line: 6,
        function: String::new(),
        condition: String::new(),
        verified: true,
    });
    h.state.lock().unwrap().source_bp_result = Some(Err(BackendError::Dap {
        message: "denied".to_string(),
    }));

    let a = args(&[("breakpoint_id", json!(10))]);
    let out = h
        .server
        .handle_remove_breakpoint(&crate::Args::new(&a))
        .await;
    assert!(out.is_error());
    // Still tracked: the response list and the active source list are unchanged.
    assert_eq!(h.session.list_breakpoints().len(), 1);
    assert_eq!(h.session.source_breakpoints_for_file("/f.c").len(), 1);
    assert!(h.session.breakpoint_info(10).is_some());
}

#[tokio::test]
async fn remove_function_breakpoint_dap_failure_keeps_it_tracked() {
    let h = Harness::connected(State::Stopped).await;
    h.session.add_function_breakpoint("main", "");
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 5,
        ty: "function".to_string(),
        file: String::new(),
        line: 0,
        function: "main".to_string(),
        condition: String::new(),
        verified: true,
    });
    h.state.lock().unwrap().function_bp_result = Some(Err(BackendError::Dap {
        message: "denied".to_string(),
    }));

    let a = args(&[("breakpoint_id", json!(5))]);
    let out = h
        .server
        .handle_remove_breakpoint(&crate::Args::new(&a))
        .await;
    assert!(out.is_error());
    assert_eq!(h.session.list_breakpoints().len(), 1);
    assert_eq!(h.session.all_function_breakpoints().len(), 1);
    assert!(h.session.breakpoint_info(5).is_some());
}

#[tokio::test]
async fn list_breakpoints_empty_is_array_not_null() {
    let h = Harness::new();
    let out = h.server.handle_list_breakpoints();
    let v = expect_json(&out);
    assert_eq!(v["breakpoints"], json!([]));
    assert_eq!(v["count"], json!(0));
}

#[tokio::test]
async fn list_breakpoints_id_sorted_with_conditional_fields() {
    let h = Harness::new();
    // Insert out of id order; conditional fields per type.
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 3,
        ty: "function".to_string(),
        file: String::new(),
        line: 0,
        function: "main".to_string(),
        condition: String::new(),
        verified: true,
    });
    h.session.add_breakpoint_response(BreakpointInfo {
        id: 1,
        ty: "source".to_string(),
        file: "/f.c".to_string(),
        line: 6,
        function: String::new(),
        condition: "i>0".to_string(),
        verified: false,
    });
    let out = h.server.handle_list_breakpoints();
    let v = expect_json(&out);
    let list = v["breakpoints"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    // Sorted ascending by id: 1 then 3.
    assert_eq!(list[0]["id"], json!(1));
    assert_eq!(list[0]["type"], json!("source"));
    assert_eq!(list[0]["file"], json!("/f.c"));
    assert_eq!(list[0]["line"], json!(6));
    assert_eq!(list[0]["condition"], json!("i>0"));
    assert!(list[0].get("function").is_none());
    assert_eq!(list[1]["id"], json!(3));
    assert_eq!(list[1]["function"], json!("main"));
    // function entry has no file/line/condition.
    assert!(list[1].get("file").is_none());
    assert!(list[1].get("line").is_none());
    assert!(list[1].get("condition").is_none());
    assert_eq!(v["count"], json!(2));
}
