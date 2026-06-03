---
title: "Foundation — Workspace + debugger-core seam"
type: phase
plan: RustPort
phase: 1
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "A compiling Cargo workspace with the neutral debugger-core contract crate (traits, neutral types, BackendError, BackendEvent, BackendFactory) and stub crates for the rest."
tasks:
  - id: "1.1"
    title: "Workspace scaffolding + toolchain/lint gates"
    status: complete
    verification: "`cargo build --workspace` succeeds with stub crates; `cargo clippy --workspace --all-targets -- -D warnings` is clean; `cargo fmt --check` passes; the workspace manifest lists all six crates (debugger-core, dap-client, lldb-backend, mcp-session, mcp-tools, debug-mcp) under crates/; testdata/ + its Makefile still build."
  - id: "1.2"
    title: "Neutral types"
    status: complete
    depends_on: ["1.1"]
    verification: "All neutral types compile and derive serde where serialized; outcome enums cover Stopped/Exited/Terminated (StopOutcome, AttachOutcome) and Stopped/Running/Exited (LaunchOutcome); `cargo tree -p debugger-core` shows no tokio/rmcp/DAP dependency (only std/serde/async-trait/futures); a unit test round-trips the serializable types."
  - id: "1.3"
    title: "DebuggerBackend + BackendFactory traits + Connection"
    status: complete
    depends_on: ["1.2"]
    verification: "Traits compile under async-trait; a throwaway stub backend + stub factory in a test implements both and returns a Connection; the trait surface has no DAP/tokio types (BackendEvent stream is futures::stream::BoxStream); doc comments name each method's Go origin."
---

# Phase 1: Foundation — Workspace + debugger-core seam

## Overview

Stand up the Cargo workspace and the neutral contract crate `debugger-core`. This is
the seam every other crate is written against. No behavior yet — the goal is a clean,
compiling skeleton with the traits and types frozen enough that Phases 2–5 can target
them in parallel. Mirrors design §Architecture (workspace) and §Interfaces.

## 1.1: Workspace scaffolding + toolchain/lint gates

### Subtasks
- [ ] Create the top-level `Cargo.toml` `[workspace]` with `members = ["crates/*"]` and a shared `[workspace.dependencies]` table (serde, serde_json, tokio, async-trait, futures, rmcp, thiserror).
- [ ] Create `crates/{debugger-core,dap-client,lldb-backend,mcp-session,mcp-tools,debug-mcp}` each with a minimal `Cargo.toml` + stub `src/lib.rs` (or `src/main.rs` for `debug-mcp`).
- [ ] Set each crate's dependency edges per the design table (e.g. `mcp-tools` → `debugger-core` + `mcp-session` + `rmcp`; `mcp-session` → `debugger-core` + tokio; `lldb-backend` → `debugger-core` + `dap-client` + tokio; `debug-mcp` → all).
- [ ] Add `rust-toolchain.toml` (stable) and a nightly note for ThreadSanitizer runs.
- [ ] Add a CI/lint script (or Makefile targets) running `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`.
- [ ] Keep `testdata/` + its Makefile at the workspace root (reused in Phase 6).
- [ ] Decide repo placement (design: `rust/` subtree or branch) and record it.

### Notes
Enforce the seam structurally from commit one: `mcp-tools`/`mcp-session` must NOT list
`dap-client` or `lldb-backend` as dependencies. A `cargo tree`/`cargo-deny` check in CI
can assert this. Honor the project convention: tests live in `tests/` and `src/tests/`,
not inline `#[cfg(test)]` modules. No `#[allow]` to silence clippy — fix at the source
(local clippy can read clean from a warm cache; CI must run it fresh).

## 1.2: Neutral types

### Subtasks
- [ ] Define enums: `Granularity {Line, Instruction}`, `EvalMode {Expression, Repl}`, `StepKind {Over, Into, Out}`.
- [ ] Define `StopInfo { reason, thread_id, description, hit_breakpoint_ids }` and `StopOutcome { Stopped(StopInfo), Exited{code: Option<i64>}, Terminated }`.
- [ ] Define `LaunchSpec` (program, args, cwd, env, stop_on_entry, source_breakpoints, function_breakpoints), `LaunchOutcome { Stopped, Running, Exited }`, `AttachSpec { pid, wait_for }`, `AttachOutcome { Stopped, Exited, Terminated }`.
- [ ] Define inspection types: `Frame`, `ThreadInfo`, `Scope`, `Variable` (incl. `variables_reference`, `named`, `indexed`), `BreakpointResult`, `EvalResult`, `MemoryRead`, `Instruction`, `SourceBp`, `FunctionBp`.
- [ ] Define `BackendError` (thiserror): `Detect`, `Spawn`, `Send`, `Protocol(type)`, `Dap{message}`, `Closed`, `Timeout`.
- [ ] Define `BackendEvent { Output{category,text}, Terminated{code: Option<i64>} }` and `Connection { backend, events }`.
- [ ] Add a unit test round-tripping the serde-serialized types.

### Notes
These types are the parity vocabulary — keep them debugger-neutral (opaque pass-through
strings for stop `reason`, instruction-pointer, etc., per Spec OQ-2). No `tokio` in the
public API: the event stream is `futures::stream::BoxStream<'static, BackendEvent>`. Map
each type back to its Go origin (design §Interfaces) in doc comments so Phase 3/5
implementers can cross-check field-by-field.

## 1.3: DebuggerBackend + BackendFactory traits + Connection

### Subtasks
- [ ] Define `#[async_trait] trait DebuggerBackend: Send + Sync` with the coarse method set: `launch`, `attach`, `disconnect`, `set_source_breakpoints`, `set_function_breakpoints`, `cont`, `step`, `pause`, `threads`, `stack_trace`, `scopes`, `variables`, `evaluate`, `read_memory`, `disassemble`, `supports_command_repl_mode`.
- [ ] Document that the trait takes NO `CancellationToken` (cancellation is at the tool layer); document the launch-vs-attach stop-waiter asymmetry and the absent `set_exception_breakpoints` (internal to each backend) as design notes.
- [ ] Define `#[async_trait] trait BackendFactory: Send + Sync { fn name(&self) -> &'static str; async fn connect(&self) -> Result<Connection, BackendError>; }`.
- [ ] Write a stub `NullBackend` + `NullFactory` in a `tests/` file to prove both traits are implementable and object-safe (`Arc<dyn DebuggerBackend>`, `Box<dyn BackendFactory>`).

### Notes
Freeze the trait surface here; later phases code against it. The `stack_trace` `start`
param is always 0 from current callers (documented for future backends). Keep the crate
dependency-light: `serde`, `async-trait`, `futures`, `thiserror` only — assert via
`cargo tree`. This task is the gate that lets Phases 2/3 and Phase 4 proceed in parallel.

## Acceptance Criteria
- [ ] `cargo build --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` are clean (no `#[allow]`).
- [ ] All six crates exist with correct dependency edges; the seam is enforced (`mcp-tools`/`mcp-session` cannot reference `dap-client`/`lldb-backend`).
- [ ] `debugger-core` exposes the full neutral type set + both traits + `Connection`, with no tokio/rmcp/DAP dependency.
- [ ] A stub backend/factory compiles against the traits and is object-safe.
- [ ] Test layout uses dedicated folders; serde round-trip test passes.
