---
title: "Integration + Differential Parity"
type: phase
plan: RustPort
phase: 6
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "An integration test suite (behind a Cargo feature, reusing the testdata C fixtures + real lldb-dap) reproducing every Go integration scenario, a differential parity harness comparing debug-mcp against the Go binary, and the structural-verification gate + updated docs."
tasks:
  - id: "6.1"
    title: "Integration harness + fixtures"
    status: complete
    verification: "Behind a `integration` Cargo feature: `make -C testdata` builds the fixtures; the harness locates fixture binaries, sets per-call timeouts, and runs disconnect cleanup; a smoke test launches testdata/simple and reaches stopped — the suite builds and the smoke test passes where lldb-dap is installed."
  - id: "6.2"
    title: "Port the integration scenarios"
    status: complete
    depends_on: ["6.1"]
    verification: "All Go integration_test.go scenarios pass against real lldb-dap: process-exit + exit code 0; stdout capture ('hello from simple'); crash/signal stop with a backtrace frame at crash.c:7; lldb-dap crash recovery (kill→terminated→relaunch); crash during a blocked continue returns within bound (no hang); the full 13-step end-to-end workflow (breakpoints at loop.c:6 and :9, continue, backtrace finds main, variables include i and sum, three step-overs, evaluate i+1, run_command register read, second breakpoint within 20 continues, remove first, list shows 1, continue to exit 0)."
  - id: "6.3"
    title: "Differential parity harness"
    status: complete
    depends_on: ["6.1"]
    verification: "A harness drives identical MCP tool sequences against the Go binary and debug-mcp over stdio and compares parsed JSON results field-by-field (structural); it is green across the scenario suite modulo the two recorded intentional deviations (server name 'debug', disassemble default 20), which it asserts explicitly rather than ignores."
  - id: "6.4"
    title: "Structural-verification gate + docs"
    status: complete
    depends_on: ["6.2", "6.3"]
    verification: "`cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean with zero `#[allow]`; `cargo test --workspace` green; the dap-client concurrency tests pass under ThreadSanitizer; README/MCP-config docs updated for the `debug-mcp` binary + `debug` server name; the two intentional deviations are documented."
---

# Phase 6: Integration + Differential Parity

## Overview

Prove feature parity end-to-end. Port the Go integration suite against real lldb-dap and
the existing C fixtures, build a differential harness that runs the Go and Rust binaries
side-by-side and diffs their JSON, and lock in the structural-verification gate. This is
the phase that operationalizes "behaviorally feature-identical." Mirrors design §Testing
Strategy and Go `internal/tools/integration_test.go`.

## 6.1: Integration harness + fixtures

### Subtasks
- [ ] Add a Cargo `integration` feature (analog of the Go `//go:build integration` tag) gating the suite.
- [ ] Reuse `testdata/*.c` + `testdata/Makefile`; build via `make -C testdata` in a setup step.
- [ ] Fixture-path discovery (relative to the workspace), per-call timeouts (launch 30s, continue 30s, etc.), and a disconnect-cleanup helper that ignores errors.
- [ ] A `parse_tool_result` helper extracting the text content + JSON.
- [ ] A smoke test: launch `testdata/simple`, assert stopped.

### Notes
The suite needs a real `lldb-dap` and the compiled fixtures, so it runs behind the feature
flag and skips/ignores cleanly where unavailable. Reuse the fixtures unchanged — line
numbers (loop.c:6/:9, crash.c:7) are part of the contract. Drive the handlers directly
(in-process) the way the Go integration tests call handler methods, OR over a child
`debug-mcp` process — pick the in-process route to mirror the Go tests' structure.

## 6.2: Port the integration scenarios

### Subtasks
- [ ] Process exit: launch simple → continue → `{status:exited, exit_code:0}` → state terminated → inspection tools error → disconnect → state idle → relaunch.
- [ ] Stdout capture: continue result contains `hello from simple` (in `stdout`, or via `read_output`).
- [ ] Crash handling: launch crash → continue → reason exception|signal, status stopped → backtrace has a frame at `crash.c:7` → `run_command bt` contains `main`.
- [ ] lldb-dap crash recovery: launch loop → status stopped → kill the subprocess → (≤200ms) status terminated → disconnect → idle → relaunch works.
- [ ] Crash during continue: set bp loop.c:6 → continue in a task → kill subprocess → continue returns within bound (terminated/stopped/exited, no hang) → state terminated|stopped → disconnect/reset → relaunch.
- [ ] Full 13-step end-to-end workflow against loop (all assertions per spec AC).

### Notes
These pin the cross-component behavior the unit tests can't (real DAP handshake, real
stop events, real output timing). Match the Go assertions exactly, including the
"within-N-continues" / "within-bound, no hang" timing guards. The crash-during-continue
test is the key proof that the async stop-wait + EOF recovery don't deadlock.

## 6.3: Differential parity harness

### Subtasks
- [ ] A test rig launching both binaries (Go `lldb-debug-mcp` + Rust `debug-mcp`) over stdio.
- [ ] Replay identical MCP tool-call sequences (the scenario suite) against both.
- [ ] Compare parsed JSON results field-by-field (structural; ignore key order/whitespace).
- [ ] Encode the two intentional deviations as explicit expectations (server name `debug` vs `lldb-debug`; `disassemble` default 20 vs 10) — assert the difference, don't mask it.
- [ ] Report field-level diffs on mismatch.

### Notes
This catches drift the per-test mirrors miss and is the operational definition of
parity. Keep the Go binary buildable for this (plan Dependencies). The deviations list is
the single source of truth for "allowed differences"; anything else is a parity bug. Runs
under the `integration` feature.

Spec open questions exercised here: **OQ-3** (repl-mode flag default) is validated
implicitly by the `run_command` scenario — a wrong default changes the backtick prefix,
which the diff would catch. **OQ-4** (`xcrun` only on macOS) is platform-specific and is
not covered on Linux; run it on a macOS CI lane or record it as a manual check.

## 6.4: Structural-verification gate + docs

### Subtasks
- [ ] Wire the full gate into CI: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`, and the `integration` suite where lldb-dap is available.
- [ ] Run the `dap-client` concurrency tests under ThreadSanitizer (nightly); confirm clean.
- [ ] Confirm zero `#[allow]` suppressions and zero `unsafe` (or justify + miri-cover any `unsafe`).
- [ ] Update README + MCP-client config docs for the `debug-mcp` binary and the `debug` server name; keep the `LLDB_DAP_PATH` and lldb-dap install instructions.
- [ ] Document the two intentional deviations (server identity, disassemble=20) in the README.

### Notes
This is the merge gate. No `#[allow]` to silence findings (fix at the source; CI runs
clippy fresh, not from a warm cache). Sanitizers are available in the dev sandbox, so the
TSan run is in-scope here, not deferred. Once green, flip the plan to `complete` and the
spec/design to `implemented` via a debrief.

## Acceptance Criteria
- [ ] The integration suite builds behind the feature flag and the smoke test passes.
- [ ] Every Go integration scenario passes against real lldb-dap (exit, stdout, crash@crash.c:7, recovery, crash-during-continue no-hang, full 13-step workflow).
- [ ] The differential harness is green across the suite, with only the two recorded deviations.
- [ ] Full structural gate is clean (clippy -D warnings, fmt, tests, TSan on dap-client, no `#[allow]`, no unjustified `unsafe`).
- [ ] README/config docs updated for `debug-mcp` + `debug`; deviations documented.
