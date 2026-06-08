---
title: "Debrief — Phase 3: windbg-backend Core (engine thread + DebuggerBackend)"
type: debrief
plan: WinDbgBackend
phase: 3
status: complete
created: 2026-06-08
updated: 2026-06-08
related: [Designs/WinDbgBackend, Plans/WinDbgBackend, Plans/WinDbgBackend/notes/02-DbgEng-Sys]
---

# Debrief — Phase 3: windbg-backend Core

Phase 3 built `windbg-backend`: a `#![forbid(unsafe_code)]` crate that owns a **dedicated MTA-COM
engine thread**, marshals async `DebuggerBackend` calls to the `dbgeng-sys::Engine` over an
`EngineCmd` channel, translates results into neutral types, and registers `WinDbgFactory` in the
binary. After this phase the existing 21 neutral tools drive WinDbg end-to-end on Windows through
the real tool-dispatch path, and `list_tools` advertises 25 tools (21 base + 4 capability-gated).

Delivered in five tasks plus an in-phase gap fix: `f69a228` (3.1), `c4a7bc1` (3.2), `f9a5f8b`
(3.3), `7735c59` (3.4), `c66520e` (3.5), `c6becc8` (runtime breakpoint setters + reconciliation).
Every task ran the full per-task discipline — `code-implementer` → `quality-scanner` → fix round
→ gate (clippy `-D warnings` / fmt / unsafe-gate / `cargo test --workspace`) → commit (no
co-author trailer) → plan-doc status update. All work was verified on a live Windows host with the
PDB-bearing fixture present, so every op ran end-to-end, not just compiled. `cargo test
--workspace` is green (0 failures); `unsafe` stays confined to `crates/dbgeng-sys/`.

## Decisions Made

- **`cont` blocks with an effectively-infinite budget (`EngineCmd::Go { timeout_ms: u32::MAX }`).**
  WinDbg/lldb `continue` semantics are "block until the next stop." The engine's `go` polls (~200 ms)
  for a stop/exit OR the cooperative interrupt flag, so an unbounded budget is the correct
  block-until-stop. User cancellation lives at the tool layer (the request token); `pause` is what
  actually breaks a running target. The `Ok(None)` deadline arm is practically unreachable and falls
  back to `break_in()` to regain a real stop-with-context (documented as the safety net).
- **`disconnect` sets the interrupt flag BEFORE marshaling `Detach`.** A free-running `cont`'s `go`
  blocks the engine thread and only polls the interrupt flag (not the command channel), so a queued
  `Detach` would never run. Tripping the flag first makes the in-flight `go` break and return, so the
  engine thread loops back and processes the queued `Detach`. With nothing running the flag is
  harmlessly consumed (the next `go` resets it at entry). This makes the common free-running-cont
  disconnect clean; it does NOT solve the uncancellable kernel-wait orphan (R2, Phase 5).
- **`thread_id`/`gran` are deliberately ignored by `cont`/`step`.** WinDbg `g` resumes the whole
  target (no per-thread continue, unlike DAP) and DbgEng step is source/line-oriented (no separate
  instruction-granularity knob). Both documented as WinDbg behavior notes, matching the C++ plugin.
- **`scopes`/`variables` reference encoding = `frame_id + 1` / `frame_index = reference - 1`.** The
  `+1` keeps frame 0 expandable (the flatten only recurses on references `> 0`). Guarded against the
  `<= 0` underflow and an implausibly large reference; `resolve_frame_id` bounds `frame_id` by the
  engine's 1024-frame cap so the saturation corner is unreachable.
- **WinDbg locals are flat (top-level only): `named = indexed = variables_reference = 0`.** DbgEng's
  symbol-group surface returns locals as a flat list, so each renders as a leaf. The unchanged
  `flatten_variables` treats `variables_reference == 0` as a leaf and renders name+value correctly —
  no data loss. Nested struct/array expansion is deferred (needs a `dbgeng-sys` child-symbol path).
- **`evaluate` Repl vs Expression.** `EvalMode::Repl` (used by `run_command`) → `Engine::execute` raw
  (no backtick stripping; `supports_command_repl_mode()` is true); `EvalMode::Expression` → `?? expr`.
- **Breakpoints are reconciled declaratively (the `BreakpointTable`).** The tool layer sends the full
  desired breakpoint list on every `set_*` call (lldb/DAP replace-all semantics), and the scenario
  suite's `hit_breakpoint_ids` contract depends on ids staying stable across re-sends. But DbgEng
  `AddBreakpoint` always mints a NEW breakpoint. So the backend keeps a per-category table
  (source: `file → line → result`; function: `name → result`) and reconciles add/reuse/remove: reuse
  the engine id for an unchanged location, add new ones, remove stale ones. Source reconcile is
  per-file; function reconcile is whole-category — mirroring DAP `setBreakpoints`/`setFunctionBreakpoints`.
- **Per-bp error vs transport error.** An unresolvable breakpoint (engine `Err`, e.g. no symbols)
  becomes an unverified `BreakpointResult { verified: false, id: 0, line: 0|requested }` and the batch
  continues — matching lldb's `body::breakpoints`, which never fails the request for one bad line.
  Only `BackendError::Closed` (engine thread dead) aborts the batch.
- **`WinDbgFactory` registered under `cfg(windows)` in the binary**, mirroring the lldb `cfg(not(windows))`
  arm; `default_backend_for_os()` already resolved to `windbg` on Windows, so registration alone lit
  up the per-OS default and the 25-tool capability-gated list.

## Requirements Assessment

All five Phase-3 acceptance criteria met:
- ✅ `windbg-backend` builds `cfg(windows)` with `#![forbid(unsafe_code)]`; the engine thread owns all
  COM; `connect()` makes zero COM calls on the caller (structural + runtime test).
- ✅ `launch`/`attach`/`disconnect`, `cont`/`step`/`pause`, and the full inspection/memory surface
  drive the 21 neutral tools end-to-end on Windows (live `integration-windbg` Normal/Attach/Pause +
  the breakpoint workflow).
- ✅ `BackendEvent` output/terminated stream integrates with the unchanged `OutputBuffer`/event-pump
  (verified live; debuggee-stdout limitation documented — see Deviations).
- ✅ `WinDbgFactory` registered; default = windbg on Windows; `list_tools` shows 25 (the non-fixture
  Protocol test asserts this live).
- ⚠️ The `integration-windbg` Normal/Attach/Pause groups pass and the lldb suite stays green; **tsan
  is NOT run on the Windows engine-thread path** (toolchain limitation — see Risks). Recorded as a
  Phase-5 CI-lane item rather than a met criterion.

## Deviations

- **Runtime breakpoint setters were an unfinished placeholder discovered at 3.5, not 3.3.**
  `set_source_breakpoints`/`set_function_breakpoints` were left returning `BackendError::Send(
  "...not yet implemented (phase 3.3)")` — mislabeled, since 3.3 was execution. They slipped through
  the task boundaries and would have silently broken the core interactive loop (launch → set bp →
  continue to it). Caught when 3.5's Normal group needed them; the 3.5 implementer correctly STOPPED
  and reported rather than papering over it. Filled as a follow-up commit (`c6becc8`), which in turn
  surfaced the declarative-reconciliation requirement (the `BreakpointTable`) — substantive logic the
  original task list never anticipated.
- **Debuggee stdout does NOT reach `BackendEvent::Output` (parity gap vs lldb).** DbgEng's
  `IDebugOutputCallbacks` carry only *engine* output (ModLoad, break notices, symbol diagnostics), and
  `dbgeng-sys::Engine::launch` uses `CREATE_NO_WINDOW`, so the child's `printf` goes nowhere
  observable. The `read_output` tool therefore returns engine output, not program output, under
  WinDbg. Documented as an in-code `LIMITATION` at `cont` and the factory output-sink wiring; surfacing
  real stdout needs a `dbgeng-sys` launch-path change (stdout pipe redirection / console handling),
  deferred. The output live test asserts on genuine program-driven engine output (`ModLoad …
  test_target.exe`) + `Terminated` instead.
- **`evaluate` ignores `frame_id` (parity gap vs lldb).** WinDbg evaluates in the current (innermost
  stopped) frame; lldb honors the DAP `frameId`. Documented in-code as deliberate; closing it needs
  DbgEng frame-scope switching (`SetScopeFrameByIndex`/`ResetScope`), deferred. A live test passes a
  non-current `frame_id` and asserts evaluate still succeeds (value comes from the current scope).
- **`stack_trace`'s `total_frames` reflects the fetched window, not true stack depth, when `start > 0`.**
  Every current caller passes `start = 0` (so window == full stack), so this is correct today; the
  misleading comment was corrected and the test annotated. A future paginating caller must not treat
  it as full depth without an unconditional depth walk.
- **Condition changes on a re-sent breakpoint location are silently ignored.** Reusing the cached
  result keeps the original condition (conditional-eval is a deferred Phase-5 feature). Documented
  in-code and pinned by a test so a future change to apply conditions on re-send is conscious.
- **No sub-agent capacity outages this phase** (unlike Phase 2's 529s) — every task and every
  fix-round ran through delegated agents as intended.

## Risks & Issues Encountered

- **The cancellation unit test originally proved nothing (3.3 review, Major).** `let fut = cont(1);
  drop(fut);` never polls the future, so no `Go` command is ever sent — it only proved a never-started
  future is harmless. Rewrote it to `tokio::spawn` the `cont`, confirm via a `FakeEngine` `go_entered`
  flag that the `Go` command actually reached the engine, THEN abort, then prove a follow-up op
  (`pause`/`threads`) still works — i.e. a mid-`go` cancel leaves the target running but does not wedge
  the backend (recover with `pause`, mirroring lldb).
- **The output live test had an abort race (3.3 review, Major).** `drain.abort()` raced event delivery
  and the `Ok(None)` channel-closed arm could cut the loop before `Terminated` was observed. Rebuilt to
  drain all events into a `Vec` to a deadline, then scan once; replaced `abort()` with a bounded join.
- **Stale-breakpoint removal was claimed but unasserted (reconciliation review, Major).**
  `FakeEngine::remove_breakpoint` was a silent no-op recording nothing, so the reconcile test couldn't
  prove a stale id was actually removed. Added a `removes: Vec<i64>` recorder field + assertions; added
  the missing `set_source_breakpoints` multi-call reconcile test and a category-isolation test (source
  clear must not remove function bps, and vice versa). These matter because the live tests don't run in
  the Ubuntu CI gate — unit coverage is the real safety net for the reconciliation logic.
- **`BreakpointTable` consistency on a mid-remove `Closed`.** If the channel dies during the stale-remove
  pass, the original code early-returned without committing the new map (losing just-added entries).
  Fixed to commit the reconciled table before propagating the transport error, so the table reflects
  engine reality even on the (terminal) `Closed` path.
- **tsan does not cover the Windows engine-thread/`InterruptHandle` path.** `make tsan` is hard-wired to
  `cargo +nightly -Zsanitizer=thread -Zbuild-std --target x86_64-unknown-linux-gnu` — ThreadSanitizer is
  an LLVM/clang facility for Linux/macOS, unavailable on the MSVC Windows toolchain. The engine-thread
  interaction is instead covered by `#![forbid(unsafe_code)]` + the unsafe-gate, the flag-only
  `AtomicBool` interrupt (no cross-thread COM), and the live pause-breaks-cont + cancelled-cont tests.
  Recorded for the Phase-5 Windows CI lane.
- **One transient flake** in `cancelled_cont_midflight_does_not_wedge_the_backend` under heavy parallel
  test load (an `abort()`/recovery timing race); passed in isolation and on re-run. Not a regression;
  worth a defensive look if it recurs in CI.

## Impact on Subsequent Phases

- **Phase 4 (extras)** builds directly on this surface:
  - The `EngineCmd` enum and `EngineOps`/`FakeEngine` split are complete and closed — `open_dump`/
    `attach_kernel` are stubbed (inherit the trait's default `Unsupported`), so Phase 4 fills bodies
    without reopening the enum. The four tool handlers were stubbed in Phase 1 and just need live calls.
  - `WinDbgFactory::capabilities()` already returns all-four-true, and registration is done — so Phase 4
    is purely engine-method + trait-method + handler-wiring work; `list_tools = 25` already holds.
  - The connect-point flow (`connect()` spawns a fresh engine thread, wires the output sink, builds the
    `BackendEvent` stream) is the template `open_dump`/`attach_kernel` follow (fresh session per dump).
  - The `BreakpointTable` + the launch-flush tracking are the model for any future declarative state.
  - The `integration-windbg` feature + the `Harness::new_windbg()`/`windbg_fixture_path`/`should_skip_windbg`
    harness + the `WaitChild` Drop guard are reusable for the Crash + Dump groups.
- **Phase 5 (parity hardening)** owns the now-concretely-scoped deferrals: conditional-BP evaluation
  (the reconcile path is ready to diff/re-set conditions), nested variable expansion (`variables`
  returns `variables_reference = 0`), `evaluate` frame-scope switching, debuggee-stdout capture (a
  `dbgeng-sys` launch-path change), the `stack_trace` `total_frames` pagination semantics, R2 (kernel
  orphan thread), R6 (ASLR address BP), and the Windows CI lane (incl. a tsan substitute and fixing the
  Make gates for cmd.exe).
- **Carry-forward open risks:** R2, R6, R7, R8 (unchanged); R1/R3/R4/R5 resolved.

## Skill Opportunities

- **`task-boundary-completeness-check`** — the runtime breakpoint setters slipped through as a
  mislabeled "(phase 3.3)" placeholder and were only caught two tasks later when an integration test
  needed them. A pre-phase-completion scan that greps the target crate for `not yet implemented` /
  `todo!` / `unimplemented!` / `BackendError::Send("...phase...")` placeholders and cross-checks every
  trait method has a real body would catch "a method fell through the task seams" before debrief.
  *Recurred once this phase with high impact (it was the core interactive loop); cheap to automate.*
- **`fake-recorder-coverage-audit`** — twice the quality-scanner found a test that *claimed* a behavior
  (stale removal, command dispatched) the `FakeEngine` couldn't actually observe because it recorded
  nothing. A checklist pass — "for each side-effecting op the fake stands in for, does the Recorder
  capture enough to assert the op happened (and with what args)?" — would catch unfalsifiable tests at
  authoring time. *Recurred across 3.3 and the reconciliation work; medium-high benefit.*
- **`async-cancel-test-pattern`** — the cancellation test that proved nothing (drop-before-poll) is a
  recurring async-Rust footgun. A documented pattern (spawn → confirm the side effect started via a
  shared flag → abort → assert recovery) codifies the only way to actually test mid-await cancellation.
  *Once this phase; recurs for any cancellable async op.*
- Carries forward the Phase-2 opportunities (`windows-rs-signature-probe`, `com-drop-audit`,
  `inline-fallback-on-capacity`), all still relevant to Phase 4's COM work.
