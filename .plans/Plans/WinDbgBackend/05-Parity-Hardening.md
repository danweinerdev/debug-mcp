---
title: "Parity Hardening — cond-BP, cancel/interrupt, test_suite port, CI lane"
type: phase
plan: WinDbgBackend
phase: 5
status: in-progress
created: 2026-06-03
updated: 2026-06-09
deliverable: "Behavioral parity with the C++ oracle closed out: engine-side conditional breakpoints, the R6 ASLR address-BP handling, the R2 orphaned-thread pump fix, cancellation/interrupt + Break recovery, backend-aware error strings, the ported Error test group + a differential Windows lane, and a CI Windows lane — with CLAUDE.md parity notes finalized."
tasks:
  - id: "5.0"
    title: "Runtime debugger-extension path discovery (R8 refinement)"
    status: complete
    verification: "`ensure_extensions_loaded` no longer hardcodes the WinKits install path: it discovers the Debuggers\\x64 root at runtime (registry `KitsRoot10` under `SOFTWARE\\Microsoft\\Windows Kits\\Installed Roots`, native + WOW6432Node views; then `WindowsSdkDir` env; then the former hardcoded default as a last resort), appends only the `winext`/`winxp`/base dirs that actually EXIST to the `.extpath`, then `.load ext.dll`; the discovery result (resolved root or 'no extensions found') is observable, not silently swallowed. Live on a host with the full Debugging Tools installed, `analyze()` returns a real `!analyze -v` report (contains a recognizable token e.g. `EXCEPTION`/`FAULTING`/`ACCESS_VIOLATION`), and the 4.4 integration analyze assertion is tightened to take the strict branch; on a host WITHOUT the extensions the discovery degrades cleanly (empty extpath, `analyze` returns the engine's `No export analyze found`) with no panic. The registry read is confined `unsafe` in `dbgeng-sys` with `// SAFETY:` comments; unit tests cover the path-assembly/existence-filter logic with injected roots."
    depends_on: []
  - id: "5.1"
    title: "Engine-side conditional breakpoints"
    status: complete
    verification: "A conditional breakpoint fires only when the condition holds (e.g. `i == 5` in a loop fixture): the `go` poll loop, on a BP stop, evaluates `@@c++( (cond) ? 1 : 0 )` and resumes when false; an **unresolvable** condition (variable out of scope) silently skips (the documented C++ footgun) — both paths are covered by fixture tests; conditions survive in the engine-side map across `remove`/`list`."
    depends_on: ["5.0"]
  - id: "5.2"
    title: "R6 ASLR address-BP handling + R2 orphaned-thread pump fix"
    status: complete
    verification: "A `module!sym` function breakpoint re-flushes correctly across a relaunch (rebase-stable); a bare `0x<addr>` breakpoint is routed via `run_command(\"bp <addr>\")` and is **not** session-tracked — a relaunch test shows no misplaced breakpoint, and the `set_function_breakpoint` description warns about address+ASLR when windbg is active; an orphaned kernel-attach engine thread still drives the session to `terminated` via a synthetic `Terminated` (dead-flag / explicit event-sender close), so a fresh `launch` reconnects without a hung event-pump."
    depends_on: ["5.1"]
  - id: "5.3"
    title: "Cancellation / interrupt + Break recovery + backend-aware error strings"
    status: complete
    verification: "A `continue` that times out returns 'still running' and a subsequent `pause` regains context via the C++ `Break()` recovery (R3); a cancelled `launch`/`attach` resets the session to idle and drops the backend; a rebuild-after-detach test is **re-run in the Phase-5 regression sweep** to confirm no module file lock is left (the task 2.3 criterion held end-to-end); windbg connect-error strings (`Debugging Tools for Windows not found` / `failed to initialize DbgEng`) surface for the windbg factory while lldb's strings are unchanged; `BackendError::Unsupported` maps to the exact `\"<tool> is not supported by the <backend> backend\"` text."
    depends_on: ["5.2"]
  - id: "5.4"
    title: "Port C++ Error group + differential parity Windows lane"
    status: complete
    verification: "The ported `test_suite.py` Error group passes (wrong-state rejections, bad/missing arguments, double launch, invalid breakpoint locations) with parity-exact guard strings; the differential harness runs the same neutral tool sequence against the windbg backend and, for shared behaviors (`backtrace`/`variables`/`threads`/`read_memory`), compares response JSON field-by-field against the lldb backend, catching neutral-surface drift."
    depends_on: ["5.3"]
  - id: "5.5"
    title: "CI Windows lane + finalize docs / structural gates"
    status: in-progress
    verification: "CI builds the full workspace and runs `integration-windbg` on a Windows runner (or a documented self-hosted/manual gate per R7), with the `unsafe`-confinement grep gate, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --check`, and ThreadSanitizer over the engine thread; **Miri is explicitly excluded for `dbgeng-sys`** (COM FFI is not Miri-compatible — noted in CLAUDE.md) while it continues to run green over the neutral crates; the existing Linux/macOS lane is unchanged and green; CLAUDE.md's parity-notes list and architecture/crate table are finalized (the unsafe deviation, the `backend` arg, the four new tools, the get_all_stacks/address-BP deviations)."
    depends_on: ["5.4"]
---

# Phase 5: Parity Hardening — cond-BP, cancel/interrupt, test_suite port, CI lane

## Overview

Close the remaining gaps between the Rust WinDbg backend and the C++ oracle, resolve the
trickier risks (R2 orphaned thread, R6 ASLR address BPs), port the C++ Error test group, add a
differential Windows lane, and wire the CI Windows lane. After this phase the WinDbg backend is
feature-complete and parity-validated. Mirrors design Decisions 4/5, Open Risks R2/R3/R6/R7, and
Migration Phase 4.

## 5.0: Runtime debugger-extension path discovery (R8 refinement)

Pulled forward from the R8 deferral (Phase 4 used the C++-parity hardcoded `.extpath`). Surfaced
by the 4.4 debrief: on a host without the full *Debugging Tools for Windows* at the exact default
path, the hardcoded `.extpath` points at non-existent dirs, `.load ext.dll` silently fails, and
`analyze_crash` returns the engine's `No export analyze found` for every call — affecting any
non-default / partial install. Doing it now (while a full toolset is installed for testing) lets
the resolved path be validated live.

### Subtasks
- [ ] Replace the hardcoded `.extpath` in `ensure_extensions_loaded` with runtime discovery of the
      `Debuggers\x64` root: registry `KitsRoot10` (`HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`
      and the `WOW6432Node` view) → `WindowsSdkDir` env → the former hardcoded default as last resort.
- [ ] Build the `.extpath` from only the candidate dirs (`<root>\winext`, `<root>\winxp`, `<root>`)
      that actually EXIST; `.load ext.dll` after. Make the outcome observable (log the resolved root
      or a clear "no debugger extensions found" rather than silently swallowing).
- [ ] Confine the registry read to `dbgeng-sys` with `// SAFETY:` comments; unit-test the
      path-assembly + existence-filter with injected roots (no live registry needed for the logic).
- [ ] Tighten the 4.4 integration analyze assertion: with the extension now resolvable on the test
      host, assert a real `!analyze -v` token (drop the lenient `No export` branch, or keep it as a
      documented graceful-degradation fallback for extension-less CI hosts).

### Notes
Discovery makes `analyze_crash` and `run_command("!...")` work wherever the SDK is actually
installed, not just the default path — the generalization the C++ hardcoding missed. On a host with
no extensions installed at all it degrades cleanly (empty extpath, the engine's own
`No export analyze found`), no panic.

## 5.1: Conditional breakpoints

### Subtasks
- [ ] Fill the `go` poll-loop conditional-BP hook: on a BP stop, look up the condition in the engine-side map, `Evaluate("@@c++( (cond) ? 1 : 0 )", DEBUG_VALUE_INT64)`, resume + re-loop when 0.
- [ ] Treat eval failure (out-of-scope variable) as false ⇒ skip (documented footgun).
- [ ] Fixture test: a loop with `i == N` fires exactly once; an unresolvable condition never fires.

### Notes
This is the only mechanism that works around the `gc`-can't-re-enter-`WaitForEvent` limitation
(Decision 5); the condition is never set on the DbgEng BP object.

## 5.2: R6 ASLR address-BP + R2 orphaned-thread pump

### Subtasks
- [ ] Allow `module!sym` through the `set_function_breakpoint` `name` field (rebase-stable, re-flush-safe); route bare `0x<addr>` BPs through `run_command("bp <addr>")` (no session tracking).
- [ ] Add the windbg-active tool-description note about address BPs + ASLR.
- [ ] R2 pump fix: on backend-drop/cancel of a hung kernel attach, force a synthetic `BackendEvent::Terminated` (dead-flag or explicit event-sender close) so the event-pump ends and the session reaches `terminated`.

### Notes
The orphaned engine thread still leaks until process exit (documented C++ behavior); the fix is
only about not hanging the session/pump so a fresh `launch` can reconnect.

## 5.3: Cancellation + Break recovery + error strings

### Subtasks
- [ ] Wire `pause` to the `Break()` recovery for the `S_FALSE`-no-context case (R3).
- [ ] Verify cancelled `launch`/`attach` reset to idle and drop the backend (the existing `cleanup_after_cancel` path).
- [ ] Finalize backend-aware `connect_error` wording for windbg; confirm `Unsupported` text.

### Notes
The continue-timeout → pause-recovers flow is the WinDbg analog of lldb's cancellation; keep the
observable messages aligned with the design's Error Handling table.

## 5.4: Error group + differential parity

### Subtasks
- [ ] Port the C++ `test_suite.py` Error group (wrong state, bad args, double launch, invalid BP) with parity-exact guard strings.
- [ ] Extend the differential harness with a Windows lane comparing windbg vs lldb shared-behavior responses field-by-field.

### Notes
The differential lane operationalizes "neutral-surface parity between backends" — it catches
response-shape drift the per-test mirrors miss.

## 5.5: CI Windows lane + docs

### Subtasks
- [ ] Add the CI Windows lane: build the workspace, run `integration-windbg`, the `unsafe`-confinement grep gate, clippy `-D warnings`, `fmt --check`, and tsan over the engine thread.
- [ ] If no hosted Windows runner is available (R7), gate `integration-windbg` to a documented self-hosted/manual run and keep the Phase-1 cross-platform gates always-on.
- [ ] Finalize CLAUDE.md: parity-notes list + architecture/crate table (unsafe deviation, `backend` arg, four new tools, get_all_stacks + address-BP deviations).

### Notes
The Linux/macOS lane must remain exactly as today (the WinDbg crates are `cfg(windows)`, absent
there); a botched WinDbg crate can never break the non-Windows build.

## Acceptance Criteria
- [ ] Engine-side conditional breakpoints fire correctly; eval-fail skips (documented).
- [ ] `module!sym` BPs re-flush safely; bare-address BPs are not session-tracked; the ASLR note is in the tool description.
- [ ] The R2 orphaned-thread pump fix drives the session to `terminated` so a fresh launch reconnects.
- [ ] Continue-timeout → pause-recovers (Break path) works; cancellation resets to idle; windbg/lldb error strings are correct.
- [ ] The ported Error group + the differential Windows lane pass.
- [ ] The CI Windows lane runs the WinDbg suite + structural gates (or a documented manual gate); the Linux/macOS lane is unchanged and green; CLAUDE.md finalized.
