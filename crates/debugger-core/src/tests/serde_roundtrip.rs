//! Round-trip every serializable neutral type through `serde_json` and assert the
//! value survives. This pins the serde derives the seam contract depends on.

use crate::*;

fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, &back, "round-trip mismatch via {json}");
}

fn sample_stop_info() -> StopInfo {
    StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: "stopped at breakpoint 1".to_string(),
        hit_breakpoint_ids: vec![1, 7],
    }
}

#[test]
fn enums_roundtrip() {
    roundtrip(&Granularity::Line);
    roundtrip(&Granularity::Instruction);
    roundtrip(&EvalMode::Expression);
    roundtrip(&EvalMode::Repl);
    roundtrip(&StepKind::Over);
    roundtrip(&StepKind::Into);
    roundtrip(&StepKind::Out);
}

#[test]
fn stop_outcomes_roundtrip() {
    roundtrip(&StopOutcome::Stopped(sample_stop_info()));
    roundtrip(&StopOutcome::Exited { code: Some(0) });
    roundtrip(&StopOutcome::Exited { code: None });
    roundtrip(&StopOutcome::Terminated);
}

#[test]
fn launch_outcomes_roundtrip() {
    roundtrip(&LaunchOutcome::Stopped(sample_stop_info()));
    roundtrip(&LaunchOutcome::Running);
    roundtrip(&LaunchOutcome::Exited { code: Some(42) });
    roundtrip(&LaunchOutcome::Exited { code: None });
}

#[test]
fn attach_outcomes_roundtrip() {
    roundtrip(&AttachOutcome::Stopped(sample_stop_info()));
    roundtrip(&AttachOutcome::Exited { code: Some(1) });
    roundtrip(&AttachOutcome::Exited { code: None });
    roundtrip(&AttachOutcome::Terminated);
}

#[test]
fn specs_roundtrip() {
    roundtrip(&LaunchSpec {
        program: "/bin/ls".to_string(),
        args: vec!["-l".to_string(), "/tmp".to_string()],
        cwd: Some("/work".to_string()),
        env: vec![("KEY".to_string(), "value".to_string())],
        stop_on_entry: true,
        source_breakpoints: vec![(
            "main.c".to_string(),
            vec![SourceBp {
                line: 10,
                condition: "i > 3".to_string(),
            }],
        )],
        function_breakpoints: vec![FunctionBp {
            name: "main".to_string(),
            condition: String::new(),
        }],
    });
    roundtrip(&AttachSpec {
        pid: Some(4321),
        wait_for: None,
    });
    roundtrip(&AttachSpec {
        pid: None,
        wait_for: Some("target".to_string()),
    });
}

#[test]
fn inspection_types_roundtrip() {
    roundtrip(&Frame {
        index: 0,
        id: 1000,
        name: "main".to_string(),
        source_path: Some("loop.c".to_string()),
        line: 6,
        instruction_pointer: Some("0x100003f50".to_string()),
    });
    roundtrip(&Frame {
        index: 1,
        id: 1001,
        name: "<unknown>".to_string(),
        source_path: None,
        line: 0,
        instruction_pointer: None,
    });
    roundtrip(&ThreadInfo {
        id: 1,
        name: "main thread".to_string(),
    });
    roundtrip(&Scope {
        name: "Locals".to_string(),
        variables_reference: 7,
    });
    roundtrip(&Variable {
        name: "sum".to_string(),
        value: "45".to_string(),
        ty: "int".to_string(),
        variables_reference: 0,
        named: 0,
        indexed: 0,
    });
    roundtrip(&BreakpointResult {
        id: 1,
        verified: true,
        line: 6,
        message: String::new(),
    });
    roundtrip(&EvalResult {
        result: "46".to_string(),
        ty: "int".to_string(),
        variables_reference: 0,
    });
    roundtrip(&MemoryRead {
        address: "0x1000".to_string(),
        data: vec![0xde, 0xad, 0xbe, 0xef],
    });
    roundtrip(&Instruction {
        address: "0x100003f50".to_string(),
        instruction: "mov eax, 1".to_string(),
        bytes: "b8 01 00 00 00".to_string(),
        symbol: "main".to_string(),
        source_path: Some("loop.c".to_string()),
        line: 6,
    });
}

#[test]
fn backend_events_roundtrip() {
    roundtrip(&BackendEvent::Output {
        category: "stdout".to_string(),
        text: "hello from simple\n".to_string(),
    });
    roundtrip(&BackendEvent::Terminated { code: Some(0) });
    roundtrip(&BackendEvent::Terminated { code: None });
}
