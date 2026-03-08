---
title: "LLDB Debug MCP Server"
type: design
status: approved
created: 2026-03-07
updated: 2026-03-07
tags: [lldb, mcp, dap, go, debugging]
related: [Brainstorm/lldb-mcp-architecture.md]
---

# LLDB Debug MCP Server

## Overview

A Go MCP server that enables AI agents to interactively debug executables on Linux and macOS. It wraps `lldb-dap` (LLDB's Debug Adapter Protocol server) via the DAP wire protocol, translating MCP tool calls into DAP requests and DAP events into MCP tool results.

```
AI Agent ←stdio/MCP→ [Go MCP Server] ←stdio/DAP→ [lldb-dap subprocess] ←SB API→ [Target Process]
```

**Key properties:**
- Pure Go — no Python, no CGo
- Single binary distribution (plus `lldb-dap` as a runtime dependency)
- Single debug session at a time
- Structured JSON responses from DAP — no text parsing
- Escape hatch via DAP evaluate/repl for arbitrary LLDB commands

## Architecture

### Components

```
┌─────────────────────────────────────────────────────────┐
│                    MCP Server (mcp-go)                   │
│                                                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │  Tool:       │  │  Tool:       │  │  Tool:       │  │
│  │  launch      │  │  set_bp      │  │  continue    │  │
│  │  attach      │  │  variables   │  │  step_over   │  │
│  │  disconnect  │  │  backtrace   │  │  step_into   │  │
│  │              │  │  evaluate    │  │  step_out    │  │
│  │              │  │  run_command │  │              │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
│         │                 │                 │           │
│         └────────────┬────┴────────┬────────┘           │
│                      │             │                    │
│              ┌───────▼─────────────▼───────┐            │
│              │       Session Manager       │            │
│              │  (owns DAP client + state)  │            │
│              └───────────┬─────────────────┘            │
│                          │                              │
│              ┌───────────▼─────────────────┐            │
│              │        DAP Client           │            │
│              │  ┌────────┐  ┌───────────┐  │            │
│              │  │ Writer │  │ Read Loop │  │            │
│              │  │ (sync) │  │(goroutine)│  │            │
│              │  └───┬────┘  └─────┬─────┘  │            │
│              │      │             │        │            │
│              │      │  ┌──────────▼─────┐  │            │
│              │      │  │ Event Dispatch │  │            │
│              │      │  │  pending map   │  │            │
│              │      │  │  event channel │  │            │
│              │      │  └────────────────┘  │            │
│              └──────┼──────────────────────┘            │
│                     │                                   │
└─────────────────────┼───────────────────────────────────┘
                      │ stdin/stdout pipes
              ┌───────▼──────────┐
              │    lldb-dap      │
              │   subprocess     │
              └───────┬──────────┘
                      │ SB API
              ┌───────▼──────────┐
              │  Target Process  │
              └──────────────────┘
```

**1. MCP Server** — Uses `mcp-go` to expose debugging tools over stdio. Handles MCP protocol, tool registration, and parameter validation. mcp-go's `StdioServer` dispatches `tools/call` requests to a worker pool (default 5 goroutines, configurable via `WithWorkerPoolSize`). This means `pause` can execute concurrently while `continue` is blocking in another worker — verified in mcp-go v0.45.0 source (`server/stdio.go`) and its own `TestRaceConditions`.

**2. Session Manager** — Owns the lifecycle of a single debug session. Manages the lldb-dap subprocess, tracks session state (not started → launched/attached → stopped → running → terminated), and serializes execution-control operations.

**3. DAP Client** — Custom implementation using `google/go-dap` types and framing. Handles:
- `Send(request) → response` — blocking: assigns seq, registers pending channel, writes message, blocks on response
- `SendAsync(request) → chan response` — non-blocking: same as Send but returns the channel instead of blocking (used in the launch handshake where we need to wait for both LaunchResponse and InitializedEvent in any order)
- Reading DAP messages in a dedicated goroutine (`readLoop`)
- Correlating responses to requests via `request_seq` → channel map
- Dispatching async events (stopped, output, terminated) to subscribers
- `cancelAllPending(err)` — on EOF, drains the pending map with error sentinels

**4. Frame Mapping** — The Session Manager caches the frame index → DAP frameId mapping. `RefreshFrameMapping` is called on every `StoppedEvent`: it sends a `StackTraceRequest` for the stopped thread and caches the result. This ensures `variables` and `evaluate` work immediately after a stop without requiring a prior `backtrace` call. The `backtrace` tool also refreshes this mapping.

### Data Flow

#### Launch Flow
```
1. MCP tool call: launch(program, args, stopOnEntry)
2. Session Manager spawns lldb-dap subprocess (stderr → ring buffer goroutine)
3. DAP Client sends InitializeRequest → waits for InitializeResponse
4. DAP Client sends LaunchRequest via SendAsync (non-blocking — read loop handles response)
5. DAP Client waits for BOTH LaunchResponse AND InitializedEvent (any order)
   — Read loop dispatches LaunchResponse to pending map channel
   — Read loop dispatches InitializedEvent to initializedChan
   — Launch handler uses select{} to collect both, order-independent
6. DAP Client sends SetBreakpointsRequest (flush pending breakpoint buffer)
7. DAP Client sends SetExceptionBreakpointsRequest (even if empty)
8. DAP Client sends ConfigurationDoneRequest → waits for response
9. If stopOnEntry:
   a. Wait for StoppedEvent via per-call stop waiter
   b. Frame mapping is auto-populated by RefreshFrameMapping (called in StoppedEvent handler)
   c. Set session state to `stopped`
   d. Return success with program name, PID, stop location
10. If NOT stopOnEntry:
   a. Set session state to `running` (process is executing freely)
   b. Frame mapping remains empty — will be populated on first StoppedEvent
   c. Return success with program name, PID, state="running"
```

**Order-independence:** The DAP handshake ordering between `LaunchResponse` and
`InitializedEvent` varies across lldb-dap versions (refactored in LLVM PRs
#138219, #140331, #171549). The launch handler must accept either order. The
implementation uses a `SendAsync()` variant that returns a channel for the
response without blocking, then loops to collect both before proceeding:

```go
launchRespChan, _ := client.SendAsync(launchReq)
var launchResp dap.Message
var gotInitialized bool
for !gotInitialized || launchResp == nil {
    select {
    case r := <-launchRespChan:
        launchResp = r
        launchRespChan = nil  // prevent re-receive
    case <-client.InitializedChan():
        gotInitialized = true
    case <-ctx.Done():
        return ctx.Err()
    }
}
// Both received — proceed to step 6 (configuration)
```

#### Breakpoint → Continue → Inspect Flow
```
1. MCP tool call: set_breakpoint(file, line)
   → DAP SetBreakpointsRequest → SetBreakpointsResponse
   → MCP result: breakpoint ID, verified status

2. MCP tool call: continue()
   → DAP ContinueRequest → ContinueResponse (immediate)
   → Block on stopped event channel
   → StoppedEvent arrives (reason: "breakpoint", hitBreakpointIds)
   → MCP result: stop reason, location, source context

3. MCP tool call: backtrace()
   → DAP ThreadsRequest → StackTraceRequest
   → MCP result: formatted stack trace

4. MCP tool call: variables(frameIndex, scope)
   → DAP ScopesRequest → VariablesRequest (recursive)
   → MCP result: flattened variable tree
```

#### Event Dispatch
```
DAP Read Loop goroutine:
  for {
    msg := ReadProtocolMessage(reader)
    if err != nil {
      // EOF or read error — lldb-dap crashed or exited
      cancelAllPending(err)             // drain pending map, send error to all waiters
      stopWaiter.Cancel()               // unblock any waiting continue/step
      session.SetState(terminated)
      return
    }
    switch msg.(type) {
    case Response types:
      pending[msg.RequestSeq] <- msg    // unblock the waiting request
    case *StoppedEvent:
      session.RefreshFrameMapping(msg)  // auto-populate frame index→DAP frameId mapping
      stopWaiter.Deliver(msg)           // deliver to registered one-shot waiter (buffered chan 1)
    case *InitializedEvent:
      initializedChan <- struct{}{}     // signal launch flow to send configuration
    case *OutputEvent:
      outputBuffer.Append(msg)          // buffer for read_output tool
    case *ExitedEvent:
      session.SetExitCode(msg.ExitCode)
      stopWaiter.DeliverExit(msg.ExitCode) // unblock continue/step with exit info
    case *TerminatedEvent:
      session.SetState(terminated)
      stopWaiter.Cancel()               // fallback unblock if no ExitedEvent was received
    }
  }
```

**Stop Waiter pattern:** Before sending a `ContinueRequest` or `Step*Request`,
the tool handler calls `stopWaiter.Register()` which returns a `chan StopResult`
(buffered size 1). The read loop's `Deliver()` sends to this channel. Only one
waiter is active at a time (enforced by session state: only `stopped` → `running`
allows registering). This eliminates the race between request-send and event-arrival
because the channel is registered *before* the DAP request is sent.

```go
type StopWaiter struct {
    mu   sync.Mutex
    ch   chan StopResult    // nil when no waiter registered
}

func (w *StopWaiter) Register() <-chan StopResult {
    w.mu.Lock()
    defer w.mu.Unlock()
    w.ch = make(chan StopResult, 1)
    return w.ch
}

func (w *StopWaiter) Deliver(event *dap.StoppedEvent) {
    w.mu.Lock()
    defer w.mu.Unlock()
    if w.ch != nil {
        w.ch <- StopResult{Event: event}
        w.ch = nil
    }
}

func (w *StopWaiter) DeliverExit(exitCode int) {
    w.mu.Lock()
    defer w.mu.Unlock()
    if w.ch != nil {
        w.ch <- StopResult{Exited: true, ExitCode: &exitCode}
        w.ch = nil
    }
}

func (w *StopWaiter) Cancel() {
    w.mu.Lock()
    defer w.mu.Unlock()
    if w.ch != nil {
        w.ch <- StopResult{Terminated: true}
        w.ch = nil
    }
}
```

**EOF / crash recovery:** When the read loop detects EOF, `cancelAllPending(err)`
iterates the pending map and sends an error sentinel to every waiting channel,
unblocking any in-flight `launch`, `evaluate`, or other request. The subprocess
exit code and stderr are included in the error returned to the MCP tool caller.

### Interfaces

#### MCP Tools

**Session Management:**

| Tool | Parameters | Description |
|------|-----------|-------------|
| `launch` | `program` (string, required), `args` (string[], optional), `cwd` (string, optional), `env` (object, optional), `stop_on_entry` (bool, optional, default true) | Launch a program under the debugger |
| `attach` | `pid` (number) OR `wait_for` (string, process name) | Attach to a running process |
| `disconnect` | `terminate` (bool, optional, default true) | End the debug session |

**Breakpoints:**

| Tool | Parameters | Description |
|------|-----------|-------------|
| `set_breakpoint` | `file` (string, required), `line` (number, required), `condition` (string, optional) | Set a source breakpoint. Returns breakpoint ID. |
| `set_function_breakpoint` | `name` (string, required), `condition` (string, optional) | Set a breakpoint on a function name. Returns breakpoint ID. |
| `remove_breakpoint` | `breakpoint_id` (number, required) | Remove a single breakpoint by ID |
| `list_breakpoints` | — | List all active breakpoints with IDs, locations, and conditions |

Breakpoint state tracking: The Session Manager maintains a `map[string][]SourceBreakpoint`
(keyed by file path) and a `[]FunctionBreakpoint` list. When `set_breakpoint` is called,
the new breakpoint is appended to the file's list and a `SetBreakpointsRequest` is sent
with the complete list for that file. When `remove_breakpoint(id)` is called, the
breakpoint is located by scanning all files' response IDs, removed from the list, and a
`SetBreakpointsRequest` is re-sent with the remaining breakpoints for that file. Function
breakpoints work the same way with `SetFunctionBreakpointsRequest`.

When breakpoints are set in `idle` state (before launch), they are stored in a pending
buffer and flushed during the `InitializedEvent` handler in the launch sequence.

**Execution Control:**

| Tool | Parameters | Description |
|------|-----------|-------------|
| `continue` | `thread_id` (number, optional) | Resume execution until next breakpoint or exit |
| `step_over` | `thread_id` (number, optional), `granularity` (string: "line"\|"instruction", optional, default "line") | Step over to next line or instruction |
| `step_into` | `thread_id` (number, optional), `granularity` (string: "line"\|"instruction", optional, default "line") | Step into function call or next instruction |
| `step_out` | `thread_id` (number, optional) | Step out of current function |
| `pause` | — | Pause a running process |

**Inspection:**

| Tool | Parameters | Description |
|------|-----------|-------------|
| `status` | — | Get current session state, program name/PID, stop location (from cached frame mapping), breakpoint summary. Valid in any state. Uses only cached session data — no live DAP calls. |
| `backtrace` | `thread_id` (number, optional), `levels` (number, optional, default 20) | Get stack trace |
| `threads` | — | List all threads with status |
| `variables` | `frame_index` (number, optional, default 0), `scope` (string: "local"\|"global"\|"register", optional, default "local"), `depth` (number, optional, default 2), `filter` (string, optional, substring match on variable name) | Get variables in scope. Global scope defaults to depth 1. Max 100 variables returned. |
| `evaluate` | `expression` (string, required), `frame_index` (number, optional, default 0) | Evaluate an expression in frame context |
| `read_memory` | `address` (string, required, hex address), `count` (number, required, bytes to read) | Read raw memory at an address. Wraps DAP ReadMemory. |
| `disassemble` | `address` (string, optional, hex address), `instruction_count` (number, optional, default 20) | Disassemble instructions at address or current PC. Wraps DAP Disassemble. |
| `read_output` | — | Read buffered stdout/stderr from the target process |

**Escape Hatch:**

| Tool | Parameters | Description |
|------|-----------|-------------|
| `run_command` | `command` (string, required) | Run an arbitrary LLDB command via the evaluate/repl interface |

#### Session State Machine

```
             launch/attach
  [idle] ──────────────────► [configuring]
                                  │
                        configurationDone
                                  │
                                  ▼
                  ┌──────── [stopped] ◄───── StoppedEvent
                  │              │
            variables,      continue,
            evaluate,       step_*
            backtrace           │
                  │              ▼
                  └──────── [running] ─────► [stopped]
                                │                │
                          TerminatedEvent    ExitedEvent
                                │                │
                                ▼                ▼
                           [terminated] ──► [idle]
                              disconnect
```

Tools are only allowed in certain states:
- `status`: any state (always valid)
- `launch`, `attach`: only in `idle`
- `set_breakpoint`, `set_function_breakpoint`, `remove_breakpoint`, `list_breakpoints`: `idle` (pending buffer) or `stopped`
- `continue`, `step_*`: only in `stopped`
- `pause`: only in `running`
- `backtrace`, `threads`, `variables`, `evaluate`, `run_command`, `read_memory`, `disassemble`: only in `stopped`
- `read_output`: any state after launch
- `disconnect`: any state except `idle`

The `configuring` state is internal — it spans from `launch`/`attach` call through
`configurationDone`. No external tool calls are processed during this window; the
`launch`/`attach` tool handler drives the entire handshake synchronously and returns
only after configuration is complete.

## Design Decisions

### Decision 1: DAP Client is Custom (not a library)

**Context:** `google/go-dap` provides types and framing but no client implementation. We need a DAP client that correlates requests to responses and dispatches events.

**Options Considered:**
1. Build a custom DAP client in this project
2. Find or fork an existing Go DAP client library

**Decision:** Build a custom client.

**Rationale:** No general-purpose Go DAP client exists. The Delve project has a DAP *server*, not client. The client is straightforward (~200 lines): a read loop goroutine, a `pending map[int]chan dap.Message`, an event channel, and a `Send(request) → response` method. Building it avoids taking on an unnecessary dependency.

### Decision 2: Blocking Tool Calls for Execution Control (not async tasks)

**Context:** `continue` and `step_*` tools send a DAP request that returns immediately, but the actual stop happens asynchronously via a `StoppedEvent`. The MCP tool call must wait for that event.

**Options Considered:**
1. Use mcp-go's `AddTaskTool` for async polling
2. Block the MCP tool handler synchronously until the `StoppedEvent` arrives (with context timeout)

**Decision:** Blocking synchronous handlers with context cancellation.

**Rationale:** MCP task polling adds complexity for both the server and the AI agent client. In practice, `continue` to next breakpoint is usually fast (sub-second). For longer waits, Go's `context.Context` provides cancellation. The tool handler blocks on a Go channel: `select { case event := <-stoppedChan: ... case <-ctx.Done(): return timeout error }`. This is simpler for both implementation and the AI agent's workflow. If a tool times out, the agent can call `pause` to interrupt, then inspect state.

### Decision 3: `--repl-mode=command` with version-aware fallback

**Context:** The `run_command` tool uses DAP's `evaluate` request with `context: "repl"` to execute arbitrary LLDB commands. In `auto` mode, lldb-dap uses heuristics to decide if input is an expression or a command, which can misfire.

**Options Considered:**
1. Use `--repl-mode=auto` (default) and prefix LLDB commands with backtick
2. Use `--repl-mode=command` so all repl-context evaluations are LLDB commands
3. Use `--repl-mode=variable` and handle commands separately

**Decision:** Use `--repl-mode=command` when the binary is `lldb-dap` (LLVM 18+). Fall back to backtick-prefixing for older `lldb-vscode` binaries.

**Rationale:** The `--repl-mode` flag was introduced mid-2023 and is reliably present in LLVM 18+ (where the binary was renamed to `lldb-dap`). Older `lldb-vscode` binaries (LLVM 17 and earlier) may not support the flag, and LLVM's `cl::opt` parser will fail with an unrecognized-option error, causing the subprocess to exit immediately.

The `evaluate` tool (for expression evaluation) omits the `context` field (or uses `context: "variables"`), which lldb-dap treats as a standard expression evaluation regardless of `--repl-mode`. The `run_command` tool uses `context: "repl"`.

Version-aware behavior:
- **`lldb-dap` binary found:** Start with `--repl-mode=command`. The `run_command` tool sends commands without any prefix.
- **`lldb-vscode` binary found:** Start without `--repl-mode`. The `run_command` tool prepends a backtick (`` ` ``) to each command before sending, which forces LLDB command interpretation in `auto` mode.
- The Session Manager tracks which mode is active via a `replModeCommand bool` flag set at subprocess spawn time. The `run_command` tool handler checks this flag to decide whether to backtick-prefix.

### Decision 4: Single Debug Session

**Context:** The server could support multiple concurrent debug sessions or a single session at a time.

**Options Considered:**
1. Multi-session with session IDs passed to every tool
2. Single session — one lldb-dap subprocess at a time

**Decision:** Single session.

**Rationale:** An AI agent debugging workflow is inherently serial — you're debugging one problem at a time. Multiple sessions add parameter complexity to every tool call (session_id) without practical benefit. The session manager can be extended later if needed.

### Decision 5: Lazy lldb-dap Subprocess Start

**Context:** When should the lldb-dap subprocess be spawned?

**Options Considered:**
1. Spawn on MCP server startup
2. Spawn on first `launch`/`attach` tool call

**Decision:** Spawn on first `launch`/`attach`.

**Rationale:** The subprocess consumes resources. If the MCP server is registered but not used for debugging, there's no reason to have lldb-dap running. The `launch`/`attach` tool handler spawns the subprocess, runs the DAP initialization handshake, then proceeds with the launch/attach.

### Decision 6: Auto-detect lldb-dap Binary

**Context:** The binary name varies: `lldb-dap` (LLVM 18+), `lldb-vscode` (older), version-suffixed variants (`lldb-dap-18`), and `xcrun lldb-dap` on macOS.

**Options Considered:**
1. Require the user to specify the path
2. Auto-detect with fallback chain
3. Both: auto-detect with an environment variable override

**Decision:** Auto-detect with `LLDB_DAP_PATH` environment variable override.

**Rationale:** Good defaults with escape hatch. Detection order:
1. `LLDB_DAP_PATH` environment variable (if set)
2. `lldb-dap` in PATH
3. `lldb-dap-<version>` for versions 20 down to 15 in PATH
4. `lldb-vscode` in PATH
5. macOS only: `xcrun lldb-dap`

### Decision 7: Output Event Buffering

**Context:** The target process's stdout/stderr arrives as DAP `OutputEvent`s at unpredictable times. The MCP server needs a strategy to make this output available.

**Options Considered:**
1. Include output in the next tool call response
2. Buffer and expose via a `read_output` tool
3. Both — include recent output in stop-event responses AND expose a separate tool

**Decision:** Option 3 — both.

**Rationale:** When the process stops at a breakpoint, the agent often wants to see what the program printed. Including buffered output in `continue`/`step_*` responses gives immediate context. The `read_output` tool provides access at any time. The buffer is cleared on read.

The `OutputBuffer` is mutex-protected (concurrent writes from the read loop goroutine, concurrent reads from tool handlers via `Drain()`). The buffer is capped at 1MB total; if a program produces more output between drains, the oldest entries are dropped and a `[output truncated]` marker is prepended to the next drain.

### Decision 8: Variable Depth Limiting

**Context:** DAP variable inspection is recursive — structs contain structs, which contain more structs. Unbounded recursion produces huge responses.

**Options Considered:**
1. Fixed depth limit (e.g., 2 levels)
2. Configurable depth parameter on the `variables` tool
3. Flat list with a "has children" indicator

**Decision:** Configurable depth with default of 2.

**Rationale:** Depth 2 covers `struct.field` and `struct.field.subfield` which handles most debugging scenarios. The agent can request deeper inspection when needed. Leaf variables with `variablesReference > 0` include a `"has_children": true` marker so the agent knows more data is available.

## Error Handling

**lldb-dap subprocess crashes:**
- The read loop detects EOF on stdout.
- `cancelAllPending(err)` drains the pending map, sending an error sentinel to every waiting channel. This unblocks any in-flight tool call (including `launch` during handshake, `evaluate`, etc.).
- `stopWaiter.Cancel()` unblocks any `continue`/`step_*` tool waiting for a `StoppedEvent`.
- The subprocess exit code and stderr output are captured and included in error messages.
- Session state transitions to `terminated`.
- Subsequent tool calls return an error indicating the debugger session ended unexpectedly.
- The agent can call `launch`/`attach` to start a new session.

**lldb-dap stderr:**
- The subprocess is spawned with `cmd.Stderr` piped to a goroutine that drains it into a ring buffer (last 4KB). This prevents pipe buffer deadlocks if lldb-dap writes diagnostic output. The stderr content is included in crash/error reports.

**DAP request errors:**
- DAP responses include `success: bool` and `message: string` on failure.
- These are returned as MCP tool error results with the DAP error message.

**Target process crashes:**
- lldb-dap sends a `StoppedEvent` with `reason: "exception"`.
- The `continue`/`step_*` tool returns the exception info including signal, description, and current location.

**Target process exits:**
- lldb-dap sends an `ExitedEvent` with `exitCode`, followed by `TerminatedEvent`.
- The read loop calls `stopWaiter.DeliverExit(exitCode)` on `ExitedEvent`, unblocking any waiting `continue`/`step_*` tool with the exit code.
- `TerminatedEvent` sets the session to `terminated` and calls `stopWaiter.Cancel()` as a fallback (in case `ExitedEvent` was missed).
- The `continue`/`step_*` tool handler checks `StopResult.Exited` and **must call `session.SetState(terminated)`** before returning the exit response. This ensures subsequent inspection tools see the correct state.
- Subsequent inspection tools return an error — the process has exited.

**Invalid state transitions:**
- Each tool checks session state before acting.
- Calling `continue` in `idle` state returns: `"Error: no debug session active. Use 'launch' or 'attach' first."`
- Calling `variables` in `running` state returns: `"Error: process is running. Use 'pause' first."`

**lldb-dap not found:**
- `launch`/`attach` returns a clear error listing which paths were searched and suggesting the user install LLDB or set `LLDB_DAP_PATH`.

**Timeouts:**
- `continue` and `step_*` tools respect context cancellation.
- If the MCP client disconnects, the Go context is cancelled, which triggers `DisconnectRequest` to lldb-dap.

## Testing Strategy

**Unit tests:**
- DAP client message framing: round-trip encode/decode of all message types used
- Request/response correlation: verify pending map + channel dispatch with mock reader
- Session state machine: verify allowed/disallowed transitions
- lldb-dap binary detection: test search order with mock PATH
- Variable tree flattening: test with nested mock DAP variable responses
- Output buffering: verify append/drain semantics

**Integration tests** (build tag: `//go:build integration`):
- Test fixtures in `testdata/` compiled with `gcc -g -O0 -fno-omit-frame-pointer`
- Fixtures compiled by `TestMain` or a Makefile before test runs
- Per-test timeout of 30 seconds (`go test -timeout 30s`)
- Launch via MCP `launch` tool, verify session enters `stopped` state
- Set breakpoint, continue, verify stop location
- Inspect variables, evaluate expressions
- Step over/into/out, verify location changes
- `run_command` for arbitrary LLDB commands
- Attach to a running process (if test environment allows)
- Process crash handling (dereference NULL, verify exception stop)
- Process exit handling (verify exit code)
- Disconnect and re-launch
- lldb-dap crash recovery (kill subprocess mid-session, verify clean error)

**Platform tests:**
- Linux: verify with distro-packaged lldb-dap
- macOS: verify with Xcode's lldb-dap via xcrun

### Structural Verification

Per Go conventions:
- `go vet ./...` on every change
- `go test -race ./...` — the DAP client uses goroutines, channels, and a shared pending map, making race detection essential
- `staticcheck ./...` if available

## Migration / Rollout

This is a greenfield project — no migration needed.

**Rollout plan:**
1. Implement core (DAP client, session manager, 3-4 essential tools)
2. Test with a simple C program on Linux
3. Add remaining tools iteratively
4. Test on macOS
5. Document `lldb-dap` installation requirements per platform
6. Publish as a standalone binary with MCP server configuration instructions

**MCP client configuration (Claude Code example):**
```json
{
  "mcpServers": {
    "lldb-debug": {
      "command": "/path/to/lldb-debug-mcp",
      "args": [],
      "env": {
        "LLDB_DAP_PATH": "/usr/bin/lldb-dap"
      }
    }
  }
}
```
