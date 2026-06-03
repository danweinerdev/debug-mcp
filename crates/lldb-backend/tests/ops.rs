//! Op-method tests over a scripted DAP peer (task 3.4): each op issues the correct DAP
//! request and maps the response to neutral types. Mirrors the DAP-issuing parts of the
//! Go `internal/tools/{breakpoints,execution,inspection,memory,run_command}.go`. No
//! response *formatting* is asserted here — that is Phase 5.

mod common;

use common::Harness;
use debugger_core::{
    DebuggerBackend, EvalMode, FunctionBp, Granularity, SourceBp, StepKind, StopOutcome,
};
use serde_json::Value;

#[tokio::test]
async fn set_source_breakpoints_request_and_mapping() {
    let (backend, mut h) = Harness::new(true);
    let bps = vec![
        SourceBp {
            line: 6,
            condition: String::new(),
        },
        SourceBp {
            line: 9,
            condition: "x>0".to_string(),
        },
    ];
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "setBreakpoints");
        assert_eq!(args["source"]["path"], Value::String("/f.c".into()));
        assert_eq!(args["breakpoints"][0]["line"], Value::from(6));
        assert!(args["breakpoints"][0].get("condition").is_none());
        assert_eq!(
            args["breakpoints"][1]["condition"],
            Value::String("x>0".into())
        );
        h.reply_ok(
            "setBreakpoints",
            seq,
            Some(serde_json::json!({"breakpoints": [
                {"id": 10, "line": 6, "verified": true, "message": ""},
                {"id": 11, "line": 9, "verified": false, "message": "pending"}
            ]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.set_source_breakpoints("/f.c", &bps), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, 10);
    assert!(got[0].verified);
    assert_eq!(got[0].line, 6);
    assert_eq!(got[1].id, 11);
    assert!(!got[1].verified);
    assert_eq!(got[1].message, "pending");
    h.close_and_join().await;
}

#[tokio::test]
async fn set_function_breakpoints_request_and_mapping() {
    let (backend, mut h) = Harness::new(true);
    let bps = vec![FunctionBp {
        name: "main".to_string(),
        condition: String::new(),
    }];
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "setFunctionBreakpoints");
        assert_eq!(args["breakpoints"][0]["name"], Value::String("main".into()));
        h.reply_ok(
            "setFunctionBreakpoints",
            seq,
            Some(serde_json::json!({"breakpoints": [{"id": 5, "verified": true}]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.set_function_breakpoints(&bps), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, 5);
    assert!(got[0].verified);
    h.close_and_join().await;
}

#[tokio::test]
async fn breakpoint_failure_maps_to_dap_error() {
    let (backend, mut h) = Harness::new(true);
    let bps = vec![SourceBp {
        line: 1,
        condition: String::new(),
    }];
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await;
        h.reply_err("setBreakpoints", seq, "bad file").await;
        h
    };
    let (result, h) = tokio::join!(backend.set_source_breakpoints("/f.c", &bps), run);
    let err = result.expect_err("fail");
    assert!(
        format!("{err:?}").contains("setBreakpoints failed: bad file"),
        "got {err:?}"
    );
    h.close_and_join().await;
}

#[tokio::test]
async fn cont_registers_waiter_before_send_and_returns_stop() {
    // The waiter is registered before the continue request is sent, so a stop delivered
    // right after the request is captured (Spec FR-8.3). No race (unlike attach).
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "continue");
        assert_eq!(args["threadId"], Value::from(3));
        h.reply_ok("continue", seq, None).await;
        h.inject_stopped("breakpoint", 3).await;
        h
    };
    let (result, h) = tokio::join!(backend.cont(3), run);
    match result.expect("ok") {
        StopOutcome::Stopped(info) => {
            assert_eq!(info.reason, "breakpoint");
            assert_eq!(info.thread_id, 3);
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn cont_exit_maps_to_exited() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await;
        h.reply_ok("continue", seq, None).await;
        h.inject_value(&serde_json::json!({
            "seq": 0, "type": "event", "event": "exited", "body": {"exitCode": 0}
        }))
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.cont(1), run);
    assert_eq!(result.expect("ok"), StopOutcome::Exited { code: Some(0) });
    h.close_and_join().await;
}

#[tokio::test]
async fn step_over_uses_next_with_granularity() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "next", "step_over → DAP `next`");
        assert_eq!(args["threadId"], Value::from(1));
        assert_eq!(args["granularity"], Value::String("instruction".into()));
        h.reply_ok("next", seq, None).await;
        h.inject_stopped("step", 1).await;
        h
    };
    let (result, h) = tokio::join!(
        backend.step(StepKind::Over, 1, Some(Granularity::Instruction)),
        run
    );
    assert!(matches!(result, Ok(StopOutcome::Stopped(_))));
    h.close_and_join().await;
}

#[tokio::test]
async fn step_into_uses_step_in_with_line_granularity() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "stepIn");
        assert_eq!(args["granularity"], Value::String("line".into()));
        h.reply_ok("stepIn", seq, None).await;
        h.inject_stopped("step", 1).await;
        h
    };
    let (result, h) = tokio::join!(
        backend.step(StepKind::Into, 1, Some(Granularity::Line)),
        run
    );
    assert!(matches!(result, Ok(StopOutcome::Stopped(_))));
    h.close_and_join().await;
}

#[tokio::test]
async fn step_out_uses_step_out_without_granularity() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "stepOut");
        assert!(
            args.get("granularity").is_none(),
            "step_out never carries granularity"
        );
        h.reply_ok("stepOut", seq, None).await;
        h.inject_stopped("step", 1).await;
        h
    };
    // Even if a granularity is passed, step_out drops it (Go has no granularity param).
    let (result, h) = tokio::join!(
        backend.step(StepKind::Out, 1, Some(Granularity::Instruction)),
        run
    );
    assert!(matches!(result, Ok(StopOutcome::Stopped(_))));
    h.close_and_join().await;
}

#[tokio::test]
async fn pause_sends_thread_zero_and_returns_immediately() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "pause");
        assert_eq!(args["threadId"], Value::from(0), "pause all threads (id 0)");
        h.reply_ok("pause", seq, None).await;
        h
    };
    let (result, h) = tokio::join!(backend.pause(), run);
    assert!(result.is_ok(), "pause returns immediately, no stop wait");
    h.close_and_join().await;
}

#[tokio::test]
async fn threads_mapping() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, _a) = h.next_request_full().await;
        assert_eq!(cmd, "threads");
        h.reply_ok(
            "threads",
            seq,
            Some(serde_json::json!({"threads": [
                {"id": 1, "name": "main"},
                {"id": 2, "name": "worker"}
            ]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.threads(), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].id, 1);
    assert_eq!(got[0].name, "main");
    assert_eq!(got[1].name, "worker");
    h.close_and_join().await;
}

#[tokio::test]
async fn stack_trace_request_and_mapping() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "stackTrace");
        assert_eq!(args["threadId"], Value::from(2));
        assert_eq!(args["startFrame"], Value::from(0));
        assert_eq!(args["levels"], Value::from(20));
        h.reply_ok(
            "stackTrace",
            seq,
            Some(serde_json::json!({
                "stackFrames": [
                    {"id": 100, "name": "main", "line": 6,
                     "source": {"path": "/loop.c"},
                     "instructionPointerReference": "0xdead"},
                    {"id": 101, "name": "_start", "line": 0}
                ],
                "totalFrames": 2
            })),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.stack_trace(2, 0, 20), run);
    let (frames, total) = result.expect("ok");
    assert_eq!(total, 2);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].index, 0);
    assert_eq!(frames[0].id, 100);
    assert_eq!(frames[0].name, "main");
    assert_eq!(frames[0].source_path.as_deref(), Some("/loop.c"));
    assert_eq!(frames[0].line, 6);
    assert_eq!(frames[0].instruction_pointer.as_deref(), Some("0xdead"));
    // Second frame: no source, no IP ⇒ both None.
    assert_eq!(frames[1].index, 1);
    assert_eq!(frames[1].source_path, None);
    assert_eq!(frames[1].instruction_pointer, None);
    h.close_and_join().await;
}

#[tokio::test]
async fn scopes_request_and_mapping() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "scopes");
        assert_eq!(args["frameId"], Value::from(100));
        h.reply_ok(
            "scopes",
            seq,
            Some(serde_json::json!({"scopes": [
                {"name": "Locals", "variablesReference": 1},
                {"name": "Registers", "variablesReference": 2}
            ]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.scopes(100), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "Locals");
    assert_eq!(got[0].variables_reference, 1);
    assert_eq!(got[1].variables_reference, 2);
    h.close_and_join().await;
}

#[tokio::test]
async fn variables_request_and_mapping() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "variables");
        assert_eq!(args["variablesReference"], Value::from(1));
        h.reply_ok(
            "variables",
            seq,
            Some(serde_json::json!({"variables": [
                {"name": "i", "value": "0", "type": "int",
                 "variablesReference": 0, "namedVariables": 0, "indexedVariables": 0},
                {"name": "p", "value": "0x1", "type": "Point *",
                 "variablesReference": 7, "namedVariables": 2, "indexedVariables": 0}
            ]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.variables(1), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].name, "i");
    assert_eq!(got[0].value, "0");
    assert_eq!(got[0].ty, "int");
    assert_eq!(got[0].variables_reference, 0);
    assert_eq!(got[1].variables_reference, 7);
    assert_eq!(got[1].named, 2);
    assert_eq!(got[1].indexed, 0);
    h.close_and_join().await;
}

#[tokio::test]
async fn evaluate_expression_uses_variables_context_with_frame() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "evaluate");
        assert_eq!(args["context"], Value::String("variables".into()));
        assert_eq!(args["frameId"], Value::from(100));
        assert_eq!(args["expression"], Value::String("i + 1".into()));
        h.reply_ok(
            "evaluate",
            seq,
            Some(serde_json::json!({"result": "1", "type": "int", "variablesReference": 0})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(
        backend.evaluate("i + 1", Some(100), EvalMode::Expression),
        run
    );
    let got = result.expect("ok");
    assert_eq!(got.result, "1");
    assert_eq!(got.ty, "int");
    assert_eq!(got.variables_reference, 0);
    h.close_and_join().await;
}

#[tokio::test]
async fn evaluate_repl_capable_no_backtick_no_frame() {
    // Capable backend (is_lldb_dap=true): repl context, NO backtick, NO frame id.
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "evaluate");
        assert_eq!(args["context"], Value::String("repl".into()));
        assert_eq!(args["expression"], Value::String("register read".into()));
        assert!(args.get("frameId").is_none(), "repl sends no frame id");
        h.reply_ok(
            "evaluate",
            seq,
            Some(serde_json::json!({"result": "rax=0", "type": ""})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.evaluate("register read", None, EvalMode::Repl), run);
    assert_eq!(result.expect("ok").result, "rax=0");
    h.close_and_join().await;
}

#[tokio::test]
async fn evaluate_repl_not_capable_prepends_backtick() {
    // Not capable (legacy lldb-vscode): the command is backtick-prefixed (Spec FR-14.2).
    let (backend, mut h) = Harness::new(false);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "evaluate");
        assert_eq!(args["context"], Value::String("repl".into()));
        assert_eq!(
            args["expression"],
            Value::String("`register read".into()),
            "backtick prepended when !supports_command_repl_mode"
        );
        h.reply_ok(
            "evaluate",
            seq,
            Some(serde_json::json!({"result": "ok", "type": ""})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.evaluate("register read", None, EvalMode::Repl), run);
    assert!(result.is_ok());
    h.close_and_join().await;
}

#[tokio::test]
async fn read_memory_base64_decodes_to_bytes() {
    // The base64 `data` is decoded here; the echoed `address` passes through.
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "readMemory");
        assert_eq!(args["memoryReference"], Value::String("0x1000".into()));
        assert_eq!(args["count"], Value::from(4));
        // "SInn6A==" = bytes 48 89 e7 e8 (captured from real lldb-dap).
        h.reply_ok(
            "readMemory",
            seq,
            Some(serde_json::json!({"address": "0x1000", "data": "SInn6A=="})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.read_memory("0x1000", 4), run);
    let got = result.expect("ok");
    assert_eq!(got.address, "0x1000");
    assert_eq!(got.data, vec![0x48, 0x89, 0xe7, 0xe8]);
    h.close_and_join().await;
}

#[tokio::test]
async fn read_memory_empty_data_is_empty_bytes() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (_c, seq, _a) = h.next_request_full().await;
        h.reply_ok(
            "readMemory",
            seq,
            Some(serde_json::json!({"address": "0x2000", "data": ""})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.read_memory("0x2000", 8), run);
    let got = result.expect("ok");
    assert_eq!(got.address, "0x2000");
    assert!(got.data.is_empty());
    h.close_and_join().await;
}

#[tokio::test]
async fn disassemble_request_and_raw_field_passthrough() {
    let (backend, mut h) = Harness::new(true);
    let run = async {
        let (cmd, seq, args) = h.next_request_full().await;
        assert_eq!(cmd, "disassemble");
        assert_eq!(args["memoryReference"], Value::String("0xdead".into()));
        assert_eq!(args["instructionCount"], Value::from(2));
        h.reply_ok(
            "disassemble",
            seq,
            Some(serde_json::json!({"instructions": [
                {"address": "0xdead", "instruction": "movq %rsp, %rdi",
                 "instructionBytes": "48 89 e7", "symbol": "_start",
                 "location": {"path": "/start.s"}, "line": 3},
                {"address": "0xdeb0", "instruction": "callq 0x100"}
            ]})),
        )
        .await;
        h
    };
    let (result, h) = tokio::join!(backend.disassemble("0xdead", 2), run);
    let got = result.expect("ok");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].address, "0xdead");
    assert_eq!(got[0].instruction, "movq %rsp, %rdi");
    assert_eq!(got[0].bytes, "48 89 e7");
    assert_eq!(got[0].symbol, "_start");
    assert_eq!(got[0].source_path.as_deref(), Some("/start.s"));
    assert_eq!(got[0].line, 3);
    // Second: no bytes/symbol/location.
    assert_eq!(got[1].address, "0xdeb0");
    assert_eq!(got[1].bytes, "");
    assert_eq!(got[1].symbol, "");
    assert_eq!(got[1].source_path, None);
    h.close_and_join().await;
}

#[tokio::test]
async fn supports_command_repl_mode_reflects_capability() {
    let (capable, h1) = Harness::new(true);
    assert!(capable.supports_command_repl_mode());
    h1.close_and_join().await;

    let (legacy, h2) = Harness::new(false);
    assert!(!legacy.supports_command_repl_mode());
    h2.close_and_join().await;
}

#[tokio::test]
async fn debugger_pid_is_none_without_a_child() {
    // The scripted-peer harness builds the backend with `child: None` (no real
    // subprocess), so there is no OS pid to report. The live integration suite covers the
    // real-child path where `debugger_pid()` returns the lldb-dap subprocess pid.
    let (backend, h) = Harness::new(true);
    assert_eq!(backend.debugger_pid(), None);
    h.close_and_join().await;
}
