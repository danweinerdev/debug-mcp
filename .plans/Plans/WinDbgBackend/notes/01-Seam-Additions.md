---
title: "Debrief — Phase 1: Seam Additions"
type: debrief
plan: WinDbgBackend
phase: 1
status: complete
created: 2026-06-04
updated: 2026-06-04
related: [Designs/WinDbgBackend, Plans/WinDbgBackend]
---

# Debrief — Phase 1: Seam Additions

Phase 1 added the cross-platform, additive scaffolding for a WinDbg backend onto the existing
pluggable seam: the capability-gated trait surface, the `BackendRegistry` runtime switcher, the
four WinDbg-only tool handlers (returning "not available"/`Unsupported` until Phase 3), the
dump-session contract, and the docs. No WinDbg code yet; the lldb surface stayed byte-identical.

Delivered in six commits on `rust-rewrite`: `974155d` (1.1), `bb34003` (1.2), `b6d4489` (1.3),
`9ff8e8a` (1.4), `f4d72f8` (1.4b), `367f822` (1.5), plus `b135ad5` (plan docs). Each task was
implemented by a `code-implementer` agent and reviewed by a `quality-scanner`; all
critical/major findings were resolved before moving on.

## Decisions Made

- **Windows per-OS default stays `windbg` even though the factory isn't registered until
  Phase 3** (user decision). `default_backend_for_os()` returns `windbg` on Windows; a no-arg
  `launch` there currently errors `unknown backend 'windbg'` until Phase 3 registers the
  factory. Rationale: the plan completes the work; the default "just works" when the factory
  lands, and the interim is covered by `DEBUG_BACKEND=lldb` / `backend:"lldb"`. The
  quality-scanner flagged this as MAJOR; we consciously accepted it.
- **`status.backend` reports the *active* backend, not the static default.** The scanner found
  the field initially reported `registry.default_name()` even mid-session under an explicit
  `backend` arg. Fixed: `ToolServer` records the selected factory name (a sync
  `std::sync::Mutex<Option<&'static str>>`, because `handle_status` is sync) at connect and
  clears it on teardown; `status` falls back to the default only pre-connect.
- **Selection precedence is a pure, injectable function.** `select_with_env(requested,
  env_value)` holds the arg→env→default logic so tests inject the `DEBUG_BACKEND` value
  instead of mutating process env (avoids racy parallel tests). Empty arg/env are normalized
  to "unset".
- **All four WinDbg handlers live in one `handlers/windbg.rs`** rather than split across
  `lifecycle.rs`/`inspection.rs` — keeps the WinDbg verbs cohesive.
- **The dump-session guard is one check in the shared `resume()` helper**, so a single `const
  DUMP_NO_CONTINUE` literal covers all four of `continue`/`step_over`/`step_into`/`step_out`.
- **`is_dump` is a flag on the existing `Stopped` state, not a new state enum variant** —
  avoids touching the state machine the lldb path depends on.
- **Frozen contracts** (so Phases 2–4 implement against literals, not guesses): the unsupported
  string `"<tool> is not supported by the <backend> backend"`, the not-available string
  `"<tool> is not available: the windbg backend is not registered on this platform"`, the
  KDNET validation `"'connection' must be a KDNET connection string starting with 'net:'"`, and
  the dump-resume rejection `"cannot continue a crash-dump session"`.

## Requirements Assessment

All six acceptance criteria met:

- ✅ `debugger-core`: `BackendCapabilities`, four default-`Unsupported` methods,
  `ModuleInfo`/`DumpOutcome`, `BackendError::Unsupported`; no new heavy deps; object-safety +
  serde round-trip tests green.
- ✅ `ToolServer` holds a `BackendRegistry`; precedence unit-tested on all OSes; `connect_error`
  is backend-name-keyed (lldb verbatim preserved).
- ✅ `list_tools`: 21 with all-false caps, 25 with all-true; the 19 untouched tools byte-identical
  (only `launch`/`attach` gained an optional `backend` enum). 13/13 schema parity tests green.
- ✅ Four handlers validate args/state and return the correct strings; `open_crash_dump`/
  `attach_kernel` hardcode windbg selection.
- ✅ Dump-session contract frozen: `is_dump` + `Stopped`, the guards, the literal.
- ✅ CLAUDE.md updated; full gate green (clippy `-D warnings`, fmt, seam, tests) modulo the
  pre-existing unrelated `lldb-backend` subprocess failures (see Risks).

## Deviations

- **Three downstream crates gained a match arm in task 1.1.** Adding
  `BackendError::Unsupported` broke three *exhaustive* `match err` blocks
  (`dap-client::clone_backend_error`, `mcp-tools::OpError::render`, `mcp-tools::map_implicit_error`).
  The task said "don't touch other crates," but `cargo build --workspace` cannot pass without
  handling the new variant. Added one behavior-preserving arm to each (the variant is
  unreachable on those paths). Necessary and correct.
- **`ToolServer::tools()` changed from an associated fn to `&self`** (task 1.3) so it can read
  the registry's capability union; the two `ServerHandler` call sites updated.
- **The `backend` arg is a new, documented extension** to the launch/attach schemas (additive,
  optional) — a deliberate post-Go deviation recorded in CLAUDE.md.
- **Post-connect failure routing fix** (task 1.4): the dump/kernel handlers initially routed
  backend-call failures through `connect_error` (which only renders `Detect`/`Spawn`); changed
  to the `"<tool> request failed: <e>"` form.

## Risks & Issues Encountered

- **3 pre-existing `lldb-backend` subprocess test failures on the Windows dev host.**
  `spawn_echo_round_trip_and_stderr`, `spawn_exit_detected_after_stdin_command`,
  `spawn_capable_flag_passes_repl_mode` fail with `program not found` for `sh`/`true` — the
  tests spawn POSIX binaries absent on Windows. `lldb-backend` was untouched by Phase 1
  (confirmed via `git diff --name-only`). **Not a regression**; flagged so Phase 5's CI lane
  accounts for it (these are green on Linux/macOS).
- **Quality-scanner CRITICAL in 1.3 (gated tools advertised but not dispatchable)** was a
  transient cross-task artifact: the four tools are only advertised when a capability flag is
  set, and no factory enables them until Phase 3, so they were never actually listed; task 1.4
  added the dispatch arms. Closed within the phase.
- **Co-author trailer**: mid-phase the user forbade `Co-Authored-By` trailers on commits
  (saved to memory). All six Phase-1 commits verified trailer-free.

## Lessons Learned

- **Adding an error enum variant ripples through exhaustive matches.** Budget for downstream
  match-arm edits whenever `BackendError` grows; the no-wildcard convention (good for catching
  real cases) means each addition is a small multi-crate change.
- **`handle_status` is synchronous**, but the connected-backend slot is a tokio `RwLock`. Any
  state `status` must report has to be reachable without `.await` — hence the separate
  `std::sync::Mutex` for the active backend name. Worth remembering for future status fields.
- **Quality-scanner findings are best triaged against plan context the scanner lacks.** Two
  "critical/major" findings were either closed by the next task or an intentional user choice;
  reasoning about phase sequencing avoided redundant fix cycles while still applying the
  genuine fixes.
- **Pure-function extraction for env-dependent logic** (`select_with_env`) keeps tests
  deterministic and parallel-safe — a pattern to reuse for any future env-driven behavior.
- **Linear task chains within a phase serialize cleanly** but are slow with one
  implementer+scanner per task; the tight `mcp-tools` coupling (1.2–1.5) made parallelism
  impossible anyway, so the serial waves were the right call here.

## Impact on Subsequent Phases

- **Phase 2 (`dbgeng-sys`)**: the neutral types it must return now exist
  (`StopOutcome`/`Frame`/`ThreadInfo`/`Variable`/`ModuleInfo`/`DumpOutcome`/`BreakpointResult`/
  `Instruction`). The frozen `"cannot continue a crash-dump session"` literal is the contract
  for its dump-session `go`/`step` guard. **R1** (windows-crate DbgEng interface coverage) and
  **R4** (cross-thread `SetInterrupt`) are still open and gate Phase 2.
- **Phase 3 (`windbg-backend`)**: registering `WinDbgFactory` (with all-true capabilities) is
  the single line that (a) lights up the four tools in `list_tools` on Windows and (b) makes
  the `windbg` per-OS default resolve. The `connect_error` "windbg" branch
  (`Debugging Tools for Windows not found` / `failed to initialize DbgEng`) is already wired
  and unit-pinned. `set_backend(backend, name)` already records the factory name for `status`.
- **Phase 4**: `open_crash_dump`'s success path already sets `is_dump=true`; the dump-outcome
  response shape (`{"status":"dump_loaded","crash_location":…}`) is built. Phase 4 fills the
  real `Engine::open_dump`/`attach_kernel`/`analyze`/`modules` bodies (stubbed in Phase 2).
- **Carry-forward test gaps** (non-blocking): cancellation-path tests for the two connect-point
  tools (`open_crash_dump`/`attach_kernel`) were deferred (consistent with the existing
  `lifecycle` gap) — add them when a blocking fake backend exists (Phase 3).

## Skill Opportunities

- **`rust-task-gate`** — the per-task verification sequence ran identically ~6 times:
  `cargo build --workspace` → `cargo test -p <crate>` → `cargo clippy -p <crate> --all-targets
  -- -D warnings` → `cargo fmt -p <crate> -- --check` (apply if dirty) → `cargo tree` seam
  check → amend commit + assert no co-author trailer. A skill that runs this gate for a given
  crate set and reports pass/fail would remove repetitive orchestration and make the
  "amend after polish" loop one step. *Recurred every task; medium benefit.*
- **`seam-guard`** — `cargo tree -p mcp-tools/-p mcp-session -e normal | grep` for forbidden
  backend/DAP crates was run repeatedly to prove the seam. A tiny scripted gate (also suitable
  for `make seam` / CI) would standardize it and catch a violation the moment it's introduced.
  *Recurred several times; low-medium benefit; already partly envisioned as a Makefile target.*
- **`no-coauthor-commit` enforcement** — the co-author rule had to be threaded into every
  delegated implementer prompt and re-verified after each commit. A commit hook (or harness
  setting) that strips/blocks the trailer would make the preference structural rather than
  prompt-dependent. *Recurred every commit; low effort, removes a recurring foot-gun.*
