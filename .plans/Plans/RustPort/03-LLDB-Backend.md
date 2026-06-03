---
title: "LLDB Backend — lldb-backend"
type: phase
plan: RustPort
phase: 3
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "lldb-backend: lldb-dap detection + subprocess spawning, the launch/attach DAP handshake, the LldbBackend implementation of DebuggerBackend (all ops translated to neutral types), and the LldbFactory that produces a Connection."
tasks:
  - id: "3.1"
    title: "lldb-dap detection"
    status: complete
    verification: "Detection returns the first hit in order (LLDB_DAP_PATH → lldb-dap → lldb-dap-20..15 → lldb-vscode → macOS xcrun) with the correct repl-mode-capable flag; versioned search prefers higher (lldb-dap-19 over -15); env-var capability uses substring 'lldb-dap'; lldb-vscode → flag false; not-found error lists every searched candidate — mirrors detect/detect_test.go (incl. the darwin-only xcrun case)."
  - id: "3.2"
    title: "Subprocess spawn + stderr ring buffer"
    status: complete
    depends_on: ["3.1"]
    verification: "Spawn passes `--repl-mode=command` only when capable and nothing otherwise; stdin/stdout/stderr piped; stderr 4096-byte keep-last-N ring (basic, overflow keeps last N, multi-write, concurrent, default-size-on-nonpositive); echo round-trip readable on stdout; exit detected after stdin close — mirrors detect/subprocess_test.go."
  - id: "3.3"
    title: "Launch/attach handshake + lldb arg shapes"
    status: complete
    depends_on: ["3.2"]
    verification: "Launch completes with response+InitializedEvent in EITHER order; launch flushes pending breakpoints (attach does not); SetExceptionBreakpoints (empty) sent before ConfigurationDone; stop-waiter registered BEFORE configurationDone on launch and AFTER on attach — pinned by a TIMING test: the scripted peer delivers the StoppedEvent in the window right after configurationDone, and the launch path must NOT lose it (proving register-before-configDone) while the attach path receives a post-configDone event (proving register-after); stop_on_entry=false → Running, =true → Stopped, exit-during → Exited/Terminated; AttachOutcome covers exit-during-attach; launch/attach args omit empty/false fields (stopOnEntry omitted when false) — scripted-peer tests."
  - id: "3.4"
    title: "LldbBackend op methods + neutral translation + event stream"
    status: complete
    depends_on: ["3.3"]
    verification: "Each op issues the correct DAP request and returns neutral types: set_source/function_breakpoints, cont/step(granularity)/pause, threads, stack_trace(start,levels)→(frames,total), scopes, variables, evaluate(Expression⇒context=variables with frame; Repl⇒context=repl, backtick prepended iff !supports_command_repl_mode, no frame), read_memory, disassemble; supports_command_repl_mode reflects the detected binary; the BackendEvent stream surfaces Output and Terminated{code}."
  - id: "3.5"
    title: "LldbFactory::connect()"
    status: complete
    depends_on: ["3.4"]
    verification: "connect() detects + spawns + builds the client + starts the read loop and returns Connection{backend, events}; detect failure → BackendError::Detect, spawn failure → BackendError::Spawn; a live smoke test (when lldb-dap is present) launches testdata/simple and reaches a stopped state."
---

# Phase 3: LLDB Backend — lldb-backend

## Overview

The lldb-specific backend: it turns the generic `dap-client` into a `DebuggerBackend` by
owning detection, subprocess lifecycle, the lldb-dap launch/attach handshake (including
the version-dependent InitializedEvent ordering and the stop-waiter placement asymmetry),
the lldb-dap argument shapes, the repl-mode/backtick decision, and the DAP→neutral
translation. Mirrors design §"LldbBackend implementation notes" and Go
`internal/detect/*` + `internal/tools/{launch,attach}.go` (handshake) + the DAP-issuing
parts of the other tool files.

## 3.1: lldb-dap detection

### Subtasks
- [ ] Implement detection in order: `LLDB_DAP_PATH` (PATH lookup then absolute-path stat; capable = basename contains `lldb-dap`), `lldb-dap` on PATH (capable), `lldb-dap-<N>` for N=20..15 descending (capable), `lldb-vscode` (not capable), macOS-only `xcrun --find lldb-dap` (capable).
- [ ] Accumulate the searched-candidate list; on no match return `BackendError::Detect` with `lldb-dap binary not found; searched: <comma-list>`.
- [ ] Tests: each branch, versioned-prefers-higher, env substring capability, lldb-vscode false, not-found lists candidates, darwin-only xcrun.

### Notes
Gate `xcrun` on the host OS (Spec OQ-4) — never attempt it on Linux. The capability flag
is what later gates `--repl-mode=command` and the backtick fallback. Mirror
`detect_test.go` exactly, including the dummy-binary fixtures (mode 0o755) and the
version-range wording (15..=20).

## 3.2: Subprocess spawn + stderr ring buffer

### Subtasks
- [ ] `spawn(path, capable)`: `tokio::process::Command`; args = `["--repl-mode=command"]` iff capable else none; pipe stdin/stdout/stderr.
- [ ] Wrap stdout in a buffered reader for the DAP client; spawn a background task draining stderr into a 4096-byte keep-last-N ring (`StderrBuffer`).
- [ ] `StderrBuffer`: default 4096 on non-positive size; `write` keeps the last N bytes (a single oversize write keeps its last N), returns full input length, never errors.
- [ ] Tests: ring basic/overflow/multi-write/concurrent/default-size; spawn `sh`/echo round-trip; exit detection after stdin close.

### Notes
Spawn does NO lifecycle management itself (kill/wait/EOF are driven by the read loop +
disconnect, Phases 2/5). Background-drain stderr to avoid pipe-buffer deadlock. Mirror
`subprocess_test.go` (it spawns `sh`/`true` — reuse that approach so the tests need no
real lldb-dap).

## 3.3: Launch/attach handshake + lldb arg shapes

### Subtasks
- [ ] Define the lldb-dap launch/attach argument structs with exact JSON tags + omitempty (program always present; args/cwd/env/stopOnEntry/initCommands/... omitted when empty/false; attach: pid/program/waitFor/stopOnEntry/attachCommands/coreFile).
- [ ] Implement `launch(LaunchSpec)`: initialize → send launch + await BOTH response and Initialized (order-independent) → flush source then function breakpoints → SetExceptionBreakpoints(empty) → register stop-waiter (before configDone) if stop_on_entry → ConfigurationDone → if stop_on_entry await stop and map to `LaunchOutcome::{Stopped,Exited}` else `Running`.
- [ ] Implement `attach(AttachSpec)`: same up to the handshake, but NO breakpoint flush; SetExceptionBreakpoints(empty) → ConfigurationDone → register stop-waiter (AFTER configDone) → await stop → `AttachOutcome::{Stopped,Exited,Terminated}`.
- [ ] Map every handshake failure to the spec's error strings (these surface to the user via the Phase 5 handler).
- [ ] Tests (scripted peer): both InitializedEvent orderings; launch-flushes/attach-doesn't; exception-bp-before-configDone; **stop-waiter placement asymmetry as a timing test** — the peer emits the StoppedEvent immediately after configurationDone; the launch path must still capture it (waiter registered before configDone) and an inverted placement makes the test fail; the attach path captures a post-configDone event; stop_on_entry true/false/exit-during; arg omitempty (stopOnEntry omitted when false).

### Notes
The InitializedEvent-vs-response ordering varies across lldb-dap versions — use the
`send_and_await_both` primitive from 2.2; never assume an order. The stop-waiter
placement asymmetry (launch before / attach after configurationDone) is load-bearing —
do not swap (design §LldbBackend notes; Go `launch.go:304` vs `attach.go:219`). Pending
breakpoints arrive in `LaunchSpec` (the session flushes its buffer into the spec); attach
has no such field.

## 3.4: LldbBackend op methods + neutral translation + event stream

### Subtasks
- [ ] Implement breakpoint ops (`set_source_breakpoints`, `set_function_breakpoints`) → `Vec<BreakpointResult>`.
- [ ] Implement execution ops: `cont`/`step(kind,gran)` register the stop-waiter before sending and return the next `StopOutcome`; `pause` (thread 0) returns immediately.
- [ ] Implement inspection ops: `threads`, `stack_trace(thread,start,levels)→(Vec<Frame>,total)`, `scopes`, `variables(ref)`, `evaluate(expr,frame,mode)`, `read_memory(addr,count)→MemoryRead`, `disassemble(addr,count)→Vec<Instruction>`.
- [ ] `evaluate`: `Expression` ⇒ `context="variables"` with the frame id; `Repl` ⇒ `context="repl"`, no frame, prepend a backtick iff `!supports_command_repl_mode()`.
- [ ] Translate DAP responses → neutral types (opaque pass-through for reason/IP strings); adapt the read-loop output/terminated signals into a `BackendEvent` stream.
- [ ] `supports_command_repl_mode()` returns the detected capability.
- [ ] Tests (scripted peer): each op's request shape + neutral mapping; repl backtick on/off; evaluate-vs-repl context; event stream surfaces Output + Terminated.

### Notes
Keep neutral types opaque (Spec OQ-2). Memory/disassemble address normalization and the
hex-dump/formatting stay in `mcp-tools` (Phase 5) — this layer returns raw bytes
(`MemoryRead`) and raw instruction fields. `read_memory` returns the backend's echoed
address plus the decoded bytes; the base64 decode happens here (DAP delivers base64),
the hex formatting happens in Phase 5.

## 3.5: LldbFactory::connect()

### Subtasks
- [ ] Implement `LldbFactory { }` + `name() == "lldb"`.
- [ ] `connect()`: detect → spawn → build `dap-client::Client` over the pipes → start the read loop → assemble the `BackendEvent` stream → return `Connection { backend: Arc<LldbBackend>, events }`.
- [ ] Surface detect/spawn failures as `BackendError::Detect`/`Spawn`.
- [ ] A `#[cfg(feature = "live")]` (or ignored-by-default) smoke test launching `testdata/simple` when lldb-dap is present.

### Notes
`connect()` corresponds to Go's lazy lldb-dap spawn at launch/attach time — it is NOT
called at server startup (Phase 5 calls it inside the launch/attach handler). The backend
is returned *not-yet-launched*; the handler then calls `backend.launch(spec)`.

## Acceptance Criteria
- [ ] Detection order, versioned preference, capability flag, and not-found message match Go exactly (incl. darwin-only xcrun).
- [ ] Subprocess spawn passes `--repl-mode=command` only when capable; stderr 4 KB ring matches Go semantics.
- [ ] Launch/attach handshakes reproduce the order-independent InitializedEvent, the flush asymmetry, the exception-bp/configDone ordering, the stop-waiter placement asymmetry, and all outcomes.
- [ ] Every `DebuggerBackend` op issues the correct DAP request and returns neutral types; repl backtick logic and evaluate/repl contexts are correct.
- [ ] `LldbFactory::connect()` returns a usable `Connection`; failures map to the right `BackendError`.
- [ ] `cargo clippy -- -D warnings` clean; scripted-peer tests deterministic; live smoke test passes where lldb-dap exists.
