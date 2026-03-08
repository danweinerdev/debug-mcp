---
title: "LLDB Debug MCP Server"
type: plan
status: active
created: 2026-03-07
updated: 2026-03-07
tags: [lldb, mcp, dap, go, debugging]
related: [Designs/LLDBDebugMCP, Brainstorm/lldb-mcp-architecture.md]
phases:
  - id: 1
    title: "Foundation — DAP Client + Project Setup"
    status: complete
    doc: "01-Foundation.md"
  - id: 2
    title: "Session Lifecycle — MCP Server + Launch/Attach"
    status: planned
    doc: "02-Session-Lifecycle.md"
    depends_on: [1]
  - id: 3
    title: "Breakpoints + Execution Control"
    status: planned
    doc: "03-Breakpoints-Execution.md"
    depends_on: [2]
  - id: 4
    title: "Inspection Tools"
    status: planned
    doc: "04-Inspection.md"
    depends_on: [3]
  - id: 5
    title: "Advanced Tools + Integration Testing"
    status: planned
    doc: "05-Advanced-Testing.md"
    depends_on: [4]
---

# LLDB Debug MCP Server

## Overview

Implement a Go MCP server that wraps `lldb-dap` via the Debug Adapter Protocol, exposing LLDB debugging capabilities as MCP tools for AI agents. The server is a single Go binary that spawns `lldb-dap` as a subprocess and communicates via DAP over stdio pipes.

The plan delivers: a fully functional MCP debugging server with ~18 tools covering session management, breakpoints, execution control, state inspection, memory/disassembly access, and an escape-hatch for arbitrary LLDB commands.

## Architecture

```
AI Agent ←stdio/MCP→ [Go MCP Server (mcp-go)] ←stdio/DAP→ [lldb-dap] ←SB API→ [Target]
```

Three internal layers:
1. **MCP Tool Handlers** — parameter validation, state guards, response formatting
2. **Session Manager** — session state machine, breakpoint tracking, output buffering
3. **DAP Client** — message framing, request/response correlation, async event dispatch

See `Designs/LLDBDebugMCP/README.md` for full architecture details.

## Key Decisions

1. **Pure Go with lldb-dap** — no Python, no CGo. DAP provides structured JSON protocol.
2. **Custom DAP client** — `google/go-dap` provides types only; we build a thin client (~200 lines).
3. **Blocking execution control** — `continue`/`step_*` block synchronously with context cancellation (no MCP task polling).
4. **`--repl-mode=command`** (lldb-dap/LLVM 18+) with backtick-prefix fallback (lldb-vscode) — separates expression evaluation from LLDB command execution.
5. **Single session** — one lldb-dap subprocess at a time. Simpler tool API.
6. **StopWaiter pattern** — per-call buffered channel registered before DAP request, eliminates race condition.

## Dependencies

- `github.com/mark3labs/mcp-go` — MCP server framework
- `github.com/google/go-dap` — DAP protocol types and message framing
- `lldb-dap` binary — runtime dependency (ships with LLVM/LLDB)
- Go 1.22+ (for standard library features)
- `gcc` or `clang` — for compiling test fixtures (integration tests only)
