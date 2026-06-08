//! Execution-control tests over a [`FakeEngine`](super::fake::FakeEngine) — `cont` (the
//! `Some(stop)` map and the `None`→`break_in` fallback), `step` (each `StepKind` is marshaled),
//! `cont` mid-flight cancellation (a cont aborted *after* its `Go` reached the engine, while
//! awaiting the reply, does not wedge the backend), and `disconnect`'s interrupt-then-detach
//! ordering. These need no live engine and run on any Windows host.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use debugger_core::{DebuggerBackend, StepKind, StopInfo, StopOutcome};

use crate::engine_ops::EngineOps;

use super::backend_over_fake;
use super::fake::{FakeEngine, Recorder};

/// A scripted `Stopped` outcome the fake returns from `go`/`step`/`break_in`.
fn stopped() -> StopOutcome {
    StopOutcome::Stopped(StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    })
}

/// `cont` marshals `Go { timeout_ms: u32::MAX }` and maps `Ok(Some(stop))` straight through to the
/// stop, without ever falling back to `break_in`. The thread_id argument is ignored (WinDbg `g`
/// resumes the whole target).
#[tokio::test]
async fn cont_maps_some_stop_through_with_infinite_budget() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(stopped(), recorder_for_fake)) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    let outcome = backend.cont(99).await.expect("cont returns a stop");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "cont maps Ok(Some(stop)) through, got {outcome:?}"
    );

    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.gos,
        vec![u32::MAX],
        "cont must marshal Go with the effectively-infinite budget"
    );
    assert_eq!(
        rec.break_ins, 0,
        "the Some(stop) path must NOT fall back to break_in"
    );
}

/// `cont`'s safety net: when `go` returns `Ok(None)` (the practically-unreachable ~u32::MAX
/// deadline), `cont` falls back to `break_in()` to regain a real stop-with-context and returns
/// that. The fake records both the `go` and the `break_in`.
#[tokio::test]
async fn cont_none_falls_back_to_break_in() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine {
            go_returns_none: true,
            recorder: recorder_for_fake,
            stop_outcome: stopped(),
            ..FakeEngine::default()
        }) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    let outcome = backend
        .cont(1)
        .await
        .expect("cont returns the fallback stop");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "the None fallback must return the break_in stop, got {outcome:?}"
    );

    let rec = recorder.lock().unwrap();
    assert_eq!(rec.gos, vec![u32::MAX], "cont marshals Go first");
    assert_eq!(
        rec.break_ins, 1,
        "Ok(None) must fall back to exactly one break_in"
    );
}

/// `step` marshals each neutral `StepKind` (Over/Into/Out) through to the engine and returns the
/// engine's stop. The fake records the kind so the mapping is provable; thread_id and gran are
/// ignored.
#[tokio::test]
async fn step_marshals_each_kind() {
    for kind in [StepKind::Over, StepKind::Into, StepKind::Out] {
        let recorder = Arc::new(Mutex::new(Recorder::default()));
        let recorder_for_fake = Arc::clone(&recorder);
        let (backend, _term) = backend_over_fake(move || {
            Ok(Box::new(FakeEngine::scripted(stopped(), recorder_for_fake)) as Box<dyn EngineOps>)
        })
        .await
        .expect("fake backend ready");

        let outcome = backend
            .step(kind, 99, None)
            .await
            .expect("step returns a stop");
        assert!(
            matches!(outcome, StopOutcome::Stopped(_)),
            "step maps the engine stop through, got {outcome:?}"
        );
        assert_eq!(
            recorder.lock().unwrap().steps,
            vec![kind],
            "step must marshal exactly the requested StepKind"
        );
    }
}

/// A `cont` cancelled *mid-await* — after its `Go` command has been dispatched to the engine
/// thread, while it is awaiting the reply — must NOT wedge the backend: a follow-up op still works.
///
/// This is the REAL hazard (a future that is merely built-and-dropped never runs its body, so it
/// would prove nothing): we `tokio::spawn` the `cont` so it is polled, wait until the fake's
/// `go_entered` flag confirms the `Go` actually reached the engine thread (i.e. the future polled
/// past `cmd_tx.send` and is now parked on `reply_rx`), then `abort()` it — dropping the reply
/// receiver while the engine still holds the reply sender. We then drive a fresh op (`pause`, then
/// `threads`/`cont`) on the SAME backend and assert it succeeds: the engine thread did not wedge
/// and the dropped reply sender poisoned nothing.
///
/// Behavioral note this guards: a `cont` cancelled mid-`go` leaves the target *running* — the
/// engine keeps polling `go` to completion and discards the orphaned reply. Recovery is via
/// `pause` (trip the interrupt flag, exactly as for lldb), after which the session is usable again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_cont_midflight_does_not_wedge_the_backend() {
    use std::sync::Arc as StdArc;

    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(stopped(), recorder_for_fake)) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");
    let backend = StdArc::new(backend);

    // Spawn the cont so its body actually runs (a built-but-dropped future never polls). It sends
    // the `Go` and then awaits the reply.
    let cont_backend = StdArc::clone(&backend);
    let handle = tokio::spawn(async move { cont_backend.cont(1).await });

    // Let the spawned task make progress: send the `Go` and reach the reply await. We confirm the
    // command genuinely reached the engine by polling the fake's `go_entered` flag — without this,
    // an abort could land before the engine ever saw the command and the test would not prove a
    // mid-await cancellation. Bounded yield loop (no arbitrary sleep): yield until `go_entered`.
    for _ in 0..10_000 {
        if recorder.lock().unwrap().go_entered {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        recorder.lock().unwrap().go_entered,
        "the Go command must have reached the engine thread before we abort (otherwise this is \
         not a mid-await cancellation)"
    );

    // Cancel the in-flight cont: drop the reply receiver while the engine holds the sender.
    handle.abort();

    // Recovery: `pause` (the documented recovery path) then a follow-up op against the SAME backend
    // must succeed — the engine thread is not wedged and nothing was poisoned by the dropped reply.
    backend.pause().await.expect("pause after a cancelled cont");
    let threads = backend
        .threads()
        .await
        .expect("a follow-up op still works after a cancelled cont");
    // (FakeEngine has no scripted threads; the point is that the call round-trips, not its content.)
    assert!(
        threads.is_empty(),
        "the fake reports no threads; the follow-up op round-tripped, got {threads:?}"
    );
}

/// `disconnect` sets the interrupt flag BEFORE marshaling the detach. This is the fix for a
/// free-running `cont`'s `go` blocking the engine thread: the flag trips first so a running `go`
/// breaks and the engine thread loops back to process the queued `Detach`. We observe both the
/// flag flip (the fake shares it) and the detach (the recorder logs it).
#[tokio::test]
async fn disconnect_interrupts_before_detaching() {
    use std::sync::atomic::AtomicBool;

    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_fake = Arc::clone(&flag);
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine {
            interrupt_flag: flag_for_fake,
            recorder: recorder_for_fake,
            ..FakeEngine::default()
        }) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    assert!(!flag.load(Ordering::Acquire), "flag starts clear");

    backend.disconnect(true).await;

    // The interrupt flag was set (the engine thread's detach does not clear it — only `go` resets
    // it at entry, and no `go` ran here).
    assert!(
        flag.load(Ordering::Acquire),
        "disconnect must set the interrupt flag before detaching"
    );
    let rec = recorder.lock().unwrap();
    // The detach was marshaled, carrying the terminate flag.
    assert_eq!(
        rec.detaches,
        vec![true],
        "disconnect must still marshal the detach after interrupting"
    );
    // ORDERING proof: the flag was ALREADY `true` at the moment `detach` ran — i.e. `disconnect`
    // tripped the interrupt before marshaling `Detach`, not after. This would catch a regression
    // that reordered the two (the post-condition assertions above would still pass if detach ran
    // first, but this would not).
    assert_eq!(
        rec.interrupt_flag_at_detach,
        vec![true],
        "the interrupt flag must already be set when detach runs (interrupt-then-detach ordering)"
    );
}
