---
title: "dbgeng-sys — confined COM FFI → safe Engine"
type: phase
plan: WinDbgBackend
phase: 2
status: in-progress
created: 2026-06-03
updated: 2026-06-04
deliverable: "A Windows-only `dbgeng-sys` crate that wraps the six DbgEng COM interfaces and exposes a safe, synchronous `Engine` (launch/attach/go/step/break/breakpoints/inspect/memory/execute/detach) plus the `Send` `InterruptHandle` — with ALL unsafe confined to this crate and proven against a live target via a smoke test."
tasks:
  - id: "2.1"
    title: "Crate scaffold + windows-crate interfaces (R1) + Engine::create + EngineError"
    status: complete
    verification: "`dbgeng-sys` is a `cfg(windows)` workspace member building on the `windows` crate; **R1 resolved** — the six interfaces (`IDebugClient5`/`Control4`/`Symbols3`/`DataSpaces4`/`Registers2`/`SystemObjects4`) are obtained via `DebugCreate` + `QueryInterface` (or a hand-rolled vtable for any missing method, documented); `Engine::create()` returns a live engine on a Windows host; the `unsafe`-confinement grep gate passes (`unsafe` appears only under `crates/dbgeng-sys/src/`); test files live in dedicated `tests/`/`src/tests/` folders (not inline `#[cfg(test)]`); `HRESULT`→`EngineError` mapping is unit-tested for success + a representative failure code. (Phase-entry task: the README phase-level `depends_on: [1]` gates the start — `dbgeng-sys` returns the `debugger-core` types added in Phase 1.)"
    depends_on: []
  - id: "2.2"
    title: "Event + output callbacks; output sink; stop-reason capture"
    status: complete
    verification: "A Rust `IDebugOutputCallbacks` impl routes `Execute()` output to `set_output_sink` (round-trip captured in a smoke test); the `IDebugEventCallbacks` impl records breakpoint id/offset, exception code/address, and exit code into the engine's last-stop state; first-chance exceptions pass through (`DEBUG_STATUS_NO_CHANGE`) while second-chance and the `0x80000003` initial breakpoint break — exercised by a controlled crash in the fixture; COM refcounting of the callback objects is correct (no leak/early-free under a create→detach cycle)."
    depends_on: ["2.1"]
  - id: "2.3"
    title: "Lifecycle: launch / attach_pid / detach + symbol path + engine-cmd surface completeness"
    status: complete
    verification: "`launch()` runs INITIAL_BREAK → `CreateProcess2` → `WaitForEvent` → `RemoveEngineOptions` → `Reload(\"/f <module>\")` and returns the initial-break `StopOutcome`; a subsequent `go()` does **not** immediately re-break (proving the option was removed); `attach_pid()` stops a separately-spawned process; `detach()` uses `EndSession(DEBUG_END_ACTIVE_DETACH)` and a rebuild-after-detach test confirms **no module file lock** is left; the symbol path is `srv*` cache-only with `SYMOPT_NO_IMAGE_SEARCH` (R5); **`Engine::open_dump` and `Engine::attach_kernel` are stubbed here** (placeholder error / `todo!`) so the `Engine` surface and the Phase-3 `EngineCmd` enum are a *complete, closed* set before Phase 3 consumes them — Phase 4 fills the bodies without reopening the enum; a dump-session `go`/`step` guard returns the frozen Phase-1 literal `\"cannot continue a crash-dump session\"`."
    depends_on: ["2.2"]
  - id: "2.4"
    title: "Execution: go (poll loop + interrupt flag + reset) / step / InterruptHandle"
    status: complete
    verification: "`go(&interrupt)` resets the flag at entry, polls `WaitForEvent(0,200ms)`, and returns `Stopped` at a breakpoint or `Exited`/'still running' on the `S_FALSE` deadline (R3: no clean context on timeout, documented); `step()` over/into land on the next line and out uses `gu`; `InterruptHandle::interrupt()` breaks a blocked `go()` within ~200 ms; a second `go()` immediately after does **not** spuriously interrupt (flag-reset test); **R4** is resolved — either the cross-thread `SetInterrupt` guarantee is cited in the `// SAFETY:` block or the flag-only fallback is taken (documented), with the `Send` `InterruptHandle` newtype + `Arc` keep-alive."
    depends_on: ["2.3"]
  - id: "2.5"
    title: "Breakpoints / inspection / memory / commands"
    status: in-progress
    verification: "Against the fixture at a known stop: a breakpoint set by function (with the `module!sym` fallback), by `file:line`, and by `0x<addr>` each resolves and is hit; `remove`/`list` reflect ids/offsets/hit-counts; `threads()`/`stack_trace()`/`locals()` (via `SetScope`+`GetScopeSymbolGroup2`)/`evaluate(\"?? expr\")` return correct values; `read_memory()` (`ReadVirtual`) returns the expected bytes and short-reads truncate cleanly; `disassemble()` returns the requested instruction count; `execute(\"r\")` returns register text via the output sink; `modules()` lists the exe with its PDB status; `current_source_location()` maps the IP to file:line."
    depends_on: ["2.4"]
---

# Phase 2: dbgeng-sys — confined COM FFI → safe Engine

> **Deviation (fixture pulled forward).** The Windows test fixture
> `testdata/win/test_target.c` (+ `build.bat`) — originally slated for Phase 3 task 3.5 — was
> created at the start of Phase 2, because tasks 2.2–2.5 need a live, PDB-bearing debuggable
> target. Built with `cl /Zi /Od /MT` via vcvars (`.exe`/`.pdb` git-ignored). Scenarios:
> `normal` (locals), `null` (null-deref AV), `av` (wild-pointer AV), `wait` (attach-by-pid).
> Phase 3 task 3.5 now *reuses/extends* it rather than creating it.

## Overview

Build the `dbgeng-sys` crate: the **only** place `unsafe` lives in the workspace. It wraps the
six DbgEng COM interfaces behind a safe, synchronous, blocking `Engine` whose methods are 1:1
with the C++ `DebugEngine` and return neutral `debugger-core` types. No async, no tokio — this
is a pure FFI layer that the Phase-3 engine thread drives. Every `unsafe` block carries a
`// SAFETY:` comment; the safe-wrapper boundary (no raw COM pointer escapes `Engine`, except
the deliberate `InterruptHandle`) is the audited invariant. Mirrors design §"The `dbgeng-sys`
safe surface", §"`InterruptHandle`", and Migration Phase 1.

## 2.1: Crate scaffold + windows interfaces + Engine::create

### Subtasks
- [x] Add `crates/dbgeng-sys` as a `cfg(windows)` member; depend on `windows` (feature `Win32_System_Diagnostics_Debug_Extensions` + `Win32_Foundation` + `Win32_System_Diagnostics_Debug`) and `debugger-core`.
- [x] **R1 RESOLVED:** the `windows` crate **v0.59** generates `DebugCreate` + all six interfaces (`IDebugClient5`/`Control4`/`Symbols3`/`DataSpaces4`/`Registers2`/`SystemObjects4`) and `.cast()` (QueryInterface); `DebugCreate` + the 6 QIs succeed at runtime on a Windows 11 host. **No hand-rolled vtables needed.**
- [x] `Engine::create()`: `DebugCreate(IDebugClient5)` + 5×`QueryInterface` + symbol path (`s!("srv*")`, ANSI) + `SYMOPT_NO_IMAGE_SEARCH`; stores the six interface smart pointers (refcount via Clone/Drop). (`SetEventCallbacks`/`SetOutputCallbacks` deferred to 2.2 — callbacks don't exist yet.)
- [x] Define `EngineError` (wraps `windows::core::Error` HRESULT + `&'static str` context) + `Display` mapping; unit-tested.
- [x] Add the `unsafe`-confinement grep gate (`make unsafe-gate`, in `all:`/`check:`) asserting `unsafe` only under `crates/dbgeng-sys/`; added `#![forbid(unsafe_code)]` to all 7 other crate roots (none tripped it).

### Notes
COM refcounting via the `windows` crate's `Drop` gives `AddRef`/`Release` for free, so the C++
manual-`Release` footgun does NOT apply — the six interfaces are held as smart pointers and drop
in field order. The engine is `!Send`/`!Sync` (thread-confinement documented at the `DebugCreate`
SAFETY block). **CI note for Phase 5:** `make unsafe-gate`/`make seam` use shell `!`-syntax that
GNU Make-on-Windows (cmd.exe) can't execute — the Windows CI lane needs a bash shell or a
PowerShell equivalent for these gates.

## 2.2: Callbacks + output sink

### Subtasks
- [x] Implement `IDebugOutputCallbacks::Output` → push text to the registered `set_output_sink(Box<dyn FnMut(OutputKind,&str)+Send>)`; `Arc<Mutex<CallbackState>>` (DbgEng calls back from internal threads).
- [x] Implement `IDebugEventCallbacks` (all methods, via the windows `#[implement]` macro): record BP id/offset, exception code/address (first/second-chance via the pure `exception_breaks`), exit code into last-stop state. `GetInterestMask` = BP|EXCEPTION|EXIT_PROCESS|LOAD_MODULE.
- [x] Provide `take_output()` (`GetAndClear` analog) for synchronous command output; `Drop for Engine` unregisters callbacks (`SetEventCallbacks(None)`/`SetOutputCallbacks(None)`) before release.

### Notes
First-chance non-breakpoint exceptions return `DEBUG_STATUS_NO_CHANGE`; only second-chance and
`0x80000003` (initial breakpoint) break — ported from `callbacks.cpp`. **windows-rs mechanics
learned:** `#[implement]` generates a `*_Impl` trait you impl on the `_Impl` wrapper; needs an
explicit `windows-core` dep; `DEBUG_STATUS_*` is conveyed as the method's `HRESULT` — `status()`
maps `0`(`NO_CHANGE`)→`Ok(())` and `6`(`BREAK`)→`Err(from_hresult(6))` (a success-range HRESULT,
so `HRESULT::ok()` must NOT be used). Live event-callback observation (crash/exit) is tested in
2.3 against the fixture; 2.2 tested the output path live + the first/second-chance logic as a
pure function.

## 2.3: Lifecycle

### Subtasks
- [x] `launch(&LaunchReq)`: `AddEngineOptions(INITIAL_BREAK)` → `CreateProcess2(DEBUG_ONLY_THIS_PROCESS|CREATE_NO_WINDOW)` → `wait_for_event` → `RemoveEngineOptions(INITIAL_BREAK)` → `Reload("/f <module>")`. (Program is quoted for space-safe paths; the raw-vtable `wait_for_event` recovers S_OK vs S_FALSE.)
- [x] `attach_pid(pid)`: best-effort `enable_debug_privilege` + `AttachProcess(DEBUG_ATTACH_DEFAULT)` + `wait_for_event` + `RemoveEngineOptions`.
- [x] `detach()`: reads the engine's own `is_dump` → `EndSession(DEBUG_END_ACTIVE_DETACH)` (live) / `DEBUG_END_PASSIVE` (dump).
- [x] Symbol path: `srv*` cache-only + `SYMOPT_NO_IMAGE_SEARCH` (R5) — set in `create()` (2.1).
- [x] **Stub** `Engine::open_dump`/`Engine::attach_kernel` (Phase-4 error) so the `Engine` API + the Phase-3 `EngineCmd` enum are complete now.
- [x] `ensure_runnable()` carries the frozen literal `"cannot continue a crash-dump session"` (the go/step guard 2.4 calls; `is_dump` set by `open_dump` in Phase 4).

### Notes
The INITIAL_BREAK removal is mandatory — leaving it set makes every `go` re-break. The
`ACTIVE_DETACH` choice (not `DetachProcesses`) is what frees module file locks for rebuilds.
Stubbing the two dump/kernel methods now keeps the `EngineCmd` enum a closed set across Phase 3
so Phase 4 is purely additive in the method bodies.

## 2.4: Execution + InterruptHandle

### Subtasks
- [x] `go(timeout_ms)`: resets the engine's shared `Arc<AtomicBool>` flag at entry, `SetExecutionStatus(GO)`, 200 ms `wait_for_event` poll loop checking the flag (Acquire); returns `Ok(Some(StopOutcome))` (stopped/exited) or `Ok(None)` (still-running, R3). (Conditional-BP hook is Phase 5.)
- [x] `step(kind)`: `STEP_OVER`/`STEP_INTO`; `Execute("gu")` for `Out`; `wait_for_event`.
- [x] `interrupt_handle()` → `InterruptHandle` holding a clone of the engine's `Arc<AtomicBool>` ONLY (no COM pointer). `interrupt()` sets the flag; the engine's own `go` poll loop turns it into a `SetInterrupt`-driven break on the engine thread.
- [x] `break_in()` + shared `break_loop()` port the C++ `Break()` recovery (SetInterrupt + ≤50×200 ms re-issue); `go`'s interrupt branch reuses it.

### Notes
**R4 RESOLVED — flag-only.** `InterruptHandle` holds no COM interface, so no DbgEng pointer ever
crosses a thread boundary (the confinement invariant holds with zero exceptions); it is trivially
`Send` with no `unsafe`. Cost: ≤200 ms latency for an off-thread `interrupt()` to be observed by
`go`'s poll. The real `SetInterrupt` is issued by `go`/`break_loop` on the engine thread. The
flag-reset-at-entry (sole `false`-writer) closes the spurious-interrupt race. `go`'s timeout is a
budget (may overshoot ~200 ms); `timeout_ms = 0` ≠ infinite.

## 2.5: Breakpoints / inspection / memory / commands

### Subtasks
- [ ] `set_breakpoint(loc, condition)`: dispatch `0x<addr>` / `file:line` (`GetOffsetByLine`) / function (`GetOffsetByName` + module-qualify fallback) → `AddBreakpoint` + `SetOffset` + `ENABLED`; store the condition in the engine-side map (eval deferred to Phase 5).
- [ ] `remove_breakpoint`/`list_breakpoints` (ids, offsets, flags, pass-count, symbolicated).
- [ ] `threads`/`stack_trace`/`locals`/`evaluate`/`read_memory`/`disassemble`/`execute`/`modules`/`current_source_location` → neutral types (all `&mut self`).
- [ ] Build the sorted module table + binary search used for fast symbol resolution (R5).

### Notes
`execute`/`evaluate`/`analyze` clear the output buffer, `Execute(DEBUG_OUTCTL_THIS_CLIENT,…)`,
then `GetAndClear`. `EnsureExtensionsLoaded` (for later `!analyze`) discovers the ext path at
runtime (R8) rather than hard-coding it.

## Acceptance Criteria
- [ ] `dbgeng-sys` builds on Windows on the `windows` crate; R1 resolved (interfaces available or vtable-backed); the `unsafe`-confinement grep gate passes.
- [ ] `Engine::create` + the output/event callbacks work against a live target; first/second-chance exception handling is correct.
- [ ] `launch`/`attach_pid`/`detach` work; INITIAL_BREAK is removed; detach leaves no file lock (rebuild test).
- [ ] `go`/`step`/`InterruptHandle` work; the flag resets at entry; R4 resolved (cited guarantee or fallback).
- [ ] Breakpoints (addr/file:line/func), inspection, memory, disassemble, execute, modules, and source-location return correct neutral values against the fixture; `dbgeng-sys` unit tests + the live smoke pass; clippy `-D warnings`/`fmt` clean.
