//! Live lifecycle tests (Windows): drive the real [`WinDbgFactory`]/[`WinDbgBackend`] against the
//! built fixture `testdata/win/test_target.exe`. These need a live DbgEng + the compiled fixture,
//! so each test skips cleanly when the fixture is absent (run `testdata/win/build.bat`).
//!
//! DbgEng keeps process-global state, so the tests that drive a target are serialized behind a
//! single mutex (independent of `--test-threads`), mirroring the `dbgeng-sys` live suite.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use debugger_core::{
    AttachOutcome, AttachSpec, BackendEvent, BackendFactory, EvalMode, FunctionBp, LaunchOutcome,
    LaunchSpec, StepKind, StopOutcome,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use windbg_backend::WinDbgFactory;

/// Serializes the live tests — only one DbgEng session at a time per process.
static LIVE: Mutex<()> = Mutex::new(());

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/win/test_target.exe")
}

/// True (and logs) when the fixture exe is absent, so the live tests skip cleanly.
fn should_skip() -> bool {
    if fixture().exists() {
        false
    } else {
        eprintln!(
            "SKIP: fixture {} not built (run testdata/win/build.bat)",
            fixture().display()
        );
        true
    }
}

/// A `LaunchSpec` for `test_target normal` with a function breakpoint on `compute` (no
/// stop_on_entry — WinDbg stops at the loader break regardless).
fn launch_spec_with_compute_bp() -> LaunchSpec {
    LaunchSpec {
        program: fixture().to_string_lossy().into_owned(),
        args: vec!["normal".to_string()],
        cwd: None,
        env: vec![],
        stop_on_entry: false,
        source_breakpoints: vec![],
        function_breakpoints: vec![FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        }],
    }
}

/// A bare `LaunchSpec` for `test_target <mode>` with no breakpoints (the loader break still stops
/// the launch). Used by the cont-to-exit / pause / output tests.
fn launch_spec(mode: &str) -> LaunchSpec {
    LaunchSpec {
        program: fixture().to_string_lossy().into_owned(),
        args: vec![mode.to_string()],
        cwd: None,
        env: vec![],
        stop_on_entry: false,
        source_breakpoints: vec![],
        function_breakpoints: vec![],
    }
}

/// A multi-thread runtime so the engine thread's `blocking_recv` (a plain `std::thread`) and the
/// async backend coexist without starving the single worker.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

/// `launch` the fixture with a `compute` function breakpoint → stops at the loader break
/// (LaunchOutcome::Stopped), even though `stop_on_entry` is false (WinDbg always breaks at the
/// loader). The launch internally flushes the pending `compute` breakpoint; resolving it requires
/// the exe's symbols, which `launch` force-loads, so a clean launch is evidence the flush ran
/// against a real symbol surface. `modules()` then confirms the session is live (the exe is loaded).
///
/// The end-to-end "continue hits the flushed breakpoint" proof is deferred to a 3.3 test: `cont` is
/// a phase-3.3 placeholder, so it cannot be exercised here. The flush *call path* is fully covered
/// by the unit test `launch_returns_stopped_and_flushes_breakpoints` (which asserts the exact
/// breakpoints marshaled to the engine via the recording fake).
#[test]
fn launch_stops_at_loader_break_with_breakpoint_flush() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        // Launch always stops at the loader break, even with stop_on_entry == false. This also
        // flushes the pending `compute` function breakpoint (best-effort, against the force-loaded
        // exe symbols).
        let outcome = backend
            .launch(launch_spec_with_compute_bp())
            .await
            .expect("launch");
        match outcome {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("WinDbg launch must stop at the loader break, got {other:?}"),
        }

        // The session is live: the fixture exe is among the loaded modules.
        let modules = backend.modules().await.expect("modules after launch");
        assert!(
            modules
                .iter()
                .any(|m| m.name.to_ascii_lowercase().contains("test_target")),
            "the launched exe should be a loaded module, got {modules:?}"
        );

        // Best-effort detach (do not kill — terminate=false).
        backend.disconnect(false).await;
    });
}

/// `attach` by pid to a spawned `test_target wait` → AttachOutcome::Stopped, with the supplied pid
/// recorded for `debugger_pid`. Then `disconnect(false)` and kill the child.
#[test]
fn attach_by_pid_stops_a_running_process() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut child = Command::new(fixture())
        .arg("wait")
        .spawn()
        .expect("spawn test_target wait");
    let pid = child.id();

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        let result = backend
            .attach(AttachSpec {
                pid: Some(pid as i64),
                wait_for: None,
            })
            .await;

        // Always detach (best-effort) before asserting so a failure does not leak the session.
        backend.disconnect(false).await;

        match result.expect("attach by pid") {
            AttachOutcome::Stopped(_) => {}
            other => panic!("expected Stopped after attach, got {other:?}"),
        }
        assert_eq!(
            backend.debugger_pid(),
            Some(pid as i64),
            "attach records the supplied pid for debugger_pid"
        );
    });

    let _ = child.kill();
    let _ = child.wait();
}

/// `cont` to the flushed `compute` function breakpoint, then a second `cont` to exit.
///
/// Proves the core 3.3 execution path end-to-end: `launch` flushes the `compute` bp and stops at
/// the loader break; the first `cont` runs the target to that breakpoint (`StopOutcome::Stopped`
/// whose reason references the breakpoint); the second `cont` runs `compute` → `printf` → exit,
/// landing `Exited { code: Some(0) }`.
#[test]
fn cont_hits_breakpoint_then_runs_to_exit() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        // Launch stops at the loader break and flushes the pending `compute` breakpoint.
        match backend
            .launch(launch_spec_with_compute_bp())
            .await
            .expect("launch")
        {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("launch must stop at the loader break, got {other:?}"),
        }

        // First cont: run to the `compute` breakpoint. thread_id is ignored (whole-target resume).
        let stop = backend.cont(0).await.expect("cont to the breakpoint");
        match stop {
            StopOutcome::Stopped(info) => {
                let reason = info.reason.to_ascii_lowercase();
                assert!(
                    reason.contains("breakpoint") || !info.hit_breakpoint_ids.is_empty(),
                    "the first cont should stop at the compute breakpoint, got {info:?}"
                );
            }
            other => panic!("first cont should hit the breakpoint (Stopped), got {other:?}"),
        }

        // Second cont: run compute → printf → return → exit 0.
        let exit = backend.cont(0).await.expect("cont to exit");
        match exit {
            StopOutcome::Exited { code } => assert_eq!(
                code,
                Some(0),
                "the second cont should run test_target normal to exit 0"
            ),
            other => panic!("second cont should run to exit, got {other:?}"),
        }

        backend.disconnect(false).await;
    });
}

/// Full inspection path (3.4): launch with a `compute` function breakpoint → `cont` to it →
/// `stack_trace` finds `compute` and `main` (names + a source line) → `scopes(frame0)` returns a
/// Locals scope → `variables(localsRef)` includes `compute`'s locals → `evaluate(Repl, "k")` and
/// `evaluate(Expression, "10 + 10")` both return output → `read_memory`/`disassemble` at the
/// current IP return data. Drives the real engine through the marshaling layer end-to-end.
#[test]
fn inspection_at_compute_breakpoint() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        // Launch stops at the loader break and flushes the pending `compute` breakpoint.
        match backend
            .launch(launch_spec_with_compute_bp())
            .await
            .expect("launch")
        {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("launch must stop at the loader break, got {other:?}"),
        }

        // Run to the `compute` breakpoint.
        let thread_id = match backend.cont(0).await.expect("cont to the breakpoint") {
            StopOutcome::Stopped(info) => {
                let reason = info.reason.to_ascii_lowercase();
                assert!(
                    reason.contains("breakpoint") || !info.hit_breakpoint_ids.is_empty(),
                    "the cont should stop at the compute breakpoint, got {info:?}"
                );
                info.thread_id
            }
            other => panic!("cont should hit the breakpoint (Stopped), got {other:?}"),
        };

        // stack_trace finds `compute` (innermost) and `main` among the frames, with source lines.
        let (frames, total) = backend
            .stack_trace(thread_id, 0, 20)
            .await
            .expect("stack_trace at the breakpoint");
        assert!(total > 0, "the walked stack must be non-empty");
        assert!(
            frames.iter().any(|f| f.name.contains("compute")),
            "stack_trace should find `compute` in the frames, got {frames:?}"
        );
        assert!(
            frames.iter().any(|f| f.name.contains("main")),
            "stack_trace should find `main` in the frames, got {frames:?}"
        );
        // At least one frame maps to a source line (compute's source line is known).
        assert!(
            frames.iter().any(|f| f.source_path.is_some() && f.line > 0),
            "at least one frame should carry a source path + line, got {frames:?}"
        );

        // Identify the `compute` frame; scope/variables are read against its id.
        let compute_frame = frames
            .iter()
            .find(|f| f.name.contains("compute"))
            .expect("a compute frame");

        // scopes(frame) returns a Locals scope.
        let scopes = backend
            .scopes(compute_frame.id)
            .await
            .expect("scopes at the compute frame");
        let locals_scope = scopes
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case("locals"))
            .expect("a Locals scope");
        assert!(
            locals_scope.variables_reference > 0,
            "the Locals scope reference must be positive (frame-encoded)"
        );

        // variables(localsRef) includes compute's locals (`sum` / `i`) with sane type+value.
        let vars = backend
            .variables(locals_scope.variables_reference)
            .await
            .expect("variables in the compute frame");
        assert!(
            vars.iter()
                .any(|v| v.name == "sum" || v.name == "i" || v.name == "n"),
            "compute's locals (sum/i/n) should appear, got {vars:?}"
        );
        assert!(
            vars.iter().all(|v| v.variables_reference == 0),
            "WinDbg top-level locals are flat (no nested child reference), got {vars:?}"
        );

        // evaluate(Repl, "k") returns the stack as raw command output (non-empty).
        let repl = backend
            .evaluate("k", None, EvalMode::Repl)
            .await
            .expect("evaluate repl `k`");
        assert!(
            !repl.result.trim().is_empty(),
            "evaluate(Repl, `k`) should return non-empty command output, got {repl:?}"
        );

        // evaluate(Expression, "10 + 10") returns a value.
        let expr = backend
            .evaluate("10 + 10", Some(compute_frame.id), EvalMode::Expression)
            .await
            .expect("evaluate expression");
        assert!(
            !expr.result.trim().is_empty(),
            "evaluate(Expression, `10 + 10`) should return a value, got {expr:?}"
        );

        // evaluate with a NON-current frame_id documents the WinDbg parity limitation: the backend
        // IGNORES `frame_id` and always evaluates in the innermost (current) stopped frame — unlike
        // the lldb/DAP backend, which evaluates in the requested frame (see `backend.rs::evaluate`).
        // We pass `main`'s frame id (an outer frame, NOT `compute`'s innermost) and assert evaluate
        // still SUCCEEDS with a value. We deliberately do NOT assert a frame-specific value, because
        // the value comes from the current scope, not the requested frame — asserting otherwise would
        // contradict the documented limitation. This pins that a non-current `frame_id` does not
        // error and is silently evaluated in the current scope.
        let main_frame = frames
            .iter()
            .find(|f| f.name.contains("main"))
            .expect("a main frame");
        assert_ne!(
            main_frame.id, compute_frame.id,
            "main must be a different (non-current) frame than compute for this to be meaningful"
        );
        let expr_outer = backend
            .evaluate("10 + 10", Some(main_frame.id), EvalMode::Expression)
            .await
            .expect(
                "evaluate with a non-current frame_id must still succeed (frame_id is ignored)",
            );
        assert!(
            !expr_outer.result.trim().is_empty(),
            "evaluate with a non-current frame_id should still return a value (evaluated in the \
             current scope, since WinDbg ignores frame_id), got {expr_outer:?}"
        );

        // read_memory at the frame's IP returns the requested byte count; disassemble returns > 0.
        let ip = compute_frame
            .instruction_pointer
            .as_deref()
            .expect("the compute frame has an instruction pointer");
        let mem = backend
            .read_memory(ip, 16)
            .await
            .expect("read_memory at IP");
        assert_eq!(
            mem.data.len(),
            16,
            "read_memory should return the requested 16 bytes at a valid IP, got {} bytes",
            mem.data.len()
        );
        let insns = backend.disassemble(ip, 4).await.expect("disassemble at IP");
        assert!(
            !insns.is_empty(),
            "disassemble at the IP should return at least one instruction"
        );

        backend.disconnect(true).await;
    });
}

/// The RUNTIME breakpoint-setter path post-launch (distinct from the launch-flush path that
/// `cont_hits_breakpoint_then_runs_to_exit` exercises): launch `normal` with NO pending bp, then
/// `set_function_breakpoints(&[compute])` at the loader break returns a VERIFIED bp, `cont` runs to
/// it (Stopped at the breakpoint), and finally `set_function_breakpoints(&[nonexistent_xyz_func])`
/// returns an UNVERIFIED result (verified:false) — NOT a panic, NOT a `Closed` error (lldb/DAP
/// parity: an unresolvable function is a per-bp `verified:false`, never a request-level error).
#[test]
fn runtime_set_function_breakpoint_resolves_then_unresolvable_is_unverified() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        // Launch with NO pending breakpoint, so the only bp set is via the RUNTIME setter below.
        match backend.launch(launch_spec("normal")).await.expect("launch") {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("launch must stop at the loader break, got {other:?}"),
        }

        // Runtime setter: set a function breakpoint on `compute` at the loader break. The real exe
        // symbols are loaded, so `compute` resolves → a verified breakpoint.
        let results = backend
            .set_function_breakpoints(&[FunctionBp {
                name: "compute".to_string(),
                condition: String::new(),
            }])
            .await
            .expect("set_function_breakpoints(compute) at the loader break");
        assert_eq!(results.len(), 1, "one result for the one function bp");
        assert!(
            results[0].verified,
            "the runtime `compute` breakpoint must resolve to a verified bp, got {:?}",
            results[0]
        );

        // cont runs the target to the runtime-set `compute` breakpoint (Stopped).
        match backend
            .cont(0)
            .await
            .expect("cont to the runtime breakpoint")
        {
            StopOutcome::Stopped(info) => {
                let reason = info.reason.to_ascii_lowercase();
                assert!(
                    reason.contains("breakpoint") || !info.hit_breakpoint_ids.is_empty(),
                    "cont should stop at the runtime-set compute breakpoint, got {info:?}"
                );
            }
            other => panic!("cont should hit the runtime breakpoint (Stopped), got {other:?}"),
        }

        // A NONEXISTENT function (the `err_bad_bp` scenario): the runtime setter must return an
        // UNVERIFIED result, NOT panic and NOT a Closed/transport error.
        let bad = backend
            .set_function_breakpoints(&[FunctionBp {
                name: "nonexistent_xyz_func".to_string(),
                condition: String::new(),
            }])
            .await
            .expect("an unresolvable function must NOT fail the call (lldb parity)");
        assert_eq!(bad.len(), 1, "one result for the one (bad) function bp");
        assert!(
            !bad[0].verified,
            "a nonexistent function must yield an unverified result, got {:?}",
            bad[0]
        );

        backend.disconnect(true).await;
    });
}

/// `step` Over and `step` Into from the loader break each land `Stopped` (a single source step
/// stays inside the loader/CRT). thread_id and gran are ignored (WinDbg steps the current thread,
/// source/line-oriented — no instruction-granularity knob).
#[test]
fn step_over_and_into_from_loader_break_stop() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    for kind in [StepKind::Over, StepKind::Into] {
        runtime().block_on(async {
            let conn = WinDbgFactory::new()
                .connect()
                .await
                .expect("connect a live WinDbg backend");
            let backend = conn.backend;

            match backend.launch(launch_spec("normal")).await.expect("launch") {
                LaunchOutcome::Stopped(_) => {}
                other => panic!("launch must stop at the loader break, got {other:?}"),
            }

            let outcome = backend.step(kind, 0, None).await.expect("step");
            assert!(
                matches!(outcome, StopOutcome::Stopped(_)),
                "step {kind:?} from the loader break should land Stopped, got {outcome:?}"
            );

            backend.disconnect(false).await;
        });
    }
}

/// pause-breaks-cont: launch `wait` (an infinite sleep loop), `cont` it on one task, and from
/// another task call `pause()` after ~150 ms. The flag-only break must turn the running `cont`
/// into a `Stopped`. Proves `pause` unblocks a free-running `cont`.
///
/// Timing math (this is inherently timing-sensitive; the first CI-flake fix is raising the bound):
/// `pause` sets the interrupt flag at ~150 ms; `go`'s ≤200 ms poll slice observes it within
/// ≤200 ms; `break_in`/`break_loop` then runs to surface the break — its first iteration almost
/// always lands in <400 ms, but its *documented worst case* is 50×200 ms = 10 s. So the bound is
/// 12 s (= 10 s worst-case break + margin). The test asserts the break HAPPENS, not its latency.
#[test]
fn pause_breaks_a_running_cont() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;

        match backend
            .launch(launch_spec("wait"))
            .await
            .expect("launch wait")
        {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("launch wait must stop at the loader break, got {other:?}"),
        }

        // Free-running cont on the shared backend; a sibling task pauses it shortly after.
        let cont_backend = std::sync::Arc::clone(&backend);
        let cont_task = tokio::spawn(async move { cont_backend.cont(0).await });

        let pause_backend = std::sync::Arc::clone(&backend);
        let pause_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            pause_backend.pause().await.expect("pause");
        });

        // The pause must break the cont into a Stopped within the worst-case break bound (see the
        // timing math above): 12 s covers `break_loop`'s 50×200 ms = 10 s worst case plus margin.
        let stop = tokio::time::timeout(Duration::from_secs(12), cont_task)
            .await
            .expect("cont must break within the pause budget (no hang)")
            .expect("cont task joins")
            .expect("cont returns a stop");
        assert!(
            matches!(stop, StopOutcome::Stopped(_)),
            "pause must break the running cont into a Stopped, got {stop:?}"
        );
        pause_task.await.expect("pause task joins");

        backend.disconnect(true).await;
    });
}

/// Output + Terminated in the event stream: launch `normal`, drain `conn.events` on a sibling task,
/// `cont` to exit, then assert program-specific output reaches the stream as `BackendEvent::Output`
/// and a `BackendEvent::Terminated` arrives (the engine thread's teardown signal). This verifies
/// the 3.1-wired output sink + terminated signal carry through live.
///
/// NOTE on what "program output" means here: DbgEng's `IDebugOutputCallbacks` (the only output the
/// sink can see) carry the *engine's* output — module loads, break-instruction notices, symbol
/// diagnostics — not the debuggee's own console writes. The fixture is launched
/// `CREATE_NO_WINDOW | DEBUG_ONLY_THIS_PROCESS` (see `dbgeng-sys::Engine::launch`), so the
/// debuggee's `printf("compute(10) = 45")` goes to its (absent) console, NOT through the engine
/// output callbacks — DbgEng does not redirect debuggee stdout to the output callbacks on this
/// launch path. So we assert on a program-specific engine line that is guaranteed for THIS target:
/// the `ModLoad: … test_target.exe` line (the engine reporting our exe loading). That line is
/// emitted only because we launched *this* program, so it is genuine program-driven output and
/// proves the sink → `BackendEvent::Output` wiring carries live engine output end-to-end.
#[test]
fn program_output_and_terminated_reach_the_event_stream() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    runtime().block_on(async {
        let conn = WinDbgFactory::new()
            .connect()
            .await
            .expect("connect a live WinDbg backend");
        let backend = conn.backend;
        let mut events = conn.events;

        // Drain the event stream on a sibling task (the event-pump analog), forwarding each event
        // to a channel so the test thread can collect them after the run. The stream ends when the
        // output channel closes AND the terminated signal has fired.
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let drain = tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                let terminal = matches!(ev, BackendEvent::Terminated { .. });
                if ev_tx.send(ev).is_err() {
                    break;
                }
                if terminal {
                    break;
                }
            }
        });

        match backend.launch(launch_spec("normal")).await.expect("launch") {
            LaunchOutcome::Stopped(_) => {}
            other => panic!("launch must stop at the loader break, got {other:?}"),
        }

        // Run to exit (no breakpoints → compute() runs, printf emits, process exits 0).
        match backend.cont(0).await.expect("cont to exit") {
            StopOutcome::Exited { code } => assert_eq!(code, Some(0)),
            other => panic!("cont should run to exit, got {other:?}"),
        }

        // Detaching drops the backend's command sender; closing the engine thread fires Terminated.
        backend.disconnect(false).await;
        drop(backend);

        // Collect ALL drained events into a Vec until the channel closes (the drain task ended and
        // dropped its sender) OR a bounded deadline elapses, THEN scan once. Draining-then-scanning
        // (rather than breaking on the first flag) means a closed channel never cuts the loop short
        // before `Terminated` is observed — the previous `Ok(None) => break` could race event
        // delivery and break before the terminal event was processed.
        let mut collected: Vec<BackendEvent> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, ev_rx.recv()).await {
                Ok(Some(ev)) => collected.push(ev),
                Ok(None) => break, // channel closed: the drain task ended — we have every event
                Err(_) => break,   // overall deadline elapsed
            }
        }

        // Bounded join of the drain task instead of `abort()` (which would race event delivery): it
        // breaks on its own once it sees Terminated or the stream ends; give it a second to finish.
        let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;

        // Scan the collected events once for the program-driven Output and the Terminated.
        let saw_output = collected.iter().any(|ev| {
            matches!(
                ev,
                // Program-specific engine output: the engine's ModLoad line naming OUR exe. See the
                // test's NOTE on why debuggee `printf` stdout is not visible to the sink.
                BackendEvent::Output { text, .. } if text.to_ascii_lowercase().contains("test_target")
            )
        });
        let saw_terminated = collected
            .iter()
            .any(|ev| matches!(ev, BackendEvent::Terminated { .. }));

        assert!(
            saw_output,
            "program-specific engine output (the test_target ModLoad line) must appear as a \
             BackendEvent::Output; collected {collected:?}"
        );
        assert!(
            saw_terminated,
            "a BackendEvent::Terminated must arrive on the event stream; collected {collected:?}"
        );
    });
}
