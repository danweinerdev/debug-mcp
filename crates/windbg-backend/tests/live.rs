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

use debugger_core::{
    AttachOutcome, AttachSpec, BackendFactory, FunctionBp, LaunchOutcome, LaunchSpec,
};
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
