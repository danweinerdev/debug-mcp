//! Marshaling / thread-lifecycle tests over a [`FakeEngine`] (no live engine, no COM on the
//! calling thread). Covers the task-3.1 round-trip, readiness success/failure, the no-COM
//! invariant, and teardown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use debugger_core::{BackendError, DebuggerBackend, ThreadInfo};

use crate::engine_ops::EngineOps;
use crate::error::EngineError;
use crate::thread::EngineCmd;

use super::fake::FakeEngine;
use super::{backend_over_fake, ok_constructor};

/// An `EngineCmd` round-trips: build a backend over a `FakeEngine` with a scripted thread list,
/// marshal `Threads` through `call(...)`, and assert the fake's canned reply comes back.
#[tokio::test]
async fn engine_cmd_round_trips_through_call() {
    let scripted = vec![
        ThreadInfo {
            id: 7,
            name: "sys=101".to_string(),
        },
        ThreadInfo {
            id: 9,
            name: "sys=102".to_string(),
        },
    ];
    let scripted_for_fake = scripted.clone();
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::with_threads(scripted_for_fake)) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    // Marshal the Threads op directly through the `call` primitive (the 3.2–3.4 ops build on it).
    let got = backend
        .call(|reply| EngineCmd::Threads { reply })
        .await
        .expect("threads reply");
    assert_eq!(got, scripted);
}

/// `connect()`-style readiness: a fake constructor that returns `Err` surfaces that error
/// (the factory maps it to `BackendError::Detect`/`Spawn`); a successful one yields a ready
/// backend.
#[tokio::test]
async fn readiness_failure_surfaces_construction_error() {
    let result = backend_over_fake(|| {
        Err(EngineError::engine(
            "failed to initialize DbgEng: scripted create failure",
        ))
    })
    .await;
    match result {
        Ok(_) => panic!("a failing constructor must surface its error at readiness"),
        Err(err) => assert!(
            err.to_string().contains("scripted create failure"),
            "the construction error should carry through readiness: {err}"
        ),
    }
}

#[tokio::test]
async fn readiness_success_yields_a_ready_backend() {
    let (backend, _term) = backend_over_fake(ok_constructor)
        .await
        .expect("a successful constructor yields a ready backend");
    // The ready backend answers a marshaled op (default fake → empty thread list).
    let threads = backend
        .call(|reply| EngineCmd::Threads { reply })
        .await
        .expect("threads reply");
    assert!(threads.is_empty());
}

/// No COM on the calling thread: spawn the engine thread (with a `FakeEngine`) from a normal
/// tokio worker that is NOT `CoInitialize`d, and assert it does not fault and the backend is
/// ready and serving — proving `connect()`/the spawn make no COM calls on the caller.
#[tokio::test]
async fn spawn_and_serve_with_no_com_on_calling_thread() {
    // The strongest guarantee that the caller makes no COM call is structural: `windbg-backend`
    // is `#![forbid(unsafe_code)]`, so it CANNOT call `CoInitializeEx`/any COM API — all COM is in
    // `dbgeng-sys`, reached only on the engine thread. This test exercises the runtime path: from a
    // non-COM-initialized tokio worker, the spawn + readiness await + a marshaled op all succeed
    // (the fake does no COM at all, so a stray `CoInitializeEx` on the caller is impossible to
    // observe here — the `forbid(unsafe_code)` attribute is the real enforcer).
    let (backend, _term) = backend_over_fake(ok_constructor)
        .await
        .expect("backend ready with no COM on the caller");
    let modules = backend
        .modules()
        .await
        .expect("a marshaled op succeeds from a non-COM caller thread");
    assert!(modules.is_empty());
}

/// Teardown: dropping the backend (closing the command channel) ends the engine thread without
/// hanging — the terminated signal arrives (the thread reached its teardown after detach).
#[tokio::test]
async fn dropping_backend_terminates_the_thread() {
    let (backend, term_rx) = backend_over_fake(ok_constructor)
        .await
        .expect("fake backend ready");

    // Drop the backend → drops the only EngineCmd sender → the engine thread's blocking_recv
    // returns None → it detaches and fires the terminated signal.
    drop(backend);

    // Awaiting the terminated oneshot must resolve (not hang). A bounded timeout guards against a
    // regression where the thread fails to exit.
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), term_rx)
        .await
        .expect("the engine thread must terminate without hanging")
        .expect("the terminated signal fires on teardown");
    assert_eq!(code, None, "the normal teardown carries no exit code");
}

/// `pause()` sets the shared interrupt flag (the flag-only design) — observable through the fake's
/// `Arc<AtomicBool>`.
#[tokio::test]
async fn pause_sets_the_interrupt_flag() {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_fake = Arc::clone(&flag);
    let (backend, _term) = backend_over_fake(move || {
        let fake = FakeEngine {
            interrupt_flag: flag_for_fake,
            ..FakeEngine::default()
        };
        Ok(Box::new(fake) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    assert!(!flag.load(Ordering::Acquire), "flag starts clear");
    backend.pause().await.expect("pause is infallible");
    assert!(
        flag.load(Ordering::Acquire),
        "pause must set the shared interrupt flag"
    );
}

/// A placeholder op (owned by a later task) surfaces the documented not-yet-implemented error,
/// so the trait compiles all-or-nothing and the seam is honored.
#[tokio::test]
async fn placeholder_op_reports_not_yet_implemented() {
    let (backend, _term) = backend_over_fake(ok_constructor)
        .await
        .expect("fake backend ready");
    let err = backend
        .cont(1)
        .await
        .expect_err("cont is a 3.3 placeholder");
    match err {
        BackendError::Send(msg) => assert!(msg.contains("phase 3.3"), "{msg}"),
        other => panic!("expected a Send placeholder error, got {other:?}"),
    }
}
