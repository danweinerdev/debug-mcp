//! Live lifecycle tests (Windows): launch / attach_pid / detach against the built fixture
//! `testdata/win/test_target.exe`, plus the Phase-4 dump surface (open_dump / analyze / modules /
//! the dump-session runnable guard) and the `#[ignore]`d live KDNET kernel attach.
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
    engine.detach(false).expect("detach");
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
    let _ = engine.detach(false);
    let _ = child.kill();
    let _ = child.wait();

    match result.expect("attach_pid") {
        StopOutcome::Stopped(_) => {}
        other => panic!("expected Stopped after attach, got {other:?}"),
    }
}

#[test]
fn detach_terminate_kills_the_target() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    // Spawn the sleep-forever fixture ourselves (so we hold its pid + can observe its exit), attach
    // to it, then detach with terminate=true (DEBUG_END_ACTIVE_TERMINATE). That must KILL the
    // debuggee — proven by the child exiting on its own afterward. (A plain detach(false) would leave
    // `wait` sleeping forever and `try_wait` would keep returning Ok(None).)
    let mut child = Command::new(fixture())
        .arg("wait")
        .spawn()
        .expect("spawn test_target wait");

    let mut engine = Engine::create().expect("create engine");
    engine.attach_pid(child.id()).expect("attach");
    engine.detach(true).expect("detach(true)");
    drop(engine);

    // Poll briefly for the child to be reaped — DEBUG_END_ACTIVE_TERMINATE killed it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Always reap the child (kill if the terminate somehow didn't take), so it is `wait()`ed on
    // every path, then assert the terminate actually killed it.
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        exited,
        "detach(true) should have terminated the debuggee, but it was still running"
    );
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
        engine.detach(false).expect("detach 1");
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
    engine2.detach(false).expect("detach 2");
}

/// `open_dump` against a path that does not exist fails cleanly (DbgEng `OpenDumpFile` errors), and
/// a fresh engine (NoTarget) is runnable — the dump guard only fires once `open_dump` succeeds and
/// sets `is_dump`. Phase 4 replaced the old "Phase-4 stub" assertions: the stub strings are gone,
/// so this now asserts the real behavior (a missing dump errors; a non-dump session is runnable).
/// No live target needed (the crate is Windows-only, so this still only builds/runs on Windows).
#[test]
fn open_dump_missing_file_errors_and_fresh_engine_is_runnable() {
    // DbgEng is process-global; hold the LIVE mutex so a deferred-validation wait inside
    // `open_dump` cannot race a concurrent live session in another test.
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());
    let mut engine = Engine::create().expect("create engine");

    // A non-existent dump path: OpenDumpFile (or the load wait) must fail — never panic, never hang.
    let dump = engine.open_dump("Z:\\does-not-exist-9d3f.dmp");
    assert!(
        dump.is_err(),
        "open_dump on a missing file should error, got {dump:?}"
    );

    // A fresh (non-dump) session is runnable; the dump guard only engages after a successful
    // open_dump sets is_dump. (open_dump above failed, so is_dump may be set defensively; assert
    // the guard wiring on a clean engine instead.)
    let clean = Engine::create().expect("create engine 2");
    assert!(
        clean.ensure_runnable().is_ok(),
        "a fresh non-dump session must be runnable"
    );
}

/// Live dump round-trip (Windows, skip-if-fixture-absent + LIVE mutex): generate a real `.dump /ma`
/// from the crashing fixture, then open it in a FRESH engine and assert the Phase-4 dump surface —
/// `open_dump` succeeds, `analyze()` returns text, `modules()` contains `test_target`, and the dump
/// session is refused by `ensure_runnable`/`go` with the frozen literal. The temp dump file is
/// removed on every path.
///
/// Leniency: `!analyze -v` output varies with symbol availability (no public symbol server is
/// assumed here), so the analyze assertion is "non-empty" rather than a specific token. The
/// `crash_location` may be `Some` or `None` depending on line-info resolution; both are accepted.
#[test]
fn live_dump_round_trip_open_analyze_modules_guard() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    // A unique temp path for the dump, removed on every exit path below.
    let dump_path = std::env::temp_dir().join(format!(
        "dbgeng_sys_test_{}_{}.dmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&dump_path);

    // --- 1. Generate the dump: launch `null` (crash_null → access violation), run to the AV,
    //         write a full minidump with `.dump /ma`, then detach. ---
    {
        let mut engine = Engine::create().expect("create engine (dump gen)");
        engine.launch(&launch_req("null")).expect("launch null");

        // Run to the access violation (the crash_null write to 0x0). `go` returns the AV stop, or
        // None if it timed out still-running — generous budget so the crash is reached.
        let _ = engine.go(10_000);

        // Write a full-memory minidump to our temp path. `.dump /ma <path>` is the WinDbg command.
        let cmd = format!(".dump /ma {}", dump_path.display());
        let out = engine.execute(&cmd);
        // Best-effort detach regardless of the dump result.
        let _ = engine.detach(true);

        match out {
            Ok(text) => eprintln!(".dump output: {}", text.trim()),
            Err(e) => {
                let _ = std::fs::remove_file(&dump_path);
                panic!("failed to write dump via .dump /ma: {e}");
            }
        }
    }

    if !dump_path.exists() {
        let _ = std::fs::remove_file(&dump_path);
        panic!(
            ".dump /ma did not produce a file at {}",
            dump_path.display()
        );
    }

    // --- 2. Fresh engine: open the dump and assert the Phase-4 dump surface. ---
    let result = (|| -> Result<(), String> {
        let mut engine = Engine::create().map_err(|e| format!("create engine (open): {e}"))?;

        let outcome = engine
            .open_dump(&dump_path.to_string_lossy())
            .map_err(|e| format!("open_dump: {e}"))?;
        // crash_location is Some or None (symbol/line dependent) — both acceptable; just log it.
        eprintln!("dump crash_location = {:?}", outcome.crash_location);

        // The dump session must be refused by the runnable guard with the frozen literal.
        let guard = engine.ensure_runnable();
        match guard {
            Err(e)
                if e.to_string()
                    .contains("cannot continue a crash-dump session") => {}
            other => {
                return Err(format!(
                    "ensure_runnable on a dump should be the frozen literal, got {other:?}"
                ))
            }
        }
        // `go` is likewise refused with the same literal (reaches ensure_runnable first).
        match engine.go(100) {
            Err(e)
                if e.to_string()
                    .contains("cannot continue a crash-dump session") => {}
            other => return Err(format!("go on a dump should be refused, got {other:?}")),
        }

        // modules() must list the fixture module `test_target`.
        let modules = engine.modules().map_err(|e| format!("modules: {e}"))?;
        let has_target = modules
            .iter()
            .any(|m| m.name.to_ascii_lowercase().contains("test_target"));
        if !has_target {
            let names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
            return Err(format!(
                "modules() did not contain test_target; got {names:?}"
            ));
        }
        // Every module honors the format contract (base 0x + 16 hex, decimal size, known status).
        for m in &modules {
            if !(m.base.starts_with("0x") && m.base.len() == 18) {
                return Err(format!("module {} has malformed base {:?}", m.name, m.base));
            }
            if !m.size.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "module {} has non-decimal size {:?}",
                    m.name, m.size
                ));
            }
            if !matches!(
                m.symbol_status.as_str(),
                "pdb" | "export" | "deferred" | "none"
            ) {
                return Err(format!(
                    "module {} has out-of-vocabulary symbol_status {:?}",
                    m.name, m.symbol_status
                ));
            }
        }

        // analyze() must return non-empty text (content varies by symbols — lenient).
        let report = engine.analyze().map_err(|e| format!("analyze: {e}"))?;
        if report.trim().is_empty() {
            return Err("analyze() returned empty text".to_string());
        }
        eprintln!("analyze() returned {} bytes", report.len());

        let _ = engine.detach(false);
        Ok(())
    })();

    // Always remove the temp dump, then surface any assertion failure.
    let _ = std::fs::remove_file(&dump_path);
    if let Err(msg) = result {
        panic!("{msg}");
    }
}

/// Live KDNET kernel attach. `#[ignore]` by default: it needs a REACHABLE KDNET target (a VM
/// configured with `bcdedit /dbgsettings net host:<this>,port=<p>,key=<k>` and listening). Against
/// an unreachable target, `attach_kernel`'s `WaitForEvent(INFINITE)` (R2) blocks the thread FOREVER
/// with no cancellation point (DbgEng's KDNET transport retries indefinitely; recovery is `/mcp`
/// disconnect) — so this must NEVER run unattended in CI. Run it manually with a live VM:
/// `cargo test -p dbgeng-sys --test lifecycle -- --ignored attach_kernel_live`.
#[test]
#[ignore = "needs a reachable KDNET VM; attach_kernel's INFINITE wait blocks forever on an unreachable target"]
fn attach_kernel_live() {
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());
    let mut engine = Engine::create().expect("create engine");
    // Adjust port/key to your VM's `bcdedit /dbgsettings net` values before running.
    let outcome = engine
        .attach_kernel("net:port=50000,key=1.2.3.4")
        .expect("attach_kernel to a reachable KDNET VM");
    assert!(
        matches!(outcome, StopOutcome::Stopped(_)),
        "kernel attach should stop at the initial break, got {outcome:?}"
    );
    engine.detach(false).expect("detach kernel");
}
