---
title: "Breakpoints + Execution Control"
type: phase
plan: "LLDBDebugMCP"
phase: 3
status: planned
created: 2026-03-07
updated: 2026-03-07
deliverable: "Breakpoint management (set, remove, list) and execution control (continue, step, pause) with output buffering. Can set a breakpoint, continue to it, step through code, and read program output."
tasks:
  - id: "3.1"
    title: "Breakpoint state tracking"
    status: planned
    verification: "Unit test: add breakpoints to multiple files, verify internal map state. Remove a breakpoint by ID, verify correct file's list is updated. List returns all tracked breakpoints. Function breakpoint list is maintained separately."
  - id: "3.2"
    title: "set_breakpoint tool"
    status: planned
    verification: "Integration test: launch loop.c with stop_on_entry. Set breakpoint at a known line inside the loop body. Verify response contains breakpoint ID and verified=true. Set breakpoint with condition (`i == 5`), verify condition is accepted."
    depends_on: ["3.1"]
  - id: "3.3"
    title: "set_function_breakpoint tool"
    status: planned
    verification: "Integration test: launch simple.c with stop_on_entry. Set function breakpoint on 'main'. Continue, verify stop at main. Verify breakpoint ID is returned."
    depends_on: ["3.1"]
  - id: "3.4"
    title: "remove_breakpoint + list_breakpoints tools"
    status: planned
    verification: "Integration test: set 2 breakpoints, list shows both. Remove one by ID, list shows only the remaining one. Remove the last one, list shows empty. Verify DAP SetBreakpointsRequest is re-sent with updated list."
    depends_on: ["3.2"]
  - id: "3.5"
    title: "Pending breakpoint buffer"
    status: planned
    verification: "Integration test: set breakpoints in idle state (before launch), then launch with stop_on_entry=false. Verify process stops at the pending breakpoint (breakpoints were flushed during InitializedEvent handler)."
    depends_on: ["3.2"]
  - id: "3.6"
    title: "continue tool"
    status: planned
    verification: "Integration test: launch loop.c, set breakpoint in loop body, continue. Verify: (1) ContinueRequest sent, (2) StopWaiter receives StoppedEvent, (3) response contains stop reason 'breakpoint', source file, line number, hit breakpoint IDs, (4) response contains `output` field with any buffered stdout/stderr. Test continue to exit: no breakpoints set, continue — response contains exit code."
    depends_on: ["3.2", "3.9"]
  - id: "3.7"
    title: "step_over, step_into, step_out tools"
    status: planned
    verification: "Integration test with loop.c: (1) step_over advances to next line, response shows new line number and includes buffered output. (2) step_into enters a function call (if fixture has one), response shows function's first line. (3) step_out returns to caller, response shows caller's line. (4) granularity='instruction' on step_over changes PC without advancing a full source line."
    depends_on: ["3.6", "3.9"]
  - id: "3.8"
    title: "pause tool"
    status: planned
    verification: "Integration test: launch loop.c with infinite loop (no breakpoints), let it run. Call pause from a separate goroutine. Verify StoppedEvent with reason 'pause' is received."
    depends_on: ["3.6"]
  - id: "3.9"
    title: "Output buffering + read_output tool"
    status: planned
    verification: "Integration test: launch a program that prints to stdout (e.g., loop.c with printf). Continue past the print. Verify: (1) continue response includes buffered output. (2) read_output returns any additional output. (3) read_output after drain returns empty. (4) Output is categorized by stdout/stderr."
    depends_on: ["3.6"]
  - id: "3.10"
    title: "Structural verification"
    status: planned
    verification: "`go vet ./...` passes; `go test -race ./...` passes; StopWaiter and breakpoint state have no data races"
    depends_on: ["3.2", "3.6", "3.7", "3.8", "3.9"]
---

# Phase 3: Breakpoints + Execution Control

## Overview

Implement the core debugging workflow: setting breakpoints, continuing execution, stepping through code, and pausing. This is the phase where the server becomes genuinely useful for debugging.

## 3.1: Breakpoint state tracking

### Subtasks
- [ ] Add `sourceBreakpoints map[string][]dap.SourceBreakpoint` to session manager (keyed by file path)
- [ ] Add `functionBreakpoints []dap.FunctionBreakpoint` to session manager
- [ ] Add `breakpointResponses map[int]BreakpointInfo` — maps DAP breakpoint ID → file path + line + condition (for remove/list)
- [ ] Implement `AddSourceBreakpoint(file, line, condition)` — appends to file's list
- [ ] Implement `RemoveBreakpointByID(id)` — finds file, removes from list, returns file for re-send
- [ ] Implement `ListBreakpoints()` — returns all tracked breakpoints with IDs

## 3.2: set_breakpoint tool

### Subtasks
- [ ] State guard: allow in `idle` (pending) or `stopped`
- [ ] If `idle`: add to pending buffer, return synthetic response (unverified)
- [ ] If `stopped`: add to state tracking, send `SetBreakpointsRequest` for the file (full list), parse `SetBreakpointsResponse` to get verified IDs
- [ ] Return: breakpoint_id, verified, source file, line, message

## 3.3: set_function_breakpoint tool

### Subtasks
- [ ] Same state guard pattern as set_breakpoint
- [ ] Append to function breakpoint list
- [ ] Send `SetFunctionBreakpointsRequest` (full list), parse response
- [ ] Return: breakpoint_id, verified, function name

## 3.4: remove_breakpoint + list_breakpoints tools

### Subtasks
- [ ] `remove_breakpoint`: look up ID in breakpointResponses, find source file or function list, remove, re-send DAP request with remaining list
- [ ] `list_breakpoints`: iterate breakpointResponses, format as table with ID, type (source/function), location, condition, verified status

## 3.5: Pending breakpoint buffer

### Subtasks
- [ ] Add `pendingSourceBPs map[string][]dap.SourceBreakpoint` and `pendingFunctionBPs []dap.FunctionBreakpoint` to session manager
- [ ] On `InitializedEvent` during launch: flush pending buffers by sending `SetBreakpointsRequest` for each file and `SetFunctionBreakpointsRequest`
- [ ] Move pending to active state tracking after flush
- [ ] Clear pending buffers

## 3.6: continue tool

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Register StopWaiter channel
- [ ] Set state to `running`
- [ ] Send `ContinueRequest` with thread_id (default: first thread from last stop)
- [ ] Block on StopWaiter channel with context cancellation
- [ ] On StoppedEvent: set state to `stopped`, return stop reason, location, source context, hit breakpoint IDs, buffered output
- [ ] On StopResult.Exited: call `session.SetState(terminated)` before returning, return exit code and buffered output
- [ ] On StopResult.Terminated: call `session.SetState(terminated)` before returning, return termination info
- [ ] On context cancellation: return timeout error (process still running, suggest `pause`)

## 3.7: step_over, step_into, step_out tools

### Subtasks
- [ ] Same pattern as continue: register StopWaiter, set running, send DAP request, wait
- [ ] `step_over`: send `NextRequest` with threadId and granularity
- [ ] `step_into`: send `StepInRequest` with threadId and granularity
- [ ] `step_out`: send `StepOutRequest` with threadId
- [ ] All return: new location (file:line or address), stop reason, buffered output

## 3.8: pause tool

### Subtasks
- [ ] State guard: must be `running`
- [ ] Send `PauseRequest` (no threadId — pauses all)
- [ ] StopWaiter will receive the StoppedEvent from the read loop
- [ ] Return success (the waiting continue/step tool will unblock separately)

### Notes
`pause` is typically called from a different MCP tool call than the one blocked on `continue`. mcp-go handles concurrent tool calls in separate goroutines, so this works.

## 3.9: Output buffering + read_output tool

### Subtasks
- [ ] Implement `OutputBuffer` with `Append(category, text)` and `Drain() []OutputEntry`
- [ ] `OutputEntry`: `{Category string, Text string}` — category is "stdout", "stderr", or "console"
- [ ] Read loop dispatches `OutputEvent` to buffer
- [ ] `continue`/`step_*` responses include `Drain()` result as `output` field
- [ ] `read_output` tool: drains buffer, formats as text grouped by category

## 3.10: Structural verification

### Subtasks
- [ ] `go vet ./...`
- [ ] `go test -race ./...`

## Acceptance Criteria
- [ ] Can set breakpoint → continue → hit breakpoint → inspect location
- [ ] Can step over/into/out and see location changes
- [ ] Can pause a running process
- [ ] Conditional breakpoints work
- [ ] Function breakpoints work
- [ ] Remove breakpoint removes only the targeted breakpoint
- [ ] Pending breakpoints (set before launch) are activated on launch
- [ ] Program output is captured and available via continue responses and read_output
- [ ] Race detector clean
