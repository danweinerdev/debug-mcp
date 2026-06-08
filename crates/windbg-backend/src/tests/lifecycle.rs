//! Lifecycle tests over a [`FakeEngine`](super::fake::FakeEngine) — `launch` (incl. the pending-
//! breakpoint flush), `attach` (pid mapping + `wait_for` resolution), and `disconnect`'s
//! kill-vs-detach choice. These need no live engine and run on any Windows host (the analog of
//! `lldb-backend`'s scripted-peer tests).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use debugger_core::{
    AttachOutcome, AttachSpec, DebuggerBackend, FunctionBp, LaunchOutcome, LaunchSpec, SourceBp,
    StopInfo, StopOutcome,
};

use crate::backend::poll_for_process;
use crate::engine_ops::EngineOps;

use super::backend_over_fake;
use super::fake::{FakeEngine, RecordedBp, Recorder};

/// A `LaunchSpec` with one source breakpoint and one function breakpoint, plus the given
/// `stop_on_entry`. The breakpoints are what the launch flush must marshal to the engine.
fn launch_spec(stop_on_entry: bool) -> LaunchSpec {
    LaunchSpec {
        program: "C:\\fixtures\\normal.exe".to_string(),
        args: vec![],
        cwd: None,
        env: vec![],
        stop_on_entry,
        source_breakpoints: vec![(
            "main.c".to_string(),
            vec![SourceBp {
                line: 42,
                condition: "x > 0".to_string(),
            }],
        )],
        function_breakpoints: vec![FunctionBp {
            name: "compute".to_string(),
            condition: String::new(),
        }],
    }
}

/// `launch` on a `Stopped` fake returns `LaunchOutcome::Stopped` AND flushes the spec's pending
/// breakpoints (launch flushes — attach does not). Even with `stop_on_entry == false`, WinDbg
/// always stops at the loader break, so the outcome is `Stopped`.
#[tokio::test]
async fn launch_returns_stopped_and_flushes_breakpoints() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let stop = StopOutcome::Stopped(StopInfo {
        reason: "Initial breakpoint".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    });
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(stop, recorder_for_fake)) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    // stop_on_entry == false, yet WinDbg always stops at the loader break.
    let outcome = backend
        .launch(launch_spec(false))
        .await
        .expect("launch succeeds");
    assert!(
        matches!(outcome, LaunchOutcome::Stopped(_)),
        "WinDbg launch always stops at the loader break, got {outcome:?}"
    );

    // The pending breakpoints were flushed: the source line and the function, with conditions.
    let recorded = recorder.lock().unwrap().breakpoints.clone();
    assert_eq!(
        recorded,
        vec![
            (
                RecordedBp::FileLine {
                    file: "main.c".to_string(),
                    line: 42,
                },
                "x > 0".to_string()
            ),
            (RecordedBp::Function("compute".to_string()), String::new()),
        ],
        "launch must flush the spec's source + function breakpoints"
    );
}

/// `launch` on an `Exited` fake maps to `LaunchOutcome::Exited` and does NOT flush breakpoints
/// (there is no stopped target to set them on).
#[tokio::test]
async fn launch_exited_maps_through_and_skips_flush() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(
            StopOutcome::Exited { code: Some(3) },
            recorder_for_fake,
        )) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    let outcome = backend
        .launch(launch_spec(true))
        .await
        .expect("launch returns");
    match outcome {
        LaunchOutcome::Exited { code } => assert_eq!(code, Some(3)),
        other => panic!("expected Exited, got {other:?}"),
    }
    assert!(
        recorder.lock().unwrap().breakpoints.is_empty(),
        "an exited launch has no target to flush breakpoints onto"
    );
}

/// `disconnect(true)` and `disconnect(false)` marshal `Detach{terminate:…}` with the right flag —
/// observable through the recorder's `detaches` log.
#[tokio::test]
async fn disconnect_marshals_terminate_flag() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let recorder_for_fake = Arc::clone(&recorder);
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine {
            recorder: recorder_for_fake,
            ..FakeEngine::default()
        }) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    backend.disconnect(true).await;
    backend.disconnect(false).await;

    assert_eq!(
        recorder.lock().unwrap().detaches,
        vec![true, false],
        "disconnect must carry its terminate flag through to detach"
    );
}

/// `attach` by pid marshals `AttachPid`, maps the engine `Stopped` → `AttachOutcome::Stopped`, and
/// records the supplied pid for `debugger_pid`.
#[tokio::test]
async fn attach_by_pid_maps_through_and_records_pid() {
    let stop = StopOutcome::Stopped(StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    });
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(stop, recorder)) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    assert_eq!(backend.debugger_pid(), None, "no pid before attach");
    let outcome = backend
        .attach(AttachSpec {
            pid: Some(4242),
            wait_for: None,
        })
        .await
        .expect("attach by pid");
    assert!(
        matches!(outcome, AttachOutcome::Stopped(_)),
        "attach maps the engine Stopped through, got {outcome:?}"
    );
    assert_eq!(
        backend.debugger_pid(),
        Some(4242),
        "attach records the supplied pid"
    );
}

/// `attach` maps an engine `Exited`/`Terminated` onto the matching `AttachOutcome`.
#[tokio::test]
async fn attach_exited_and_terminated_map_through() {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let (exited, _t1) = backend_over_fake({
        let rec = Arc::clone(&recorder);
        move || {
            Ok(Box::new(FakeEngine::scripted(
                StopOutcome::Exited { code: Some(7) },
                rec,
            )) as Box<dyn EngineOps>)
        }
    })
    .await
    .expect("fake backend ready");
    match exited
        .attach(AttachSpec {
            pid: Some(1),
            wait_for: None,
        })
        .await
        .expect("attach")
    {
        AttachOutcome::Exited { code } => assert_eq!(code, Some(7)),
        other => panic!("expected Exited, got {other:?}"),
    }

    let (terminated, _t2) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(
            StopOutcome::Terminated,
            Arc::new(Mutex::new(Recorder::default())),
        )) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");
    match terminated
        .attach(AttachSpec {
            pid: Some(1),
            wait_for: None,
        })
        .await
        .expect("attach")
    {
        AttachOutcome::Terminated => {}
        other => panic!("expected Terminated, got {other:?}"),
    }
}

/// `attach` with `wait_for` drives the full arm end-to-end (`resolve_wait_for` → `poll_for_process`
/// → the real `find_process_by_name` → `AttachPid`) with no spawned helper, by waiting for THIS
/// test process's own exe — guaranteed present in the Toolhelp32 snapshot. Proves the `wait_for`
/// branch is wired (the `poll_for_process` loop logic itself is unit-tested separately below).
#[tokio::test]
async fn attach_wait_for_resolves_a_live_process_and_records_pid() {
    let exe = std::env::current_exe().expect("current exe path");
    let name = exe
        .file_name()
        .expect("exe file name")
        .to_string_lossy()
        .into_owned();

    let stop = StopOutcome::Stopped(StopInfo {
        reason: "breakpoint".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    });
    let (backend, _term) = backend_over_fake(move || {
        Ok(Box::new(FakeEngine::scripted(
            stop,
            Arc::new(Mutex::new(Recorder::default())),
        )) as Box<dyn EngineOps>)
    })
    .await
    .expect("fake backend ready");

    let outcome = backend
        .attach(AttachSpec {
            pid: None,
            wait_for: Some(name),
        })
        .await
        .expect("attach via wait_for resolves the running test process");
    assert!(
        matches!(outcome, AttachOutcome::Stopped(_)),
        "attach(wait_for) maps the engine Stopped through, got {outcome:?}"
    );
    assert!(
        backend.debugger_pid().is_some(),
        "attach(wait_for) records the resolved pid"
    );
}

/// `wait_for` polling: a scripted lookup that returns `None` a few times then `Some(pid)` resolves;
/// a lookup that never matches times out with the documented error. This unit-tests the poll loop
/// with an injected lookup (no live process needed). Uses tiny intervals/timeouts so the test runs
/// in real time without the tokio `test-util` clock.
#[tokio::test]
async fn wait_for_resolves_then_times_out() {
    use std::sync::atomic::{AtomicU32, Ordering};

    // Resolves on the 3rd poll.
    let calls = AtomicU32::new(0);
    let lookup = |_name: &str| -> Option<u32> {
        if calls.fetch_add(1, Ordering::SeqCst) >= 2 {
            Some(9999)
        } else {
            None
        }
    };
    let pid = poll_for_process(
        "target.exe",
        lookup,
        Duration::from_millis(1),
        Duration::from_secs(5),
    )
    .await
    .expect("wait_for resolves once the process appears");
    assert_eq!(pid, 9999);

    // Never matches → the documented timeout error (a short bound so the test is fast).
    let err = poll_for_process(
        "ghost.exe",
        |_name: &str| None,
        Duration::from_millis(1),
        Duration::from_millis(20),
    )
    .await
    .expect_err("wait_for must time out when nothing appears");
    assert!(
        err.to_string()
            .contains("wait_for: no process named 'ghost.exe' appeared"),
        "unexpected error: {err}"
    );
}
