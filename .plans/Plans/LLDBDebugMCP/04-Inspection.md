---
title: "Inspection Tools"
type: phase
plan: "LLDBDebugMCP"
phase: 4
status: planned
created: 2026-03-07
updated: 2026-03-07
deliverable: "Full state inspection: backtrace, threads, variables with recursive flattening, expression evaluation, and arbitrary LLDB command execution via run_command"
tasks:
  - id: "4.1"
    title: "threads tool"
    status: planned
    verification: "Integration test with multithread.c: launch, set breakpoint after thread creation, continue. Verify threads response lists ≥2 threads with id and name. Single-threaded program returns 1 thread."
  - id: "4.2"
    title: "backtrace tool"
    status: planned
    verification: "Integration test: launch loop.c, set breakpoint inside a function called from main. Continue. Backtrace shows ≥2 frames: current function and main. Each frame has id, name, source file, line. Levels parameter limits depth. Thread_id parameter selects specific thread."
    depends_on: ["4.1"]
  - id: "4.3"
    title: "Variable tree flattening"
    status: planned
    verification: "Unit test: given mock DAP responses for Scopes→Variables→child Variables (3 levels deep), verify flattened output includes parent.child.grandchild names. Depth=1 stops at first level. Depth=2 includes children. Variables with children beyond depth limit have has_children=true marker. Max 100 variables enforced. Filter parameter filters by name substring."
  - id: "4.4"
    title: "variables tool"
    status: planned
    verification: "Integration test with loop.c (has int counter, maybe a struct): breakpoint in loop, continue. (1) variables with scope='local' returns loop counter with correct value and type. (2) scope='global' returns globals (if any). (3) scope='register' returns register names and values. (4) depth=0 returns only top-level names with has_children markers. (5) filter='counter' returns only matching variable."
    depends_on: ["4.2", "4.3"]
  - id: "4.5"
    title: "evaluate tool"
    status: planned
    verification: "Integration test: breakpoint in loop.c with int i. (1) evaluate('i') returns current value. (2) evaluate('i + 1') returns computed value. (3) evaluate('(char*)0') returns error about null pointer. (4) frame_index=1 evaluates in parent frame context."
    depends_on: ["4.2"]
  - id: "4.6"
    title: "run_command tool (escape hatch)"
    status: planned
    verification: "Integration test: (1) run_command('bt') returns backtrace text. (2) run_command('register read') returns register dump. (3) run_command('memory read &i') returns memory contents. (4) Invalid command returns error message from lldb-dap."
  - id: "4.7"
    title: "Structural verification"
    status: planned
    verification: "`go vet ./...` passes; `go test -race ./...` passes"
    depends_on: ["4.1", "4.2", "4.4", "4.5", "4.6"]
---

# Phase 4: Inspection Tools

## Overview

Implement the tools that let the agent examine program state: thread listing, stack traces, variable inspection with recursive flattening, expression evaluation, and the arbitrary LLDB command escape hatch.

## 4.1: threads tool

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Send `ThreadsRequest` → parse response
- [ ] Format: list of `{id, name, is_stopped}` for each thread
- [ ] Include which thread is the current stop thread (from last StoppedEvent's threadId)

## 4.2: backtrace tool

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Resolve thread_id: use parameter or default to last stopped thread
- [ ] Send `StackTraceRequest{ThreadId, StartFrame: 0, Levels: levels_param}`
- [ ] Format each frame: `#N name at file:line` (or `#N name at 0xaddr` if no source)
- [ ] Include frame IDs for use with variables/evaluate frame_index parameter
- [ ] Store frame IDs in session for frame_index → frameId mapping

### Notes
The `frame_index` parameter in other tools (variables, evaluate) is an index into the backtrace, not the DAP frameId. The session manager maintains a mapping from index → DAP frameId, refreshed on each backtrace call or stop event.

## 4.3: Variable tree flattening

### Subtasks
- [ ] Implement `FlattenVariables(client, variablesReference, depth, maxCount, filter) ([]Variable, error)`
- [ ] Recursive: for each variable, if `variablesReference > 0` and `depth > 0`, recurse
- [ ] Prefix child names with parent name: `parent.child.grandchild`
- [ ] If `variablesReference > 0` and `depth == 0`, set `has_children: true`
- [ ] Apply `filter` (case-insensitive substring match on variable name) at top level
- [ ] Enforce `maxCount` (default 100) — stop recursion when limit reached, set `truncated: true`
- [ ] Output struct: `{name, value, type, has_children, children_count}`

## 4.4: variables tool

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Resolve frame: use `frame_index` to look up DAP frameId from session mapping
- [ ] Send `ScopesRequest{FrameId}` → get scopes
- [ ] Select scope by `scope` parameter: "local" → first scope named "Locals" or "Local", "global" → "Globals", "register" → "Registers"
- [ ] Call `FlattenVariables` with the selected scope's `variablesReference`
- [ ] For "global" scope: default depth=1, not 2 (can be large)
- [ ] Format as JSON array of variable objects

## 4.5: evaluate tool

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Resolve frame: frame_index → DAP frameId
- [ ] Send `EvaluateRequest{Expression, FrameId, Context: "variables"}`
- [ ] Return: `{result, type, variables_reference}` — if variablesReference > 0, note "has children" for structured results
- [ ] On DAP error response: return the error message as tool error

## 4.6: run_command tool (escape hatch)

### Subtasks
- [ ] State guard: must be `stopped`
- [ ] Check `session.replModeCommand` flag
- [ ] If true (`--repl-mode=command`): send `EvaluateRequest{Expression: command, Context: "repl"}`
- [ ] If false (fallback): prepend backtick to command, send `EvaluateRequest{Expression: "`" + command, Context: "repl"}`
- [ ] Return the raw text output from lldb-dap
- [ ] On error: return the error message

### Notes
This is the most important tool for edge cases. If the structured tools don't cover something, the agent can always fall back to `run_command("memory read 0x7fff5000 -c 64")` or any other LLDB command.

## 4.7: Structural verification

### Subtasks
- [ ] `go vet ./...`
- [ ] `go test -race ./...`

## Acceptance Criteria
- [ ] Agent can: launch → set breakpoint → continue → backtrace → variables → evaluate → run_command
- [ ] Variable flattening handles nested structs with depth control
- [ ] Filter parameter narrows variable results
- [ ] Global scope doesn't produce enormous responses
- [ ] run_command successfully executes arbitrary LLDB commands
- [ ] All inspection tools return descriptive errors in wrong states
- [ ] Race detector clean
