---
title: "Session Lifecycle — MCP Server + Launch/Attach"
type: phase
plan: "LLDBDebugMCP"
phase: 2
status: complete
created: 2026-03-07
updated: 2026-03-08
deliverable: "MCP server with launch, attach, disconnect, and status tools. Can launch a program under lldb-dap, receive StoppedEvent on entry, query status, and disconnect cleanly."
tasks:
  - id: "2.1"
    title: "Session manager + state machine"
    status: complete
    verification: "Unit test: state transitions idle→configuring→stopped→running→stopped→terminated→idle all succeed. Invalid transitions (e.g. running→idle, idle→stopped) return descriptive errors. State is queryable at any time."
  - id: "2.2"
    title: "MCP server skeleton + stdio transport"
    status: complete
    verification: "Server starts, registers tools, serves over stdio. Manual test: pipe a valid MCP initialize message to stdin, receive valid response on stdout."
    depends_on: ["2.1"]
  - id: "2.3"
    title: "Launch tool — full DAP handshake"
    status: complete
    verification: "Integration test: launch a simple C program (compiled with -g, contains main() that returns 0) with stop_on_entry=true. Verify: (1) lldb-dap subprocess is running, (2) InitializeResponse received, (3) LaunchResponse and InitializedEvent both received (order-independent), (4) ConfigurationDone succeeds, (5) StoppedEvent received with reason 'entry', (6) session state is 'stopped', (7) frame index mapping is populated (frame 0 resolves). Test with stop_on_entry=false: program runs to completion, ExitedEvent received."
    depends_on: ["2.1", "2.2"]
  - id: "2.4"
    title: "Attach tool"
    status: complete
    verification: "Integration test: spawn a long-running process (e.g., `sleep 60`), attach by PID. Verify session enters stopped state. Detach without killing. Verify the original process continues running."
    depends_on: ["2.3"]
  - id: "2.5"
    title: "Disconnect tool"
    status: complete
    verification: "Integration test: launch, then disconnect with terminate=true — target process is killed, lldb-dap subprocess exits, session returns to idle. Disconnect with terminate=false — target continues (when attached). After disconnect, launch works again for a new session."
    depends_on: ["2.3"]
  - id: "2.6"
    title: "Status tool"
    status: complete
    verification: "Returns correct info in each state: idle (no session), stopped (program name, PID, stop location, thread), terminated (exit code). Does not error in any state."
    depends_on: ["2.3"]
  - id: "2.7"
    title: "Test fixtures setup"
    status: complete
    verification: "`testdata/` directory contains C source files and a Makefile. `make -C testdata` compiles fixtures with `gcc -g -O0 -fno-omit-frame-pointer`. Fixtures include: simple.c (main returns 0), loop.c (counted loop with variables), crash.c (NULL dereference), multithread.c (pthread_create)."
    depends_on: ["2.3"]
  - id: "2.8"
    title: "Structural verification"
    status: complete
    verification: "`go vet ./...` passes; `go test -race ./...` passes including integration tests"
    depends_on: ["2.3", "2.4", "2.5", "2.6", "2.7"]
---

# Phase 2: Session Lifecycle — MCP Server + Launch/Attach

## Overview

Wire up the MCP server using mcp-go and implement the session manager that owns the lldb-dap subprocess lifecycle. This phase delivers the first usable tools: `launch`, `attach`, `disconnect`, and `status`.

## 2.1: Session manager + state machine

### Subtasks
- [x] Define `State` type: `idle`, `configuring`, `stopped`, `running`, `terminated`
- [x] Implement `SessionManager` struct: holds `*dap.Client`, `State`, process info (program, PID), exit code, breakpoint state maps, output buffer
- [x] Implement `CheckState(allowed ...State) error` for tool state guards
- [x] Implement `SetState(State)` with mutex protection
- [x] Implement `Reset()` to return to idle (clears all session state)

## 2.2: MCP server skeleton + stdio transport

### Subtasks
- [x] Create `server.NewMCPServer("lldb-debug", "1.0.0")` in main.go
- [x] Register all tools (with placeholder handlers that return "not implemented" for Phase 3-5 tools)
- [x] Call `server.ServeStdio(s)` for stdio transport
- [x] Define tool parameter schemas using `mcp.NewTool()` with `mcp.WithString`, `mcp.WithNumber`, `mcp.WithBoolean`

### Notes
Register all 18 tools upfront so the agent sees the full capability set from the start. Only `launch`, `attach`, `disconnect`, `status` have real handlers in this phase.

## 2.3: Launch tool — full DAP handshake

### Subtasks
- [x] In the `launch` handler: check state is idle
- [x] Call `detect.FindLLDBDAP()` to locate binary
- [x] Call subprocess spawn — pass `--repl-mode=command` only when binary name is `lldb-dap` (LLVM 18+); omit for `lldb-vscode`. Set `session.replModeCommand` flag accordingly.
- [x] Create `dap.Client` with stdin/stdout pipes
- [x] Start read loop goroutine
- [x] Send `InitializeRequest` → wait for `InitializeResponse`
- [x] Send `LaunchRequest` with `LLDBDAPLaunchArgs` via `SendAsync` (non-blocking)
- [x] Wait for BOTH `LaunchResponse` AND `InitializedEvent` in any order (select on response channel + initializedChan)
- [x] Flush pending breakpoint buffer: send `SetBreakpointsRequest` for each file (empty initially, expanded in task 3.5) and `SetFunctionBreakpointsRequest` (empty initially)
- [x] Send `SetExceptionBreakpointsRequest` (empty filters for now)
- [x] Send `ConfigurationDoneRequest` → wait for response
- [x] If `stop_on_entry`: wait for `StoppedEvent` via StopWaiter; frame mapping auto-populated by RefreshFrameMapping in StoppedEvent handler; set state to `stopped`; return with stop location
- [x] If NOT `stop_on_entry`: set state to `running`; frame mapping remains empty (populated on first StoppedEvent); return with state="running"
- [x] Return success with program name, PID, stop location (if stopped) or state (if running)

### Notes
The launch handler drives the entire DAP handshake synchronously. The session is in `configuring` state during this sequence. If any step fails, clean up the subprocess and return to idle with error.

## 2.4: Attach tool

### Subtasks
- [x] Similar to launch but sends `AttachRequest` with `LLDBDAPAttachArgs`
- [x] Support `pid` (number) and `wait_for` (string) parameters
- [x] Same handshake: initialize → attach → initialized → configurationDone
- [x] Set stop_on_entry=true by default for attach

## 2.5: Disconnect tool

### Subtasks
- [x] Send `DisconnectRequest` with `TerminateDebuggee` parameter
- [x] Wait for disconnect response
- [x] Wait for subprocess to exit (with timeout)
- [x] Kill subprocess if it doesn't exit within 5 seconds
- [x] Call `session.Reset()` to return to idle
- [x] Cancel any pending StopWaiter

## 2.6: Status tool

### Subtasks
- [x] No state guard — valid in any state
- [x] In idle: return `{"state": "idle", "message": "No active debug session"}`
- [x] In stopped: return state, program, PID, stop location from cached frame mapping (no live DAP calls), thread info from last StoppedEvent
- [x] In running: return state, program, PID
- [x] In terminated: return state, exit code

## 2.7: Test fixtures setup

### Subtasks
- [x] Create `testdata/simple.c` — main returns 0
- [x] Create `testdata/loop.c` — for loop with int counter variable
- [x] Create `testdata/crash.c` — NULL pointer dereference
- [x] Create `testdata/structs.c` — nested struct with 2-3 levels of fields (for variable depth testing)
- [x] Create `testdata/multithread.c` — creates a pthread, joins it
- [x] Create `testdata/Makefile` — compiles all with `gcc -g -O0 -fno-omit-frame-pointer`
- [x] Add `//go:build integration` tag to integration test files

## 2.8: Structural verification

### Subtasks
- [x] `go vet ./...`
- [x] `go test -race ./...`

## Acceptance Criteria
- [x] `launch` spawns lldb-dap, completes DAP handshake, receives StoppedEvent
- [x] `attach` connects to a running process
- [x] `disconnect` cleanly terminates session and returns to idle
- [x] `status` returns correct information in all states
- [x] Can launch → disconnect → launch again (session re-use)
- [x] Race detector clean
