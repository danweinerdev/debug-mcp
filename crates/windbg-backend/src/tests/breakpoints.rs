//! Runtime breakpoint-setter tests over a [`FakeEngine`] (no live engine, no COM on the calling
//! thread). Cover `set_source_breakpoints` / `set_function_breakpoints`: the positional
//! collect-and-return shape the tool layer expects (one `BreakpointResult` per input bp, in
//! order), the malformed-line skip, and the lldb-parity per-bp-error handling (an unresolvable bp
//! becomes a `verified:false` result and does NOT abort the batch; only a transport `Closed` is
//! fatal).

use debugger_core::{BreakpointResult, DebuggerBackend, FunctionBp, SourceBp};

use crate::engine_ops::EngineOps;
use crate::error::EngineError;

use super::backend_over_fake;
use super::fake::{FakeEngine, RecordedBp, Recorder, ScriptedBp};

use std::sync::{Arc, Mutex};

/// Build a backend over a `FakeEngine` whose `set_breakpoint` returns `scripted` in order (then the
/// default verified bp), returning the backend plus the shared recorder so the test can assert what
/// the backend marshaled.
async fn backend_with_scripted_bps(
    scripted: Vec<ScriptedBp>,
) -> (impl DebuggerBackend, Arc<Mutex<Recorder>>) {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(
            Box::new(FakeEngine::with_scripted_bps(scripted, recorder_for_fake))
                as Box<dyn EngineOps>,
        )
    })
    .await
    .expect("fake backend ready");
    (backend, recorder)
}

/// Read back the recorded `(loc, condition)` of every `set_breakpoint` the backend marshaled.
fn recorded(recorder: &Arc<Mutex<Recorder>>) -> Vec<(RecordedBp, String)> {
    recorder
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .breakpoints
        .clone()
}

/// Read back the recorded ids of every `remove_breakpoint` the backend marshaled (the stale-removal
/// pass of replace-all reconciliation).
fn removed(recorder: &Arc<Mutex<Recorder>>) -> Vec<i64> {
    recorder
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .removes
        .clone()
}

/// `set_source_breakpoints` with two lines → two `BreakpointResult`s in order, and the engine was
/// asked to set exactly those two `BpLoc::FileLine`s with the right file/line/condition.
#[tokio::test]
async fn set_source_breakpoints_sets_each_line_in_order() {
    let (backend, recorder) = backend_with_scripted_bps(vec![
        ScriptedBp::Ok(BreakpointResult {
            id: 11,
            verified: true,
            line: 10,
            message: String::new(),
            rejected: false,
        }),
        ScriptedBp::Ok(BreakpointResult {
            id: 12,
            verified: true,
            line: 20,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let bps = [
        SourceBp {
            line: 10,
            condition: String::new(),
        },
        SourceBp {
            line: 20,
            condition: "x > 0".to_string(),
        },
    ];
    let results = backend
        .set_source_breakpoints("main.c", &bps)
        .await
        .expect("set_source_breakpoints");

    assert_eq!(results.len(), 2, "one result per input bp, in order");
    assert_eq!((results[0].id, results[0].verified), (11, true));
    assert_eq!((results[1].id, results[1].verified), (12, true));

    assert_eq!(
        recorded(&recorder),
        vec![
            (
                RecordedBp::FileLine {
                    file: "main.c".to_string(),
                    line: 10,
                },
                String::new(),
            ),
            (
                RecordedBp::FileLine {
                    file: "main.c".to_string(),
                    line: 20,
                },
                "x > 0".to_string(),
            ),
        ],
        "the engine is asked to set both file:line bps with their conditions",
    );
}

/// A `SourceBp` whose line is > `u32::MAX` is malformed: it yields a `verified:false` result at
/// that position, the batch still returns ALL results, and the engine is NOT asked to set that one
/// (the recorder shows only the valid line).
#[tokio::test]
async fn set_source_breakpoints_skips_a_line_out_of_u32_range() {
    let (backend, recorder) = backend_with_scripted_bps(vec![ScriptedBp::Ok(BreakpointResult {
        id: 7,
        verified: true,
        line: 42,
        message: String::new(),
        rejected: false,
    })])
    .await;

    let bad_line = i64::from(u32::MAX) + 1;
    let bps = [
        SourceBp {
            line: 42,
            condition: String::new(),
        },
        SourceBp {
            line: bad_line,
            condition: String::new(),
        },
    ];
    let results = backend
        .set_source_breakpoints("main.c", &bps)
        .await
        .expect("set_source_breakpoints does not abort on a malformed line");

    assert_eq!(results.len(), 2, "all inputs produce a positional result");
    assert!(results[0].verified, "the valid line resolves");
    assert!(
        !results[1].verified,
        "the out-of-range line is an unverified result"
    );
    assert_eq!(
        results[1].line, bad_line,
        "the unverified result echoes the requested (malformed) line"
    );
    assert!(
        results[1].message.contains("line out of range"),
        "the unverified result explains why, got {:?}",
        results[1].message
    );

    // The engine was asked to set ONLY the valid line — the malformed one is skipped entirely.
    assert_eq!(
        recorded(&recorder),
        vec![(
            RecordedBp::FileLine {
                file: "main.c".to_string(),
                line: 42,
            },
            String::new(),
        )],
        "the malformed line never reaches the engine",
    );
}

/// `set_function_breakpoints` with two functions → two results in order; the engine is asked to set
/// two `BpLoc::Function`s with the right names/conditions.
#[tokio::test]
async fn set_function_breakpoints_sets_each_function_in_order() {
    let (backend, recorder) = backend_with_scripted_bps(vec![
        ScriptedBp::Ok(BreakpointResult {
            id: 1,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
        ScriptedBp::Ok(BreakpointResult {
            id: 2,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let bps = [
        FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        },
        FunctionBp {
            name: "main".to_string(),
            condition: "argc > 1".to_string(),
        },
    ];
    let results = backend
        .set_function_breakpoints(&bps)
        .await
        .expect("set_function_breakpoints");

    assert_eq!(results.len(), 2);
    assert_eq!((results[0].id, results[0].verified), (1, true));
    assert_eq!((results[1].id, results[1].verified), (2, true));

    assert_eq!(
        recorded(&recorder),
        vec![
            (RecordedBp::Function("compute".to_string()), String::new()),
            (
                RecordedBp::Function("main".to_string()),
                "argc > 1".to_string(),
            ),
        ],
        "the engine is asked to set both function bps with their conditions",
    );
}

/// A per-bp engine `Err` (an unresolvable function — the `nonexistent_xyz` / `err_bad_bp` path,
/// scripted, NON-`Closed`) becomes a `verified:false` result at that position; the OTHER bps still
/// resolve; the call returns `Ok(vec)` (does NOT abort the batch). This is the lldb/DAP parity: an
/// unresolvable bp is a per-bp `verified:false`, never a request-level error.
#[tokio::test]
async fn set_function_breakpoints_unresolvable_is_unverified_not_fatal() {
    let (backend, _recorder) = backend_with_scripted_bps(vec![
        // First function resolves.
        ScriptedBp::Ok(BreakpointResult {
            id: 3,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
        // Second function does NOT resolve — the engine surfaces an error (mapped by the backend to
        // BackendError::Dap, the non-transport per-bp failure path).
        ScriptedBp::Err(EngineError::engine(
            "GetOffsetByName failed: nonexistent_xyz_func",
        )),
        // Third function resolves — proves the batch continued past the failure.
        ScriptedBp::Ok(BreakpointResult {
            id: 5,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let bps = [
        FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        },
        FunctionBp {
            name: "nonexistent_xyz_func".to_string(),
            condition: String::new(),
        },
        FunctionBp {
            name: "main".to_string(),
            condition: String::new(),
        },
    ];
    let results = backend
        .set_function_breakpoints(&bps)
        .await
        .expect("an unresolvable bp must NOT fail the whole call");

    assert_eq!(results.len(), 3, "one positional result per input");
    assert!(results[0].verified, "the first function resolves");
    assert!(
        !results[1].verified,
        "the unresolvable function is unverified, not an error"
    );
    assert_eq!(
        results[1].id, 0,
        "the unverified fallback uses the id-0 sentinel"
    );
    assert_eq!(
        results[1].line, 0,
        "a function bp's unverified fallback uses the no-line sentinel (0)"
    );
    assert!(
        results[1].message.contains("nonexistent_xyz_func"),
        "the unverified result carries the engine's message, got {:?}",
        results[1].message
    );
    assert!(
        !results[1].rejected,
        "an unresolvable SYMBOL is the lldb-parity unverified case (may resolve on relaunch), \
         NOT rejected — so the tool layer still tracks it; only the ADDRESS rejection sets rejected"
    );
    assert!(
        results[2].verified,
        "the batch continued and resolved the function after the failure"
    );
}

/// The same per-bp-error rule for the source path: a scripted non-`Closed` engine `Err` on the
/// second of two source bps yields a `verified:false` result echoing that line, the first still
/// resolves, and the call returns `Ok`.
#[tokio::test]
async fn set_source_breakpoints_unresolvable_line_is_unverified_not_fatal() {
    let (backend, _recorder) = backend_with_scripted_bps(vec![
        ScriptedBp::Ok(BreakpointResult {
            id: 9,
            verified: true,
            line: 5,
            message: String::new(),
            rejected: false,
        }),
        ScriptedBp::Err(EngineError::engine(
            "GetOffsetByLine failed: no code at line",
        )),
    ])
    .await;

    let bps = [
        SourceBp {
            line: 5,
            condition: String::new(),
        },
        SourceBp {
            line: 999,
            condition: String::new(),
        },
    ];
    let results = backend
        .set_source_breakpoints("main.c", &bps)
        .await
        .expect("an unresolvable line must NOT fail the whole call");

    assert_eq!(results.len(), 2);
    assert!(results[0].verified);
    assert!(!results[1].verified, "the unresolvable line is unverified");
    assert_eq!(
        results[1].line, 999,
        "a source bp's unverified fallback echoes the requested line (handler matches by line)"
    );
}

/// Replace-all reconciliation (lldb/DAP parity, the core of the integration breakpoint workflow):
/// re-sending a function bp at an unchanged name REUSES the engine id (does NOT add a duplicate),
/// and a name dropped from a later full list is REMOVED from the engine. Drives:
///   1. `set_function_breakpoints([compute])` → engine sets `compute` (id 1).
///   2. `set_function_breakpoints([compute, main])` → `compute` reused (no new set), `main` set
///      (id 2). The recorder shows the engine was asked to set `compute` ONCE and `main` ONCE.
///   3. `set_function_breakpoints([compute])` → `main` removed, `compute` still reused (id 1).
#[tokio::test]
async fn set_function_breakpoints_reconciles_reuses_id_and_removes_stale() {
    let (backend, recorder) = backend_with_scripted_bps(vec![
        // compute → id 1
        ScriptedBp::Ok(BreakpointResult {
            id: 1,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
        // main → id 2
        ScriptedBp::Ok(BreakpointResult {
            id: 2,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let compute = FunctionBp {
        name: "compute".to_string(),
        condition: String::new(),
    };
    let main = FunctionBp {
        name: "main".to_string(),
        condition: String::new(),
    };

    // 1. Set compute.
    let r1 = backend
        .set_function_breakpoints(std::slice::from_ref(&compute))
        .await
        .expect("set compute");
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0].id, 1, "compute gets engine id 1");

    // 2. Add main (full list = [compute, main]); compute must be REUSED (same id), not re-added.
    let r2 = backend
        .set_function_breakpoints(&[compute.clone(), main.clone()])
        .await
        .expect("set [compute, main]");
    assert_eq!(r2.len(), 2);
    assert_eq!(r2[0].id, 1, "compute keeps its original engine id (reused)");
    assert_eq!(r2[1].id, 2, "main gets a fresh engine id");

    // 3. Remove main (full list = [compute]); compute still reused, main removed from the engine.
    let r3 = backend
        .set_function_breakpoints(std::slice::from_ref(&compute))
        .await
        .expect("set [compute] again");
    assert_eq!(r3.len(), 1);
    assert_eq!(
        r3[0].id, 1,
        "compute still has its original id after reconcile"
    );

    // The stale `main` (engine id 2) was actually REMOVED from the engine in step 3 — and ONLY
    // `main` (compute, still desired, is never removed). This is the only unit-level proof the
    // stale-removal path executes (the live tests don't run in the Ubuntu CI gate).
    assert_eq!(
        removed(&recorder),
        vec![2],
        "exactly main's engine id (2) was removed when it dropped from the desired list"
    );

    // The engine was asked to set each function EXACTLY ONCE (no duplicate `compute` add) — the
    // reuse path skips the marshal for an already-tracked location.
    let bps = recorded(&recorder);
    let compute_sets = bps
        .iter()
        .filter(|(loc, _)| *loc == RecordedBp::Function("compute".to_string()))
        .count();
    let main_sets = bps
        .iter()
        .filter(|(loc, _)| *loc == RecordedBp::Function("main".to_string()))
        .count();
    assert_eq!(
        compute_sets, 1,
        "compute must be set on the engine exactly once across the re-sends (no duplicate), got {bps:?}"
    );
    assert_eq!(main_sets, 1, "main set exactly once, got {bps:?}");
}

/// `Closed` propagation: a setter call against a backend whose engine thread is gone (its command
/// channel closed) surfaces `BackendError::Closed` (the transport-failure path), NOT a swallowed
/// `verified:false` result. `with_closed_channel_for_test` builds a backend over an already-closed
/// channel, so the marshaled `SetBreakpoint`'s `send` fails immediately → `call` maps it to
/// `Closed` → `bp_result_or_continue` PROPAGATES it (rather than converting it to a bogus unverified
/// result, the way a non-transport per-bp error is converted). This pins that the engine-died path
/// is fatal for the setters.
#[tokio::test]
async fn set_breakpoints_propagates_closed_from_a_dead_engine() {
    use crate::backend::WinDbgBackend;
    use debugger_core::BackendError;

    let backend = WinDbgBackend::with_closed_channel_for_test();

    // The function setter rides `call`; against a dead channel it must yield `Closed`, NOT Ok.
    let err = backend
        .set_function_breakpoints(&[FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        }])
        .await
        .expect_err("a setter against a dead engine thread must fail, not return Ok");
    match err {
        BackendError::Closed => {}
        other => panic!("a dead engine thread must surface Closed, got {other:?}"),
    }

    // The source setter rides the same `call` path; assert it too propagates `Closed`.
    let err = backend
        .set_source_breakpoints(
            "main.c",
            &[SourceBp {
                line: 10,
                condition: String::new(),
            }],
        )
        .await
        .expect_err("the source setter must also surface Closed against a dead engine");
    assert!(
        matches!(err, BackendError::Closed),
        "the source setter must propagate Closed, got {err:?}"
    );
}

/// Replace-all reconciliation for the SOURCE path (structurally separate from the function loop, so
/// it needs its own multi-call coverage). Drives:
///   1. `set_source_breakpoints("f.c", [10])` → engine sets line 10 (id 11), once.
///   2. `set_source_breakpoints("f.c", [10, 20])` → line 10 REUSES id 11 (engine NOT asked again),
///      line 20 is a fresh add (id 12). The recorder shows only line 20 set in this call.
///   3. `set_source_breakpoints("f.c", [10])` → line 20 REMOVED (its id 12), line 10 still reused.
#[tokio::test]
async fn set_source_breakpoints_reconciles_reuses_id_and_removes_stale() {
    let (backend, recorder) = backend_with_scripted_bps(vec![
        // line 10 → id 11
        ScriptedBp::Ok(BreakpointResult {
            id: 11,
            verified: true,
            line: 10,
            message: String::new(),
            rejected: false,
        }),
        // line 20 → id 12
        ScriptedBp::Ok(BreakpointResult {
            id: 12,
            verified: true,
            line: 20,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let line10 = SourceBp {
        line: 10,
        condition: String::new(),
    };
    let line20 = SourceBp {
        line: 20,
        condition: String::new(),
    };

    // 1. Set line 10.
    let r1 = backend
        .set_source_breakpoints("f.c", std::slice::from_ref(&line10))
        .await
        .expect("set line 10");
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0].id, 11, "line 10 gets engine id 11");

    // 2. Add line 20 (full list = [10, 20]); line 10 must be REUSED (same id), not re-set.
    let r2 = backend
        .set_source_breakpoints("f.c", &[line10.clone(), line20.clone()])
        .await
        .expect("set [10, 20]");
    assert_eq!(r2.len(), 2);
    assert_eq!(
        r2[0].id, 11,
        "line 10 keeps its original engine id (reused)"
    );
    assert_eq!(r2[1].id, 12, "line 20 gets a fresh engine id");

    // The engine was asked to set ONLY line 20 in this call — line 10 was reused (no re-marshal).
    // Across calls 1+2 the engine saw line 10 set exactly once and line 20 exactly once.
    let bps = recorded(&recorder);
    let line10_sets = bps
        .iter()
        .filter(|(loc, _)| {
            *loc == RecordedBp::FileLine {
                file: "f.c".to_string(),
                line: 10,
            }
        })
        .count();
    let line20_sets = bps
        .iter()
        .filter(|(loc, _)| {
            *loc == RecordedBp::FileLine {
                file: "f.c".to_string(),
                line: 20,
            }
        })
        .count();
    assert_eq!(
        line10_sets, 1,
        "line 10 set on the engine exactly once (reused on the re-send), got {bps:?}"
    );
    assert_eq!(line20_sets, 1, "line 20 set exactly once, got {bps:?}");

    // 3. Drop line 20 (full list = [10]); line 20 REMOVED (id 12), line 10 still reused.
    let r3 = backend
        .set_source_breakpoints("f.c", std::slice::from_ref(&line10))
        .await
        .expect("set [10] again");
    assert_eq!(r3.len(), 1);
    assert_eq!(
        r3[0].id, 11,
        "line 10 still has its original id after reconcile"
    );
    assert_eq!(
        removed(&recorder),
        vec![12],
        "exactly line 20's engine id (12) was removed when it dropped from the desired list"
    );
}

/// Category isolation: source and function reconciliation must NOT cross-remove each other's engine
/// breakpoints (they are tracked in separate maps). Drives BOTH directions:
///   A. set function `compute`, set source line 10, then clear source → only the source line's id is
///      removed; `compute`'s id is NEVER removed.
///   B. set source line 30 + function `helper`, then clear functions → only `helper`'s id is removed;
///      the source line's id is NEVER removed.
#[tokio::test]
async fn breakpoint_categories_do_not_cross_remove() {
    let (backend, recorder) = backend_with_scripted_bps(vec![
        // A: compute → id 100
        ScriptedBp::Ok(BreakpointResult {
            id: 100,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
        // A: source line 10 → id 101
        ScriptedBp::Ok(BreakpointResult {
            id: 101,
            verified: true,
            line: 10,
            message: String::new(),
            rejected: false,
        }),
        // B: source line 30 → id 200
        ScriptedBp::Ok(BreakpointResult {
            id: 200,
            verified: true,
            line: 30,
            message: String::new(),
            rejected: false,
        }),
        // B: function helper → id 201
        ScriptedBp::Ok(BreakpointResult {
            id: 201,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    // --- Direction A: clear SOURCE, function must be untouched ---
    backend
        .set_function_breakpoints(&[FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        }])
        .await
        .expect("set compute (id 100)");
    backend
        .set_source_breakpoints(
            "f.c",
            &[SourceBp {
                line: 10,
                condition: String::new(),
            }],
        )
        .await
        .expect("set source line 10 (id 101)");
    // Clear the source file (full desired list = []).
    backend
        .set_source_breakpoints("f.c", &[])
        .await
        .expect("clear source");

    assert_eq!(
        removed(&recorder),
        vec![101],
        "clearing source removes ONLY the source line's id (101); compute's id (100) is untouched"
    );
    assert!(
        !removed(&recorder).contains(&100),
        "compute's engine id must never be removed by a source-only reconcile"
    );

    // --- Direction B: clear FUNCTIONS, source must be untouched ---
    backend
        .set_source_breakpoints(
            "g.c",
            &[SourceBp {
                line: 30,
                condition: String::new(),
            }],
        )
        .await
        .expect("set source line 30 (id 200)");
    backend
        .set_function_breakpoints(&[FunctionBp {
            name: "helper".to_string(),
            condition: String::new(),
        }])
        .await
        .expect("set helper (id 201)");
    // Clear the functions (full desired list = []).
    backend
        .set_function_breakpoints(&[])
        .await
        .expect("clear functions");

    // Clearing functions reconciles the WHOLE function category, so it removes BOTH still-tracked
    // function ids — `helper` (201) AND `compute` (100, set in direction A and never cleared) — but
    // NEVER a source id. The load-bearing isolation property: the function-clear touches only
    // function ids; the source line (id 200) is untouched. Cumulative removes across A+B are
    // [101 (source clear in A), 100 + 201 (function clear in B)].
    let removes = removed(&recorder);
    assert!(
        removes.contains(&201),
        "clearing functions removed helper's id (201), got {removes:?}"
    );
    assert!(
        removes.contains(&100),
        "clearing functions also removed compute's id (100) — the function category is global, got {removes:?}"
    );
    assert!(
        !removes.contains(&200),
        "the source line's engine id (200) must never be removed by a function-only reconcile, got {removes:?}"
    );
    // The function-clear pass removed exactly the two tracked function ids (100, 201) and no source
    // id — assert as a set so the (unordered HashMap iteration) order is not load-bearing.
    let function_clear_removes: std::collections::HashSet<i64> =
        removes.iter().copied().filter(|id| *id != 101).collect();
    assert_eq!(
        function_clear_removes,
        std::collections::HashSet::from([100, 201]),
        "the function-clear removed exactly the tracked function ids (100, 201), no source id, got {removes:?}"
    );
}

/// R6 — ASLR-safe address breakpoints. A bare-address function-bp `name` (`0x…` prefix) is REJECTED
/// without touching the engine: it yields an unverified result carrying the guidance message, the
/// engine is NEVER asked to set it (the recorder shows no `SetBreakpoint` for that name), and it is
/// NOT tracked (a later reconcile that drops it never tries to remove an id for it — no spurious
/// remove). A `module!sym` name and a plain name in the SAME batch flow through normally (engine set,
/// tracked), proving the rejection is per-bp and does not abort the batch.
#[tokio::test]
async fn set_function_breakpoints_rejects_bare_address_keeps_module_sym() {
    use crate::backend::ADDRESS_BP_GUIDANCE;

    let (backend, recorder) = backend_with_scripted_bps(vec![
        // `test_target!compute` (module!sym) resolves → id 1.
        ScriptedBp::Ok(BreakpointResult {
            id: 1,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
        // `compute` (plain name) resolves → id 2.
        ScriptedBp::Ok(BreakpointResult {
            id: 2,
            verified: true,
            line: 0,
            message: String::new(),
            rejected: false,
        }),
    ])
    .await;

    let bps = [
        FunctionBp {
            name: "0x7ff6abcd1234".to_string(),
            condition: String::new(),
        },
        FunctionBp {
            name: "test_target!compute".to_string(),
            condition: String::new(),
        },
        FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        },
    ];
    let results = backend
        .set_function_breakpoints(&bps)
        .await
        .expect("a bare-address name must not fail the batch");

    assert_eq!(results.len(), 3, "one positional result per input");

    // The bare-address name → unverified, id-0 sentinel, no line, the guidance message.
    assert!(
        !results[0].verified,
        "the bare-address name is rejected as unverified"
    );
    assert_eq!(results[0].id, 0, "the rejection uses the id-0 sentinel");
    assert_eq!(results[0].line, 0, "the rejection carries no line");
    assert_eq!(
        results[0].message, ADDRESS_BP_GUIDANCE,
        "the rejection carries the address-bp guidance message"
    );
    assert!(
        results[0].rejected,
        "the bare-address result carries the neutral rejected flag so the tool layer skips tracking"
    );

    // The `module!sym` and plain names flow through and resolve normally — and are NOT rejected.
    assert!(
        results[1].verified && results[1].id == 1 && !results[1].rejected,
        "module!sym resolves and is tracked (id 1), rejected:false"
    );
    assert!(
        results[2].verified && results[2].id == 2 && !results[2].rejected,
        "the plain name resolves and is tracked (id 2), rejected:false"
    );

    // The engine was asked to set ONLY the module!sym and the plain name — never the bare address.
    let bps_set = recorded(&recorder);
    assert_eq!(
        bps_set,
        vec![
            (
                RecordedBp::Function("test_target!compute".to_string()),
                String::new(),
            ),
            (RecordedBp::Function("compute".to_string()), String::new()),
        ],
        "the bare-address name never reaches the engine; only module!sym + plain do, got {bps_set:?}"
    );
    assert!(
        !bps_set
            .iter()
            .any(|(loc, _)| *loc == RecordedBp::Function("0x7ff6abcd1234".to_string())),
        "the engine was never asked to set the bare-address name"
    );

    // The bare address is NOT tracked: a follow-up reconcile that drops everything must remove the
    // two REAL tracked ids (module!sym + plain) but NEVER an id for the address (it was never cached).
    backend
        .set_function_breakpoints(&[])
        .await
        .expect("clear functions");
    let removes = removed(&recorder);
    assert_eq!(
        removes
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([1, 2]),
        "only the two tracked (resolvable) ids are removed; the un-tracked address produces no remove, got {removes:?}"
    );
    assert!(
        !removes.contains(&0),
        "the id-0 address sentinel must never be sent to the engine's remove path"
    );
}

/// R6 — a `module!sym` (or plain name) is rebase-stable: re-sending it across reconciles reuses its
/// id and it is tracked normally (the rejection is ONLY for bare `0x…` names). This guards that the
/// address rule did not accidentally start rejecting `module!sym` (which contains no `0x` prefix).
#[tokio::test]
async fn set_function_breakpoints_module_sym_is_tracked_and_reused() {
    let (backend, recorder) = backend_with_scripted_bps(vec![ScriptedBp::Ok(BreakpointResult {
        id: 7,
        verified: true,
        line: 0,
        message: String::new(),
        rejected: false,
    })])
    .await;

    let sym = FunctionBp {
        name: "test_target!compute".to_string(),
        condition: String::new(),
    };

    let r1 = backend
        .set_function_breakpoints(std::slice::from_ref(&sym))
        .await
        .expect("set module!sym");
    assert_eq!(r1[0].id, 7, "module!sym resolves to a real engine id");

    // Re-send the same module!sym — it must be REUSED (tracked), not set again.
    let r2 = backend
        .set_function_breakpoints(std::slice::from_ref(&sym))
        .await
        .expect("re-send module!sym");
    assert_eq!(r2[0].id, 7, "module!sym keeps its id (tracked/reused)");

    let sets = recorded(&recorder)
        .iter()
        .filter(|(loc, _)| *loc == RecordedBp::Function("test_target!compute".to_string()))
        .count();
    assert_eq!(
        sets, 1,
        "module!sym is set on the engine exactly once (tracked + reused), not rejected"
    );
}

/// Condition-change behavior (PINNED, intentional/deferred): re-sending the SAME location with a
/// DIFFERENT condition reuses the cached result and does NOT re-apply the condition to the engine
/// (conditional-eval is a deferred Phase-5 feature; a re-sent location keeps its original
/// condition). This test makes a future change to that behavior conscious — if conditional-eval
/// lands and the reuse branch starts diffing/re-setting the condition, this assertion will flip and
/// force the author to update it deliberately.
#[tokio::test]
async fn set_source_breakpoints_resend_with_changed_condition_does_not_reapply() {
    let (backend, recorder) = backend_with_scripted_bps(vec![ScriptedBp::Ok(BreakpointResult {
        id: 42,
        verified: true,
        line: 42,
        message: String::new(),
        rejected: false,
    })])
    .await;

    // 1. Set line 42 with no condition.
    let r1 = backend
        .set_source_breakpoints(
            "f.c",
            &[SourceBp {
                line: 42,
                condition: String::new(),
            }],
        )
        .await
        .expect("set line 42, no condition");
    assert_eq!(r1[0].id, 42);

    // 2. Re-send line 42 with a DIFFERENT condition; the cached (same-id) result is reused and the
    //    engine is NOT asked to set line 42 again.
    let r2 = backend
        .set_source_breakpoints(
            "f.c",
            &[SourceBp {
                line: 42,
                condition: "x > 0".to_string(),
            }],
        )
        .await
        .expect("re-send line 42 with a changed condition");
    assert_eq!(
        r2[0].id, 42,
        "the re-send reuses the cached result (same engine id)"
    );

    // The engine saw line 42 set EXACTLY ONCE total — the changed condition did not trigger a
    // re-set (deferred Phase-5 conditional-eval).
    let bps = recorded(&recorder);
    let line42_sets = bps
        .iter()
        .filter(|(loc, _)| {
            *loc == RecordedBp::FileLine {
                file: "f.c".to_string(),
                line: 42,
            }
        })
        .count();
    assert_eq!(
        line42_sets, 1,
        "line 42 was set on the engine exactly once; the changed condition was NOT re-applied \
         (intentional/deferred), got {bps:?}"
    );
    // The one recorded set carried the ORIGINAL (empty) condition, not the re-sent one.
    assert_eq!(
        bps[0].1,
        String::new(),
        "the engine breakpoint kept its original (empty) condition; the re-send's condition was not applied"
    );
}
