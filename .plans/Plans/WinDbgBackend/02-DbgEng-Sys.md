---
title: "dbgeng-sys — confined COM FFI → safe Engine"
type: phase
plan: WinDbgBackend
phase: 2
status: pending
created: 2026-06-03
updated: 2026-06-03
deliverable: "A Windows-only `dbgeng-sys` crate that wraps the six DbgEng COM interfaces and exposes a safe, synchronous `Engine` (launch/attach/go/step/break/breakpoints/inspect/memory/execute/detach) plus the `Send` `InterruptHandle` — with ALL unsafe confined to this crate and proven against a live target via a smoke test."
tasks:
  - id: "2.1"
    title: "Crate scaffold + windows-crate interfaces (R1) + Engine::create + EngineError"
    status: pending
    verification: "`dbgeng-sys` is a `cfg(windows)` workspace member building on the `windows` crate; **R1 resolved** — the six interfaces (`IDebugClient5`/`Control4`/`Symbols3`/`DataSpaces4`/`Registers2`/`SystemObjects4`) are obtained via `DebugCreate` + `QueryInterface` (or a hand-rolled vtable for any missing method, documented); `Engine::create()` returns a live engine on a Windows host; the `unsafe`-confinement grep gate passes (`unsafe` appears only under `crates/dbgeng-sys/src/`); test files live in dedicated `tests/`/`src/tests/` folders (not inline `#[cfg(test)]`); `HRESULT`→`EngineError` mapping is unit-tested for success + a representative failure code. (Phase-entry task: the README phase-level `depends_on: [1]` gates the start — `dbgeng-sys` returns the `debugger-core` types added in Phase 1.)"
    depends_on: []
  - id: "2.2"
    title: "Event + output callbacks; output sink; stop-reason capture"
    status: pending
    verification: "A Rust `IDebugOutputCallbacks` impl routes `Execute()` output to `set_output_sink` (round-trip captured in a smoke test); the `IDebugEventCallbacks` impl records breakpoint id/offset, exception code/address, and exit code into the engine's last-stop state; first-chance exceptions pass through (`DEBUG_STATUS_NO_CHANGE`) while second-chance and the `0x80000003` initial breakpoint break — exercised by a controlled crash in the fixture; COM refcounting of the callback objects is correct (no leak/early-free under a create→detach cycle)."
    depends_on: ["2.1"]
  - id: "2.3"
    title: "Lifecycle: launch / attach_pid / detach + symbol path + engine-cmd surface completeness"
    status: pending
    verification: "`launch()` runs INITIAL_BREAK → `CreateProcess2` → `WaitForEvent` → `RemoveEngineOptions` → `Reload(\"/f <module>\")` and returns the initial-break `StopOutcome`; a subsequent `go()` does **not** immediately re-break (proving the option was removed); `attach_pid()` stops a separately-spawned process; `detach()` uses `EndSession(DEBUG_END_ACTIVE_DETACH)` and a rebuild-after-detach test confirms **no module file lock** is left; the symbol path is `srv*` cache-only with `SYMOPT_NO_IMAGE_SEARCH` (R5); **`Engine::open_dump` and `Engine::attach_kernel` are stubbed here** (placeholder error / `todo!`) so the `Engine` surface and the Phase-3 `EngineCmd` enum are a *complete, closed* set before Phase 3 consumes them — Phase 4 fills the bodies without reopening the enum; a dump-session `go`/`step` guard returns the frozen Phase-1 literal `\"cannot continue a crash-dump session\"`."
    depends_on: ["2.2"]
  - id: "2.4"
    title: "Execution: go (poll loop + interrupt flag + reset) / step / InterruptHandle"
    status: pending
    verification: "`go(&interrupt)` resets the flag at entry, polls `WaitForEvent(0,200ms)`, and returns `Stopped` at a breakpoint or `Exited`/'still running' on the `S_FALSE` deadline (R3: no clean context on timeout, documented); `step()` over/into land on the next line and out uses `gu`; `InterruptHandle::interrupt()` breaks a blocked `go()` within ~200 ms; a second `go()` immediately after does **not** spuriously interrupt (flag-reset test); **R4** is resolved — either the cross-thread `SetInterrupt` guarantee is cited in the `// SAFETY:` block or the flag-only fallback is taken (documented), with the `Send` `InterruptHandle` newtype + `Arc` keep-alive."
    depends_on: ["2.3"]
  - id: "2.5"
    title: "Breakpoints / inspection / memory / commands"
    status: pending
    verification: "Against the fixture at a known stop: a breakpoint set by function (with the `module!sym` fallback), by `file:line`, and by `0x<addr>` each resolves and is hit; `remove`/`list` reflect ids/offsets/hit-counts; `threads()`/`stack_trace()`/`locals()` (via `SetScope`+`GetScopeSymbolGroup2`)/`evaluate(\"?? expr\")` return correct values; `read_memory()` (`ReadVirtual`) returns the expected bytes and short-reads truncate cleanly; `disassemble()` returns the requested instruction count; `execute(\"r\")` returns register text via the output sink; `modules()` lists the exe with its PDB status; `current_source_location()` maps the IP to file:line."
    depends_on: ["2.4"]
---

# Phase 2: dbgeng-sys — confined COM FFI → safe Engine

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
- [ ] Add `crates/dbgeng-sys` as a `cfg(windows)` member; depend on `windows` (features for `Win32_System_Diagnostics_Debug` / `Extensions`) and `debugger-core` (neutral types only).
- [ ] **Resolve R1:** confirm the six interfaces + the methods used are generated; for any gap, hand-roll a `#[repr(C)]` vtable in a `vtable` module (confined unsafe) and record it.
- [ ] `Engine::create()`: `DebugCreate(IDebugClient5)` + 5×`QueryInterface` + `SetEventCallbacks`/`SetOutputCallbacks`; store the six pointers; set the symbol path + options.
- [ ] Define `EngineError` (wraps `HRESULT` + context) and the `HRESULT`→`EngineError` mapper.
- [ ] Add the `unsafe`-confinement grep gate (CI script) asserting `unsafe` only under `crates/dbgeng-sys/src/`.

### Notes
COM refcounting via the `windows` crate's `Drop` gives `AddRef`/`Release` for free; the C++
`unique_ptr`-vs-COM-refcount footgun (callbacks AddRef'd by DbgEng) must be reproduced
faithfully — model callback ownership so the refcount, not a Rust owner, drives teardown.

## 2.2: Callbacks + output sink

### Subtasks
- [ ] Implement `IDebugOutputCallbacks::Output` → push text to the registered `set_output_sink(Box<dyn FnMut(OutputKind,&str)+Send>)`; mutex-guard the sink (DbgEng may call back from internal threads).
- [ ] Implement `IDebugEventCallbacks` (the 13 methods): record BP id/offset, exception code/address (first- vs second-chance logic), exit code into last-stop state; log module loads.
- [ ] Provide `GetAndClear`-style capture for synchronous command output (`execute`/`evaluate`).

### Notes
First-chance exceptions return `DEBUG_STATUS_NO_CHANGE` (pass through); only second-chance and
`0x80000003` (initial breakpoint) break — port the C++ `callbacks.cpp` logic exactly.

## 2.3: Lifecycle

### Subtasks
- [ ] `launch(&LaunchReq)`: `AddEngineOptions(INITIAL_BREAK)` → `CreateProcess2(DEBUG_ONLY_THIS_PROCESS|CREATE_NO_WINDOW)` → `WaitForEvent` → `RemoveEngineOptions(INITIAL_BREAK)` → `Reload("/f <module>")`.
- [ ] `attach_pid(pid)`: `EnableDebugPrivilege` (caller) + `AttachProcess(DEBUG_ATTACH_DEFAULT)` + `WaitForEvent` + `RemoveEngineOptions`.
- [ ] `detach(is_dump)`: `EndSession(DEBUG_END_ACTIVE_DETACH)` (live) / `EndSession(DEBUG_END_PASSIVE)` (dump); reset session state.
- [ ] Symbol path: `srv*` cache-only + `SYMOPT_NO_IMAGE_SEARCH` (R5).
- [ ] **Stub** `Engine::open_dump`/`Engine::attach_kernel` (placeholder error) so the `Engine` API + the Phase-3 `EngineCmd` enum are complete now; Phase 4 fills the bodies.
- [ ] Guard dump-session `go`/`step` with the frozen literal `"cannot continue a crash-dump session"` (Phase-1 contract).

### Notes
The INITIAL_BREAK removal is mandatory — leaving it set makes every `go` re-break. The
`ACTIVE_DETACH` choice (not `DetachProcesses`) is what frees module file locks for rebuilds.
Stubbing the two dump/kernel methods now keeps the `EngineCmd` enum a closed set across Phase 3
so Phase 4 is purely additive in the method bodies.

## 2.4: Execution + InterruptHandle

### Subtasks
- [ ] `go(&AtomicBool)`: reset the flag at entry (Relaxed store), `SetExecutionStatus(GO)`, 200 ms `WaitForEvent` poll loop checking the flag (Acquire) + the conditional-BP hook (Phase 5 fills the eval); return `Stopped`/`Exited`/'still running' (S_FALSE).
- [ ] `step(kind)`: `STEP_OVER`/`STEP_INTO`; `gu` (Execute) for `OUT`; `WaitForEvent`.
- [ ] `interrupt_handle()` → mint a separately-AddRef'd `IDebugControl4` `InterruptHandle` (`NonNull`, `unsafe impl Send`, `// SAFETY:` with the R4 citation/fallback); `InterruptHandle::interrupt()` issues `SetInterrupt(DEBUG_INTERRUPT_ACTIVE)`.
- [ ] Port the `Break()` recovery shape (re-`SetInterrupt` + `WaitForEvent`) for the S_FALSE-no-context case (consumed by `pause` in Phase 3/5).

### Notes
R4 is load-bearing: confirm the MS-docs cross-thread `SetInterrupt` guarantee; if unconfirmable,
drop `SetInterrupt` and rely solely on the 200 ms flag poll (≤200 ms pause latency, zero
cross-thread COM). The flag-reset-at-entry closes the spurious-interrupt race.

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
