//! Live breakpoint / inspection / memory / command tests (Windows): the task-2.5 surface driven
//! end-to-end against the built fixture `testdata/win/test_target.exe`.
//!
//! Each test launches `test_target normal`, sets a breakpoint on `compute` by function name
//! (whose symbols `launch`'s `Reload /f` force-loaded), `go()`s to hit it, and then exercises one
//! of the new engine methods at that real, symbolicated break.
//!
//! DbgEng keeps process-global state, so every test that drives a target serializes behind a
//! single per-file mutex (independent of `--test-threads`). Each test skips cleanly if the fixture
//! has not been built (`testdata/win/build.bat`). Assertions are lenient where the value is
//! environment-dependent (symbol availability) but every method is exercised live.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Mutex;

use dbgeng_sys::{BpLoc, Engine, LaunchReq};
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

fn launch_req() -> LaunchReq {
    LaunchReq {
        program: fixture().to_string_lossy().into_owned(),
        args: vec!["normal".to_string()],
        cwd: None,
    }
}

/// Launch `normal`, set a breakpoint on `compute` by name, and `go()` to hit it. Returns the
/// engine parked at the `compute` breakpoint plus the stop info (for the IP / thread id). Panics
/// (failing the test) if the breakpoint is never hit — that is the precondition every test relies
/// on, so a missed hit is a real failure, not a skip.
fn at_compute_breakpoint(engine: &mut Engine) -> debugger_core::StopInfo {
    engine.launch(&launch_req()).expect("launch normal");

    let bp = engine
        .set_breakpoint(&BpLoc::Function("compute".to_string()), "")
        .expect("set breakpoint on compute");
    assert!(bp.verified, "compute breakpoint should be verified");
    assert!(
        bp.id >= 0,
        "breakpoint id should be non-negative, got {}",
        bp.id
    );

    let outcome = engine.go(10_000).expect("go to compute breakpoint");
    match outcome {
        Some(StopOutcome::Stopped(info)) => info,
        other => panic!("expected to stop at the compute breakpoint, got {other:?}"),
    }
}

/// Full breakpoint lifecycle: set on `compute` → `list` shows it → `go` stops at it (the stop
/// reason / hit ids reference a breakpoint) → `remove` → `list` shows none.
#[test]
fn breakpoint_set_list_hit_remove() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    engine.launch(&launch_req()).expect("launch normal");

    let bp = engine
        .set_breakpoint(&BpLoc::Function("compute".to_string()), "")
        .expect("set breakpoint on compute");
    assert!(bp.verified);

    let listed = engine.list_breakpoints().expect("list breakpoints");
    assert!(
        listed.iter().any(|b| b.id == bp.id),
        "list_breakpoints should include the breakpoint just set ({}), got {listed:?}",
        bp.id
    );

    let outcome = engine.go(10_000).expect("go to breakpoint");
    match outcome {
        Some(StopOutcome::Stopped(info)) => {
            // The breakpoint hit should be reflected either in the recorded hit ids or in a
            // breakpoint-flavored stop reason (DbgEng surfaces the hit via the Breakpoint event).
            let referenced = info.hit_breakpoint_ids.contains(&bp.id)
                || info.reason.to_lowercase().contains("break");
            assert!(
                referenced,
                "the stop should reference the breakpoint (ids {:?} or reason {:?})",
                info.hit_breakpoint_ids, info.reason
            );
        }
        other => panic!("expected Stopped at the breakpoint, got {other:?}"),
    }

    engine.remove_breakpoint(bp.id).expect("remove breakpoint");
    let after = engine.list_breakpoints().expect("list after remove");
    assert!(
        !after.iter().any(|b| b.id == bp.id),
        "after remove, list_breakpoints should not include {}, got {after:?}",
        bp.id
    );

    let _ = engine.detach();
}

/// `threads` returns at least one thread at the breakpoint.
#[test]
fn threads_returns_at_least_one() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let threads = engine.threads().expect("threads");
    assert!(
        !threads.is_empty(),
        "a running target should have at least one thread, got {threads:?}"
    );
    let _ = engine.detach();
}

/// `stack_trace` at the `compute` breakpoint includes a frame named `compute` (and likely `main`).
#[test]
fn stack_trace_includes_compute() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let stop = at_compute_breakpoint(&mut engine);

    let frames = engine.stack_trace(stop.thread_id, 50).expect("stack_trace");
    assert!(!frames.is_empty(), "stack_trace should return frames");
    assert!(
        frames.iter().any(|f| f.name.contains("compute")),
        "the call stack at the compute breakpoint should contain a `compute` frame, got {:?}",
        frames.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    // The top frame should carry an instruction pointer.
    assert!(
        frames[0].instruction_pointer.is_some(),
        "the top frame should have an instruction pointer, got {:?}",
        frames[0]
    );
    let _ = engine.detach();
}

/// `locals(0)` at the `compute` breakpoint includes `sum` and/or `i`.
#[test]
fn locals_at_compute_include_sum_or_i() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let locals = engine.locals(0).expect("locals(0)");
    let names: Vec<&str> = locals.iter().map(|v| v.name.as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "sum" || *n == "i"),
        "locals at compute should include `sum` and/or `i`, got {names:?}"
    );
    let _ = engine.detach();
}

/// `locals(1)` exercises the `frame_index > 0` scope-switch path (`GetStackTrace` + `SetScope` +
/// `ResetScope`). At the `compute` breakpoint frame 1 is `main`, whose locals (`mode`/`r`) should
/// appear. Lenient on exact names — the key is that the scoped path returns a plausible list and
/// restores scope without error.
#[test]
fn locals_at_frame_one_uses_the_scope_switch_path() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let frame1 = engine.locals(1).expect("locals(1) — scoped path");
    let names: Vec<&str> = frame1.iter().map(|v| v.name.as_str()).collect();
    // `main`'s locals are `mode` and `r`; assert at least one shows up (symbols are loaded for the
    // exe). If the CRT frame ordering surprises us, the path at least returned Ok above.
    assert!(
        names
            .iter()
            .any(|n| *n == "mode" || *n == "r" || *n == "argc" || *n == "argv"),
        "locals at frame 1 (main) should include a main local, got {names:?}"
    );
    let _ = engine.detach();
}

/// `evaluate("10 + 1")` returns a result whose text mentions `11` (DbgEng renders as `0n11`/`0xb`,
/// but the decimal `11` appears in the default `??` rendering).
#[test]
fn evaluate_arithmetic() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let eval = engine.evaluate("10 + 1").expect("evaluate");
    assert!(
        eval.result.contains("11"),
        "evaluate(\"10 + 1\") should mention 11, got {:?}",
        eval.result
    );
    let _ = engine.detach();
}

/// `read_memory` of a code address (the top frame's IP) returns a non-empty byte slice.
#[test]
fn read_memory_returns_bytes() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let stop = at_compute_breakpoint(&mut engine);

    let ip = ip_of_top_frame(&mut engine, stop.thread_id);
    let mem = engine.read_memory(ip, 16).expect("read_memory");
    assert_eq!(
        mem.address,
        format!("0x{ip:016X}"),
        "the echoed address should match the requested one"
    );
    assert!(
        !mem.data.is_empty(),
        "reading 16 bytes of live code should return some bytes"
    );
    let _ = engine.detach();
}

/// `disassemble` of the IP returns instructions, each with non-empty mnemonic text.
#[test]
fn disassemble_returns_instructions() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let stop = at_compute_breakpoint(&mut engine);

    let ip = ip_of_top_frame(&mut engine, stop.thread_id);
    let instrs = engine.disassemble(ip, 3).expect("disassemble");
    assert!(
        !instrs.is_empty(),
        "disassemble should return at least one instruction"
    );
    for ins in &instrs {
        assert!(
            !ins.instruction.trim().is_empty(),
            "each disassembled instruction should carry mnemonic text, got {ins:?}"
        );
    }
    let _ = engine.detach();
}

/// `execute("r")` returns non-empty register text.
#[test]
fn execute_register_command() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let out = engine.execute("r").expect("execute r");
    assert!(
        !out.trim().is_empty(),
        "the `r` register dump should produce output, got {out:?}"
    );
    let _ = engine.detach();
}

/// `modules()` includes the fixture exe module, carrying a symbol_status token.
#[test]
fn modules_include_the_exe() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let mods = engine.modules().expect("modules");
    assert!(!mods.is_empty(), "modules should not be empty");
    let exe = mods
        .iter()
        .find(|m| m.name.to_lowercase().contains("test_target"));
    assert!(
        exe.is_some(),
        "modules should include the test_target exe, got {:?}",
        mods.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    let exe = exe.unwrap();
    assert!(
        ["pdb", "export", "deferred", "none"].contains(&exe.symbol_status.as_str()),
        "the exe module's symbol_status should be a known token, got {:?}",
        exe.symbol_status
    );
    // base/size should be populated, not the empty default.
    assert!(
        exe.base.starts_with("0x"),
        "base should be hex, got {:?}",
        exe.base
    );
    let _ = engine.detach();
}

/// `current_source_location()` at the `compute` breakpoint returns `Some((file, line))` that names
/// the fixture source (lenient: assert Some + a plausible line).
#[test]
fn current_source_location_at_compute() {
    if should_skip() {
        return;
    }
    let _guard = LIVE.lock().unwrap_or_else(|p| p.into_inner());

    let mut engine = Engine::create().expect("create engine");
    let _stop = at_compute_breakpoint(&mut engine);

    let loc = engine
        .current_source_location()
        .expect("current_source_location");
    match loc {
        Some((file, line)) => {
            assert!(
                file.to_lowercase().contains("test_target"),
                "the source file should be the fixture, got {file:?}"
            );
            assert!(line > 0, "the source line should be positive, got {line}");
        }
        None => panic!("expected a source location at the compute breakpoint, got None"),
    }
    let _ = engine.detach();
}

/// Helper: the instruction pointer of the top stack frame for `thread_id`, used by the memory /
/// disassembly tests to pick a guaranteed-mapped code address.
fn ip_of_top_frame(engine: &mut Engine, thread_id: i64) -> u64 {
    let frames = engine
        .stack_trace(thread_id, 1)
        .expect("stack_trace for IP");
    let ip_str = frames[0]
        .instruction_pointer
        .as_ref()
        .expect("top frame IP");
    let hex = ip_str.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).expect("parse IP hex")
}
