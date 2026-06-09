---
title: "WinDbg Extras — dump / kernel / analyze / modules (25 tools)"
type: phase
plan: WinDbgBackend
phase: 4
status: in-progress
created: 2026-06-03
updated: 2026-06-08
deliverable: "The four WinDbg-only capabilities wired end-to-end: open_crash_dump, attach_kernel, analyze_crash, get_modules — backed by dbgeng-sys dump/kernel/analyze/modules and the windbg-backend trait methods — with the Crash and Dump integration groups green and 25 tools live on Windows."
tasks:
  - id: "4.1"
    title: "dbgeng-sys: open_dump / attach_kernel / analyze / modules"
    status: complete
    verification: "`open_dump(path)` runs `OpenDumpFile` → `WaitForEvent(30s)` and returns a `DumpOutcome` whose `crash_location` comes from `current_source_location()`; `attach_kernel(conn)` validates KDNET and either polls with cancellation (R2 option a) or uses `INFINITE` with the orphan caveat (R2 option b) — the chosen behavior is documented and the bad/unreachable-connection path is covered; `analyze()` runs `!analyze -v` (extensions discovered at runtime per R8) and returns its text; `modules()` lists modules and a unit test asserts the `ModuleInfo` **format contract** (base = `\"0x{:016X}\"`, size = decimal string, symbol_status ∈ {pdb,export,deferred,none}) against a fixed value; dump sessions reject `go`/`step` with the frozen `\"cannot continue a crash-dump session\"` literal. (Phase-entry task; README phase-level `depends_on: [3]` gates the start.)"
    depends_on: []
  - id: "4.2"
    title: "windbg-backend: the four trait methods → engine"
    status: complete
    verification: "`open_dump`/`attach_kernel`/`analyze`/`modules` trait impls marshal to the engine thread and return the neutral types; `WinDbgFactory::capabilities()` reflects all four as supported; `open_dump`/`attach_kernel` follow the connect-point pattern (a fresh engine thread per session); a dump session correctly maps the `cannot continue a crash-dump session` error from the engine."
    depends_on: ["4.1"]
  - id: "4.3"
    title: "Wire the four tools end-to-end + capability listing"
    status: in-progress
    verification: "`open_crash_dump`/`attach_kernel` (connect points selecting windbg) and `analyze_crash`/`get_modules` (active backend) drive their engine methods; an end-to-end dump flow (`open_crash_dump` → `analyze_crash` → `backtrace` → `variables`) succeeds; `get_modules` returns the module list; the `attach_kernel` error path returns/cancels cleanly; `list_tools` advertises 25 on Windows; the four tools still return `Unsupported` against an active lldb session."
    depends_on: ["4.2"]
  - id: "4.4"
    title: "Integration: Crash + Dump groups"
    status: pending
    verification: "The `integration-windbg` Crash group (breakpoint on the crash function, access-violation detection, `!analyze -v`, exception record) and Dump group (generate a dump via `.dump`, `open_crash_dump`, full analyze→backtrace→variables) pass against the ported fixture; the lldb suite stays green; clippy `-D warnings`/`fmt` clean on the Windows lane."
    depends_on: ["4.3"]
---

# Phase 4: WinDbg Extras — dump / kernel / analyze / modules (25 tools)

## Overview

Light up WinDbg's distinctive value: crash-dump analysis, kernel debugging, `!analyze -v`, and
module listing. This phase implements the engine-side operations (`dbgeng-sys`), the four
neutral `DebuggerBackend` methods (`windbg-backend`), and connects them to the four tool
handlers that Phase 1 stubbed — so the full 25-tool surface is live on Windows. Mirrors design
§"`open_crash_dump` / `attach_kernel` are full connect points", §"Tool surface (additions)",
and Migration Phase 3.

## 4.1: dbgeng-sys dump / kernel / analyze / modules

### Subtasks
- [ ] `open_dump(path)`: `OpenDumpFile` → `WaitForEvent(30s)`; set `isDumpSession`; call `current_source_location()` for `DumpOutcome.crash_location`.
- [ ] `attach_kernel(conn)`: `AddEngineOptions(INITIAL_BREAK)` → `AttachKernel(DEBUG_ATTACH_KERNEL_CONNECTION, conn)` → wait; **decide R2** (polled-with-cancellation vs `INFINITE`+orphan) and document; on failure `EndSession(ACTIVE_DETACH)`.
- [ ] `analyze()`: `EnsureExtensionsLoaded` (runtime ext-path discovery, R8) → `execute("!analyze -v")`; truncate to the C++ cap.
- [ ] `modules()`: enumerate modules → `ModuleInfo` (base `0x{:016X}`, size decimal, symbol-status).
- [ ] Guard dump sessions: `go`/`step` return `cannot continue a crash-dump session`.

### Notes
R2 and R8 are resolved here. If the KDNET transport rejects short `WaitForEvent` timeouts (the
C++ claim), take the `INFINITE`+orphan path and ensure the Phase-5 pump fix forces a synthetic
`Terminated`.

## 4.2: windbg-backend trait methods

### Subtasks
- [ ] Implement `open_dump`/`attach_kernel`/`analyze`/`modules` → `EngineCmd` marshaling → neutral types.
- [ ] `WinDbgFactory::capabilities()` → all four true.
- [ ] `open_dump`/`attach_kernel` use the connect-point flow (fresh engine thread, set_backend, spawn_event_pump) exactly like `launch`.

### Notes
`analyze`/`modules` operate on the already-connected backend (no new connection); they require a
session and (for analyze) a stopped/dump-loaded state.

## 4.3: Wire the four tools

### Subtasks
- [ ] Finish `handle_open_crash_dump`/`handle_attach_kernel` (connect points → windbg) and `handle_analyze_crash`/`handle_get_modules` (active backend) to call the real methods.
- [ ] Confirm `list_tools` = 25 on Windows; the four return `Unsupported` on an active lldb session.
- [ ] Response shapes: dump-loaded status + crash_location; modules list; analyze text.

### Notes
These handlers were stubbed in Phase 1 (returning `Unsupported`); this task replaces the stubs
with live calls now that the backend implements the methods.

## 4.4: Integration Crash + Dump

### Subtasks
- [ ] Port the C++ `test_suite.py` Crash group (BP on crash fn, AV detection, `!analyze`, `.exr`/exception record).
- [ ] Port the Dump group (generate a `.dmp` via `.dump`, `open_crash_dump`, analyze→backtrace→variables).
- [ ] Add an `attach_kernel` error-path test (bad/unreachable connection) per the R2 decision; mark a live-KDNET test `#[ignore]` unless a VM is available.

### Notes
The Crash/Dump groups are the headline WinDbg parity; they validate the extension-load path
(R8) and the dump-session guards.

## Acceptance Criteria
- [ ] `dbgeng-sys` implements dump/kernel/analyze/modules; R2 and R8 resolved and documented; dump sessions reject execution with a clear error.
- [ ] `windbg-backend` implements the four trait methods; `capabilities()` all-true; connect-point flow for dump/kernel.
- [ ] The four tools are live end-to-end; `list_tools` shows 25 on Windows; they return `Unsupported` against lldb.
- [ ] The `integration-windbg` Crash + Dump groups pass; the lldb suite stays green; clippy/fmt clean.
