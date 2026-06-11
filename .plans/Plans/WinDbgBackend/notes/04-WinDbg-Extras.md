---
title: "Debrief — Phase 4: WinDbg Extras (dump / kernel / analyze / modules)"
type: debrief
plan: WinDbgBackend
phase: 4
status: complete
created: 2026-06-11
updated: 2026-06-11
related: [Designs/WinDbgBackend, Plans/WinDbgBackend, Plans/WinDbgBackend/notes/03-WinDbg-Backend-Core]
---

# Debrief — Phase 4: WinDbg Extras

Phase 4 lit up WinDbg's distinctive value — crash-dump analysis, kernel debugging, `!analyze -v`,
and module listing — wiring the four capability-gated tools (`open_crash_dump`, `attach_kernel`,
`analyze_crash`, `get_modules`) end-to-end so the full **25-tool** surface is live on Windows. The
21 lldb tools stay byte-identical. All work was verified on a live Windows host with the
PDB-bearing fixture, so every op ran end-to-end, not just compiled.

Delivered in four tasks: `3174478` (4.1 dbgeng-sys engine ops), `2576d89` (4.2 windbg-backend
trait methods), `4c854d1`→`fff2bfd` (4.3 tool wiring + tests), `e79e57f` (4.4 Crash + Dump
integration groups). Each task ran the full per-task discipline — `code-implementer` →
`quality-scanner` → fix round → gate (clippy `-D warnings` / fmt / unsafe-gate / `cargo test
--workspace`) → commit (no co-author trailer) → plan-doc status update.

## Decisions Made

- **R2 (kernel-attach wait) resolved = `WaitForEvent(INFINITE)`**, mirroring the C++ ("INFINITE is
  sadly the only supported wait there" for KDNET). The uncancellable-wait orphan-thread teardown
  was deferred to Phase 5 (and landed there as the R2 pump fix). On the failure path,
  `EndSession(DEBUG_END_ACTIVE_DETACH)` releases the KDNET port; the INFINITE-blocks-on-unreachable
  caveat is documented in-code. The design floated a polled-with-cancellation alternative (R2
  option a); we took option b (C++ parity) since KDNET's tolerance of short timeouts was unverified
  and a real VM wasn't available to test it.
- **R8 (extension path) resolved = hardcoded WinKits `.extpath` + `.load ext.dll`** for Phase 4
  (strict C++ parity), with runtime discovery explicitly deferred. This deferral became the
  headline finding of the phase (see below) and was pulled forward to Phase-5 task 5.0.
- **`open_dump` guards the live-session case via `GetExecutionStatus`** (not just `is_dump`), fully
  mirroring the C++ `state != NoTarget` guard — a review fix that closed a parity gap the
  fresh-engine-per-session model made unlikely but which the comment had misrepresented.
- **`attach_kernel` surfaces a `RemoveEngineOptions` failure even on the success path** (a
  deliberate, documented deviation from the C++, which drops that HRESULT) — a lingering
  INITIAL_BREAK would corrupt every later `go`/`step`, consistent with how `launch`/`attach_pid`
  already handle it.
- **`analyze` does NOT guard session state at the engine layer**; the `analyze_crash` tool handler
  owns the `check_state(&[Stopped])` guard (confirmed already present during the 4.1 review). The
  engine method's doc records this contract so a future reader knows the guard lives above it.
- **`truncate_output` caps `!analyze` at 32 KiB** (the C++ `MAX_OUTPUT_SIZE`), rounding the cut down
  to a UTF-8 char boundary so the `String` stays valid on multi-byte content.
- **4.3 was a verification task, not new code.** The four tool handlers were already fully wired as
  connect-points back in Phase 1 (task 1.4); they returned `Unsupported` only because the *backend
  trait methods* did, which 4.2 fixed. So 4.3 added the cross-platform fake-backend tests that pin
  the contracts (25-tool list, Unsupported-on-lldb, response shapes, connect-failure) rather than
  re-writing the handlers.

## Requirements Assessment

All four acceptance criteria met:
- ✅ `dbgeng-sys` implements dump/kernel/analyze/modules; R2 and R8 resolved + documented; dump
  sessions reject `go`/`step` with the frozen `"cannot continue a crash-dump session"` literal.
- ✅ `windbg-backend` implements the four trait methods; `capabilities()` all-true; `open_dump`/
  `attach_kernel` follow the connect-point flow (fresh engine thread per session).
- ✅ The four tools are live end-to-end; `list_tools` shows 25 on Windows; they return `Unsupported`
  / not-available against an active lldb session (cross-platform fake-backend tests).
- ✅ The `integration-windbg` Crash + Dump groups pass live; the lldb suite stays green; clippy/fmt
  clean on the Windows lane.

## Deviations

- **`modules` was already implemented** (Phase 2/3) — 4.1's modules subtask reduced to adding a
  format-contract unit test (`base = 0x{:016X}`, size decimal, `symbol_status ∈ {pdb,export,
  deferred,none}`).
- **The dump-session execution guard was already wired** — `go`/`step`/`break_in` already called
  `ensure_runnable()`; 4.1 confirmed it and added the live assertion rather than adding the call.
- **`analyze_crash` token-content assertions kept lenient** in 4.4 — see the finding below. The
  *structural* contracts (AV-is-a-stop, `crash_null` in the backtrace, `dump_loaded` status, the
  execution-guard literal, `read_memory("0x0")` errors) are strictly asserted.
- **Live KDNET path is `#[ignore]`d** — no VM available, and a real `attach_kernel` against an
  unreachable target would block forever on the INFINITE wait; the connection-string validation is
  unit-tested instead, and no non-ignored test can reach the INFINITE wait.

## Risks & Issues Encountered

- **`analyze_crash` returned `"No export analyze found"` on the dev host (the headline finding).**
  The 4.4 quality scan diagnosed it precisely: the hardcoded R8 `.extpath` pointed at WinKits
  directories that lacked `ext.dll` because the host had only the *Windows SDK debugger-support
  subset* (the four redist DLLs), not the full *Debugging Tools for Windows*. **Not a regression** —
  the C++ hardcoded the identical paths and only worked because its author had the full tools at the
  default location — but it meant `analyze_crash` (and any `run_command("!...")`) is non-functional
  for any user on a partial or non-default install. The 4.4 tests handle this honestly (lenient
  where the extension genuinely can't load, strict the moment it resolves). The fix (runtime ext-path
  discovery) was scoped as Phase-5 task 5.0 and validated live there once the full tools were
  installed.
- **PCSTR/PCWSTR lifetime scrutiny on the new COM FFI** (`OpenDumpFile`, `AttachKernel`, the
  `.extpath`/`.load` Execute calls): the 4.1 review confirmed every encoded string is bound to a
  named local that outlives its `unsafe` call — the recurring footgun from Phase 2, handled right.
- **`truncate_output` UTF-8 boundary**: the review flagged the missing multi-byte test; added one
  (`'€'`-fill past 32768) proving the char-boundary walk-back never panics.
- **Test-quality drift caught by the scanner, not shipped**: the 4.4 analyze comment originally
  claimed "the dump test produces the full report" while the dump test itself documented the
  opposite; the over-broad `reason_is_access_violation` `"access"` matcher; a stale-dump false-pass
  window. All three fixed before the task closed.

## Impact on Subsequent Phases

- **Phase 5** inherited the R8 deferral as its first task (5.0, pulled forward), and the R2
  orphan-thread teardown as part of 5.2. Both landed.
- The `TempDump` RAII pattern and the lenient-vs-strict assertion policy from 4.4 became the model
  for 5.x test hygiene.
- The `analyze`/`run_command` ext-path discovery (5.0) closed the headline finding so the rich
  `!analyze` report is available on any properly-installed host — validated live.
- **Carry-forward open risks:** R6 (Phase 5), R7 (CI lane, Phase 5); R1/R3/R4/R5/R8 resolved, R2
  resolved (INFINITE + the Phase-5 pump fix).

## Lessons Learned

- **"Already wired" is a real and good outcome.** Phase 1's foresight (writing the four handlers as
  full connect-points up front, gated only by the backend's default `Unsupported`) meant 4.3
  collapsed to verification. Checking the actual state before implementing — rather than assuming the
  task description implies net-new code — saved a redundant rewrite.
- **An honest lenient test beats a falsely-strict one, IF the leniency is documented and self-tightens.**
  The 4.4 analyze assertion that tolerates a missing extension but asserts a real token the moment the
  extension resolves was the right shape — it neither hid the defect nor failed spuriously on the dev
  host, and it became a strict assertion for free once 5.0 + the toolset install made `!analyze` work.
- **The quality scanner's value is separating "test bug" from "product finding."** On 4.4 it cleanly
  split the analyze leniency into (a) a test that accepts an error string and (b) a real product
  defect in `ensure_extensions_loaded`, and said so — which is exactly what let the product defect get
  its own scoped task rather than being silently masked.

## Skill Opportunities

- **`com-ffi-string-lifetime-lint`** — a focused check for the recurring "is this `CString`/encoded
  buffer bound to a named local that outlives the `unsafe` FFI call, or is it a dropped temporary?"
  pattern. It was the first thing the scanner checked on every COM-touching task (4.1, 5.0) and is
  mechanical enough to automate. *Recurred across Phases 2/4/5; high value for the remaining FFI.*
- **`already-implemented-audit`** — before implementing a task, grep the target for an existing impl
  (4.3's handlers, 4.1's `modules`/`ensure_runnable`). A pre-task scan that flags "this method/handler
  already exists — the task may be verification, not implementation" would have framed 4.3 correctly
  from the start. *Recurred 4.1 + 4.3.*
- Carries forward the Phase-2/3 opportunities (`windows-rs-signature-probe`, `com-drop-audit`,
  `fake-recorder-coverage-audit`), all exercised again here.
