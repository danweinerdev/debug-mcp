//! Live conditional-breakpoint tests (Windows): the Phase-5 `wait_for_event` re-loop driven
//! end-to-end against the built fixture `testdata/win/test_target.exe`.
//!
//! The fixture's `compute(int n)` runs `for (int i = 0; i < n; i++) sum += i;` (the `sum += i;` is
//! a source line of its own — see `testdata/win/test_target.c`). A SOURCE line breakpoint there
//! with condition `i == 5` is the canonical conditional target: it must surface a stop only on the
//! iteration where `i == 5`, never on `i == 0..4`. These tests prove:
//!
//! 1. A true condition fires only when true, at `i == 5` (the earlier iterations were skipped).
//! 2. An unresolvable condition (garbage variable) never fires — the eval-fails-→-false footgun —
//!    so `go` runs the target to exit.
//! 3. An unconditional breakpoint still stops (the empty-conditions fast-path regression guard).
//!
//! ## Symbol warm-up
//!
//! At the loader break right after `launch`, the line table for `compute` is not yet loaded, so a
//! `file:line` breakpoint fails to resolve (`GetOffsetByLine` → 0x80004005). The realistic
//! workflow — and what these tests do — is to first hit `compute` by *function* name (which the
//! launch-time `Reload /f` makes resolvable), then set the `file:line` breakpoint once symbols are
//! warm. The transient function breakpoint is removed before the conditional run so it cannot mask
//! the result.
//!
//! DbgEng keeps process-global state, so every test that drives a target serializes behind a
//! single per-file mutex (independent of `--test-threads`). Each test skips cleanly if the fixture
//! has not been built (`testdata/win/build.bat`). Every `go` is bounded so a regression cannot hang.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Mutex;

use dbgeng_sys::{BpLoc, Engine, LaunchReq};
use debugger_core::StopOutcome;

/// Serializes the live tests — only one DbgEng session at a time per process.
static LIVE: Mutex<()> = Mutex::new(());

/// The fixture source line of the `sum += i;` statement inside `compute`'s loop. A breakpoint here
/// is hit once per loop iteration, so a condition on `i` is meaningful (a function breakpoint on
/// `compute` would only fire once, on entry, before the loop body runs). Kept in sync with
/// `testdata/win/test_target.c`.
const SUM_LINE: u32 = 23;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/win/test_target.exe")
}

/// The fixture source file name. DbgEng's `GetOffsetByLine` matches against the source file by its
/// recorded name; once the module's line table is loaded (see the symbol warm-up note), the bare
/// `test_target.c` resolves — which is exactly what an agent would pass.
fn source_file() -> &'static str {
    "test_target.c"
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

fn launch_req() -> LaunchReq {
    LaunchReq {
        program: fixture().to_string_lossy().into_owned(),
        args: vec!["normal".to_string()],
        cwd: None,
    }
}

/// Launch `normal`, hit `compute` by function name (warming the module's line table), then remove
/// that transient function breakpoint. Leaves the engine stopped at `compute`'s entry, with
/// `GetOffsetByLine` now resolvable for the `sum += i;` line. Panics (failing the test) if the
/// function breakpoint is never hit — that is the precondition every conditional test relies on.
fn warm_at_compute(engine: &mut Engine) {
    engine.launch(&launch_req()).expect("launch normal");

    let fn_bp = engine
        .set_breakpoint(&BpLoc::Function("compute".to_string()), "")
        .expect("set warm-up breakpoint on compute");
    let outcome = engine.go(10_000).expect("go to compute (warm-up)");
    match outcome {
        Some(StopOutcome::Stopped(_)) => {}
        other => panic!("warm-up: expected to stop at compute, got {other:?}"),
    }
    // Drop the transient function breakpoint so it cannot mask the conditional line breakpoint's
    // behavior on the subsequent run.
    engine
        .remove_breakpoint(fn_bp.id)
        .expect("remove warm-up breakpoint");
}

fn line_loc() -> BpLoc {
    BpLoc::FileLine {
        file: source_file().to_string(),
        line: SUM_LINE,
    }
}

/// A conditional breakpoint inside `compute`'s loop with `i == 5` must surface a stop ONLY on the
/// iteration where `i == 5` — proving the engine resumed (skipped) `i == 0..4` rather than stopping
/// on the first hit. We confirm by evaluating `i` at the stop and asserting it reads 5: if the
/// re-loop were broken, the stop would land at `i == 0` (the first hit) and the evaluation would
/// not read 5.
#[test]
fn conditional_bp_fires_only_when_true() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    warm_at_compute(&mut engine);

    let bp = engine
        .set_breakpoint(&line_loc(), "i == 5")
        .expect("set conditional line breakpoint");
    assert!(bp.verified, "the conditional line breakpoint should verify");

    // Bounded: `compute(10)` reaches `i == 5` quickly; 10 s is generous and the re-loop cannot run
    // past the loop, so a broken condition (always-stop) just lands at the wrong `i`, never hangs.
    let outcome = engine.go(10_000).expect("go to conditional breakpoint");
    let info = match outcome {
        Some(StopOutcome::Stopped(info)) => info,
        other => panic!("expected to stop at the conditional breakpoint, got {other:?}"),
    };

    // The stop should reference the conditional breakpoint (by id or a break-flavored reason). The
    // `reason` fallback covers DbgEng builds that don't populate `hit_breakpoint_ids`; the
    // authoritative proof that the condition fired at i==5 is the `evaluate("i") == 5` check below,
    // not this referencing check.
    let referenced =
        info.hit_breakpoint_ids.contains(&bp.id) || info.reason.to_lowercase().contains("break");
    assert!(
        referenced,
        "the stop should reference the conditional breakpoint (ids {:?} or reason {:?})",
        info.hit_breakpoint_ids, info.reason
    );

    // The decisive assertion: `i` is 5 at the stop. DbgEng's `??` rendering of an int shows the
    // decimal (`0n5`) and/or hex (`0x5`). This proves iterations i==0..4 were resumed (skipped),
    // not stopped on.
    let eval = engine
        .evaluate("i")
        .expect("evaluate i at the conditional stop");
    eprintln!("conditional stop: i evaluates to {:?}", eval.result);
    assert!(
        eval.result.contains("0n5") || eval.result.contains("0x5") || eval.result.trim() == "5",
        "at the `i == 5` conditional stop, evaluating `i` should read 5 (e.g. `0n5`/`0x5`), \
         proving i==0..4 were skipped, got {:?}",
        eval.result
    );

    let _ = engine.detach(false);
}

/// An unresolvable condition (a garbage variable not in scope) must NEVER surface a stop: the
/// `Evaluate` call fails, which the re-loop treats as "condition not met" and resumes. So `go`
/// runs the target to completion and returns `Exited`, NOT `Stopped` at that breakpoint. This is
/// the documented eval-fails-→-false footgun. Bounded at a finite timeout so a re-loop bug (which
/// would keep resuming forever, or fall through to a spurious stop) is caught rather than hanging.
#[test]
fn unresolvable_condition_never_fires_runs_to_exit() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    warm_at_compute(&mut engine);

    let bp = engine
        .set_breakpoint(&line_loc(), "nonexistent_xyz == 1")
        .expect("set unresolvable-condition line breakpoint");
    assert!(bp.verified, "the breakpoint location itself should verify");

    // `compute(10)` plus CRT teardown finishes well within 10 s. If the re-loop ever surfaced this
    // breakpoint we'd get `Stopped`; if it hung resuming forever the bounded `go` returns `None`
    // (still running) — both are failures. The correct behavior is `Exited`.
    let outcome = engine
        .go(10_000)
        .expect("go with an unresolvable condition");
    match outcome {
        Some(StopOutcome::Exited { code }) => {
            assert_eq!(
                code,
                Some(0),
                "the target should exit 0 (the unresolvable-condition BP was skipped every hit)"
            );
        }
        Some(StopOutcome::Stopped(info)) => {
            panic!("an unresolvable condition must never surface a stop, but go stopped: {info:?}")
        }
        Some(StopOutcome::Terminated) => {
            // Process terminated rather than exiting cleanly. Acceptable: the conditional BP was
            // never surfaced (no `Stopped`), which is the property under test.
        }
        None => panic!(
            "go returned still-running: the unresolvable-condition re-loop did not run to exit \
             within the bound (possible resume-forever bug)"
        ),
    }

    let _ = engine.detach(false);
}

/// Regression guard for the empty-/no-condition fast path: a breakpoint with NO stored condition
/// still stops normally (the re-loop's `!breakpoint_conditions.is_empty()` gate is bypassed
/// entirely when no conditions exist). A function breakpoint on `compute` (unconditional) must
/// stop on entry.
#[test]
fn unconditional_bp_still_stops() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req()).expect("launch normal");

    let bp = engine
        .set_breakpoint(&BpLoc::Function("compute".to_string()), "")
        .expect("set unconditional breakpoint on compute");
    assert!(bp.verified, "the unconditional breakpoint should verify");

    let outcome = engine.go(10_000).expect("go to unconditional breakpoint");
    match outcome {
        Some(StopOutcome::Stopped(info)) => {
            let referenced = info.hit_breakpoint_ids.contains(&bp.id)
                || info.reason.to_lowercase().contains("break");
            assert!(
                referenced,
                "the unconditional stop should reference the breakpoint (ids {:?} or reason {:?})",
                info.hit_breakpoint_ids, info.reason
            );
        }
        other => panic!("an unconditional breakpoint should stop, got {other:?}"),
    }

    let _ = engine.detach(false);
}
