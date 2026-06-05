//! Live lifecycle tests (Windows): launch / attach_pid / detach against the built fixture
//! `testdata/win/test_target.exe`, plus the open_dump/attach_kernel stubs and the dump guard.
//!
//! DbgEng keeps process-global state, so the tests that drive a target are serialized behind a
//! single mutex (independent of `--test-threads`). Each test skips cleanly if the fixture has not
//! been built (`testdata/win/build.bat`).
#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use dbgeng_sys::{Engine, LaunchReq};
use debugger_core::StopOutcome;

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

#[test]
fn launch_stops_at_initial_break() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let outcome = engine.launch(&launch_req("normal")).expect("launch");
    match outcome {
        StopOutcome::Stopped(info) => assert_eq!(
            info.reason, "Initial breakpoint",
            "launch should stop at the relabeled loader break"
        ),
        other => panic!("expected Stopped at the initial break, got {other:?}"),
    }
    engine.detach().expect("detach");
}

#[test]
fn attach_pid_stops_a_running_process() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    // Spawn the fixture in its sleep-forever mode, then attach to it by pid.
    let mut child = Command::new(fixture())
        .arg("wait")
        .spawn()
        .expect("spawn test_target wait");

    let mut engine = Engine::create().expect("create engine");
    let result = engine.attach_pid(child.id());

    // Always detach + kill the child, even if the assertion below fails.
    let _ = engine.detach();
    let _ = child.kill();
    let _ = child.wait();

    match result.expect("attach_pid") {
        StopOutcome::Stopped(_) => {}
        other => panic!("expected Stopped after attach, got {other:?}"),
    }
}

#[test]
fn detach_allows_a_fresh_session() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    // First session: launch + detach + drop the engine.
    {
        let mut engine = Engine::create().expect("create engine 1");
        engine.launch(&launch_req("normal")).expect("launch 1");
        engine.detach().expect("detach 1");
    }

    // A second full session on a fresh engine proves the first tore down cleanly — `EndSession`
    // released the engine's session/module state so a new `DebugCreate` + `launch` succeeds (a
    // leaked session would make the second launch fail). This is the reliable detach assertion.
    //
    // The file-lock-specific check (that ACTIVE_DETACH leaves the target image writable for a
    // rebuild, vs DetachProcesses' lingering lock) is intentionally NOT asserted here: it depends
    // on the detached process exiting within a poll window, which is timing-racy from the loader
    // break. That regression is scheduled for Phase 5 (rebuild-after-detach), per the plan.
    let mut engine2 = Engine::create().expect("create engine 2");
    let outcome = engine2.launch(&launch_req("normal")).expect("launch 2");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "a second session should launch and stop after the first detached"
    );
    engine2.detach().expect("detach 2");
}

#[test]
fn open_dump_and_attach_kernel_are_phase4_stubs() {
    // No live target needed (crate is Windows-only, so this still only builds/runs on Windows).
    let mut engine = Engine::create().expect("create engine");

    let dump = engine.open_dump("ignored.dmp");
    assert!(dump.is_err(), "open_dump is a Phase-4 stub");
    assert!(dump
        .unwrap_err()
        .to_string()
        .contains("not implemented until Phase 4"));

    let kernel = engine.attach_kernel("net:port=50000,key=1.2.3.4");
    assert!(kernel.is_err(), "attach_kernel is a Phase-4 stub");

    // A non-dump session is runnable (the dump guard only fires once open_dump sets the flag).
    assert!(engine.ensure_runnable().is_ok());
}
