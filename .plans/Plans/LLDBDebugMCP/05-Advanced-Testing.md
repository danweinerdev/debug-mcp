---
title: "Advanced Tools + Integration Testing"
type: phase
plan: "LLDBDebugMCP"
phase: 5
status: complete
created: 2026-03-07
updated: 2026-03-08
deliverable: "Complete tool surface with read_memory, disassemble. Full integration test suite. Error handling edge cases validated. Documentation."
tasks:
  - id: "5.1"
    title: "read_memory tool"
    status: complete
    verification: "Integration test: launch loop.c, set breakpoint, continue. Get address of a local variable via evaluate('&i'). Call read_memory with that address and count=4. Verify returned bytes match the variable's value. Test invalid address returns error."
  - id: "5.2"
    title: "disassemble tool"
    status: complete
    verification: "Integration test: breakpoint in loop.c, continue. (1) disassemble with no address — disassembles at current PC, returns ≥1 instruction with address, opcode, operands. (2) disassemble with explicit address returns instructions at that address. (3) instruction_count parameter limits output."
  - id: "5.3"
    title: "Crash handling"
    status: complete
    verification: "Integration test with crash.c (from testdata, created in 2.7): launch with no breakpoints, continue. Verify StoppedEvent with reason 'exception' at the crash location. Verify backtrace shows crash frame with file:line. Verify variables shows the null pointer. Verify run_command('bt') works in crashed state."
  - id: "5.4"
    title: "Process exit handling"
    status: complete
    verification: "Integration test: launch simple.c (returns 0), continue with no breakpoints. Verify response includes exit_code=0. Verify subsequent variables/backtrace calls return 'process has exited' error. Launch again works."
  - id: "5.5"
    title: "lldb-dap crash recovery"
    status: complete
    verification: "Integration test: launch a program, kill the lldb-dap subprocess (cmd.Process.Kill). Verify: (1) any blocked continue/step returns error. (2) status reports terminated. (3) launch works again to start fresh session."
  - id: "5.6"
    title: "End-to-end debugging workflow test"
    status: complete
    verification: "Integration test: full debugging scenario. Launch loop.c → set breakpoint in loop → continue to breakpoint → inspect variables (verify loop counter) → step over 3 times (verify counter increments) → evaluate an expression → run_command('register read') → set a second breakpoint → continue → hit second breakpoint → remove first breakpoint → list breakpoints shows only second → continue to exit → verify exit code. All in one test."
    depends_on: ["5.3", "5.4", "5.5"]
  - id: "5.7"
    title: "CLAUDE.md + MCP configuration docs"
    status: complete
    verification: "CLAUDE.md exists at project root with build instructions, architecture summary, and development guidelines. README.md (or similar) documents: installation, lldb-dap requirements per platform, MCP client configuration example, tool reference."
    depends_on: ["5.6"]
  - id: "5.8"
    title: "Final structural verification"
    status: complete
    verification: "`go vet ./...` clean. `go test -race ./...` all pass (unit + integration). `go build -o lldb-debug-mcp ./cmd/lldb-debug-mcp` produces working binary. Binary tested with Claude Code MCP config."
    depends_on: ["5.6", "5.7"]
---

# Phase 5: Advanced Tools + Integration Testing

## Overview

Complete the tool surface with memory and disassembly access, validate all error handling edge cases, run a comprehensive end-to-end integration test, and write documentation.

## 5.1: read_memory tool

### Subtasks
- [x] State guard: must be `stopped`
- [x] Parse `address` parameter as hex string (with or without `0x` prefix)
- [x] Send `ReadMemoryRequest{MemoryReference: address, Count: count}`
- [x] Decode base64 response data
- [x] Format as hex dump: `0xADDR: XX XX XX XX  XX XX XX XX  |ascii...|`
- [x] Return error for inaccessible memory

## 5.2: disassemble tool

### Subtasks
- [x] State guard: must be `stopped`
- [x] If no `address` parameter: get current PC from the top stack frame (StackTraceRequest, use frame's instructionPointerReference)
- [x] Send `DisassembleRequest{MemoryReference: address, InstructionCount: count}`
- [x] Format each instruction: `0xADDR:  opcode  operands   ; source if available`
- [x] Mark the current PC instruction with `→`

## 5.3: Crash handling

### Subtasks
- [x] Use existing `testdata/crash.c` (created in task 2.7)
- [x] Integration test: launch with no breakpoints, continue, verify stopped with reason "exception"
- [x] Verify backtrace includes crash frame with file:line
- [x] Verify variables are inspectable in crash state
- [x] Verify run_command works in crash state

## 5.4: Process exit handling

### Subtasks
- [x] Integration test: program exits normally, verify ExitedEvent → exit code
- [x] Verify continue tool returns exit info (not hangs)
- [x] Verify inspection tools return clear error after exit
- [x] Verify session transitions to terminated → idle on disconnect

## 5.5: lldb-dap crash recovery

### Subtasks
- [x] Integration test: kill lldb-dap subprocess mid-session
- [x] Verify cancelAllPending unblocks all waiters
- [x] Verify stopWaiter.Cancel unblocks continue
- [x] Verify session enters terminated state with descriptive error
- [x] Verify new launch works after crash recovery

## 5.6: End-to-end debugging workflow test

### Subtasks
- [x] Single integration test that exercises the complete workflow
- [x] Tests the realistic agent debugging scenario
- [x] Verifies tool outputs contain enough information for an AI agent to make decisions

## 5.7: CLAUDE.md + MCP configuration docs

### Subtasks
- [x] Create CLAUDE.md with: project purpose, build commands, test commands, architecture overview, code conventions
- [x] Document lldb-dap installation: Linux (`apt install lldb`), macOS (Xcode Command Line Tools), Fedora (`dnf install lldb`), Arch (`pacman -S lldb`)
- [x] Document MCP client configuration for Claude Code and Claude Desktop
- [x] Document `LLDB_DAP_PATH` environment variable
- [x] List all tools with brief descriptions

## 5.8: Final structural verification

### Subtasks
- [x] `go vet ./...`
- [x] `go test -race ./...` (all unit + integration tests)
- [x] `go build -o lldb-debug-mcp ./cmd/lldb-debug-mcp`
- [x] Manual smoke test: configure as MCP server in Claude Code, launch a test program, set breakpoint, continue, inspect variables

## Acceptance Criteria
- [x] All 18 tools implemented and tested
- [x] Error handling validated: crash, exit, timeout, invalid state, missing lldb-dap
- [x] End-to-end test passes
- [x] Binary builds and works as MCP server
- [x] Documentation complete
- [x] Race detector clean across all tests
