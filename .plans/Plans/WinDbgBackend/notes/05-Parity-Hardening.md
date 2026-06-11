---
title: "Debrief — Phase 5: Parity Hardening (cond-BP, R6/R2, cancel/Break, Error group, CI lane)"
type: debrief
plan: WinDbgBackend
phase: 5
status: complete
created: 2026-06-11
updated: 2026-06-11
related: [Designs/WinDbgBackend, Plans/WinDbgBackend, Plans/WinDbgBackend/notes/04-WinDbg-Extras]
---

# Debrief — Phase 5: Parity Hardening

Phase 5 closed the remaining gaps between the Rust WinDbg backend and the C++ oracle, resolved the
trickier risks (R2 orphaned thread, R6 ASLR address BPs, R8 ext-path), ported the C++ Error group +
a differential Windows lane, and wired the CI Windows lane. After this phase the WinDbg backend is
**feature-complete and parity-validated**, and the WinDbgBackend plan is done. Everything was
verified on a live Windows host with the full Debugging Tools installed mid-phase.

Delivered in six tasks (one pulled forward): `e196d64` (5.0 runtime ext-path discovery), `317d89a`
(5.1 conditional breakpoints), `f50d733` (5.2 R6 + R2), `0f3e766` (5.3 cancel/Break/file-lock),
`aab212e` (5.4 Error group + differential), `fc717ea` (5.5 CI lane + docs). Each task ran the full
discipline — `code-implementer` → `quality-scanner` → fix round → gate → commit (no co-author
trailer) → plan-doc status update. Final gate green: clippy `-D warnings`, fmt, unsafe-gate (unsafe
confined to `dbgeng-sys`), seam intact, `cargo test --workspace` 0 failures.

## Decisions Made

- **5.0 (R8 refinement) pulled forward into Phase 5** as task 5.0, surfaced by the Phase-4 finding
  that `analyze_crash` was non-functional on non-default installs. Done first (while a full toolset
  was installed for testing) so the registry discovery could be **validated live**: `analyze_crash`
  went from the 24-byte `"No export analyze found"` to a real 15.8 KB `!analyze -v` report.
  Resolution order: registry `KitsRoot10` (native + WOW6432 views, accepting `REG_SZ` and
  `REG_EXPAND_SZ`) → `WindowsSdkDir` env → the former hardcoded default; only existing dirs are
  appended to `.extpath`. The pure path-assembly logic is behind an injectable seam (`reg_reader`/
  `exists` predicates) so it's unit-testable without a live registry; the registry read is the thin
  `unsafe` FFI boundary.
- **5.1 conditional BPs evaluate `@@c++( (cond) ? 1 : 0 )` in the `wait_for_event` S_OK branch** (the
  C++ mechanism): on a `DEBUG_STATUS_BREAK` with a stored condition, `GetLastEventInformation` →
  `Evaluate(DEBUG_VALUE_INT64)`; false (or eval-fail) → `SetExecutionStatus(GO)` + `continue` the
  wait. The condition is **never** set on the DbgEng BP object (the `gc`/`g` can't-re-enter-the-event-
  loop limitation, design Decision 5). Eval failure (out-of-scope/typo) = treated as false = the BP is
  silently SKIPPED (the documented C++ footgun, now surfaced in `set_breakpoint`'s docstring).
- **R6 (ASLR address BPs) resolved with a NEW neutral `BreakpointResult.rejected` field** (not an id
  sentinel). A bare `0x<addr>` name passed to `set_function_breakpoint` is rejected (unverified +
  guidance, never tracked) because the session re-flushes function BPs by name and a raw address would
  be ASLR-misplaced on relaunch; `module!sym` (rebase-stable) is allowed; address BPs go through
  `run_command("bp <addr>")`. The detection is the `0x`/`0X` prefix only (C++ parity — `deadbeef` is a
  valid function name).
- **R2 (orphaned kernel thread) resolved with a drop-triggered synthetic Terminated.** The backend
  holds a `_drop_signal: oneshot::Sender<()>` whose Drop closes a channel that `build_event_stream`
  races against the engine's real `term_rx`; whichever fires first emits exactly one `Terminated` and
  ends the stream — so a backend drop forces the event-pump to complete even when the orphaned engine
  thread (stuck in `WaitForEvent(INFINITE)`) never fires the real signal and never closes the output
  sink. The orphan leaks until process exit (documented C++ behavior; recovery = disconnect/restart).
- **5.3 was mostly verification** — R3 Break-recovery (the continue-timeout → "still running" → pause
  flow), cancelled-launch/attach reset, and the backend-aware error strings were all already
  implemented in earlier phases. The genuine new work was the **rebuild-after-detach file-lock
  regression** (deferred from Phase 2): it confirmed `DEBUG_END_ACTIVE_TERMINATE` releases the image
  module mapping so a rebuild can overwrite the exe (removable on the first attempt).
- **The differential Windows lane is a golden-shape conformance check, not a live cross-backend diff.**
  lldb (Unix) and windbg (Windows) are platform-exclusive and cannot co-run, so a true binary-vs-binary
  diff is impossible; instead the windbg responses for the shared neutral behaviors (backtrace/
  variables/threads/read_memory) are asserted to carry the same neutral-type key sets + JSON types the
  lldb golden lane documents — catching neutral-surface drift without both backends live.
- **R7 (CI Windows runner) resolved with a hosted `windows-latest` lane**, not the design's manual-gate
  fallback. DbgEng is OS-bundled (System32), MSVC is preinstalled to build the fixture (via
  `ilammy/msvc-dev-cmd`), and tests needing the full Debugging Tools degrade gracefully. The lane uses
  raw `cargo` throughout (no `make` dependency on Windows); hard gates (build/clippy/fmt/unsafe-gate/
  unit) plus a self-skipping live-integration step.

## Requirements Assessment

All acceptance criteria met (across the six tasks):
- ✅ Engine-side conditional breakpoints fire correctly; eval-fail skips (validated live: `i == 5`
  stops at exactly 5, proving 0–4 skipped; an unresolvable condition runs to clean exit).
- ✅ `module!sym` BPs re-flush safely across relaunch (rebase-stable, validated live); bare-address
  BPs are rejected and not session-tracked; the behavior difference is in CLAUDE.md's parity notes.
- ✅ The R2 pump fix drives the session to terminated on backend drop so a fresh launch reconnects
  (proven at the channel level — no live KDNET needed).
- ✅ Continue-timeout → pause-recovers (R3) works (live `pause_breaks_a_running_continue`);
  cancellation resets to idle + clears the backend; windbg/lldb error strings are exact + tested.
- ✅ The ported Error group passes with parity-exact guard strings; the golden-shape differential lane
  passes.
- ✅ The CI Windows lane is wired (YAML validated, 4 jobs, the 3 Ubuntu jobs untouched); the unsafe-gate
  + clippy + fmt run on Windows; tsan stays Ubuntu-only (Miri excluded for dbgeng-sys); CLAUDE.md
  finalized.

## Deviations

- **R6 took a NEW `debugger-core` field, not the originally-attempted id gate.** The design said "route
  bare-address BPs through `run_command`"; the implementation also had to stop the *tool layer* from
  tracking the rejected name in session state. The first attempt (gate tool-tracking on `id != 0`)
  broke a live test and was reverted (see below); the additive `rejected: bool` is the seam-respecting
  replacement (a strict no-op for lldb, kept out of the wire format so the differential lane is
  unaffected).
- **The 5.4 differential lane is golden-shape, not Rust-vs-Go binary diff** (the lldb lane's model) —
  forced by lldb/windbg being platform-exclusive. Documented as the deliberate framing.
- **tsan does not cover the Windows engine-thread path** (it's an LLVM/Linux facility on the MSVC
  toolchain) — recorded in CLAUDE.md; the interaction is covered by `forbid(unsafe_code)` + the
  unsafe-gate + the flag-only `AtomicBool` interrupt + the live pause tests.
- **One 5.4 cross-file edit (the lldb golden lane's typed assertions) is Unix-gated** and could not be
  compiled/run on the Windows host — eyeballed as well-formed Rust; its first real execution is the
  Ubuntu `live` CI job.

## Risks & Issues Encountered

- **The `id != 0` R6 gate broke a live test (the sharpest finding).** DbgEng numbers breakpoints from
  0, so a legitimate FIRST WinDbg breakpoint (`compute`, id 0) is indistinguishable from the
  rejection/unresolved sentinel (`id: 0`). Gating tool-layer tracking on `id != 0` silently dropped the
  real `compute` BP — caught by the live `normal_session_breakpoint_workflow` (count 1 instead of 2).
  The implementer correctly hit the STOP-and-report condition rather than guessing a different signal;
  the resolution was the neutral `rejected` field. **Lesson: id values are backend-specific and must
  never be repurposed as cross-seam discriminators.**
- **My relayed `remainder != 0` guard (5.1) was wrong for the DbgEng API.** `IDebugControl::Evaluate`
  writes `remainder` as the index of the first UNCONSUMED character, so a *complete* parse yields
  `remainder == expr_len`, not 0. The literal `remainder != 0` guard ran the conditional-BP test to a
  clean exit instead of stopping at `i == 5`. The implementer diagnosed it empirically
  (`remainder=25, expr_len=24`), corrected to `remainder < expr_len`, and re-verified the stop. **Had
  the instruction been followed literally, conditional BPs would have silently broken** — the live test
  loop is exactly what surfaced it.
- **The R2 `build_event_stream` rewrite changed the termination semantics of every WinDbg session.**
  The scanner confirmed no regression: the `terminate` future is a single long-lived boxed future
  polled across unfold iterations (not recreated), the `terminated` flag enforces exactly-one
  Terminated, and `biased;` (a review fix) drains buffered output before the terminal event.
- **The new registry `unsafe` (5.0) passed scrutiny** — `RegGetValueW` buffer byte/u16 accounting,
  NUL stripping, `PCWSTR` lifetimes, no spurious `HKEY_LOCAL_MACHINE` close, and the runtime `.extpath`
  `CString` bound to a named local. A review fix added `RRF_RT_REG_EXPAND_SZ` (a real correctness gap
  for non-standard installers that write `KitsRoot10` as `REG_EXPAND_SZ`).
- **The rebuild-after-detach test was hardened against a trivial pass** — added an explicit
  `copy.exe.exists()` assertion after the engine drop so the removability proof can't pass on an
  already-absent file.

## Impact on Subsequent Phases

This was the final phase — the WinDbgBackend plan is complete. Carry-forwards beyond the plan:
- **One CI-validated-on-first-push item**: the 5.4 lldb-lane typed-assertion edit (Unix-gated, not
  compiled on the Windows dev host) runs for real in the Ubuntu `live` job.
- **The Windows CI lane's first real run is on the next push** (Actions can't be executed locally); the
  YAML is validated and `make integration-windbg`'s underlying command is proven live.
- **Documented WinDbg behavior differences** (in CLAUDE.md) that a future maintainer/agent should know:
  debuggee stdout is not surfaced via `read_output` (`CREATE_NO_WINDOW`); `evaluate` ignores
  `frame_id`; failing conditional BPs silently skip; address BPs go via `run_command`; `!analyze`
  needs the Debugging Tools installed.

## Lessons Learned

- **A live test that exercises the real engine is worth more than a reviewer's confident instruction.**
  Twice this phase, a plausible-sounding change (the `id != 0` gate, the `remainder != 0` guard) was
  wrong against real DbgEng behavior and was caught only because a live test ran against the actual
  engine. The instinct to delegate with precise prompts is good, but the prompts must invite empirical
  correction ("if X turns out not to hold, STOP and report") rather than mandate a specific mechanism
  the orchestrator can't verify.
- **"STOP and report" is a feature, not a failure.** The implementer halting on the `id != 0` collision
  — rather than inventing an alternative discriminator — is exactly the behavior that let the right fix
  (a deliberate `debugger-core` change) be made consciously instead of a guess being shipped.
- **Seam-respecting fixes sometimes require a small neutral-layer addition.** R6 couldn't be solved
  purely below the seam (the tool layer was the thing over-tracking) nor with backend-specific logic
  above it (would break lldb). The additive `rejected` field is the textbook resolution: neutral,
  additive, default-false (no-op for the existing backend), and kept out of the wire format.
- **Timing a deferred task to coincide with the right environment pays off.** Doing 5.0 while the full
  Debugging Tools were installed turned a "we believe discovery works" into a live-proven 15.8 KB
  `!analyze` report and flipped the 4.4 lenient assertion to strict for free.

## Skill Opportunities

- **`backend-id-as-discriminator-lint`** — flag any place where a backend-assigned id (DAP id, DbgEng
  id) is used as a semantic sentinel across the seam. The `id == 0` collision would have been caught
  statically. *High value; the bug was subtle and only a live test caught it.*
- **`ffi-out-param-semantics-probe`** — the `remainder` misunderstanding came from assuming an
  out-param's meaning (index-of-first-unconsumed vs nonzero-on-partial). A skill that surfaces the
  exact documented semantics of a windows-crate out-param (from the Win32 docs / the generated
  signature) before it's relied upon would prevent this class. *Recurred as a near-miss this phase.*
- **`actions-yaml-local-lint`** — CI YAML can't run locally but CAN be parsed; a quick `pyyaml` parse +
  job/step structural check (which caught the misleading step name + confirmed the 3 existing jobs
  intact) should be a standard step whenever a workflow is touched. *Used ad hoc here; worth
  codifying.*
- Carries forward `com-ffi-string-lifetime-lint`, `fake-recorder-coverage-audit`, and
  `async-cancel-test-pattern` from the prior phases — all exercised again.
