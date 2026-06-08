//! Marshal/mapping tests for the capability-gated WinDbg-only verbs `open_dump`, `attach_kernel`,
//! and `analyze` over a [`FakeEngine`](super::fake::FakeEngine) — no live engine, no COM on the
//! calling thread. Each drives a `WinDbgBackend` op (the real marshaling path through `call`) and
//! asserts the neutral result + the argument the engine received.
//!
//! `modules` is covered by `tests/marshal.rs` (the no-COM round-trip) and the live suite; here we
//! pin the three verbs added in task 4.2: the direct `open_dump` marshal, the
//! `StopOutcome → AttachOutcome` mapping `attach_kernel` shares with `attach`, the `analyze`
//! marshal, the non-`Closed` engine-error path (`map_engine_err` → `BackendError::Dap`) for
//! `open_dump`/`analyze`, a `Closed`-channel propagation case, and the factory's all-four-true
//! capabilities.

use std::sync::{Arc, Mutex};

use debugger_core::{
    AttachOutcome, BackendCapabilities, BackendError, BackendFactory, DebuggerBackend, DumpOutcome,
    StopInfo, StopOutcome,
};

use crate::backend::WinDbgBackend;
use crate::engine_ops::EngineOps;
use crate::error::EngineError;
use crate::factory::WinDbgFactory;

use super::backend_over_fake;
use super::fake::{FakeEngine, Recorder};

/// Build a backend over a `FakeEngine` produced by `customize` (which receives a default fake to
/// tweak) and a shared recorder the test reads back. Returns the backend + the recorder clone.
/// (Mirrors `inspection.rs::backend_with`, kept local so the two modules stay independent.)
async fn backend_with(
    customize: impl FnOnce(&mut FakeEngine) + Send + 'static,
) -> (WinDbgBackend, Arc<Mutex<Recorder>>) {
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

/// `open_dump` marshals the dump path verbatim to the engine and returns the engine's neutral
/// `DumpOutcome` (its `stop` + `crash_location`) straight through.
#[tokio::test]
async fn open_dump_marshals_path_and_returns_outcome() {
    let scripted = DumpOutcome {
        stop: Some(StopInfo {
            reason: "exception".to_string(),
            thread_id: 3,
            description: "Access violation".to_string(),
            hit_breakpoint_ids: Vec::new(),
        }),
        crash_location: Some("C:\\src\\crash.c:42".to_string()),
    };
    let scripted_for_fake = scripted.clone();
    let (backend, recorder) = backend_with(move |f| f.dump_outcome = Ok(scripted_for_fake)).await;

    let outcome = backend
        .open_dump("C:\\dumps\\crash.dmp")
        .await
        .expect("open_dump");
    assert_eq!(
        outcome, scripted,
        "the engine DumpOutcome maps straight through"
    );

    // The engine received the dump path verbatim.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.open_dumps,
        vec!["C:\\dumps\\crash.dmp".to_string()],
        "open_dump must marshal the dump path verbatim"
    );
}

/// `attach_kernel` marshals the connection string verbatim and maps an engine `StopOutcome::Stopped`
/// → `AttachOutcome::Stopped`, preserving the stop info.
#[tokio::test]
async fn attach_kernel_marshals_connection_and_maps_stopped() {
    let stop = StopInfo {
        reason: "break".to_string(),
        thread_id: 1,
        description: String::new(),
        hit_breakpoint_ids: Vec::new(),
    };
    let stop_for_fake = stop.clone();
    let (backend, recorder) =
        backend_with(move |f| f.stop_outcome = StopOutcome::Stopped(stop_for_fake)).await;

    let outcome = backend
        .attach_kernel("net:port=50000,key=1.2.3.4")
        .await
        .expect("attach_kernel");
    assert_eq!(
        outcome,
        AttachOutcome::Stopped(stop),
        "Stopped maps to AttachOutcome::Stopped, preserving the stop info"
    );

    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.attach_kernels,
        vec!["net:port=50000,key=1.2.3.4".to_string()],
        "attach_kernel must marshal the connection string verbatim"
    );
}

/// `attach_kernel` maps an engine `StopOutcome::Exited{code}` → `AttachOutcome::Exited{code}`,
/// preserving the exit code.
#[tokio::test]
async fn attach_kernel_maps_exited() {
    let (backend, recorder) =
        backend_with(|f| f.stop_outcome = StopOutcome::Exited { code: Some(7) }).await;

    let outcome = backend
        .attach_kernel("net:port=50000,key=k")
        .await
        .expect("attach_kernel");
    assert_eq!(
        outcome,
        AttachOutcome::Exited { code: Some(7) },
        "Exited maps to AttachOutcome::Exited, preserving the code"
    );

    // The connection string reaches the engine regardless of the returned outcome variant.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.attach_kernels,
        vec!["net:port=50000,key=k".to_string()],
        "attach_kernel must marshal the connection string verbatim even on Exited"
    );
}

/// `attach_kernel` maps an engine `StopOutcome::Terminated` → `AttachOutcome::Terminated`.
#[tokio::test]
async fn attach_kernel_maps_terminated() {
    let (backend, recorder) = backend_with(|f| f.stop_outcome = StopOutcome::Terminated).await;

    let outcome = backend
        .attach_kernel("net:port=50000,key=k")
        .await
        .expect("attach_kernel");
    assert_eq!(
        outcome,
        AttachOutcome::Terminated,
        "Terminated maps to AttachOutcome::Terminated"
    );

    // The connection string reaches the engine regardless of the returned outcome variant.
    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.attach_kernels,
        vec!["net:port=50000,key=k".to_string()],
        "attach_kernel must marshal the connection string verbatim even on Terminated"
    );
}

/// `analyze` marshals the `Analyze` op to the engine and returns its scripted `!analyze -v` text.
#[tokio::test]
async fn analyze_marshals_and_returns_text() {
    let (backend, recorder) =
        backend_with(|f| f.analyze_result = Ok("BUGCHECK_CODE: c0000005".to_string())).await;

    let text = backend.analyze().await.expect("analyze");
    assert_eq!(text, "BUGCHECK_CODE: c0000005");

    let rec = recorder.lock().unwrap();
    assert_eq!(
        rec.analyzes, 1,
        "analyze must marshal the Analyze op exactly once"
    );
}

/// `open_dump` surfaces a non-`Closed` engine error as `BackendError::Dap`: a scripted
/// `Err(EngineError::engine(..))` from the engine flows through `call`'s `map_engine_err`, carrying
/// the engine's verbatim message in the neutral "the debugger reported an error" channel.
#[tokio::test]
async fn open_dump_engine_error_maps_to_dap() {
    let (backend, _recorder) = backend_with(|f| {
        f.dump_outcome = Err(EngineError::engine("dump file is corrupt"));
    })
    .await;

    let err = backend
        .open_dump("C:\\dumps\\bad.dmp")
        .await
        .expect_err("a scripted engine error must surface as an error");
    assert!(
        matches!(err, BackendError::Dap { .. }),
        "a non-Closed engine error maps to BackendError::Dap: {err:?}"
    );
    assert!(
        err.to_string().contains("dump file is corrupt"),
        "the engine's verbatim message is carried through: {err}"
    );
}

/// `analyze` surfaces a non-`Closed` engine error as `BackendError::Dap`: the same `map_engine_err`
/// path as `open_dump`, driven by a scripted `Err(EngineError::engine(..))` from the engine.
#[tokio::test]
async fn analyze_engine_error_maps_to_dap() {
    let (backend, _recorder) = backend_with(|f| {
        f.analyze_result = Err(EngineError::engine("!analyze failed"));
    })
    .await;

    let err = backend
        .analyze()
        .await
        .expect_err("a scripted engine error must surface as an error");
    assert!(
        matches!(err, BackendError::Dap { .. }),
        "a non-Closed engine error maps to BackendError::Dap: {err:?}"
    );
    assert!(
        err.to_string().contains("!analyze failed"),
        "the engine's verbatim message is carried through: {err}"
    );
}

/// A closed command channel (the engine thread is gone) makes each of the three verbs surface
/// `BackendError::Closed` rather than hang or panic — the deterministic dead-engine path, reusing
/// the `with_closed_channel_for_test` backend.
#[tokio::test]
async fn closed_channel_surfaces_closed_for_each_verb() {
    let backend = WinDbgBackend::with_closed_channel_for_test();

    let dump_err = backend
        .open_dump("C:\\x.dmp")
        .await
        .expect_err("open_dump on a dead engine must error");
    assert!(
        matches!(dump_err, BackendError::Closed),
        "open_dump on a closed channel is Closed: {dump_err:?}"
    );

    let kernel_err = backend
        .attach_kernel("net:port=1,key=k")
        .await
        .expect_err("attach_kernel on a dead engine must error");
    assert!(
        matches!(kernel_err, BackendError::Closed),
        "attach_kernel on a closed channel is Closed: {kernel_err:?}"
    );

    let analyze_err = backend
        .analyze()
        .await
        .expect_err("analyze on a dead engine must error");
    assert!(
        matches!(analyze_err, BackendError::Closed),
        "analyze on a closed channel is Closed: {analyze_err:?}"
    );
}

/// `WinDbgFactory::capabilities()` enables every capability-gated verb (crash dump, kernel,
/// analyze, modules) — the gate the tool layer reads to advertise the four WinDbg-only tools.
#[test]
fn factory_capabilities_are_all_true() {
    let caps = WinDbgFactory::new().capabilities();
    assert_eq!(
        caps,
        BackendCapabilities {
            crash_dump: true,
            kernel: true,
            analyze: true,
            modules: true,
        },
        "WinDbg enables all four capability-gated verbs"
    );
}
