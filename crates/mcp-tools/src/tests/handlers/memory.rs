//! Memory + output + run_command handler tests (Spec FR-13/FR-12/FR-14).

use debugger_core::{EvalMode, EvalResult, Frame, Instruction, MemoryRead};
use mcp_session::State;
use serde_json::json;

use crate::tests::fake::Call;
use crate::tests::handlers::support::{args, expect_error, expect_json, Harness};

// ---- read_memory ----

#[tokio::test]
async fn read_memory_missing_params() {
    let h = Harness::connected(State::Stopped).await;
    let a = args(&[("count", json!(4))]);
    assert!(
        expect_error(&h.server.handle_read_memory(&crate::Args::new(&a)).await)
            .starts_with("missing required parameter:")
    );

    let a = args(&[("address", json!("0x1000"))]);
    assert!(
        expect_error(&h.server.handle_read_memory(&crate::Args::new(&a)).await)
            .starts_with("missing required parameter:")
    );
}

#[tokio::test]
async fn read_memory_rejects_non_positive_count() {
    // Rust numeric-validation policy: zero/negative `count` is rejected at the boundary and
    // never forwarded to the backend.
    let h = Harness::connected(State::Stopped).await;
    for bad in [json!(0), json!(-4), json!(-0.5)] {
        let a = args(&[("address", json!("0x1000")), ("count", bad)]);
        let out = h.server.handle_read_memory(&crate::Args::new(&a)).await;
        assert_eq!(expect_error(&out), "'count' must be a positive integer");
    }
    // No ReadMemory call was made.
    assert!(!h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::ReadMemory { .. })));
}

#[tokio::test]
async fn read_memory_forwards_large_positive_count() {
    // A large positive count is still forwarded unchanged (only clearly-invalid values are
    // rejected — no caps).
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().read_memory_result = Some(Ok(MemoryRead {
        address: "0x1000".to_string(),
        data: Vec::new(),
    }));
    let a = args(&[("address", json!("0x1000")), ("count", json!(1_000_000))]);
    let _ = h.server.handle_read_memory(&crate::Args::new(&a)).await;
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::ReadMemory {
            count: 1_000_000,
            ..
        }
    )));
}

#[tokio::test]
async fn read_memory_normalizes_address_and_formats_hex_dump() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().read_memory_result = Some(Ok(MemoryRead {
        address: "0x1000".to_string(),
        data: vec![0x48, 0x65, 0x6c, 0x6c, 0x6f],
    }));
    // Pass the address WITHOUT 0x — the handler must normalize to 0x1000.
    let a = args(&[("address", json!("1000")), ("count", json!(5))]);
    let out = h.server.handle_read_memory(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["address"], json!("0x1000"));
    assert_eq!(v["bytes_read"], json!(5));
    let dump = v["hex_dump"].as_str().unwrap();
    assert!(dump.starts_with("0x00001000: 48 65 6c 6c 6f"));
    assert!(dump.contains("|Hello"));
    // The backend received the normalized 0x1000.
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::ReadMemory { address, .. } if address == "0x1000")));
}

#[tokio::test]
async fn read_memory_empty_data_omits_hex_dump() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().read_memory_result = Some(Ok(MemoryRead {
        address: "0x2000".to_string(),
        data: Vec::new(),
    }));
    let a = args(&[("address", json!("0x2000")), ("count", json!(8))]);
    let out = h.server.handle_read_memory(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["address"], json!("0x2000"));
    assert_eq!(v["bytes_read"], json!(0));
    assert!(v.get("hex_dump").is_none());
}

// ---- disassemble ----

#[tokio::test]
async fn disassemble_default_count_is_20() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().disassemble_result = Some(Ok(Vec::new()));
    let a = args(&[("address", json!("0xdead"))]);
    let _ = h.server.handle_disassemble(&crate::Args::new(&a)).await;
    // Spec OQ-1 — default is 20 (not Go's code value of 10).
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Disassemble { count: 20, .. })));
}

#[tokio::test]
async fn disassemble_explicit_count_overrides() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().disassemble_result = Some(Ok(Vec::new()));
    let a = args(&[
        ("address", json!("0xdead")),
        ("instruction_count", json!(3)),
    ]);
    let _ = h.server.handle_disassemble(&crate::Args::new(&a)).await;
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::Disassemble { count: 3, .. })));
}

#[tokio::test]
async fn disassemble_current_pc_path_marks_is_current_pc() {
    let h = Harness::connected(State::Stopped).await;
    // No address → resolve current PC via stackTrace(levels=1).
    h.state.lock().unwrap().stack_trace_result = Some(Ok((
        vec![Frame {
            index: 0,
            id: 100,
            name: "main".to_string(),
            source_path: None,
            line: 0,
            instruction_pointer: Some("0xdead".to_string()),
        }],
        1,
    )));
    h.state.lock().unwrap().disassemble_result = Some(Ok(vec![
        Instruction {
            address: "0xdead".to_string(),
            instruction: "movq %rsp, %rdi".to_string(),
            bytes: "48 89 e7".to_string(),
            symbol: "_start".to_string(),
            source_path: Some("/start.s".to_string()),
            line: 3,
        },
        Instruction {
            address: "0xdeb0".to_string(),
            instruction: "callq 0x100".to_string(),
            bytes: String::new(),
            symbol: String::new(),
            source_path: None,
            line: 0,
        },
    ]));
    let empty = args(&[]);
    let out = h.server.handle_disassemble(&crate::Args::new(&empty)).await;
    let v = expect_json(&out);
    assert_eq!(v["start_address"], json!("0xdead"));
    assert_eq!(v["count"], json!(2));
    let insts = v["instructions"].as_array().unwrap();
    assert_eq!(insts[0]["address"], json!("0xdead"));
    assert_eq!(insts[0]["bytes"], json!("48 89 e7"));
    assert_eq!(insts[0]["symbol"], json!("_start"));
    assert_eq!(insts[0]["file"], json!("/start.s"));
    assert_eq!(insts[0]["line"], json!(3));
    assert_eq!(insts[0]["is_current_pc"], json!(true));
    // Second instruction: no bytes/symbol/file, not current PC.
    assert!(insts[1].get("bytes").is_none());
    assert!(insts[1].get("symbol").is_none());
    assert!(insts[1].get("file").is_none());
    assert!(insts[1].get("is_current_pc").is_none());
    // stackTrace levels=1 for the current-PC path.
    assert!(h
        .calls()
        .iter()
        .any(|c| matches!(c, Call::StackTrace { levels: 1, .. })));
}

#[tokio::test]
async fn disassemble_no_ip_errors() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().stack_trace_result = Some(Ok((
        vec![Frame {
            index: 0,
            id: 100,
            name: "main".to_string(),
            source_path: None,
            line: 0,
            instruction_pointer: None,
        }],
        1,
    )));
    let empty = args(&[]);
    let out = h.server.handle_disassemble(&crate::Args::new(&empty)).await;
    assert_eq!(
        expect_error(&out),
        "no instruction pointer available for current frame"
    );
}

// ---- read_output ----

#[tokio::test]
async fn read_output_empty_buffer() {
    let h = Harness::new();
    h.set_state(State::Stopped);
    let out = h.server.handle_read_output();
    let v = expect_json(&out);
    assert_eq!(v["output"], json!(""));
    assert_eq!(v["count"], json!(0));
}

#[tokio::test]
async fn read_output_groups_and_drains() {
    let h = Harness::new();
    h.set_state(State::Stopped);
    h.session.output_buffer().append("stdout", "out\n");
    h.session.output_buffer().append("console", "info\n");
    let out = h.server.handle_read_output();
    let v = expect_json(&out);
    assert_eq!(v["count"], json!(2));
    assert_eq!(v["stdout"], json!("out\n"));
    assert_eq!(v["console"], json!("info\n"));
    assert!(v.get("stderr").is_none());
    assert!(v.get("output").is_none());
    // Drain is idempotent.
    let v2 = expect_json(&h.server.handle_read_output()).clone();
    assert_eq!(v2["count"], json!(0));
}

// ---- run_command ----

#[tokio::test]
async fn run_command_missing_command() {
    let h = Harness::connected(State::Stopped).await;
    let empty = args(&[]);
    let out = h.server.handle_run_command(&crate::Args::new(&empty)).await;
    assert!(expect_error(&out).starts_with("missing required parameter:"));
}

#[tokio::test]
async fn run_command_repl_mode_no_has_children() {
    let h = Harness::connected(State::Stopped).await;
    h.state.lock().unwrap().evaluate_result = Some(Ok(EvalResult {
        result: "rax=0".to_string(),
        ty: String::new(),
        // A children ref must be DISCARDED (no has_children key).
        variables_reference: 9,
    }));
    let a = args(&[("command", json!("register read"))]);
    let out = h.server.handle_run_command(&crate::Args::new(&a)).await;
    let v = expect_json(&out);
    assert_eq!(v["result"], json!("rax=0"));
    assert_eq!(v["type"], json!(""));
    assert!(v.get("has_children").is_none());
    assert!(v.get("variables_reference").is_none());
    // Sent via EvalMode::Repl with NO frame id (the backend owns the backtick decision).
    assert!(h.calls().iter().any(|c| matches!(
        c,
        Call::Evaluate {
            frame_id: None,
            mode: EvalMode::Repl,
            ..
        }
    )));
}
