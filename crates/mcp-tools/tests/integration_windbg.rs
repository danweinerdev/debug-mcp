//! Phase 3.5: the Windows WinDbg live integration suite (Normal / Attach / Pause groups),
//! a port of the C++ plugin's `test/test_suite.py` behavior oracle onto the Rust tool
//! surface + the Rust fixture (`testdata/win/test_target.c`).
//!
//! Gated behind the `integration-windbg` feature AND `windows`; without the feature this
//! file compiles to an empty shell, so `cargo test --workspace` never drives DbgEng. Each
//! LIVE test skips cleanly (logs + returns) when `testdata/win/test_target.exe` is absent
//! (build it via `testdata/win/build.bat`). The PROTOCOL-group test needs no fixture — it
//! just builds a `Harness::new_windbg()` and lists tools, proving registration + the
//! capability-gated 25-tool surface on Windows.
//!
//! IMPORTANT: this uses the EXISTING lldb-parity Rust tool names (`launch`, `continue`,
//! `set_function_breakpoint`, `step_over`/`step_into`/`step_out`, `backtrace`, `variables`,
//! `evaluate`, `threads`, `get_modules`, `read_memory`, `run_command`, `disconnect`,
//! `status`, `pause`) — NOT the C++ plugin's `debug_launch`/`continue_execution`/`modules`
//! names. The fixture functions are `compute`/`main` (locals `sum`/`i`/`n`), NOT the C++
//! `add`/`runNormal`/`sumLoop`.
//!
//! DbgEng keeps process-global state, so every test that drives a live target serializes
//! behind a single process-wide mutex (the same pattern `dbgeng-sys/tests/lifecycle.rs`
//! uses), independent of `--test-threads`. The non-live PROTOCOL test does not take the
//! lock (it never connects).

#![cfg(all(feature = "integration-windbg", windows))]

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use integration_tests::harness::{
    Harness, expect_error, expect_json_obj, obj, should_skip_windbg, windbg_fixture_path,
};
use mcp_session::State;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

/// A fresh empty argument map for a no-arg tool call.
fn empty() -> Map<String, Value> {
    Map::new()
}

/// Serializes the LIVE tests — only one DbgEng-driven target at a time per process
/// (DbgEng's engine state is process-global). An **async** mutex: the guard is held across
/// `.await` points (the tool calls), so a `std::sync::Mutex` would trip clippy's
/// `await_holding_lock`. A `tokio::sync::Mutex` cannot be poisoned, so there is no recovery
/// to handle. The non-live PROTOCOL test does not take this lock (it never connects).
static LIVE: Mutex<()> = Mutex::const_new(());

/// Acquire the live-test serialization guard (await-safe).
async fn live_guard() -> tokio::sync::MutexGuard<'static, ()> {
    LIVE.lock().await
}

/// The fixture path as a display string for the `program` arg.
fn fixture() -> String {
    windbg_fixture_path().display().to_string()
}

/// Collect the `name` field of every entry in `arr` (e.g. backtrace frames, threads,
/// modules) into a Vec<&str> for `.contains`/`.iter().any` assertions.
fn names(arr: &[Value]) -> Vec<&str> {
    arr.iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect()
}

// --- PROTOCOL group (non-live: needs no fixture) ---------------------------------------

/// The key non-fixture proof that registration + capability gating works on Windows: a
/// `Harness::new_windbg()` advertises exactly 25 tools (21 base + the four capability-gated
/// WinDbg tools, since `WinDbgFactory::capabilities()` is all-true) and the server identity
/// is the renamed `debug`.
#[tokio::test]
async fn protocol_windbg_advertises_25_tools_and_debug_identity() {
    let h = Harness::new_windbg();

    let tools = h.server.tools();
    assert_eq!(
        tools.len(),
        25,
        "a windbg registry must advertise 21 base + 4 capability-gated = 25 tools, got {}: {:?}",
        tools.len(),
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
    );

    // The four capability-gated WinDbg tools are present (the base 21 plus these).
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for extra in [
        "open_crash_dump",
        "attach_kernel",
        "analyze_crash",
        "get_modules",
    ] {
        assert!(
            tool_names.contains(&extra),
            "windbg tool surface must include {extra}, got {tool_names:?}"
        );
    }

    // Server identity is the intentional rename `debug` (matches the differential lane's
    // assertion). `get_info` is sync on the ServerHandler; read it via the trait.
    use rmcp::ServerHandler;
    let info = h.server.get_info();
    assert_eq!(
        info.server_info.name, "debug",
        "the Windows server identity must be the renamed 'debug'"
    );
}

// --- NORMAL group (live: port of test_normal_session) ----------------------------------

/// Port of the C++ `test_normal_session` over the IMPLEMENTED WinDbg ops (tasks 3.2/3.4):
/// launch `test_target.exe normal` → stops at the loader break; status before/after;
/// threads; get_modules contains test_target; backtrace returns frames; read_memory at the
/// current IP returns a hex dump; step_over/step_into/step_out each land Stopped;
/// run_command `k`/`lm`; evaluate a literal; variables (local scope) returns without error;
/// disconnect → idle; idempotent second disconnect is a tool error (not a transport
/// error/panic).
///
/// The breakpoint-driven slice of the C++ scenario (set/list/remove breakpoints, then
/// continue INTO `compute` to inspect `sum`/`i`/`n`) is split out into
/// [`normal_session_breakpoint_workflow`] below, which exercises `windbg-backend`'s runtime
/// `set_source_breakpoints`/`set_function_breakpoints` (now implemented).
#[tokio::test]
async fn normal_session_workflow() {
    if should_skip_windbg("normal_session_workflow") {
        return;
    }
    let _guard = live_guard().await;

    let h = Harness::new_windbg();

    // status before launch is idle ("No active debug session"), backend = windbg.
    let pre = h.call_default("status", empty()).await;
    let pre = expect_json_obj("status(pre)", &pre);
    assert_eq!(pre["state"], json!("idle"));
    assert_eq!(pre["backend"], json!("windbg"));
    assert_eq!(h.state(), State::Idle);

    // launch normal → stops at the loader break (Stopped).
    let launch = h.launch_windbg(&windbg_fixture_path(), "normal").await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    // status now reports stopped.
    let st = h.call_default("status", empty()).await;
    let st = expect_json_obj("status(stopped)", &st);
    assert_eq!(st["state"], json!("stopped"));

    // threads shows at least one thread.
    let threads = h.call_default("threads", empty()).await;
    let threads = expect_json_obj("threads", &threads);
    assert!(
        threads["count"].as_i64().is_some_and(|c| c >= 1),
        "threads must report at least one thread, got {:?}",
        threads["count"]
    );

    // get_modules contains test_target.
    let modules = h.call_default("get_modules", empty()).await;
    let modules = expect_json_obj("get_modules", &modules);
    let mod_names = names(modules["modules"].as_array().expect("modules array"));
    assert!(
        mod_names.iter().any(|n| n.contains("test_target")),
        "get_modules must include test_target, got {mod_names:?}"
    );

    // backtrace returns frames at the loader break.
    let bt = h.call_default("backtrace", empty()).await;
    let bt = expect_json_obj("backtrace", &bt);
    let frames = bt["frames"].as_array().expect("frames").clone();
    assert!(!frames.is_empty(), "backtrace must return frames");

    // read_memory at the current IP returns a hex dump.
    let ip = frames[0]
        .get("address")
        .and_then(Value::as_str)
        .expect("top frame instruction pointer address");
    let mem = h
        .call_default(
            "read_memory",
            obj(&[
                ("address", Value::String(ip.to_string())),
                ("count", Value::from(16)),
            ]),
        )
        .await;
    let mem = expect_json_obj("read_memory", &mem);
    assert!(
        mem["bytes_read"].as_i64().is_some_and(|n| n > 0),
        "read_memory should read bytes at the current IP, got {:?}",
        mem["bytes_read"]
    );
    assert!(
        mem.get("hex_dump").and_then(Value::as_str).is_some(),
        "read_memory should return a hex_dump"
    );

    // variables (local scope) returns without error at the loader break (the loader/CRT
    // frame's locals — content is loader-dependent, so we assert the call succeeds + the
    // scope is `local`, not specific names, which require stopping inside `compute`).
    let vars = h.call_default("variables", empty()).await;
    let vars = expect_json_obj("variables", &vars);
    assert_eq!(vars["scope"], json!("local"));

    // evaluate a literal expression (no frame context needed for an arithmetic literal).
    let eval = h
        .call_default(
            "evaluate",
            obj(&[("expression", Value::String("10 + 10".into()))]),
        )
        .await;
    let eval = expect_json_obj("evaluate", &eval);
    assert!(
        eval["result"].as_str().is_some_and(|r| r.contains("20")),
        "evaluate('10 + 10') should contain 20, got {:?}",
        eval["result"]
    );

    // step_over / step_into / step_out each land on a stop (Stopped) — never error/hang.
    for tool in ["step_over", "step_into", "step_out"] {
        let stepped = h.call_default(tool, empty()).await;
        let stepped = expect_json_obj(tool, &stepped);
        assert_eq!(
            stepped["status"],
            json!("stopped"),
            "{tool} should land Stopped, got {:?}",
            stepped["status"]
        );
        assert_eq!(
            h.state(),
            State::Stopped,
            "{tool} leaves the session stopped"
        );
    }

    // run_command "k" returns a stack; "lm" lists modules incl. test_target.
    let k = h
        .call_default(
            "run_command",
            obj(&[("command", Value::String("k".into()))]),
        )
        .await;
    let k = expect_json_obj("run_command(k)", &k);
    assert!(
        k.get("result").and_then(Value::as_str).is_some(),
        "run_command('k') returns a result"
    );

    let lm = h
        .call_default(
            "run_command",
            obj(&[("command", Value::String("lm".into()))]),
        )
        .await;
    let lm = expect_json_obj("run_command(lm)", &lm);
    assert!(
        lm["result"]
            .as_str()
            .is_some_and(|r| r.contains("test_target")),
        "run_command('lm') output should mention test_target, got {:?}",
        lm["result"]
    );

    // disconnect → idle.
    let disc = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let disc = expect_json_obj("disconnect", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
    assert_eq!(h.state(), State::Idle);

    // Second disconnect from idle: the state guard rejects it as a tool error (is_error),
    // never a transport error / panic — that is the parity-shaped "idempotent" behavior.
    let disc2 = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let _ = expect_error("disconnect(second, from idle)", &disc2);
    assert_eq!(h.state(), State::Idle);
}

/// The breakpoint-driven slice of the Normal scenario (now that `windbg-backend`'s runtime
/// `set_function_breakpoints` is implemented): set a function breakpoint on `compute` and
/// `main`, list them, remove one, then `continue` into `compute` and inspect its locals
/// (`sum`/`i`/`n`). Goes through the real `set_function_breakpoint`/`list_breakpoints`/
/// `remove_breakpoint`/`continue` tool handlers end-to-end. Skips cleanly if the fixture is
/// absent; must pass when it is present.
#[tokio::test]
async fn normal_session_breakpoint_workflow() {
    if should_skip_windbg("normal_session_breakpoint_workflow") {
        return;
    }
    let _guard = live_guard().await;

    let h = Harness::new_windbg();
    let _ = h.launch_windbg(&windbg_fixture_path(), "normal").await;

    // Set a function breakpoint on `compute` and one on `main`.
    let bp_compute = h
        .call_default(
            "set_function_breakpoint",
            obj(&[("name", Value::String("compute".into()))]),
        )
        .await;
    let bp_compute = expect_json_obj("set_function_breakpoint(compute)", &bp_compute);
    let compute_id = bp_compute["breakpoint_id"]
        .as_i64()
        .expect("compute breakpoint_id");

    let bp_main = h
        .call_default(
            "set_function_breakpoint",
            obj(&[("name", Value::String("main".into()))]),
        )
        .await;
    let _ = expect_json_obj("set_function_breakpoint(main)", &bp_main);

    // list_breakpoints shows both.
    let list = h.call_default("list_breakpoints", empty()).await;
    let list = expect_json_obj("list_breakpoints", &list);
    assert_eq!(list["count"].as_i64(), Some(2));

    // Remove the `main` breakpoint; one remains.
    let main_id = list["breakpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|bp| bp.get("function").and_then(Value::as_str) == Some("main"))
        .and_then(|bp| bp.get("id").and_then(Value::as_i64))
        .expect("main breakpoint id");
    let removed = h
        .call_default(
            "remove_breakpoint",
            obj(&[("breakpoint_id", Value::from(main_id))]),
        )
        .await;
    assert_eq!(
        expect_json_obj("remove_breakpoint", &removed)["removed"],
        json!(true)
    );

    let list2 = h.call_default("list_breakpoints", empty()).await;
    assert_eq!(
        expect_json_obj("list_breakpoints(after remove)", &list2)["count"].as_i64(),
        Some(1)
    );

    // continue → hits the `compute` breakpoint (Stopped).
    let cont = h.continue_().await;
    assert_eq!(cont["status"], json!("stopped"));
    if let Some(hits) = cont.get("hit_breakpoint_ids").and_then(Value::as_array) {
        assert!(hits.iter().any(|v| v.as_i64() == Some(compute_id)));
    }

    // backtrace finds `compute` and `main`; locals show sum/i/n.
    let bt = h.call_default("backtrace", empty()).await;
    let bt = expect_json_obj("backtrace", &bt);
    let frame_names = names(bt["frames"].as_array().expect("frames"));
    assert!(frame_names.iter().any(|n| n.contains("compute")));
    assert!(frame_names.iter().any(|n| n.contains("main")));

    let vars = h.call_default("variables", empty()).await;
    let vars = expect_json_obj("variables", &vars);
    let var_names: Vec<&str> = vars["variables"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();
    assert!(var_names.contains(&"n"));
    assert!(var_names.contains(&"sum") || var_names.contains(&"i"));

    h.disconnect_cleanup().await;
}

/// R6 end-to-end (live): on a STOPPED session, a bare-address `set_function_breakpoint("0x…")` is
/// REJECTED — the response carries the ASLR guidance and is unverified, and crucially the address
/// NEVER enters the tracked breakpoint table (`list_breakpoints` does not contain it). A subsequent
/// `set_function_breakpoint("compute")` then lists ONLY `compute` (count 1) — proving no phantom
/// `0x…` entry accumulates. This is the regression the earlier `id != 0` gate could not express
/// (DbgEng numbers BPs from 0, so a legitimate first BP has id 0). The fix gates on the neutral
/// `BreakpointResult.rejected` flag instead, so a legitimate id-0 BP is still tracked.
#[tokio::test]
async fn address_function_breakpoint_rejected_and_not_tracked() {
    if should_skip_windbg("address_function_breakpoint_rejected_and_not_tracked") {
        return;
    }
    let _guard = live_guard().await;

    let h = Harness::new_windbg();
    let _ = h.launch_windbg(&windbg_fixture_path(), "normal").await;

    // A bare-address function breakpoint is rejected (ASLR guidance, unverified) and NOT tracked.
    let addr = h
        .call_default(
            "set_function_breakpoint",
            obj(&[("name", Value::String("0x7ff6abcd1234".into()))]),
        )
        .await;
    // The handler surfaces the backend's neutral rejection. It is either a tool-error carrying the
    // guidance or an unverified JSON result carrying the guidance message — accept either shape, but
    // the guidance text must be present and the bp must NOT be tracked.
    let addr_text = format!("{addr:?}");
    assert!(
        addr_text.contains("ASLR") || addr_text.contains("address breakpoints are not"),
        "the address bp response must carry the ASLR guidance, got {addr_text}"
    );

    // list_breakpoints must NOT contain the address entry — nothing was tracked.
    let list = h.call_default("list_breakpoints", empty()).await;
    let list = expect_json_obj("list_breakpoints(after address reject)", &list);
    assert_eq!(
        list["count"].as_i64(),
        Some(0),
        "the rejected address bp must not be tracked, got {list:?}"
    );

    // A legitimate `compute` function bp is then tracked and listed alone (count 1) — no phantom
    // `0x…` accumulation, and the legitimate first BP (id may be 0 under DbgEng) is NOT dropped.
    let _ = h
        .call_default(
            "set_function_breakpoint",
            obj(&[("name", Value::String("compute".into()))]),
        )
        .await;
    let list2 = h.call_default("list_breakpoints", empty()).await;
    let list2 = expect_json_obj("list_breakpoints(after compute)", &list2);
    assert_eq!(
        list2["count"].as_i64(),
        Some(1),
        "only the legitimate compute bp is tracked, got {list2:?}"
    );
    let only = &list2["breakpoints"].as_array().unwrap()[0];
    assert_eq!(
        only.get("function").and_then(Value::as_str),
        Some("compute"),
        "the single tracked bp is compute, not a phantom address, got {only:?}"
    );

    h.disconnect_cleanup().await;
}

// --- ATTACH group (live: port of test_attach) ------------------------------------------

/// A spawned `test_target.exe wait` child that is ALWAYS killed + reaped on every path
/// (Drop), so neither an early `?`/panic nor a clippy `zombie_processes` lint can leak it.
struct WaitChild(Option<Child>);

impl WaitChild {
    fn spawn() -> std::io::Result<WaitChild> {
        let child = Command::new(fixture()).arg("wait").spawn()?;
        Ok(WaitChild(Some(child)))
    }

    fn pid(&self) -> u32 {
        self.0.as_ref().expect("child present").id()
    }
}

impl Drop for WaitChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn `test_target.exe wait`, attach by pid, inspect threads/frames/modules, disconnect.
/// The spawned child is killed + reaped on every exit path via `WaitChild`'s Drop.
#[tokio::test]
async fn attach_by_pid_inspects_and_disconnects() {
    if should_skip_windbg("attach_by_pid_inspects_and_disconnects") {
        return;
    }
    let _guard = live_guard().await;

    let child = WaitChild::spawn().expect("spawn test_target wait");
    let pid = child.pid();

    // Give the child a moment to enter its wait loop before attaching.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let h = Harness::new_windbg();

    let attached = h
        .call_default("attach", obj(&[("pid", Value::from(pid))]))
        .await;
    let attached = expect_json_obj("attach", &attached);
    assert_eq!(attached["status"], json!("attached"));
    assert_eq!(attached["state"], json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    // threads shows at least one thread.
    let threads = h.call_default("threads", empty()).await;
    let threads = expect_json_obj("threads", &threads);
    assert!(
        threads["count"].as_i64().is_some_and(|c| c >= 1),
        "attach: threads must report at least one thread, got {:?}",
        threads["count"]
    );

    // backtrace returns frames.
    let bt = h.call_default("backtrace", empty()).await;
    let bt = expect_json_obj("backtrace", &bt);
    assert!(
        bt["frames"].as_array().is_some_and(|f| !f.is_empty()),
        "attach: backtrace must return frames"
    );

    // get_modules contains test_target.
    let modules = h.call_default("get_modules", empty()).await;
    let modules = expect_json_obj("get_modules", &modules);
    let mod_names = names(modules["modules"].as_array().expect("modules array"));
    assert!(
        mod_names.iter().any(|n| n.contains("test_target")),
        "attach: get_modules must include test_target, got {mod_names:?}"
    );

    let disc = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let disc = expect_json_obj("disconnect", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
    assert_eq!(h.state(), State::Idle);

    // child is killed + reaped by WaitChild::drop here.
}

// --- PAUSE group (live: port of test_pause / R3 S_FALSE-Break recovery) -----------------

/// `pause` when not running is a tool error (parity-shaped: is_error, not a transport
/// error). Then launch `wait` mode, free-run via `continue` on a spawned task, `pause` it,
/// and assert the target breaks (Stopped) within a generous bound (~12 s, matching the
/// windbg-backend `pause_breaks_a_running_cont` live test's documented break worst case).
/// This is the in-process tool-dispatch analog of that backend test — it goes through the
/// real `pause`/`continue` handlers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_breaks_a_running_continue() {
    if should_skip_windbg("pause_breaks_a_running_continue") {
        return;
    }
    let _guard = live_guard().await;

    let h = std::sync::Arc::new(Harness::new_windbg());

    // pause while idle (not running) is a tool error, not a transport error / panic.
    let bad_pause = h.call_default("pause", empty()).await;
    let _ = expect_error("pause(while idle)", &bad_pause);

    // launch wait → stops at the loader break.
    let launch = h.launch_windbg(&windbg_fixture_path(), "wait").await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));

    // Free-run: spawn the `continue` so it blocks in the engine's `go` (the target spins in
    // `wait_forever`). Give continue a generous bound (the engine's run-to-stop loop only
    // returns once pause breaks in).
    let cont_h = std::sync::Arc::clone(&h);
    let cont = tokio::spawn(async move {
        cont_h
            .call("continue", empty(), Duration::from_secs(20))
            .await
    });

    // Let the continue actually reach the engine and start running before pausing. The
    // session transitions to Running synchronously inside the resume handler, so poll the
    // observable state rather than sleeping a fixed amount.
    for _ in 0..200 {
        if h.state() == State::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.state(),
        State::Running,
        "continue must put the session in Running before we pause"
    );

    // pause: trips the interrupt flag; the blocked continue returns its resulting stop.
    let paused = h.call_default("pause", empty()).await;
    let paused = expect_json_obj("pause", &paused);
    assert_eq!(paused["status"], json!("pause_requested"));

    // The blocked continue must return Stopped within the generous bound.
    let cont_outcome = tokio::time::timeout(Duration::from_secs(12), cont)
        .await
        .expect("the paused continue must return within 12s (break worst case)")
        .expect("continue task did not panic");
    let cont_obj = expect_json_obj("continue(after pause)", &cont_outcome);
    assert_eq!(
        cont_obj["status"],
        json!("stopped"),
        "pausing a free-running continue must break the target (Stopped), got {:?}",
        cont_obj["status"]
    );
    assert_eq!(h.state(), State::Stopped);

    let disc = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let disc = expect_json_obj("disconnect", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
}

// --- CRASH group (live: port of test_crash_session) -------------------------------------

/// Run the `null`-mode fixture forward until it faults, returning the AV stop response. The
/// C++ oracle continues, and — if it first stops at an intermediate point (the loader-to-main
/// transition / a breakpoint) — continues again to reach the actual access violation. We do
/// the same empirically: bounded `continue` calls (the AV is a stop event and arrives fast,
/// so a 15 s bound is generous), stopping as soon as the stop reason reflects the crash.
///
/// A null-pointer write raises `EXCEPTION_ACCESS_VIOLATION`; dbgeng-sys's `Exception`
/// callback records the stop reason as `"<First|Second>-chance exception 0xC0000005 at 0x…"`,
/// so the AV is recognizable by the `c0000005` code (case-insensitively) in `reason`. We match
/// only on `c0000005` or `violation` — both are specific and unambiguous for
/// `EXCEPTION_ACCESS_VIOLATION`. A bare `"access"` substring is deliberately NOT matched: it
/// appears in many unrelated DbgEng strings ("access denied", "file access") and would risk an
/// early/wrong-stop false match in the continue-to-AV loop.
fn reason_is_access_violation(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("c0000005") || lower.contains("violation")
}

/// Crash-session end to end through the TOOL layer (port of C++ `test_crash_session`):
/// launch `null` → continue to the access violation → backtrace finds `crash_null` →
/// `analyze_crash` returns a non-trivial report → `.exr -1` / `k` raw commands return →
/// reading the null page (`0x0`) is a tool error → disconnect.
///
/// STRICT (real parity guarantees): the AV surfaces as a Stopped stop event whose reason is
/// crash-indicative; `crash_null` appears in the faulting backtrace; reading `0x0` errors.
/// LENIENT (symbol-availability-dependent): the exact `!analyze -v` token set and the `.exr`
/// content — these come from `!analyze`/`.exr` and vary with which symbols resolve, so we
/// only require a non-empty, recognizable report rather than a fixed token list.
#[tokio::test]
async fn crash_session_detects_access_violation_and_analyzes() {
    if should_skip_windbg("crash_session_detects_access_violation_and_analyzes") {
        return;
    }
    let _guard = live_guard().await;

    let h = Harness::new_windbg();

    // launch null → stops at the loader break (Stopped).
    let launch = h.launch_windbg(&windbg_fixture_path(), "null").await;
    assert_eq!(launch["status"], json!("launched"));
    assert_eq!(launch["state"], json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    // continue forward to the access violation. The first continue may only reach an
    // intermediate stop (loader-to-main); continue again until the stop reason reflects the
    // AV. Bound each continue at 15 s (the fault arrives fast — this just prevents a hang).
    let mut crash = None;
    for _ in 0..3 {
        let cont = h.call("continue", empty(), Duration::from_secs(15)).await;
        // A continue past the fault could exit the process; that would be a real defect (the
        // AV must be a stop event, not a silent exit), so a non-Stopped outcome here is fatal.
        let cont = expect_json_obj("continue(to crash)", &cont);
        assert_eq!(
            cont["status"],
            json!("stopped"),
            "the access violation must surface as a Stopped stop event, got {:?}",
            cont
        );
        let reason = cont["reason"].as_str().unwrap_or("");
        if reason_is_access_violation(reason) {
            crash = Some(cont);
            break;
        }
    }
    let crash = crash
        .expect("the null-mode fixture must reach an access-violation stop within a few continues");
    let reason = crash["reason"].as_str().unwrap_or("");
    assert!(
        reason_is_access_violation(reason),
        "the crash stop reason must reflect the access violation, got {reason:?}"
    );
    assert_eq!(h.state(), State::Stopped);

    // backtrace → the faulting frame `crash_null` is on the stack.
    let bt = h.call_default("backtrace", empty()).await;
    let bt = expect_json_obj("backtrace", &bt);
    let frame_names = names(bt["frames"].as_array().expect("frames"));
    assert!(
        frame_names.iter().any(|n| n.contains("crash_null")),
        "the faulting backtrace must contain crash_null, got {frame_names:?}"
    );

    // analyze_crash → returns an `analysis` string. The C++ asserts a long report with specific
    // `!analyze -v` tokens (EXCEPTION_RECORD / STACK_TEXT / FAULTING_LOCAL_VARIABLE_NAME). The rich
    // report requires the analyze EXTENSION (`ext.dll`) to resolve via the `.extpath`; task 5.0
    // (runtime ext-path discovery: registry `KitsRoot10` → `WindowsSdkDir` → default, existence-
    // filtered) makes that resolve wherever the Debugging Tools for Windows are installed, so on a
    // properly-installed host this is now a RICH report and the STRICT token branch fires.
    //
    // We therefore PREFER the strict crash-token check and only fall back to graceful degradation on
    // the explicit `No export` sentinel — the documented escape for an extension-less CI host that
    // lacks the Debugging Tools entirely (there `analyze_crash` returns the DbgEng error string
    // `"No export analyze found\n"`). A resolved-but-garbage analyze path still fails the strict
    // branch.
    let analyzed = h.call_default("analyze_crash", empty()).await;
    let analyzed = expect_json_obj("analyze_crash", &analyzed);
    let analysis = analyzed["analysis"].as_str().unwrap_or("");
    assert!(
        !analysis.trim().is_empty(),
        "analyze_crash returns a non-empty analysis string"
    );
    if analysis.to_ascii_lowercase().contains("no export") {
        eprintln!(
            "analyze_crash degraded: '!analyze' extension unavailable (No export) — \
             install Debugging Tools for Windows for the rich report"
        );
    } else {
        // The analyze extension resolved (task 5.0) — assert it's a real crash report, not garbage.
        let lower = analysis.to_lowercase();
        assert!(
            ["exception", "faulting", "stack", "access_violation"]
                .iter()
                .any(|t| lower.contains(t)),
            "a resolved !analyze -v must produce a recognizable crash report, got: {analysis:.200}"
        );
    }

    // run_command(".exr -1") → the exception-record dump. Lenient: assert it returns a
    // result string (ideally mentioning ExceptionAddress, but symbol/format-dependent).
    let exr = h
        .call_default(
            "run_command",
            obj(&[("command", Value::String(".exr -1".into()))]),
        )
        .await;
    let exr = expect_json_obj("run_command(.exr -1)", &exr);
    assert!(
        exr.get("result").and_then(Value::as_str).is_some(),
        "run_command('.exr -1') must return a result"
    );

    // run_command("k") → the crashing stack; lenient on exact content (ideally crash_null).
    let k = h
        .call_default(
            "run_command",
            obj(&[("command", Value::String("k".into()))]),
        )
        .await;
    let k = expect_json_obj("run_command(k)", &k);
    assert!(
        k.get("result").and_then(Value::as_str).is_some(),
        "run_command('k') must return a result"
    );

    // read_memory at the null page (0x0) faults — must be a TOOL error (matches the C++
    // `must_not_error=False`). This is a STRICT structural contract: the null page is
    // unreadable, so the engine's ReadVirtual fails and the handler surfaces a tool error.
    let null_read = h
        .call_default(
            "read_memory",
            obj(&[
                ("address", Value::String("0x0".into())),
                ("count", Value::from(16)),
            ]),
        )
        .await;
    let _ = expect_error("read_memory(0x0)", &null_read);

    // disconnect → idle.
    let disc = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let disc = expect_json_obj("disconnect", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
    assert_eq!(h.state(), State::Idle);
}

// --- DUMP group (live: port of test_dump) -----------------------------------------------

/// A temp `.dmp` path that is removed on every exit path (Drop), so neither an early panic
/// nor a normal return leaks the file. Mirrors the dbgeng-sys 4.1 live test's cleanup intent
/// with a RAII guard instead of scattered `remove_file` calls.
struct TempDump(PathBuf);

impl TempDump {
    /// A uniquely named temp dump path (distinct from the dbgeng-sys 4.1 live test's name so
    /// the two can never collide), pre-cleaned in case a prior aborted run left one behind.
    fn new() -> TempDump {
        let path = std::env::temp_dir().join("test_target_4_4.dmp");
        let _ = std::fs::remove_file(&path);
        TempDump(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDump {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Dump end to end through the TOOL layer (port of C++ `test_dump`): generate a full minidump
/// from the crashing fixture via `.dump /ma`, then open it in a FRESH session and exercise the
/// dump surface — `open_crash_dump` → `{status:"dump_loaded"}`, status stopped, backtrace
/// finds `crash_null`, variables/get_modules/analyze_crash succeed — and assert the headline
/// dump-session execution guard: `continue`/`step_over` are rejected with the frozen literal
/// `"cannot continue a crash-dump session"`.
///
/// STRICT: the dump file is produced; `open_crash_dump` reports `dump_loaded` + Stopped;
/// `crash_null` is in the dump backtrace; `get_modules` contains test_target; and the
/// execution guard returns the frozen literal verbatim for both continue and step_over.
/// LENIENT: the `analyze_crash` report content (non-empty only) and `.exr` output.
#[tokio::test]
async fn dump_generate_open_analyze_and_reject_execution() {
    if should_skip_windbg("dump_generate_open_analyze_and_reject_execution") {
        return;
    }
    let _guard = live_guard().await;

    let dump = TempDump::new();

    // --- 1. Generate the dump: launch null, continue to the AV, write a full minidump. ---
    {
        let h = Harness::new_windbg();
        let _ = h.launch_windbg(&windbg_fixture_path(), "null").await;

        // Continue to the access violation (possibly more than once — see the crash test).
        let mut reached = false;
        for _ in 0..3 {
            let cont = h.call("continue", empty(), Duration::from_secs(15)).await;
            let cont = expect_json_obj("continue(to crash, dump-gen)", &cont);
            assert_eq!(
                cont["status"],
                json!("stopped"),
                "the AV must be a stop event during dump generation, got {cont:?}"
            );
            if reason_is_access_violation(cont["reason"].as_str().unwrap_or("")) {
                reached = true;
                break;
            }
        }
        assert!(reached, "dump generation must reach the access violation");

        // Guard against a stale-dump false pass: `TempDump::new()` pre-cleaned the path, so it
        // must NOT exist immediately before `.dump`. Asserting non-existence here, then
        // existence after, proves the file was FRESHLY written by this `.dump /ma` call (a
        // failed `.dump` returning an error string plus a leftover file could otherwise
        // false-pass the later size/exists checks).
        assert!(
            !dump.path().exists(),
            "the dump path must be clean before `.dump /ma` (stale file would mask a failed dump): {}",
            dump.path().display()
        );

        // Write a full-memory minidump to the temp path via the raw-command escape hatch.
        let cmd = format!(".dump /ma {}", dump.path().display());
        let written = h
            .call_default("run_command", obj(&[("command", Value::String(cmd))]))
            .await;
        let written = expect_json_obj("run_command(.dump /ma)", &written);
        let result = written
            .get("result")
            .and_then(Value::as_str)
            .expect("run_command('.dump /ma') must return a result");
        // DbgEng's `.dump /ma` emits (observed on this host):
        //   "Creating <path> - mini user dump\nDump successfully written\n"
        // Assert the stable "successfully" substring (case-insensitive) so a failed dump —
        // which prints an error, NOT "Dump successfully written" — is caught here rather than
        // silently false-passing the size/exists checks below.
        assert!(
            result.to_lowercase().contains("successfully"),
            "`.dump /ma` must report success ('Dump successfully written'), got: {result:?}"
        );

        let disc = h
            .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
            .await;
        let _ = expect_json_obj("disconnect(dump-gen)", &disc);
    }

    // The dump file now exists and is non-trivial in size.
    assert!(
        dump.path().exists(),
        "`.dump /ma` must produce a file at {}",
        dump.path().display()
    );
    let dump_size = std::fs::metadata(dump.path())
        .expect("dump file metadata")
        .len();
    assert!(
        dump_size > 1024,
        "the minidump should be non-trivial in size, got {dump_size} bytes"
    );

    // --- 2. Open the dump in a FRESH session (a dump is a new connect point). ---
    let h = Harness::new_windbg();

    let opened = h
        .call(
            "open_crash_dump",
            obj(&[(
                "dump_path",
                Value::String(dump.path().display().to_string()),
            )]),
            Duration::from_secs(30),
        )
        .await;
    let opened = expect_json_obj("open_crash_dump", &opened);
    assert_eq!(
        opened["status"],
        json!("dump_loaded"),
        "open_crash_dump must report dump_loaded, got {opened:?}"
    );
    assert_eq!(h.state(), State::Stopped);

    // status reports stopped.
    let st = h.call_default("status", empty()).await;
    let st = expect_json_obj("status(dump)", &st);
    assert_eq!(st["state"], json!("stopped"));

    // backtrace → the dump captured the faulting frame `crash_null`.
    let bt = h.call_default("backtrace", empty()).await;
    let bt = expect_json_obj("backtrace(dump)", &bt);
    let frame_names = names(bt["frames"].as_array().expect("frames"));
    assert!(
        frame_names.iter().any(|n| n.contains("crash_null")),
        "the dump backtrace must contain crash_null, got {frame_names:?}"
    );

    // variables (local scope) returns without error on the dump's faulting frame.
    let vars = h.call_default("variables", empty()).await;
    let vars = expect_json_obj("variables(dump)", &vars);
    assert_eq!(vars["scope"], json!("local"));

    // get_modules contains test_target.
    let modules = h.call_default("get_modules", empty()).await;
    let modules = expect_json_obj("get_modules(dump)", &modules);
    let mod_names = names(modules["modules"].as_array().expect("modules array"));
    assert!(
        mod_names.iter().any(|n| n.contains("test_target")),
        "get_modules on the dump must include test_target, got {mod_names:?}"
    );

    // analyze_crash → non-empty. Like the live crash session, the rich `!analyze -v` report depends
    // on the analyze EXTENSION (`ext.dll`) resolving via the `.extpath`; task 5.0 (runtime ext-path
    // discovery via the registry `KitsRoot10`) makes that resolve on a properly-installed host, so
    // we PREFER the strict crash-token check here too and only fall back to graceful degradation on
    // the explicit `No export` sentinel (an extension-less CI host with no Debugging Tools install).
    let analyzed = h.call_default("analyze_crash", empty()).await;
    let analyzed = expect_json_obj("analyze_crash(dump)", &analyzed);
    let analysis = analyzed["analysis"].as_str().unwrap_or("");
    assert!(
        !analysis.trim().is_empty(),
        "analyze_crash returns a non-empty analysis string"
    );
    if analysis.to_ascii_lowercase().contains("no export") {
        eprintln!(
            "analyze_crash(dump) degraded: '!analyze' extension unavailable (No export) — \
             install Debugging Tools for Windows for the rich report"
        );
    } else {
        // The analyze extension resolved (task 5.0) — assert it's a real crash report, not garbage.
        let lower = analysis.to_lowercase();
        assert!(
            ["exception", "faulting", "stack", "access_violation"]
                .iter()
                .any(|t| lower.contains(t)),
            "a resolved !analyze -v must produce a recognizable crash report, got: {analysis:.200}"
        );
    }

    // The headline contract: a crash-dump session is Stopped but STATIC — continue/step_* are
    // rejected with the frozen Phase-1 literal. Assert the exact string for both.
    let cont = h.call_default("continue", empty()).await;
    let cont_err = expect_error("continue(on dump)", &cont);
    assert!(
        cont_err.contains("cannot continue a crash-dump session"),
        "continue on a dump must return the frozen literal, got {cont_err:?}"
    );

    let stepped = h.call_default("step_over", empty()).await;
    let step_err = expect_error("step_over(on dump)", &stepped);
    assert!(
        step_err.contains("cannot continue a crash-dump session"),
        "step_over on a dump must return the frozen literal, got {step_err:?}"
    );

    // disconnect → idle; TempDump::drop removes the file.
    let disc = h
        .call_default("disconnect", obj(&[("terminate", Value::Bool(true))]))
        .await;
    let disc = expect_json_obj("disconnect(dump)", &disc);
    assert_eq!(disc["status"], json!("disconnected"));
    assert_eq!(h.state(), State::Idle);
}

// --- ERROR group (live: port of C++ test_suite.py `test_errors`) ------------------------

/// Port of the C++ plugin's `test_errors` (test/test_suite.py) onto the Rust tool surface +
/// the EXACT Rust parity-frozen guard/validation strings (CLAUDE.md: "the guard strings are
/// parity-exact"). The C++ oracle only asserts "this call fails"; the Rust port asserts the
/// SPECIFIC error string each tool produces, so a guard-string regression is caught here
/// rather than masked behind a bare `is_error`.
///
/// Five independent scenarios (each step is order-independent — the LIVE mutex serializes
/// them, and a launched session is always disconnected before the next):
///
/// 1. **Wrong-state (no target / idle).** On a FRESH idle harness, the stopped-guarded
///    inspection/execution tools (`backtrace`/`step_over`/`continue`/`threads`/`variables`/
///    `get_locals`-analog) are rejected with the idle state-guard literal
///    `"no debug session active. Use 'launch' or 'attach' first."` (`check_state` Idle arm,
///    `mcp-session/src/manager.rs`). `pause` guards `Running`; from idle the SAME idle literal
///    fires (the guard's Idle arm precedes the Running arm).
/// 2. **Double launch.** After a successful `launch normal` (Stopped), a second `launch`
///    without disconnecting hits the launch idle-guard. The current state is `stopped`, so the
///    guard's fallthrough arm fires: `"invalid state: stopped, expected one of: idle"`.
/// 3. **Invalid breakpoint location.** `set_function_breakpoint("nonexistent_xyz_func")` on a
///    stopped session is NOT a tool error — both lldb and WinDbg model an unresolvable symbol
///    as an UNVERIFIED (pending) breakpoint result (`verified:false`), distinct from the R6
///    ASLR bare-address `rejected` path (which IS a tool error). This asserts the neutral-
///    surface parity: an unresolvable symbol → a tracked, unverified breakpoint, not a hard
///    error. (The C++ oracle expected a generic failure; the Rust/lldb parity model surfaces
///    it as unverified — the documented deviation we assert rather than mask.)
/// 4. **remove_breakpoint(99999).** A nonexistent breakpoint id is a TOOL ERROR — the handler
///    looks up `breakpoint_info(id)`, finds nothing, and returns
///    `"failed to remove breakpoint: breakpoint ID 99999 not found"` (`handle_remove_breakpoint`,
///    `breakpoints.rs`). This matches the C++ `err_bad_bp_remove` (expects an error).
/// 5. **Bad/missing required args.** `set_function_breakpoint` with no `name`, `read_memory`
///    with no `address`, and `launch` with no `program` each surface the `Args::require_string`
///    parity literal `missing required parameter: required argument "<key>" not found`.
#[tokio::test]
async fn error_group_wrong_state_bad_args_double_launch_invalid_bp() {
    if should_skip_windbg("error_group_wrong_state_bad_args_double_launch_invalid_bp") {
        return;
    }
    let _guard = live_guard().await;

    // The parity-frozen guard/validation literals asserted below (read from
    // `mcp-session/src/manager.rs` `check_state` + `mcp-tools/src/args.rs`/`breakpoints.rs`).
    const IDLE_GUARD: &str = "no debug session active. Use 'launch' or 'attach' first.";
    const DOUBLE_LAUNCH_GUARD: &str = "invalid state: stopped, expected one of: idle";

    // --- 1. Wrong-state (no target / idle): every stopped-guarded tool returns the idle
    // state-guard literal on a fresh, never-launched harness. ---
    {
        let h = Harness::new_windbg();
        assert_eq!(h.state(), State::Idle);

        // Each stopped-guarded inspection/execution tool → the idle state-guard literal.
        // `variables` IS the Rust `get_locals` analog (the C++ `get_locals` tool); there is no
        // separate `get_locals` tool name in the Rust 21-tool surface.
        for tool in [
            "backtrace",
            "step_over",
            "step_into",
            "step_out",
            "continue",
            "threads",
            "variables",
        ] {
            let out = h.call_default(tool, empty()).await;
            let msg = expect_error(&format!("{tool}(no target / idle)"), &out);
            assert_eq!(
                msg, IDLE_GUARD,
                "{tool} from idle must return the idle state-guard literal, got {msg:?}"
            );
            assert_eq!(h.state(), State::Idle, "{tool} from idle leaves state idle");
        }

        // `pause` guards Running; from idle the guard's Idle arm (which precedes the Running
        // arm) fires the SAME idle literal — not a "process is running" message.
        let paused = h.call_default("pause", empty()).await;
        let pause_msg = expect_error("pause(no target / idle)", &paused);
        assert_eq!(
            pause_msg, IDLE_GUARD,
            "pause from idle must return the idle state-guard literal (Idle arm precedes \
             Running arm), got {pause_msg:?}"
        );
    }

    // --- 2. Double launch: a second launch on a stopped session hits the launch idle-guard,
    // whose fallthrough arm reports the current state. ---
    {
        let h = Harness::new_windbg();
        let launch = h.launch_windbg(&windbg_fixture_path(), "normal").await;
        assert_eq!(launch["status"], json!("launched"));
        assert_eq!(h.state(), State::Stopped);

        // A second launch WITHOUT disconnecting: the idle-guard rejects it (current = stopped).
        let args_json = serde_json::to_string(&vec!["normal"]).expect("serialize args array");
        let relaunch = h
            .call_default(
                "launch",
                obj(&[
                    ("program", Value::String(fixture())),
                    ("args", Value::String(args_json)),
                ]),
            )
            .await;
        let relaunch_msg = expect_error("launch(double)", &relaunch);
        assert_eq!(
            relaunch_msg, DOUBLE_LAUNCH_GUARD,
            "a double launch must return the not-idle state-guard literal, got {relaunch_msg:?}"
        );
        // The first session is unharmed (still stopped).
        assert_eq!(h.state(), State::Stopped);

        h.disconnect_cleanup().await;
    }

    // --- 3. Invalid breakpoint location: an unresolvable symbol is UNVERIFIED (not an
    // error) — the neutral-surface parity (lldb/WinDbg model a pending symbol the same way).
    // The R6 `rejected` path (ASLR bare address) is the ONLY set_function_breakpoint tool
    // error; an ordinary unresolvable symbol is a tracked, unverified result. ---
    {
        let h = Harness::new_windbg();
        let _ = h.launch_windbg(&windbg_fixture_path(), "normal").await;

        let bad_bp = h
            .call_default(
                "set_function_breakpoint",
                obj(&[("name", Value::String("nonexistent_xyz_func".into()))]),
            )
            .await;
        // ASSERTED BEHAVIOR: a JSON result with `verified:false` (a tool result, NOT a tool
        // error). If this ever surfaces as a tool error instead, that is a real neutral-surface
        // change and the test would fail here (we do not weaken to accept either).
        let bad_bp = expect_json_obj("set_function_breakpoint(nonexistent)", &bad_bp);
        assert_eq!(
            bad_bp["verified"],
            json!(false),
            "an unresolvable symbol must be an UNVERIFIED breakpoint result (not a tool error), \
             got {bad_bp:?}"
        );
        assert_eq!(
            bad_bp.get("function").and_then(Value::as_str),
            Some("nonexistent_xyz_func"),
            "the unverified result echoes the requested function name, got {bad_bp:?}"
        );

        // --- 4. remove_breakpoint(99999): a nonexistent id is a TOOL ERROR with the exact
        // "ID not found" literal (handle_remove_breakpoint). ---
        let removed = h
            .call_default(
                "remove_breakpoint",
                obj(&[("breakpoint_id", Value::from(99_999))]),
            )
            .await;
        let removed_msg = expect_error("remove_breakpoint(99999)", &removed);
        assert_eq!(
            removed_msg, "failed to remove breakpoint: breakpoint ID 99999 not found",
            "removing a nonexistent breakpoint id must return the exact not-found literal, \
             got {removed_msg:?}"
        );

        h.disconnect_cleanup().await;
    }

    // --- 5. Bad/missing required args: the `Args::require_string` parity literal for a couple
    // of representative tools. These need no live target (the arg validation precedes the
    // backend), but `launch`'s missing-program check follows its idle guard, so it runs on a
    // fresh idle harness. ---
    {
        let h = Harness::new_windbg();

        // set_function_breakpoint with no `name` (idle is allowed by the [Idle, Stopped] guard,
        // so require_string fires and returns the missing-arg error).
        let no_name = h.call_default("set_function_breakpoint", empty()).await;
        let no_name_msg = expect_error("set_function_breakpoint(no name)", &no_name);
        assert_eq!(
            no_name_msg, "missing required parameter: required argument \"name\" not found",
            "set_function_breakpoint without `name` must return the require_string literal, \
             got {no_name_msg:?}"
        );

        // read_memory with no `address`: guarded `stopped`, so on a fresh idle harness it is the
        // idle state guard that fires FIRST (the guard precedes arg validation). Assert that.
        let no_addr_idle = h.call_default("read_memory", empty()).await;
        let no_addr_idle_msg = expect_error("read_memory(no address, idle)", &no_addr_idle);
        assert_eq!(
            no_addr_idle_msg, IDLE_GUARD,
            "read_memory from idle hits the stopped-guard before arg validation, got \
             {no_addr_idle_msg:?}"
        );

        // launch with no `program`: the idle guard passes (state is idle), so require_string
        // surfaces the missing-program literal.
        let no_program = h.call_default("launch", empty()).await;
        let no_program_msg = expect_error("launch(no program)", &no_program);
        assert_eq!(
            no_program_msg, "missing required parameter: required argument \"program\" not found",
            "launch without `program` must return the require_string literal, got \
             {no_program_msg:?}"
        );
        // launch's missing-program failure leaves the session idle (the connect never started).
        assert_eq!(h.state(), State::Idle);
    }

    // read_memory's missing-`address` arg validation (on a STOPPED session, past the guard) is
    // the require_string literal — assert it on a launched session so the arg path is reached.
    {
        let h = Harness::new_windbg();
        let _ = h.launch_windbg(&windbg_fixture_path(), "normal").await;

        let no_addr = h.call_default("read_memory", empty()).await;
        let no_addr_msg = expect_error("read_memory(no address, stopped)", &no_addr);
        assert_eq!(
            no_addr_msg, "missing required parameter: required argument \"address\" not found",
            "read_memory without `address` (past the stopped guard) must return the \
             require_string literal, got {no_addr_msg:?}"
        );

        // read_memory WITH `address` but no `count` (on the same stopped session, past the
        // guard): the handler validates `address` before `count` (memory.rs), so with address
        // present the missing-`count` validation fires. This guards against `count` silently
        // becoming optional — the require_int/require_positive_int missing-key literal is the
        // same `missing required parameter: required argument "<key>" not found` shape.
        let no_count = h
            .call_default(
                "read_memory",
                obj(&[("address", Value::String("0x1234".into()))]),
            )
            .await;
        let no_count_msg = expect_error("read_memory(address, no count, stopped)", &no_count);
        assert_eq!(
            no_count_msg, "missing required parameter: required argument \"count\" not found",
            "read_memory with `address` but no `count` must return the missing-count \
             require_int literal, got {no_count_msg:?}"
        );

        h.disconnect_cleanup().await;
    }
}

// --- DIFFERENTIAL (golden shape) group ---------------------------------------------------

/// Cross-backend neutral-surface conformance — the Windows analog of the lldb golden lane
/// (`integration_differential.rs::golden_response_shapes_over_stdio`).
///
/// **Why not a true binary-vs-binary diff?** lldb (lldb-dap) is a macOS/Linux backend and
/// WinDbg is a Windows backend; they are platform-exclusive and CANNOT co-run on one host, so
/// a live cross-backend differential run is impossible. Instead this asserts that the WinDbg
/// backend's responses for the SHARED neutral behaviors conform to the SAME neutral JSON field
/// shapes the lldb golden lane documents — the keys + JSON types that the neutral
/// `debugger-core` types (`Frame`/`Variable`→`FlatVariable`/`ThreadInfo`/`MemoryRead`) and the
/// handler response builders serialize to. This catches neutral-surface drift (a WinDbg
/// response missing/renaming a field the neutral contract requires) without needing lldb live.
///
/// **Approach taken: neutral-type-key conformance.** We assert each shared-behavior response
/// carries the exact top-level + per-element key set (with the correct JSON types) that the
/// lldb golden test asserts and the neutral serde types produce — NOT the same values
/// (addresses/ids differ run-to-run and between backends; the SHAPE is the contract). The
/// per-key references below cite the lldb golden assertions (`golden_response_shapes_over_stdio`)
/// and the neutral serde definitions so the two lanes stay in lockstep.
#[tokio::test]
async fn differential_windbg_shared_behavior_shapes() {
    if should_skip_windbg("differential_windbg_shared_behavior_shapes") {
        return;
    }
    let _guard = live_guard().await;

    let h = Harness::new_windbg();
    let _ = h.launch_windbg(&windbg_fixture_path(), "normal").await;

    // Reuse the `normal_session_breakpoint_workflow` setup: set a `compute` function bp, then
    // continue to it so there is a real stopped frame with locals (sum/i/n).
    let _ = h
        .call_default(
            "set_function_breakpoint",
            obj(&[("name", Value::String("compute".into()))]),
        )
        .await;
    let cont = h.continue_().await;
    assert_eq!(
        cont["status"],
        json!("stopped"),
        "continue must stop at the compute breakpoint, got {cont:?}"
    );
    assert_eq!(h.state(), State::Stopped);

    // --- backtrace: `{frames:[...], total_frames, thread_id}`; each frame `{index, id, name,
    // [file, line], [address]}`. The lldb golden lane asserts top-level total_frames/thread_id
    // and per-frame index/name/id (the always-present neutral `Frame` keys). file/line/address
    // are conditionally present (omitted when the source/IP is empty), so we assert their TYPE
    // only when present, and require compute+main by name. ---
    {
        let bt = h.call_default("backtrace", empty()).await;
        let bt = expect_json_obj("backtrace(shape)", &bt);

        // Top-level neutral keys + types (golden: total_frames/thread_id present; here typed).
        assert!(
            bt.get("total_frames").and_then(Value::as_i64).is_some(),
            "backtrace.total_frames must be an integer, got {:?}",
            bt.get("total_frames")
        );
        assert!(
            bt.get("thread_id").and_then(Value::as_i64).is_some(),
            "backtrace.thread_id must be an integer, got {:?}",
            bt.get("thread_id")
        );
        let frames = bt["frames"].as_array().expect("backtrace.frames array");
        assert!(!frames.is_empty(), "backtrace.frames must be non-empty");

        for (i, frame) in frames.iter().enumerate() {
            let f = frame
                .as_object()
                .unwrap_or_else(|| panic!("frame[{i}] must be an object, got {frame:?}"));
            // Always-present neutral Frame keys (golden asserts index/name/id present).
            assert!(
                f.get("index").and_then(Value::as_i64).is_some(),
                "frame[{i}].index must be an integer, got {f:?}"
            );
            assert!(
                f.get("id").and_then(Value::as_i64).is_some(),
                "frame[{i}].id must be an integer, got {f:?}"
            );
            assert!(
                f.get("name").and_then(Value::as_str).is_some(),
                "frame[{i}].name must be a string, got {f:?}"
            );
            // Conditional keys: when present they carry the neutral type (file→string,
            // line→integer, address→string). Absence is allowed (omitted when empty).
            if let Some(file) = f.get("file") {
                assert!(
                    file.is_string(),
                    "frame[{i}].file must be a string, got {file:?}"
                );
                assert!(
                    f.get("line").and_then(Value::as_i64).is_some(),
                    "frame[{i}] with a file must carry an integer line, got {f:?}"
                );
            }
            if let Some(addr) = f.get("address") {
                assert!(
                    addr.is_string(),
                    "frame[{i}].address must be a string, got {addr:?}"
                );
            }
        }

        // The shared-behavior content guarantee: compute + main are on the stack by name.
        let frame_names = names(frames);
        assert!(
            frame_names.iter().any(|n| n.contains("compute")),
            "backtrace must contain compute, got {frame_names:?}"
        );
        assert!(
            frame_names.iter().any(|n| n.contains("main")),
            "backtrace must contain main, got {frame_names:?}"
        );
    }

    // --- variables: `{variables:[...], count, scope, truncated}`; each entry is the neutral
    // `FlatVariable` shape — `name`/`value` always (strings), `type` when non-empty (string),
    // `has_children` when true (bool), `children_count` when non-zero (integer). The lldb
    // golden lane asserts count/scope=="local"/truncated present + per-entry `name`. ---
    {
        let vars = h.call_default("variables", empty()).await;
        let vars = expect_json_obj("variables(shape)", &vars);

        // Top-level neutral keys + types (golden: count/scope/truncated present).
        assert!(
            vars.get("count").and_then(Value::as_i64).is_some(),
            "variables.count must be an integer, got {:?}",
            vars.get("count")
        );
        assert_eq!(
            vars["scope"],
            json!("local"),
            "variables.scope defaults to local, got {:?}",
            vars.get("scope")
        );
        assert!(
            vars.get("truncated").and_then(Value::as_bool).is_some(),
            "variables.truncated must be a bool, got {:?}",
            vars.get("truncated")
        );

        let entries = vars["variables"].as_array().expect("variables array");
        for (i, v) in entries.iter().enumerate() {
            let o = v
                .as_object()
                .unwrap_or_else(|| panic!("variable[{i}] must be an object, got {v:?}"));
            // Always-present FlatVariable keys.
            assert!(
                o.get("name").and_then(Value::as_str).is_some(),
                "variable[{i}].name must be a string, got {o:?}"
            );
            assert!(
                o.get("value").and_then(Value::as_str).is_some(),
                "variable[{i}].value must be a string, got {o:?}"
            );
            // Conditional FlatVariable keys carry their neutral type when present.
            if let Some(ty) = o.get("type") {
                assert!(
                    ty.is_string(),
                    "variable[{i}].type must be a string, got {ty:?}"
                );
            }
            if let Some(hc) = o.get("has_children") {
                assert!(
                    hc.is_boolean(),
                    "variable[{i}].has_children must be a bool, got {hc:?}"
                );
            }
            if let Some(cc) = o.get("children_count") {
                assert!(
                    cc.as_i64().is_some(),
                    "variable[{i}].children_count must be an integer, got {cc:?}"
                );
            }
        }

        // The shared-behavior content guarantee: compute's local `n` is present (matches
        // `normal_session_breakpoint_workflow`).
        let var_names = names(entries);
        assert!(
            var_names.contains(&"n"),
            "variables must include compute's local `n`, got {var_names:?}"
        );
    }

    // --- threads: `{threads:[...], count}`; each thread is the neutral `ThreadInfo` shape —
    // `{id, name}` (the marker keys `is_stopped`/`is_current` are additive and present only on
    // the stopped thread). The lldb golden lane drives threads via the normal session; here we
    // assert the neutral ThreadInfo key/type contract. ---
    {
        let threads = h.call_default("threads", empty()).await;
        let threads = expect_json_obj("threads(shape)", &threads);

        assert!(
            threads.get("count").and_then(Value::as_i64).is_some(),
            "threads.count must be an integer, got {:?}",
            threads.get("count")
        );
        let entries = threads["threads"].as_array().expect("threads array");
        assert!(
            !entries.is_empty(),
            "threads must report at least one thread"
        );
        for (i, t) in entries.iter().enumerate() {
            let o = t
                .as_object()
                .unwrap_or_else(|| panic!("thread[{i}] must be an object, got {t:?}"));
            // Neutral ThreadInfo keys.
            assert!(
                o.get("id").and_then(Value::as_i64).is_some(),
                "thread[{i}].id must be an integer, got {o:?}"
            );
            assert!(
                o.get("name").and_then(Value::as_str).is_some(),
                "thread[{i}].name must be a string, got {o:?}"
            );
            // Additive stopped-thread markers carry bool type when present.
            for marker in ["is_stopped", "is_current"] {
                if let Some(m) = o.get(marker) {
                    assert!(
                        m.is_boolean(),
                        "thread[{i}].{marker} must be a bool, got {m:?}"
                    );
                }
            }
        }
    }

    // --- read_memory: `{address, bytes_read, [hex_dump]}` — the neutral `MemoryRead`-derived
    // handler shape. `address` (string, the backend's echoed address) and `bytes_read`
    // (integer) are always present; `hex_dump` (string) is present whenever bytes were read
    // (omitted only on an empty read). Read at a valid frame IP so bytes_read > 0 and the
    // hex_dump key is present, then assert the key/type contract. ---
    {
        // Resolve a valid address from the top frame's instruction pointer.
        let bt = h.call_default("backtrace", empty()).await;
        let bt = expect_json_obj("backtrace(for read_memory)", &bt);
        let frames = bt["frames"].as_array().expect("frames");
        let ip = frames
            .iter()
            .find_map(|f| f.get("address").and_then(Value::as_str))
            .expect("a frame with an instruction-pointer address for read_memory");

        let mem = h
            .call_default(
                "read_memory",
                obj(&[
                    ("address", Value::String(ip.to_string())),
                    ("count", Value::from(16)),
                ]),
            )
            .await;
        let mem = expect_json_obj("read_memory(shape)", &mem);

        // Neutral MemoryRead-derived keys + types.
        assert!(
            mem.get("address").and_then(Value::as_str).is_some(),
            "read_memory.address must be a string, got {:?}",
            mem.get("address")
        );
        let bytes_read = mem
            .get("bytes_read")
            .and_then(Value::as_i64)
            .expect("read_memory.bytes_read must be an integer");
        assert!(
            bytes_read > 0,
            "reading 16 bytes at a valid IP must read > 0 bytes, got {bytes_read}"
        );
        // hex_dump is present (string) whenever bytes were read.
        assert!(
            mem.get("hex_dump").and_then(Value::as_str).is_some(),
            "read_memory with bytes_read > 0 must carry a string hex_dump, got {:?}",
            mem.get("hex_dump")
        );
    }

    h.disconnect_cleanup().await;
}
