---
title: "Foundation — DAP Client + Project Setup"
type: phase
plan: "LLDBDebugMCP"
phase: 1
status: complete
created: 2026-03-07
updated: 2026-03-07
deliverable: "Go module with a working DAP client that can spawn lldb-dap, perform the initialize handshake, and exchange messages with request/response correlation and async event dispatch"
tasks:
  - id: "1.1"
    title: "Go module init + dependencies"
    status: complete
    verification: "`go build ./...` succeeds; `go mod tidy` produces no changes; both mcp-go and go-dap are in go.mod"
  - id: "1.2"
    title: "DAP message framing and types"
    status: complete
    verification: "Round-trip test: encode a DAP request, decode it, verify fields match. Test with InitializeRequest, LaunchRequest, ContinueRequest, EvaluateRequest. Test malformed input returns error."
    depends_on: ["1.1"]
  - id: "1.3"
    title: "DAP client — send/receive with pending map"
    status: complete
    verification: "Unit test: mock reader feeds 3 responses out-of-order (by request_seq); verify each Send() call receives the correct response. Test concurrent Send() calls from multiple goroutines. Race detector clean (`go test -race`)."
    depends_on: ["1.2"]
  - id: "1.4"
    title: "DAP client — read loop + event dispatch"
    status: complete
    verification: "Unit test: mock reader feeds interleaved responses and events (StoppedEvent, OutputEvent, InitializedEvent, ExitedEvent, TerminatedEvent). Verify responses go to pending map, events go to event channels. Test ExitedEvent calls stopWaiter.DeliverExit, TerminatedEvent calls stopWaiter.Cancel. Test EOF triggers cancelAllPending + stopWaiter.Cancel — all waiting Send() calls return error."
    depends_on: ["1.3"]
  - id: "1.5"
    title: "StopWaiter implementation"
    status: complete
    verification: "Unit test: Register() then Deliver() — waiter receives event. Register() then Cancel() — waiter receives terminated sentinel. Deliver() with no waiter registered — no panic, no block. Race detector clean."
    depends_on: ["1.4"]
  - id: "1.6"
    title: "lldb-dap binary detection"
    status: complete
    verification: "Unit test with mock PATH: finds `lldb-dap`, falls back to `lldb-dap-18`, falls back to `lldb-vscode`, respects `LLDB_DAP_PATH` override. Returns descriptive error when nothing found."
    depends_on: ["1.1"]
  - id: "1.7"
    title: "Subprocess management + stderr drain"
    status: complete
    verification: "Unit test: spawn a trivial subprocess (`echo`), verify stdin/stdout pipes work, stderr is drained to buffer. Test subprocess exit is detected (cmd.Wait). Test stderr buffer captures last 4KB."
    depends_on: ["1.6"]
  - id: "1.8"
    title: "Structural verification"
    status: complete
    verification: "`go vet ./...` passes; `go test -race ./...` passes; no data races in DAP client or StopWaiter"
    depends_on: ["1.3", "1.4", "1.5", "1.7"]
---

# Phase 1: Foundation — DAP Client + Project Setup

## Overview

Set up the Go project and implement the core DAP client that all subsequent phases depend on. This phase produces no MCP tools — it delivers the internal infrastructure layer.

## 1.1: Go module init + dependencies

### Subtasks
- [x] `go mod init github.com/danweinerdev/lldb-debug-mcp` (or appropriate module path)
- [x] `go get github.com/mark3labs/mcp-go`
- [x] `go get github.com/google/go-dap`
- [x] Create directory structure: `cmd/lldb-debug-mcp/`, `internal/dap/`, `internal/session/`, `internal/detect/`
- [x] Create minimal `cmd/lldb-debug-mcp/main.go` with placeholder

### Notes
Directory layout:
```
cmd/lldb-debug-mcp/main.go     — binary entrypoint
internal/dap/client.go          — DAP client
internal/dap/client_test.go     — DAP client tests
internal/dap/stopwaiter.go      — StopWaiter
internal/dap/stopwaiter_test.go
internal/dap/types.go           — lldb-dap-specific launch/attach arg structs
internal/session/session.go     — session manager (Phase 2)
internal/detect/detect.go       — lldb-dap binary detection
internal/detect/detect_test.go
```

## 1.2: DAP message framing and types

### Subtasks
- [x] Define `LLDBDAPLaunchArgs` struct with json tags (program, args, cwd, env, stopOnEntry, initCommands, preRunCommands, postRunCommands, stopCommands, exitCommands, terminateCommands)
- [x] Define `LLDBDAPAttachArgs` struct with json tags (pid, program, waitFor, stopOnEntry, attachCommands, coreFile)
- [x] Write round-trip encode/decode tests using `dap.WriteProtocolMessage` / `dap.ReadProtocolMessage`
- [x] Test that `LaunchRequest.Arguments` correctly marshals to `json.RawMessage` from `LLDBDAPLaunchArgs`

## 1.3: DAP client — send/receive with pending map

### Subtasks
- [x] Implement `Client` struct with `conn io.ReadWriteCloser`, `reader *bufio.Reader`, `mu sync.Mutex`, `seq int`, `pending map[int]chan dap.Message`
- [x] Implement `Send(request) (dap.Message, error)` — assigns sequence number, registers pending channel, writes message, blocks on channel
- [x] Implement `SendAsync(request) (<-chan dap.Message, error)` — same as Send but returns the channel instead of blocking (for launch handshake order-independence)
- [x] Implement `dispatch(msg)` — looks up `pending[requestSeq]`, sends to channel, removes entry
- [x] Write concurrent Send() unit test with a mock pipe

## 1.4: DAP client — read loop + event dispatch

### Subtasks
- [x] Implement `readLoop()` goroutine — reads messages, dispatches responses to pending map, events to typed channels
- [x] Define event channels: `initializedChan chan struct{}`, `outputChan chan *dap.OutputEvent`, `stoppedChan` (via StopWaiter)
- [x] Implement `cancelAllPending(err error)` — on EOF, send error to all pending channels
- [x] Handle type switch for all DAP message types we use (InitializeResponse, LaunchResponse, SetBreakpointsResponse, ConfigurationDoneResponse, ContinueResponse, NextResponse, StepInResponse, StepOutResponse, PauseResponse, ThreadsResponse, StackTraceResponse, ScopesResponse, VariablesResponse, EvaluateResponse, DisconnectResponse, ReadMemoryResponse, DisassembleResponse, StoppedEvent, InitializedEvent, TerminatedEvent, ExitedEvent, OutputEvent, ThreadEvent, BreakpointEvent, ProcessEvent, ContinuedEvent)
- [x] Test: mock reader with interleaved responses and events, verify correct dispatch
- [x] Test: mock reader returns EOF, verify all pending channels receive error

## 1.5: StopWaiter implementation

### Subtasks
- [x] Implement `StopWaiter` as specified in design (Register, Deliver, Cancel)
- [x] Define `StopResult` struct: `Event *dap.StoppedEvent`, `Exited bool`, `ExitCode *int`, `Terminated bool`, `Err error`
- [x] Implement `DeliverExit(exitCode int)` — sends `StopResult{Exited: true, ExitCode: &exitCode}`
- [x] Unit tests for all combinations: register→deliver, register→deliverExit, register→cancel, deliver with no waiter, concurrent access

## 1.6: lldb-dap binary detection

### Subtasks
- [x] Implement `FindLLDBDAP() (string, error)` in `internal/detect/`
- [x] Check `LLDB_DAP_PATH` env var first
- [x] Search PATH for `lldb-dap`, then `lldb-dap-{20..15}`, then `lldb-vscode`
- [x] On macOS (detected via `runtime.GOOS`): try `xcrun --find lldb-dap`
- [x] Return descriptive error with all paths searched on failure
- [x] Unit tests with `t.Setenv` to mock `PATH` and `LLDB_DAP_PATH`

## 1.7: Subprocess management + stderr drain

### Subtasks
- [x] Implement `SpawnLLDBDAP(path string) (*exec.Cmd, io.WriteCloser, *bufio.Reader, *StderrBuffer, error)`
- [x] Pipe stdin/stdout for DAP framing
- [x] Pipe stderr to a goroutine that drains into a ring buffer (last 4KB)
- [x] `StderrBuffer` type with `Write(p []byte)` and `String() string` methods
- [x] Test: spawn a real subprocess, verify pipe connectivity

## 1.8: Structural verification

### Subtasks
- [x] Run `go vet ./...`
- [x] Run `go test -race ./...`
- [x] Fix any issues found

## Acceptance Criteria
- [x] `go build ./...` succeeds with no errors
- [x] All unit tests pass with race detector enabled
- [x] DAP client can send a request and receive the correct response via mock pipes
- [x] StopWaiter delivers events without races
- [x] `FindLLDBDAP()` returns a valid path on the development machine
