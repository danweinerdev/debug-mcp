---
title: "windbg-backend Core — engine thread + DebuggerBackend (21 ops)"
type: phase
plan: WinDbgBackend
phase: 3
status: pending
created: 2026-06-03
updated: 2026-06-03
deliverable: "A Windows-only `windbg-backend` crate (#![forbid(unsafe_code)]) that owns a dedicated MTA-COM engine thread, marshals async DebuggerBackend calls to the dbgeng-sys Engine, translates results to neutral types, and registers WinDbgFactory — so the existing 21 neutral tools drive WinDbg end-to-end, with the Normal/Attach/Pause integration groups green."
tasks:
  - id: "3.1"
    title: "Crate scaffold + EngineOps trait + FakeEngine + dedicated engine thread + EngineCmd marshaling"
    status: pending
    verification: "`windbg-backend` is a `cfg(windows)` member with `#![forbid(unsafe_code)]`; test files live in dedicated `tests/`/`src/tests/` folders (not inline `#[cfg(test)]`); the `dbgeng-sys` `Engine` surface is abstracted behind an `EngineOps` trait (object-safe) so a scripted **`FakeEngine`** can stand in for a live engine in unit tests (the `lldb-backend` `tokio::io::duplex` analog); `WinDbgFactory::connect()` spawns one `std::thread` whose **first** action is `CoInitializeEx(MTA)` + `Engine::create()`, signaling readiness via a `oneshot` (init failure → `BackendError::Detect`/`Spawn`); an `EngineCmd` round-trips through the channel and returns its `oneshot` reply (proven against `FakeEngine`); dropping the backend closes the command channel and ends the thread in the normal (non-kernel) path; the no-COM-on-calling-thread invariant is enforced by a `connect()` doc-comment and a unit test that calls `connect()` from a thread that was **not** `CoInitialize`d and asserts it does not fault and returns a ready backend. (Phase-entry task; README phase-level `depends_on: [2]` gates the start.)"
    depends_on: []
  - id: "3.2"
    title: "DebuggerBackend lifecycle: launch / attach / disconnect"
    status: pending
    verification: "Unit tests against `FakeEngine` cover the `launch` breakpoint-flush ordering, the `wait_for`→`findProcessByName`→pid mapping, and the connect-error wording (no live target); live integration then confirms: `launch(spec)` returns `LaunchOutcome::Stopped` at the initial break with the spec's pending breakpoints flushed and set (verified by an immediate `list`); `attach(pid)` stops a spawned process and `wait_for` resolves a named process; `debugger_pid()` reports the target/engine pid as the handlers expect; `disconnect(terminate)` detaches and ends the event-pump; a connect-failure resets the session to idle."
    depends_on: ["3.1"]
  - id: "3.3"
    title: "Execution + pause/interrupt + BackendEvent stream"
    status: pending
    verification: "`cont`/`step` block and return the next `StopOutcome`; `pause` (and a cancelled `cont` via the request token) breaks the target so it does not run forever (agent recovers with `pause`, mirroring lldb) — **and this holds under both R4 resolutions:** if 2.4 took `SetInterrupt`, `pause` sets the flag + calls `InterruptHandle::interrupt()`; if 2.4 took the flag-only fallback, `pause` sets the flag only and `InterruptHandle::interrupt()` is absent/no-op — the ≤200 ms break requirement is met either way (the handler reads the R4 decision recorded by task 2.4); the output-sink closure runs on the engine thread and contains only a `tokio::mpsc` `send()` (no await, no direct `OutputBuffer` write — Decision 6), with the `BackendEvent::Output` stream built from the receiver on the async side; process exit/EOF emits `BackendEvent::Terminated{code}` and the existing (already-tested) event-pump flips state to `terminated`."
    depends_on: ["3.2"]
  - id: "3.4"
    title: "Inspection / memory translation to neutral types"
    status: pending
    verification: "Unit tests against `FakeEngine` pin the DbgEng→neutral translation tables (frame/thread/variable/instruction field mapping, scopes→Locals group, `Variable.named`/`indexed`) without a live target; live integration then confirms at a known fixture stop that `threads`/`stack_trace`/`scopes`/`variables`/`evaluate`/`read_memory`/`disassemble` produce neutral structs the **existing** mcp-tools handlers format without modification; a full launch→breakpoint→backtrace(finds `main`)→variables(include locals)→step-over→evaluate flow succeeds through the real tool dispatch path; `run_command` routes a raw WinDbg command through `evaluate(EvalMode::Repl)` → `Engine::execute`."
    depends_on: ["3.3"]
  - id: "3.5"
    title: "Register WinDbgFactory + per-OS default + Normal/Attach/Pause integration"
    status: pending
    verification: "`WinDbgFactory` is registered in `main.rs` under `cfg(windows)` with `capabilities()` all-true; the per-OS default resolves to `windbg` on Windows (overridable via the `backend` arg / `DEBUG_BACKEND`); `list_tools` shows 25 on Windows; the `integration-windbg` Normal/Attach/Pause groups (port of the C++ `test_suite.py`) pass against the ported `testdata/win/test_target` fixture; the existing lldb suite stays green; ThreadSanitizer is clean over the engine-thread/`InterruptHandle` interaction."
    depends_on: ["3.4"]
---

# Phase 3: windbg-backend Core — engine thread + DebuggerBackend (21 ops)

## Overview

Stand up `windbg-backend`: a safe (`#![forbid(unsafe_code)]`) crate that owns a **dedicated
engine thread** (MTA COM, exclusive owner of the `dbgeng-sys::Engine`) and marshals async
`DebuggerBackend` calls to it over an `EngineCmd` channel, translating results into neutral
types. After this phase, the existing 21 neutral tools drive WinDbg through the real dispatch
path on Windows. Mirrors design §"The engine-thread model", Decisions 2/4/6, and Migration
Phase 2. Also introduces the ported Windows fixture (`testdata/win/test_target`) and the
`integration-windbg` suite.

## 3.1: Engine thread + marshaling

### Subtasks
- [ ] `crates/windbg-backend` (`cfg(windows)`, `#![forbid(unsafe_code)]`); deps `debugger-core`, `dbgeng-sys`, `tokio`. Tests in dedicated `tests/`/`src/tests/` folders.
- [ ] Extract an object-safe `EngineOps` trait over the `dbgeng-sys::Engine` surface; provide a scripted **`FakeEngine`** impl for unit tests (the `tokio::io::duplex` scripted-peer analog).
- [ ] Define `EngineCmd` (one variant per `EngineOps` op, each carrying a `oneshot` reply sender) + the engine-thread loop (`recv` → call engine → `reply.send`).
- [ ] `WinDbgFactory::connect()`: spawn the thread, build the command + event channels, await the readiness `oneshot`; map init failure to `Detect`/`Spawn`.
- [ ] Build the `BackendEvent` stream from the output `mpsc` + a `Terminated` signal (the `lldb-backend::build_event_stream` analog).
- [ ] Teardown: command-channel close → engine `detach` + `CoUninitialize` + thread exit.
- [ ] Unit test (against `FakeEngine`): an `EngineCmd` round-trip; `connect()` from a non-`CoInitialize`d thread does not fault and returns a ready backend; the no-COM-on-calling-thread invariant is documented in `connect()`.

### Notes
`connect()` runs on a tokio task and makes **zero** COM calls — the thread does COM init +
`Engine::create()` first and signals back. The `EngineOps`/`FakeEngine` split is what lets
3.2–3.4 unit-test marshaling/translation with no live target. The orphaned-kernel-thread
teardown edge (R2) is deferred to Phase 5; the normal path joins cleanly here.

## 3.2: Lifecycle

### Subtasks
- [ ] `launch`: map `LaunchSpec` → `Engine::launch`; after the initial break, flush the spec's source/function breakpoints (launch flushes; attach does not).
- [ ] `attach`: pid → `Engine::attach_pid`; `wait_for` → `findProcessByName` poll → pid.
- [ ] `disconnect(terminate)` → `Engine::detach(is_dump)`; `debugger_pid()` accessor.

### Notes
The neutral `LaunchSpec` already carries the pending breakpoints; WinDbg sets them *after* the
loader break (target already stopped), unlike lldb's during-config flush — the observable
outcome (launch returns Stopped with breakpoints set) is identical.

## 3.3: Execution + pause/interrupt + events

### Subtasks
- [ ] `cont`/`step` → `EngineCmd::Go`/`Step` (blocking; reply = `StopOutcome`).
- [ ] `pause` → set the interrupt flag, and (only if 2.4 resolved R4 with `SetInterrupt`) also call `InterruptHandle::interrupt()` — out-of-band, not a queued command; tool-layer cancel does the same via the request token. Read the R4 decision recorded by task 2.4; under the flag-only fallback, `InterruptHandle::interrupt()` is absent/no-op and `pause` relies on the 200 ms poll.
- [ ] Wire `Engine::set_output_sink` (a closure that only `send()`s to a `tokio::mpsc`, no await) → `BackendEvent::Output`; emit `Terminated{code}` on exit/EOF/engine-thread death.

### Notes
Serial command processing means `pause` must be out-of-band (Decision 4) or it would deadlock
behind the blocked `cont`. A cancelled `cont` leaves the session `running`; the agent recovers
with `pause` — exactly lldb's behavior. The output sink must not call into the async
`OutputBuffer` directly (it runs on the engine thread) — it only feeds the `mpsc`.

## 3.4: Inspection / memory translation

### Subtasks
- [ ] Map `threads`/`stack_trace`/`scopes`/`variables`/`evaluate`/`read_memory`/`disassemble` to neutral types; ensure `scopes` exposes the Locals group and `Variable.named`/`indexed` are populated for the flatten.
- [ ] `evaluate(EvalMode::Repl)` (used by `run_command`) → `Engine::execute` (no backtick; `supports_command_repl_mode()` = true); `evaluate(EvalMode::Expression)` → `?? expr`.
- [ ] Verify the existing handlers format these without change (response parity).

### Notes
This is where neutral-surface drift would show; lean on the existing mcp-tools formatters and
the Phase-5 differential harness.

## 3.5: Register + integration

### Subtasks
- [ ] Register `WinDbgFactory` in `main.rs` under `cfg(windows)`; flip the per-OS default.
- [ ] Port `test/test_target.cpp` → `testdata/win/test_target.c` (+ a build script producing a PDB): normal run, null-deref, access-violation, infinite-wait-for-attach.
- [ ] Add the `integration-windbg` Cargo feature + `cfg(windows)` gate; port the Normal/Attach/Pause groups.
- [ ] Run tsan over the engine-thread/`InterruptHandle` interaction.

### Notes
The Pause group specifically exercises the `S_FALSE`/`Break` recovery (R3): a `continue` timeout
returns "still running" and `pause` regains context.

## Acceptance Criteria
- [ ] `windbg-backend` builds `cfg(windows)` with `#![forbid(unsafe_code)]`; the engine thread owns all COM; `connect()` makes no COM calls.
- [ ] `launch`/`attach`/`disconnect`, `cont`/`step`/`pause`, and the full inspection/memory surface drive the 21 neutral tools end-to-end on Windows.
- [ ] `BackendEvent` output/terminated stream integrates with the unchanged `OutputBuffer`/event-pump.
- [ ] `WinDbgFactory` registered; default = windbg on Windows; `list_tools` shows 25.
- [ ] The `integration-windbg` Normal/Attach/Pause groups pass; the lldb suite stays green; tsan clean.
