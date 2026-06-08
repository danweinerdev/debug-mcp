//! Inspection / memory translation tests over a [`FakeEngine`](super::fake::FakeEngine) — they
//! pin the DbgEng→neutral mapping the task-3.4 backend ops perform, with NO live target. Each
//! drives a `WinDbgBackend` op (the real marshaling path through `call`) against scripted engine
//! data and asserts the neutral result + the command string the engine received.

use std::sync::{Arc, Mutex};

use debugger_core::{DebuggerBackend, EvalMode, Frame, Instruction, Variable};

use crate::engine_ops::EngineOps;

use super::backend_over_fake;
use super::fake::{FakeEngine, Recorder};

/// Build a backend over a `FakeEngine` produced by `customize` (which receives a default fake to
/// tweak) and a shared recorder the test reads back. Returns the backend + the recorder clone.
async fn backend_with(
    customize: impl FnOnce(&mut FakeEngine) + Send + 'static,
) -> (crate::backend::WinDbgBackend, Arc<Mutex<Recorder>>) {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        let mut fake = FakeEngine {
            recorder: recorder_for_fake,
            ..FakeEngine::default()
        };
        customize(&mut fake);
        Ok(Box::new(fake) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");
    (backend, recorder)
}

/// A scripted three-frame engine stack (innermost first), shaped exactly as `Engine::stack_trace`
/// emits: `index == id == frame index`, symbolicated `name`, a source `path`/`line` where one
/// maps, and the IP as the `0x{:016X}` `instruction_pointer`.
fn three_frames() -> Vec<Frame> {
    vec![
        Frame {
            index: 0,
            id: 0,
            name: "compute".to_string(),
            source_path: Some("C:\\src\\test_target.c".to_string()),
            line: 12,
            instruction_pointer: Some("0x00007FF600001000".to_string()),
        },
        Frame {
            index: 1,
            id: 1,
            name: "main".to_string(),
            source_path: Some("C:\\src\\test_target.c".to_string()),
            line: 30,
            instruction_pointer: Some("0x00007FF600002000".to_string()),
        },
        Frame {
            index: 2,
            id: 2,
            name: "kernel32!BaseThreadInitThunk".to_string(),
            source_path: None,
            line: 0,
            instruction_pointer: Some("0x00007FFE12345678".to_string()),
        },
    ]
}

/// `stack_trace` maps the engine's frames straight to neutral `Frame`s and reports `total_frames`
/// as the full count the engine walked.
#[tokio::test]
async fn stack_trace_maps_three_frames() {
    let (backend, recorder) = backend_with(|f| f.frames = three_frames()).await;

    let (frames, total) = backend
        .stack_trace(7, 0, 20)
        .await
        .expect("stack_trace returns frames");

    // start=0 here, so the fetched window IS the full stack — total equals the fetched count (3).
    assert_eq!(
        total, 3,
        "with start=0 the fetched count is the full stack count"
    );
    assert_eq!(frames.len(), 3);
    // Frame 0: compute, with id/index/source/line/address preserved.
    assert_eq!(frames[0].index, 0);
    assert_eq!(frames[0].id, 0);
    assert_eq!(frames[0].name, "compute");
    assert_eq!(
        frames[0].source_path.as_deref(),
        Some("C:\\src\\test_target.c")
    );
    assert_eq!(frames[0].line, 12);
    assert_eq!(
        frames[0].instruction_pointer.as_deref(),
        Some("0x00007FF600001000")
    );
    // Frame 1: main.
    assert_eq!(frames[1].name, "main");
    assert_eq!(frames[1].line, 30);
    // Frame 2: no source line maps (None/0).
    assert_eq!(frames[2].name, "kernel32!BaseThreadInitThunk");
    assert_eq!(frames[2].source_path, None);

    // The engine was asked for the current thread and a `start + levels` fetch bound.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.stack_traces,
        vec![(7, 20)],
        "stack_trace marshals (thread_id, start+levels) to the engine"
    );
}

/// `stack_trace` honors the `start`/`levels` window: `start` drops leading frames, `levels` bounds
/// the engine fetch, and `total_frames` reflects the count FETCHED in this request.
///
/// CAVEAT (what this test does and does NOT prove): the `FakeEngine` returns its canned frame set
/// in full, IGNORING the `max` the backend asks for. So `total == 3` here is only COINCIDENTALLY
/// equal to the true stack depth (the canned set happens to be the whole stack). With `start = 1`
/// the production `total_frames` is `frames.len()` of the FETCHED window (`start + levels`-bounded
/// against the real engine, which DOES honor `max`), NOT the true stack depth. This test therefore
/// pins the `start` slice and the `start + levels` fetch bound the backend requests; it does NOT
/// (and cannot, with a max-ignoring fake) prove the live-engine `start > 0` total-frames semantic
/// documented in `backend.rs::stack_trace`. The live `inspection_at_compute_breakpoint` test
/// exercises the real engine with `start = 0`, the only window any current caller uses.
#[tokio::test]
async fn stack_trace_honors_start_and_levels() {
    // Engine returns all frames up to the requested `max`; the fake returns the canned set
    // regardless, so we assert the backend's `start` slice + the fetch bound it requested.
    let (backend, recorder) = backend_with(|f| f.frames = three_frames()).await;

    let (frames, total) = backend
        .stack_trace(1, 1, 2)
        .await
        .expect("windowed stack_trace");

    // `start = 1` drops the innermost frame; the returned window starts at `main`.
    assert_eq!(frames.len(), 2, "start=1 drops the leading frame");
    assert_eq!(frames[0].name, "main");
    assert_eq!(frames[0].index, 1, "absolute frame index is preserved");
    // `total == 3` here only because the fake ignores `max` and returns all 3 canned frames; see the
    // doc-comment caveat — this is the fetched-window count, not a proof of the true depth.
    assert_eq!(
        total, 3,
        "total_frames is the fetched-window count (fake returns all 3 frames)"
    );

    // The engine fetch bound is `start + levels` (1 + 2 = 3).
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.stack_traces,
        vec![(1, 3)],
        "the engine fetch bound must be start + levels"
    );
}

/// `scopes` synthesizes a single `Locals` scope whose `variables_reference` encodes the frame as
/// `frame_id + 1` (so frame 0 yields a positive, expandable reference).
#[tokio::test]
async fn scopes_yields_locals_with_frame_encoded_reference() {
    let (backend, _rec) = backend_with(|_| {}).await;

    // Frame 0 → reference 1 (the `+1` keeps frame 0 expandable in the flatten).
    let scopes0 = backend.scopes(0).await.expect("scopes(0)");
    assert_eq!(scopes0.len(), 1, "exactly one (Locals) scope");
    assert_eq!(scopes0[0].name, "Locals");
    assert_eq!(
        scopes0[0].variables_reference, 1,
        "frame 0 encodes to reference 1 (frame_id + 1)"
    );

    // Frame 3 → reference 4.
    let scopes3 = backend.scopes(3).await.expect("scopes(3)");
    assert_eq!(scopes3[0].variables_reference, 4);
}

/// `variables` decodes the frame index from the scope reference (`reference - 1`) and maps the
/// engine locals → neutral `Variable`s. The top-level locals carry `named`/`indexed`/reference == 0
/// (the documented WinDbg flat, one-level-deep limitation), so the flatten treats each as a leaf.
#[tokio::test]
async fn variables_decodes_frame_and_maps_locals() {
    let scripted = vec![
        Variable {
            name: "i".to_string(),
            value: "0n5".to_string(),
            ty: "int".to_string(),
            variables_reference: 0,
            named: 0,
            indexed: 0,
        },
        Variable {
            name: "result".to_string(),
            value: "0n45".to_string(),
            ty: "int".to_string(),
            variables_reference: 0,
            named: 0,
            indexed: 0,
        },
    ];
    let scripted_for_fake = scripted.clone();
    let (backend, recorder) = backend_with(move |f| f.locals = scripted_for_fake).await;

    // Reference 1 ⇒ frame 0 (matches `scopes(0)`'s `frame_id + 1`).
    let vars = backend.variables(1).await.expect("variables(1)");
    assert_eq!(vars, scripted, "the engine locals map straight through");
    for v in &vars {
        assert_eq!(
            (v.variables_reference, v.named, v.indexed),
            (0, 0, 0),
            "WinDbg top-level locals are flat: reference/named/indexed are 0 (the flatten then \
             treats each as a leaf — the documented one-level limitation)"
        );
    }

    // The engine `locals` was called with the DECODED frame index 0 (reference 1 - 1).
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.locals_frames,
        vec![0],
        "variables(reference) must decode the frame index as reference - 1"
    );
}

/// A reference of 0 (not one we minted) yields no variables without ever touching the engine.
#[tokio::test]
async fn variables_zero_reference_is_empty() {
    let (backend, recorder) = backend_with(|f| {
        f.locals = vec![Variable {
            name: "x".to_string(),
            value: "1".to_string(),
            ty: "int".to_string(),
            variables_reference: 0,
            named: 0,
            indexed: 0,
        }]
    })
    .await;

    let vars = backend.variables(0).await.expect("variables(0)");
    assert!(vars.is_empty(), "a 0 reference yields no variables");
    assert!(
        recorder.lock().unwrap().locals_frames.is_empty(),
        "a 0 reference must not call the engine"
    );
}

/// `evaluate(Expression)` forms the `?? expr` C++-expression-eval command and returns the engine's
/// rendered output as the `result` (type/var_reference empty/0).
#[tokio::test]
async fn evaluate_expression_forms_double_question_command() {
    let (backend, recorder) = backend_with(|f| f.evaluate_result = "int 0n20".to_string()).await;

    let result = backend
        .evaluate("10 + 10", Some(0), EvalMode::Expression)
        .await
        .expect("evaluate expression");
    assert_eq!(result.result, "int 0n20");
    assert_eq!(result.ty, "", "the `??` text carries the type inline");
    assert_eq!(result.variables_reference, 0);

    // The engine received the expression wrapped as `?? <expr>` (the C++ plugin's eval form), via
    // the Evaluate path — NOT the raw Execute path.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.evaluates,
        vec!["?? 10 + 10".to_string()],
        "Expression mode must form `?? expr`"
    );
    assert!(
        rec.executes.is_empty(),
        "Expression mode must NOT go through the raw Execute path"
    );
}

/// `evaluate(Repl)` (the `run_command` escape hatch) marshals the command VERBATIM through Execute
/// — NO backtick prefix (supports_command_repl_mode() is true) — and returns the raw output.
#[tokio::test]
async fn evaluate_repl_marshals_raw_command_no_backtick() {
    let (backend, recorder) =
        backend_with(|f| f.execute_result = "frame 0: compute\nframe 1: main\n".to_string()).await;

    let result = backend
        .evaluate("k", None, EvalMode::Repl)
        .await
        .expect("evaluate repl");
    assert_eq!(result.result, "frame 0: compute\nframe 1: main\n");
    assert_eq!(result.ty, "");
    assert_eq!(result.variables_reference, 0);

    // The engine received the RAW command `k` (no backtick), via Execute — NOT the `?? ` Evaluate
    // path.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.executes,
        vec!["k".to_string()],
        "Repl mode must marshal the raw command with no backtick prefix"
    );
    assert!(
        rec.evaluates.is_empty(),
        "Repl mode must NOT go through the `?? ` Evaluate path"
    );
}

/// `read_memory` parses the address (hex), maps the count → size, and returns the scripted bytes
/// with the echoed `0x{:016X}` address — the shape the `read_memory` handler base64-encodes.
#[tokio::test]
async fn read_memory_parses_address_and_maps_bytes() {
    let (backend, recorder) = backend_with(|f| f.memory = vec![0xDE, 0xAD, 0xBE, 0xEF]).await;

    let mem = backend
        .read_memory("0x7FF600001000", 4)
        .await
        .expect("read_memory");
    assert_eq!(
        mem.address, "0x00007FF600001000",
        "address echoed as 0x{{:016X}}"
    );
    assert_eq!(mem.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);

    // The address parsed to the right u64 and the count became the read size.
    let rec = recorder.lock().unwrap();
    assert_eq!(rec.read_memories, vec![(0x7FF600001000, 4)]);
}

/// `read_memory` accepts a decimal address too, and a short canned buffer truncates to the size.
#[tokio::test]
async fn read_memory_decimal_address_and_truncation() {
    let (backend, recorder) = backend_with(|f| f.memory = vec![0x01, 0x02]).await;

    let mem = backend
        .read_memory("4096", 8)
        .await
        .expect("read_memory decimal");
    // Only 2 bytes were available; the fake truncates to the requested size (here it is shorter).
    assert_eq!(mem.data, vec![0x01, 0x02]);
    assert_eq!(
        recorder.lock().unwrap().read_memories,
        vec![(4096, 8)],
        "decimal address parses to its value; the size is the requested count"
    );
}

/// A malformed address surfaces a user-facing error (not a transport error / panic).
#[tokio::test]
async fn read_memory_rejects_bad_address() {
    let (backend, _rec) = backend_with(|_| {}).await;
    let err = backend
        .read_memory("not-an-address", 4)
        .await
        .expect_err("a bad address must error");
    assert!(
        err.to_string().contains("invalid address"),
        "the bad-address error should be clear: {err}"
    );
}

/// `disassemble` parses the address, honors the requested count verbatim, and maps each engine
/// instruction → neutral `Instruction`.
#[tokio::test]
async fn disassemble_maps_instructions() {
    let scripted = vec![
        Instruction {
            address: "0x00007FF600001000".to_string(),
            instruction: "mov dword ptr [rsp+8],ecx".to_string(),
            bytes: String::new(),
            symbol: String::new(),
            source_path: None,
            line: 0,
        },
        Instruction {
            address: "0x00007FF600001008".to_string(),
            instruction: "ret".to_string(),
            bytes: String::new(),
            symbol: String::new(),
            source_path: None,
            line: 0,
        },
    ];
    let scripted_for_fake = scripted.clone();
    let (backend, recorder) = backend_with(move |f| f.instructions = scripted_for_fake).await;

    let insns = backend
        .disassemble("0x7FF600001000", 2)
        .await
        .expect("disassemble");
    assert_eq!(insns, scripted);

    // The address parsed and the count was honored verbatim (no tool-layer default applied here).
    let rec = recorder.lock().unwrap();
    assert_eq!(rec.disassembles, vec![(0x7FF600001000, 2)]);
}

/// Direct unit tests for [`parse_address`](crate::backend::parse_address): the neutral address →
/// `u64` parse the memory/disassembly ops sit on. Exercised directly (not only through the async
/// `read_memory`/`disassemble` paths) so each accept/reject case is pinned independently. Every
/// failure must surface as a [`BackendError`] (the user-facing "invalid address" channel), NOT a
/// panic — `parse_address` returns `Result`, so a malformed input is an `Err`, never an unwind.
mod parse_address {
    use crate::backend::parse_address;
    use debugger_core::BackendError;

    #[test]
    fn accepts_0x_prefixed_hex() {
        assert_eq!(parse_address("0x7FF600001000").unwrap(), 0x7FF6_0000_1000);
        // Lowercase hex digits parse too.
        assert_eq!(parse_address("0xdeadbeef").unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    fn accepts_uppercase_0x_prefix() {
        // The `0X` (capital X) prefix is accepted as well as `0x`.
        assert_eq!(parse_address("0X1000").unwrap(), 0x1000);
        assert_eq!(parse_address("0XABCDEF").unwrap(), 0xABCDEF);
    }

    #[test]
    fn accepts_bare_decimal() {
        assert_eq!(parse_address("4096").unwrap(), 4096);
        assert_eq!(parse_address("0").unwrap(), 0);
    }

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        assert_eq!(parse_address("  0x1000  ").unwrap(), 0x1000);
        assert_eq!(parse_address("\t4096\n").unwrap(), 4096);
        assert_eq!(parse_address(" 0Xff ").unwrap(), 0xFF);
    }

    #[test]
    fn hex_overflow_is_error_not_panic() {
        // 17 hex digits = 2^64, one past u64::MAX: must be a clean BackendError, never a panic.
        let err = parse_address("0x10000000000000000").expect_err("hex overflow must error");
        assert!(matches!(err, BackendError::Dap { .. }), "got {err:?}");
        assert!(err.to_string().contains("invalid address"), "got {err}");
    }

    #[test]
    fn u64_max_is_accepted() {
        // The largest valid address parses without overflow.
        assert_eq!(parse_address("0xFFFFFFFFFFFFFFFF").unwrap(), u64::MAX);
    }

    #[test]
    fn empty_string_is_error() {
        let err = parse_address("").expect_err("empty must error");
        assert!(matches!(err, BackendError::Dap { .. }), "got {err:?}");
        assert!(err.to_string().contains("invalid address"), "got {err}");
    }

    #[test]
    fn whitespace_only_is_error() {
        // Trims to empty, which is not a valid decimal or hex address.
        let err = parse_address("   ").expect_err("whitespace-only must error");
        assert!(matches!(err, BackendError::Dap { .. }), "got {err:?}");
        assert!(err.to_string().contains("invalid address"), "got {err}");
    }

    #[test]
    fn non_numeric_is_error() {
        // A bare garbage token (no prefix, not decimal) is rejected as an error, not a panic.
        let err = parse_address("not-an-address").expect_err("garbage must error");
        assert!(matches!(err, BackendError::Dap { .. }), "got {err:?}");
    }
}
