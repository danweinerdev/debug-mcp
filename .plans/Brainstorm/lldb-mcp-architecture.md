---
title: "LLDB MCP Server Architecture"
type: brainstorm
status: archived
created: 2026-03-07
updated: 2026-03-07
tags: [lldb, mcp, debugging, go, architecture]
related: []
---

# LLDB MCP Server Architecture

## Problem Statement

Build an MCP server in Go (using `github.com/mark3labs/mcp-go`) that wraps LLDB to let an AI agent interactively debug executables on Linux and macOS. The server must expose debugging operations (breakpoints, stepping, inspection, expression evaluation) as MCP tools over stdio transport.

**Key constraints:**
- Cross-platform: Linux and macOS
- Go codebase using mcp-go
- Must maintain session state between tool calls (debugger stays attached)
- Must handle blocking operations like `continue` (which blocks until next breakpoint)
- LLDB itself has no stable Go bindings

**Prior art:** Several Python-based LLDB MCP servers already exist (stass/lldb-mcp, benpm/claude_lldb_mcp, stableversion/lldb_mcp, ant4g0nist/lisa.py). A C#/.NET server also exists (tonyredondo/debugger-mcp-server). LLDB itself has a nascent built-in MCP server in recent LLVM builds. No Go-based implementations exist yet.

## Ideas

### Idea 1: Go + Python Bridge (JSON-RPC over stdin/stdout)

**Description:**
Go MCP server spawns a long-lived Python subprocess that imports the `lldb` Python module. The Python script uses `SBDebugger` with `SetAsync(False)` and `SBCommandInterpreter.HandleCommand()` to execute commands synchronously. Communication between Go and Python uses newline-delimited JSON over stdin/stdout. The Go server translates MCP tool calls into JSON requests, sends them to the Python bridge, and returns the results.

For structured data (variables, threads, frames), the Python bridge can use the SB API directly to build JSON objects rather than parsing text output. For power-user needs, a `run_command` escape-hatch tool passes arbitrary LLDB commands through `HandleCommand`.

For the `continue` operation (which blocks until next stop), the Go server uses mcp-go's `AddTaskTool` for async execution, with the Python bridge running the continue in a separate thread and reporting back when the process stops.

**Pros:**
- Full access to LLDB's stable, well-documented Python API (SB classes)
- Python bridge can be very thin (~200 lines for the core loop)
- Structured JSON output from Python — no fragile text parsing
- Works on both Linux and macOS with system LLDB
- Clean separation: Go handles MCP protocol, Python handles LLDB
- Battle-tested pattern — most existing LLDB MCP servers use this approach
- Easy to extend: adding a new tool means adding a Python handler function

**Cons:**
- Requires Python 3 + LLDB Python module installed on the host
- LLDB Python module path varies by platform (`lldb -P` to discover)
- Python version must match what LLDB was built against
- Two-process architecture adds IPC complexity
- Bridge process must handle its own error recovery
- Distribution requires bundling or documenting the Python dependency

**Effort:** Medium

### Idea 2: Go + lldb-dap (Debug Adapter Protocol intermediary)

**Description:**
Go MCP server spawns `lldb-dap` as a subprocess and communicates using the Debug Adapter Protocol (JSON with Content-Length framing, same as LSP). Google's `github.com/google/go-dap` library provides Go types for the full DAP spec. The Go server translates MCP tool calls into DAP requests (SetBreakpoints, Next, StepIn, StackTrace, Variables, Evaluate, etc.) and converts DAP responses into MCP tool results.

**Pros:**
- DAP is a well-defined, versioned protocol with structured JSON messages
- `google/go-dap` provides complete Go types — no manual parsing
- Pure Go solution (no Python dependency)
- `lldb-dap` is maintained by the LLDB team and ships with LLVM
- Natural mapping: DAP operations map cleanly to debugging MCP tools
- Session management (launch/attach) is protocol-defined

**Cons:**
- DAP has a verbose handshake ceremony (initialize → initialized → configurationDone)
- DAP's variable inspection is multi-round-trip (scopes → variables → child variables)
- `lldb-dap` must be installed separately (not always present by default)
- No escape-hatch for arbitrary LLDB commands — DAP only exposes what it supports
- DAP expression evaluation is limited compared to direct LLDB `expr`
- More protocol boilerplate than the Python bridge approach
- `lldb-dap` was renamed from `lldb-vscode` in LLVM 18 — binary name varies

**Effort:** Medium-High

### Idea 3: Go + CGo + liblldb (Direct C++ API)

**Description:**
Use CGo to link against `liblldb.so` (Linux) or `LLDB.framework` (macOS) and call the SB API directly from Go. Write thin C wrapper functions around the C++ SB classes, then call those from Go via CGo. The Go server directly controls LLDB without any subprocess.

**Pros:**
- Single-process architecture — no IPC overhead
- Direct API access — maximum control and performance
- No Python or external binary dependency at runtime
- Can expose the full SB API surface

**Cons:**
- **Extremely complex build setup** — requires LLVM/LLDB development headers, platform-specific linking
- C++ ABI fragility across LLDB versions
- CGo disables Go's goroutine preemption and garbage collector optimizations
- Cross-compilation becomes very difficult (need platform-specific LLDB builds)
- Must write and maintain C wrapper layer for C++ classes
- Debugging CGo crashes is painful
- Distribution requires users to have matching LLDB development packages
- Very few Go projects successfully wrap large C++ APIs via CGo

**Effort:** Very High

### Idea 4: Go + Raw LLDB Subprocess (Text Parsing)

**Description:**
Go MCP server spawns `lldb` as a subprocess and drives it via stdin/stdout, sending LLDB commands as text and parsing the text output with regex/string matching. The simplest possible architecture — just a process wrapper.

**Pros:**
- Simplest conceptual model — no dependencies beyond the `lldb` binary
- Pure Go implementation
- Works with any LLDB version
- Easy to prototype quickly

**Cons:**
- **Fragile text parsing** — LLDB's output format is human-readable, not machine-stable
- Output format changes between LLDB versions
- Detecting command completion boundaries is unreliable
- No structured data extraction (variables, threads, frames become string parsing nightmares)
- Error detection is heuristic-based
- Multi-line outputs (backtraces, memory dumps) are hard to delimit
- The deprecated `lldb-mi` (Machine Interface) would have helped but is no longer maintained

**Effort:** Low initially, High for robustness

### Idea 5: Proxy to LLDB's Built-in MCP Server

**Description:**
Recent LLVM builds include a native MCP server in LLDB itself, started via `protocol-server start MCP listen://localhost:<port>`. The Go server would act as a proxy/enhancer: it starts LLDB, activates the built-in MCP server, connects to it, and either forwards requests directly or adds higher-level tools on top.

**Pros:**
- Maintained by the LLDB/LLVM team
- Native C++ implementation — maximum performance
- Already implements the MCP protocol
- No Python dependency

**Cons:**
- **Only available in very recent LLVM builds (2025+)** — not on most distros yet
- Exposes only a single `lldb_command` tool (no structured tool definitions)
- Requires LLDB to already be running (not standalone)
- Socket-based, not stdio — adds networking complexity
- Minimal resources exposed (debugger ID, target index)
- Still nascent/experimental — API may change
- Users must build LLVM from source to get it on most Linux distros

**Effort:** Low (if available), but practically inaccessible today

## Evaluation

### Comparison Matrix

| Criteria              | Python Bridge | lldb-dap    | CGo+liblldb | Raw Subprocess | Built-in MCP Proxy |
|-----------------------|---------------|-------------|-------------|----------------|--------------------|
| **Feasibility**       | High          | High        | Low         | High           | Low (availability) |
| **API Richness**      | Full SB API   | DAP subset  | Full SB API | Text only      | Single command     |
| **Structured Output** | Excellent     | Good        | Excellent   | Poor           | Text only          |
| **Cross-platform**    | Good          | Good        | Poor        | Good           | Poor (recent only) |
| **Build Complexity**  | Low           | Low         | Very High   | Low            | Low                |
| **Runtime Deps**      | Python+lldb   | lldb-dap    | liblldb-dev | lldb           | Latest LLDB        |
| **Maintainability**   | Good          | Good        | Poor        | Poor           | Unknown            |
| **Extensibility**     | Excellent     | Limited     | Excellent   | Poor           | Limited            |
| **Distribution**      | Medium        | Medium      | Hard        | Easy           | Hard               |
| **Effort**            | Medium        | Medium-High | Very High   | Low→High       | Low (if available) |

### Recommendation

**Idea 2: Go + lldb-dap is the chosen approach** — a pure Go solution with no Python dependency.

Rationale:

1. **Pure Go.** No Python runtime, no version compatibility issues, no LLDB Python module path discovery. Single static binary distribution.

2. **Structured protocol.** DAP provides typed JSON messages for every debugging operation — no text parsing. `google/go-dap` gives complete Go types.

3. **Escape hatch exists.** DAP's `evaluate` request with `context: "repl"` passes commands directly to the LLDB command interpreter, covering arbitrary LLDB commands that don't have dedicated DAP requests.

4. **Manageable ceremony.** The DAP handshake (initialize → configurationDone) is ~20 lines of Go and runs once per session. Variable inspection is multi-round-trip but the Go server can flatten it into a single MCP tool response.

5. **Maintained dependency.** `lldb-dap` ships with LLVM and is actively maintained by the LLDB team. It's the official IDE integration path.

6. **Same runtime dependency class.** If the user has `lldb` installed, `lldb-dap` is typically available alongside it (or can be installed from the same package).

The Python bridge (Idea 1) remains a viable fallback if `lldb-dap` proves insufficient for specific use cases, but for a Go-only project the DAP approach is the right choice.

## Next Steps

- Set up Go module with mcp-go and google/go-dap dependencies
- Implement DAP client: connection management, message framing, request/response correlation
- Implement DAP session lifecycle: initialize → launch/attach → configurationDone
- Implement core MCP tools: `launch`, `attach`, `set_breakpoint`, `continue`, `step_over`, `step_into`, `step_out`, `backtrace`, `variables`, `evaluate`, `run_command` (via DAP evaluate/repl)
- Handle `continue` blocking with async patterns
- Flatten DAP's multi-round-trip variable inspection into single MCP responses
- Auto-detect `lldb-dap` vs `lldb-vscode` binary name
- Test on both Linux and macOS
