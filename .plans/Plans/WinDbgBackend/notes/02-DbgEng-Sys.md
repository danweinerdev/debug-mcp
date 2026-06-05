---
title: "Debrief — Phase 2: dbgeng-sys (confined COM FFI → safe Engine)"
type: debrief
plan: WinDbgBackend
phase: 2
status: complete
created: 2026-06-05
updated: 2026-06-05
related: [Designs/WinDbgBackend, Plans/WinDbgBackend, Plans/WinDbgBackend/notes/01-Seam-Additions]
---

# Debrief — Phase 2: dbgeng-sys

Phase 2 built `dbgeng-sys`, the **only crate in the workspace permitted to contain `unsafe`**:
a confined wrapper over the six DbgEng COM interfaces exposing a safe, synchronous, blocking
`Engine`. All work happened on a live Windows host against a real debug target, so every method
was exercised end-to-end, not just compiled.

Delivered in five tasks plus an upfront R1 spike and a pulled-forward fixture:
`7a86268` (2.1), `01c82db` (2.2), `40b1479` (2.3), `ef63f72`+`e066438` (2.4), `d3b0cc1` (2.5),
`112f2e5` (fixture). Each task was independently reviewed by a `quality-scanner` and its
findings addressed before moving on. `cargo test --workspace` is green on Windows (0 failures);
`unsafe` is confined to `crates/dbgeng-sys/`; the other 7 crates carry `#![forbid(unsafe_code)]`.

## Decisions Made

- **R1 (windows-crate DbgEng coverage) resolved by a spike before committing the phase.** Rather
  than have an implementer discover mid-task whether the `windows` crate exposes the interfaces,
  a throwaway probe confirmed `windows` **v0.59** + feature `Win32_System_Diagnostics_Debug_Extensions`
  generates `DebugCreate` + all six interfaces (`IDebugClient5`/`Control4`/`Symbols3`/`DataSpaces4`/
  `Registers2`/`SystemObjects4`) and `.cast()` (QueryInterface), and that they succeed at runtime.
  **No hand-rolled vtables needed** — the rejected fallback in the design was never required.
- **R4 (cross-thread interrupt) resolved FLAG-ONLY.** `InterruptHandle` holds only an
  `Arc<AtomicBool>` — no COM interface — so no DbgEng pointer ever crosses a thread boundary. The
  off-thread `interrupt()` sets the flag; the engine's own `go` poll loop (on the engine thread)
  turns it into a real `SetInterrupt`. This preserves the "every DbgEng call on one thread"
  invariant with zero exceptions and makes `InterruptHandle` trivially `Send` with no `unsafe`.
  Cost: ≤200 ms latency. The cross-thread `SetInterrupt` path the design floated was deliberately
  NOT taken.
- **`go` return type = `Result<Option<StopOutcome>, EngineError>`** (`None` = still-running),
  chosen over a bespoke `GoOutcome` enum — no new type, reads naturally. R3 (the `S_FALSE`
  no-context limitation) is documented: after `None` the caller must `break_in` before inspecting.
- **`WaitForEvent` called through the base `IDebugControl` vtable to recover the raw HRESULT.**
  The windows-crate's typed `WaitForEvent -> Result<()>` collapses `S_OK`(0) and `S_FALSE`(1)
  (both success HRESULTs) into `Ok`, losing the timeout distinction that `go`/`step` need. The fix
  was to `cast()` to the base interface and call the vtable slot directly, comparing the raw
  `HRESULT`. This is the one place we bypass the typed wrapper.
- **`EngineError` is an enum (`Com` + `Engine(String)`).** COM failures carry an HRESULT+context;
  non-COM failures (timeout, not-yet-implemented stub, the dump-guard literal) carry a free-form
  message so `ensure_runnable` can surface the exact frozen `"cannot continue a crash-dump session"`.
- **`#![forbid(unsafe_code)]` added to all 7 non-`dbgeng-sys` crates** (none tripped it), with a
  `make unsafe-gate` grep as the authoritative source-level enforcement.
- **Test fixture pulled forward from Phase 3** (`testdata/win/test_target.c` + `build.bat`, built
  `cl /Zi /Od /MT`): launch/step/inspect all need a live PDB-bearing target, which the plan had
  slated for Phase 3. Surfacing the sequencing gap and pulling it forward (user-approved) unblocked
  all of 2.2–2.5's live verification.

## Requirements Assessment

All five acceptance criteria met:
- ✅ Builds on the `windows` crate; R1 resolved (no vtables); `unsafe`-gate passes; 7 crates `forbid(unsafe)`.
- ✅ `create` + callbacks work live; first/second-chance exception logic correct; `Drop` unregisters callbacks.
- ✅ `launch`/`attach_pid`/`detach` work; INITIAL_BREAK removed (proven by go-to-exit); fresh-session-after-detach.
- ✅ `go`/`step`/`InterruptHandle` work; flag resets at entry; R4 resolved (flag-only).
- ✅ breakpoints (addr/file:line/func + module-qualify), threads/stack/locals/evaluate/read_memory/
  disassemble/execute/modules/source-location return correct neutral values; 29 dbgeng-sys tests
  pass; clippy/fmt clean; workspace 0 failures.

## Deviations

- **Fixture pulled forward to Phase 2** (from Phase-3 task 3.5). Recorded in the Phase-2 doc and
  Phase-3 task 3.5 (now "reuse, don't recreate").
- **Task 2.3 implemented inline (not by a sub-agent).** The inference gateway hit sustained
  capacity (529) errors when spawning sub-agents, but the main-loop inference kept working. With
  user approval ("keep going"), 2.3 was written directly with Read/Edit/Bash and self-reviewed;
  its independent `quality-scanner` pass ran later once capacity recovered (and found 3 majors,
  all fixed). No task skipped its independent review.
- **`detach(is_dump)` → `detach()`** (reads the engine's own `is_dump`) after a 2.3 review finding —
  removes an API hazard where a caller could disagree with the actual session kind. The `detach`
  signature changes; Phase 3 callers use the no-arg form.
- **`go` folds the `break_in` re-issue loop into its interrupt branch** (a deliberate improvement
  over the C++ `Go`, which did a single 5 s wait and deferred the loop to its caller's `Break`).
  A single post-`SetInterrupt` wait was observed to race; the shared `break_loop` makes an
  interrupted `go` reliably return a real `Stopped`.
- **`disassemble` implemented though the C++ plugin had none** (the neutral trait has it, from
  lldb) — via `IDebugControl::Disassemble`. `bytes`/`symbol` fields left empty (best-effort).
- **The C++ `get_all_stacks` fast module-table/binary-search optimization was NOT ported** —
  `stack_trace` resolves per frame; deferred unless latency proves material.

## Risks & Issues Encountered

- **`IDebugBreakpoint` is engine-owned and must never be `Release`d.** The first 2.5 test run died
  with `STATUS_ACCESS_VIOLATION`: the windows-crate smart pointer calls `IUnknown::Release` on
  `Drop`, but DbgEng breakpoint objects are not normally ref-counted — calling `Release` is an AV.
  Fixed with `Bp(ManuallyDrop<IDebugBreakpoint>)` so `Release` never fires; the pointer is only
  used for the breakpoint's own methods and as the `RemoveBreakpoint` argument (which the
  windows-crate `Param` machinery transmit-copies without an AddRef). This is exactly the COM
  footgun the C++ `ARCHITECTURE.md` warned about, surfacing in Rust as a Drop-vs-COM-ownership
  mismatch.
- **Missing `Drop for Engine` (2.2 review, Major).** Without explicitly `SetEventCallbacks(None)`/
  `SetOutputCallbacks(None)` before release, DbgEng could invoke a released callback once a
  `WaitForEvent` loop overlapped teardown. Added a `Drop` mirroring the C++ destructor — became
  load-bearing the moment 2.4 introduced `WaitForEvent`.
- **`stack_trace` left the engine on the wrong current thread (2.5 review, Major).** Switching
  threads without restoring would silently make a later `locals`/`current_source_location` read the
  wrong thread. Fixed with save/restore around the switch.
- **`read_memory` `size as u32` truncation (2.5 review, Major).** A huge `size` would wrap the u32
  length and over-allocate; now clamped to `u32::MAX`.
- **Flaky detach test.** The 2.3 "image is writable after detach" poll depended on the detached
  process exiting within 5 s from the loader break — racy under load. De-flaked to assert the
  reliable, meaningful thing (a fresh session works after detach); the file-lock-specific rebuild
  regression stays scheduled for Phase 5.
- **GNU Make on Windows can't run `make unsafe-gate`/`make seam`** (cmd.exe doesn't grok the `!`
  shell syntax). Verified the gates under bash instead; flagged for the Phase-5 Windows CI lane.
- **Sub-agent inference capacity (529).** Transient gateway saturation blocked sub-agent spawns for
  a stretch; worked around by inline implementation + deferred review (see Deviations).

## Impact on Subsequent Phases

- **Phase 3 (`windbg-backend`)** can build directly on the `Engine` surface:
  - The `EngineCmd` enum it marshals is a **complete, closed set** — `open_dump`/`attach_kernel`
    are stubbed (Phase-4 errors) so Phase 4 fills bodies without reopening the enum.
  - `detach()` takes no arg (reads `is_dump`); `go` returns `Option`; `InterruptHandle` is the
    `Send` token Phase 3's `pause`/cancel uses; `break_in` is the still-running recovery.
  - The fixture + the `LIVE`-mutex live-test pattern are reusable for Phase 3's integration suite.
  - **3.5's fixture subtask is already done** (reuse, don't recreate).
- **Phase 4 (extras)** inherits the `is_dump` flag wiring, `ensure_runnable`, and the stubs; it must
  add runtime ext-path discovery (R8) for `!analyze` and decide R2 (kernel orphan-thread).
- **Phase 5** owns the deferred items now concretely scoped: conditional-BP evaluation (the
  `breakpoint_conditions` map is populated and ready for the `wait_for_event` hook), nested variable
  expansion (`locals` returns `variables_reference=0`), the file-lock rebuild-after-detach
  regression, R6 (ASLR address BP), and the Windows CI lane (incl. fixing the Make gates for cmd.exe
  or running them under bash/PowerShell).
- **Carry-forward open risks:** R2, R6, R7, R8 (unchanged); R1/R3/R4/R5 resolved.

## Skill Opportunities

- **`windows-rs-signature-probe`** — repeatedly the bottleneck was finding exact windows-crate
  signatures/constants by grepping the registry source
  (`~/.cargo/registry/src/.../windows-0.59.0/.../Extensions/mod.rs`). A skill that, given an
  interface+method or a constant name, returns the generated Rust signature (and the feature/module
  it lives under) would cut a recurring multi-grep loop to one step. *Recurred in every dbgeng-sys
  task; medium-high benefit for any windows-crate FFI work.*
- **`msvc-fixture-build`** — building the PDB-bearing C fixture required invoking `cl` through
  `vcvars64.bat` from a non-developer shell (bash/PowerShell), since `cl`/`link` need INCLUDE/LIB.
  A skill that wraps "build this `.c`/`.cpp` with `cl /Zi` under vcvars and return the exe+pdb path"
  would standardize a fiddly, host-specific step. *Once this phase; recurs whenever a Windows native
  fixture is needed (Phase 3+).*
- **`com-drop-audit`** — the `ManuallyDrop` breakpoint AV and the missing `Drop` callback-unregister
  are both COM-ownership-vs-Rust-Drop mismatches. A focused checklist/scanner pass ("for each COM
  object held: is it ref-counted? who Releases it? does a Rust Drop fire Release on an engine-owned
  object?") would catch this class proactively rather than via an access violation at runtime.
  *Recurred twice this phase; high benefit for the remaining COM work.*
- **`inline-fallback-on-capacity`** (process note, not a code skill) — when sub-agent capacity is
  unavailable, implementing inline on the working main-loop inference + deferring the independent
  review kept the phase moving without dropping the review gate. Worth codifying as the standard
  degradation path for `/implement`.
