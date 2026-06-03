---
title: "Rust Rewrite — debug-mcp"
type: plan
status: complete
created: 2026-06-02
updated: 2026-06-03
tags: [rust, rmcp, lldb, dap, debugging, parity, port, windbg, tokio]
related: [Specs/RustPort, Designs/RustPort]
phases:
  - id: 1
    title: "Foundation — Workspace + debugger-core seam"
    status: complete
    doc: "01-Foundation.md"
  - id: 2
    title: "DAP Transport — dap-client"
    status: complete
    doc: "02-DAP-Client.md"
    depends_on: [1]
  - id: 3
    title: "LLDB Backend — lldb-backend"
    status: complete
    doc: "03-LLDB-Backend.md"
    depends_on: [2]
  - id: 4
    title: "Session — mcp-session"
    status: complete
    doc: "04-Session.md"
    depends_on: [1]
  - id: 5
    title: "Tools + Server — mcp-tools + debug-mcp"
    status: complete
    doc: "05-Tools-Server.md"
    depends_on: [3, 4]
  - id: 6
    title: "Integration + Differential Parity"
    status: complete
    doc: "06-Integration-Parity.md"
    depends_on: [5]
---

# Rust Rewrite — debug-mcp

## Overview

Implement the Rust rewrite of the Go `lldb-debug-mcp` server, shipping as the
**`debug-mcp`** binary, per the approved [`Specs/RustPort`](../../Specs/RustPort/README.md)
and [`Designs/RustPort`](../../Designs/RustPort/README.md). The deliverable is a
behaviorally feature-identical MCP debugging server (21 tools, same defaults, response
shapes, error strings, state machine, DAP handshake) built on a `DebuggerBackend` seam
so a future WinDbg backend plugs in without touching the tool layer. Only the
lldb-dap/DAP backend is implemented.

The plan delivers, phase by phase: the neutral contract crate → the generic DAP
transport → the lldb backend → the session manager → the 21 tools + rmcp server →
integration tests and a differential parity harness against the Go binary.

**Two intentional deviations from the Go oracle** (both recorded in the spec's Resolved
Decisions and enforced by tests): the server identity rename (`lldb-debug-mcp`→`debug-mcp`
binary, `lldb-debug`→`debug` MCP server name) and `disassemble` default
`instruction_count` = **20** (documented intent; Go code uses 10). Everything else is
strict behavioral parity.

## Architecture

Six fully-isolated crates in a Cargo workspace; the seam is compiler-enforced
(`mcp-tools`/`mcp-session` depend only on the neutral `debugger-core`, never on DAP or
lldb types). See the design for full rationale.

```mermaid
graph TD
    bin["debug-mcp (bin)<br/>Phase 5"] --> tools["mcp-tools<br/>Phase 5"]
    bin --> session["mcp-session<br/>Phase 4"]
    bin --> lldb["lldb-backend<br/>Phase 3"]
    tools --> core["debugger-core<br/>Phase 1"]
    tools --> session
    session --> core
    lldb --> core
    lldb --> dap["dap-client<br/>Phase 2"]
    lldb -. spawns .-> ext["lldb-dap subprocess"]

    classDef neutral fill:#1b3a2b,stroke:#3fa66a,color:#d6f5e3;
    classDef dapc fill:#3a1b1b,stroke:#a63f3f,color:#f5d6d6;
    class tools,session,core neutral;
    class lldb,dap dapc;
```

```mermaid
flowchart LR
    P1["P1 Foundation<br/>debugger-core"] --> P2["P2 dap-client"]
    P1 --> P4["P4 mcp-session"]
    P2 --> P3["P3 lldb-backend"]
    P3 --> P5["P5 mcp-tools + bin"]
    P4 --> P5
    P5 --> P6["P6 integration + parity"]
```

Phases 2/3 (below-seam transport + backend) and Phase 4 (session) both depend only on
Phase 1 and may proceed in parallel; Phase 5 joins them.

## Key Decisions

Carried from the approved design (see `Designs/RustPort` Design Decisions 1–8):

1. **Six-crate workspace with a `BackendFactory` seam** — `debugger-core`, `dap-client`,
   `lldb-backend`, `mcp-session`, `mcp-tools`, `debug-mcp` (bin). Compiler-enforced
   neutrality; adding WinDbg = new crate + one registration line.
2. **Coarse, blocking `DebuggerBackend` trait** — `launch`/`attach` run the whole DAP
   handshake internally; `cont`/`step` block and return the next `StopOutcome`. All DAP
   quirks (InitializedEvent ordering, stop-waiter race) stay below the seam.
3. **Explicit tool schemas + raw-`Args` accessor** (not the `#[tool]` macro) — to
   reproduce Go's exact validation error strings and permissive numeric/JSON-string handling.
4. **tokio translation** — read-loop task, write `Mutex`, `AtomicI64` seq, `oneshot`
   pending map + stop waiter, single neutral `BackendEvent` stream, `AbortGuard`
   cancel-safety; cancellation lives in the tool handler via `tokio::select!`.
5. **`serde_json::Map` response builders** with conditional inserts (structural JSON
   parity; sorted keys ≈ Go's output for free).
6. **Session `generation` epoch** guards the `running → stopped/terminated` transition
   against a concurrent `disconnect` (Rust dispatches tool calls concurrently).
7. **Tests in dedicated `tests/`/`src/tests/` folders**; `tokio::io::duplex` scripted-peer
   fakes for transport/backend tests; `clippy -D warnings` with **no `#[allow]`**;
   ThreadSanitizer for `dap-client` concurrency.

## Dependencies

- `rmcp` (Rust MCP SDK — server, stdio transport, tool registration).
- `serde` / `serde_json`; `tokio` (runtime, process, sync); `async-trait`; `futures`
  (runtime-neutral `Stream` in `debugger-core`).
- A DAP types source — local `serde` structs in `dap-client` (leaning) or an existing
  crate (resolve in Phase 2; design risk R3).
- Runtime: `lldb-dap` (LLVM 18+) or `lldb-vscode`. Build: `gcc`/`clang` for fixtures.
- The Go binary stays buildable as the parity oracle for the Phase 6 differential harness.
- Open design risks to resolve during implementation: R1 (rmcp concurrent dispatch),
  R2 (rmcp manual schema API), R3 (DAP type source), R5 (`AbortGuard` cancel-safety).
