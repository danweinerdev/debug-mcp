//! Live execution-control tests (Windows): go / step / break_in / InterruptHandle against the
//! built fixture `testdata/win/test_target.exe`.
//!
//! These prove task 2.4's surface end-to-end, including that task 2.3's `RemoveEngineOptions(
//! INITIAL_BREAK)` actually took effect (a `go` with no breakpoints runs to exit instead of
//! immediately re-breaking).
//!
//! DbgEng keeps process-global state, so every test that drives a target serializes behind a
//! single mutex (independent of `--test-threads`). Each test skips cleanly if the fixture has not
//! been built (`testdata/win/build.bat`).
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use dbgeng_sys::{Engine, InterruptHandle, LaunchReq};
use debugger_core::{StepKind, StopOutcome};

/// Serializes the live tests — only one DbgEng session at a time per process. Shared with the
/// lifecycle suite's intent (each integration-test binary has its own `LIVE`, and the two binaries
/// run in separate processes, so a per-file static is sufficient).
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

fn launch_req(mode: &str) -> LaunchReq {
    LaunchReq {
        program: fixture().to_string_lossy().into_owned(),
        args: if mode.is_empty() {
            vec![]
        } else {
            vec![mode.to_string()]
        },
        cwd: None,
    }
}

/// `launch` then `go()` with no breakpoints must run `test_target normal` to completion and exit
/// 0 — NOT immediately re-break. This is the end-to-end proof that task 2.3's
/// `RemoveEngineOptions(INITIAL_BREAK)` took effect (with INITIAL_BREAK still set, `go` would
/// re-break instantly) AND that `go`'s run-to-stop loop works.
#[test]
fn go_runs_to_exit_with_no_breakpoints() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("normal")).expect("launch");

    let outcome = engine.go(10_000).expect("go");
    match outcome {
        Some(StopOutcome::Exited { code }) => assert_eq!(
            code,
            Some(0),
            "test_target normal should exit 0 (INITIAL_BREAK removed, go ran to exit)"
        ),
        other => panic!("expected Exited(0) after go-to-exit, got {other:?}"),
    }
    // Detach after the process has exited is a no-op-ish cleanup; ignore any error.
    let _ = engine.detach();
}

/// `step` Over from the initial break must land on a real stop (not run to exit). A single
/// instruction step from the loader break stays inside the loader/CRT, so it is `Stopped`.
#[test]
fn step_over_from_initial_break_stops() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("normal")).expect("launch");

    let outcome = engine.step(StepKind::Over).expect("step over");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "a single step-over from the initial break should land Stopped, got {outcome:?}"
    );
    let _ = engine.detach();
}

/// `step` Into from the initial break must likewise land Stopped.
#[test]
fn step_into_from_initial_break_stops() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("normal")).expect("launch");

    let outcome = engine.step(StepKind::Into).expect("step into");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "a single step-into from the initial break should land Stopped, got {outcome:?}"
    );
    let _ = engine.detach();
}

/// `step` Out exercises the distinct `Execute("gu")` path (no `DEBUG_STATUS_STEP_OUT` exists).
/// From the loader break we first step *into* a frame, then step *out* of it; the exact landing
/// is loader/CRT-dependent, so we only assert the `gu` path runs end-to-end and returns a stop
/// (Stopped, or Exited if "go up" unwound to process exit) — never an error or a hang.
#[test]
fn step_out_runs_gu_and_returns_a_stop() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("normal")).expect("launch");

    // Get one frame deep so "step out" has a frame to return from.
    let _ = engine.step(StepKind::Into).expect("step into");
    let outcome = engine.step(StepKind::Out).expect("step out (gu)");
    assert!(
        matches!(
            outcome,
            StopOutcome::Stopped(_) | StopOutcome::Exited { .. }
        ),
        "step-out via `gu` should return a stop, got {outcome:?}"
    );
    let _ = engine.detach();
}

/// `go` with a short timeout against `test_target wait` (an infinite Sleep loop) must time out
/// still-running (`Ok(None)` — R3), after which `break_in()` must regain a real `Stopped`.
#[test]
fn go_times_out_still_running_then_break_in_recovers() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("wait")).expect("launch wait");

    // Short budget: the target never stops on its own, so this must hit the still-running path.
    let outcome = engine.go(700).expect("go (short timeout)");
    assert!(
        outcome.is_none(),
        "go on an infinitely-running target should return None (still running), got {outcome:?}"
    );

    // R3 recovery: break_in regains a proper break with context.
    let recovered = engine.break_in().expect("break_in");
    assert!(
        matches!(recovered, StopOutcome::Stopped(_)),
        "break_in should regain a Stopped after a still-running go, got {recovered:?}"
    );

    let _ = engine.detach();
}

/// An `InterruptHandle.interrupt()` from another thread while `go()` blocks on `test_target wait`
/// must break `go` out within ~1 s (flag-only path: the engine's 200 ms poll observes the flag
/// within one slice, then issues the real `SetInterrupt`). Proves the off-thread interrupt seam.
#[test]
fn interrupt_handle_breaks_running_go_from_another_thread() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("wait")).expect("launch wait");

    let handle: InterruptHandle = engine.interrupt_handle();
    // Off-thread interrupter: wait briefly so `go` is well inside its poll loop, then interrupt.
    let interrupter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        handle.interrupt();
    });

    // Generous overall budget; the interrupt should fire long before it.
    let started = Instant::now();
    let outcome = engine.go(10_000).expect("go");
    let elapsed = started.elapsed();

    interrupter.join().expect("join interrupter");

    assert!(
        matches!(outcome, Some(StopOutcome::Stopped(_))),
        "an off-thread interrupt should break go into a Stopped, got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "interrupt latency should be ~1s (flag poll ≤200ms + SetInterrupt break), was {elapsed:?}"
    );

    let _ = engine.detach();
}

/// After an interrupted `go`, a subsequent `go()` on a still-running target must NOT return paused
/// immediately: `go` resets the interrupt flag at entry, so the stale `true` from the prior run
/// cannot break the next run. The second `go` (short budget) therefore times out still-running.
#[test]
fn go_resets_interrupt_flag_at_entry() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req("wait")).expect("launch wait");

    // First go: interrupt it from another thread (leaves the flag `true` until the next reset).
    let handle = engine.interrupt_handle();
    let interrupter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        handle.interrupt();
    });
    let first = engine.go(10_000).expect("first go");
    interrupter.join().expect("join");
    assert!(
        matches!(first, Some(StopOutcome::Stopped(_))),
        "first go should be interrupted to Stopped, got {first:?}"
    );

    // Second go on the still-running target: the entry reset cleared the flag, so this must run
    // its full (short) budget and return still-running — NOT break immediately on the stale flag.
    let started = Instant::now();
    let second = engine.go(700).expect("second go");
    let elapsed = started.elapsed();
    assert!(
        second.is_none(),
        "after the flag reset, the second go should time out still-running, got {second:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(500),
        "the second go must consume its budget (proving no instant stale-flag break), was {elapsed:?}"
    );

    // Recover a context and detach cleanly.
    let _ = engine.break_in();
    let _ = engine.detach();
}
