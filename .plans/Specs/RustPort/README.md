---
title: "Rust Port — Feature-Parity Rewrite with Pluggable Debugger Backend"
type: spec
status: implemented
created: 2026-06-02
updated: 2026-06-03
tags: [rust, rmcp, lldb, dap, debugging, parity, port, windbg]
related: [Designs/LLDBDebugMCP, Plans/LLDBDebugMCP, Brainstorm/lldb-mcp-architecture.md]
---

# Rust Port — Feature-Parity Rewrite with Pluggable Debugger Backend

## Overview

Re-implement the existing Go MCP server (`lldb-debug-mcp`) in Rust using the
[`rmcp`](https://crates.io/crates/rmcp) (official Rust MCP SDK) crate. The Rust
server MUST be **feature-identical** to the Go version at the level of observable
behavior: the same 21 MCP tools, the same parameters and defaults, the same
session state machine, the same DAP handshake, the same response shapes, and the
same error semantics.

The *motivation* for the rewrite is to make the debugger backend **pluggable**.
Today the only backend is `lldb-dap` driven over the Debug Adapter Protocol
(DAP) on stdio. The Rust version introduces a `DebuggerBackend` trait so that a
future WinDbg backend can be added without touching the MCP tool layer. In this
spec, **only the lldb-dap/DAP backend is implemented**; WinDbg is out of scope but
the abstraction must be sufficient to support it.

```
AI Agent ←stdio/MCP(rmcp)→ [Rust MCP Server]
                                  │
                          [Tool Handlers]         ← debugger-neutral
                                  │
                          [Session Manager]       ← debugger-neutral
                                  │
                       [DebuggerBackend trait]    ← the new seam
                                  │
                    ┌─────────────┴───────────────┐
                    │  DapBackend (lldb-dap)       │  (this version)
                    │  WinDbgBackend               │  (future, out of scope)
                    └──────────────────────────────┘
```

**Source of truth.** The Go implementation is the authority for every behavior in
this spec. Where this document cites `file:line`, it refers to the Go tree at the
time of writing. Reviewers and implementers MUST treat the Go code and its tests
as the parity oracle. Key files: `cmd/lldb-debug-mcp/main.go`,
`internal/tools/*.go`, `internal/session/session.go`, `internal/dap/*.go`,
`internal/detect/*.go`, and all `*_test.go`.

**Parity decisions (from product owner):**
- **Backend seam:** introduce the `DebuggerBackend` trait now; lldb-dap is the
  only implementation.
- **JSON parity:** structural/semantic — identical field names, types, presence
  rules, and values. Object key ordering and whitespace MAY differ.
- **Intent over transcription:** the goal is feature parity at the level of
  *intended* behavior. Where a Go behavior is an artifact of the language
  (map key ordering, slice-vs-`nil`, returning an internal map by reference) the
  Rust version MAY adapt it as long as observable behavior is unchanged. Genuine
  code/doc discrepancies (not language artifacts) are captured in
  [Open Questions](#open-questions) for explicit decision, not silently changed.

## Goals

- Deliver a Rust binary that an MCP client can use interchangeably with the Go
  binary: same tool names, parameters, defaults, response fields, and errors.
- Preserve the full session state machine and all state-guard error messages.
- Preserve the DAP handshake (initialize → launch/attach with order-independent
  `InitializedEvent`, breakpoint flush, exception breakpoints, configurationDone)
  and crash/EOF recovery semantics.
- Preserve the exact behavior of the variable-flattening algorithm, the hex-dump
  formatter, the output buffer, and breakpoint tracking.
- Introduce a `DebuggerBackend` trait that the tool/session layers depend on,
  with a single DAP/lldb-dap implementation, sufficient to host a future WinDbg
  backend unchanged above the seam.
- Port the test suite: every Go unit test and integration scenario has a Rust
  equivalent that pins the same behavior.

## Non-Goals

- Implementing a WinDbg backend (or any backend other than lldb-dap/DAP).
- Adding new tools, parameters, or capabilities beyond the Go feature set.
- Multi-session support (the server remains single-session).
- Changing the MCP transport (remains stdio).
- Byte-for-byte JSON output parity (sorted keys / exact whitespace are not required).
- Reproducing Go-language incidental artifacts that have no observable effect
  (see the [Language-Difference Determinations](#appendix-a--language-difference-determinations) appendix).
- Performance optimization beyond "no worse than the Go version for interactive use."

## Requirements

### Functional Requirements

#### FR-1 — Server bootstrap & transport

1. The server MUST register with MCP name `"debug"` and version `"1.0.0"`. The Go
   server used `"lldb-debug"` (`main.go:14`, `server.NewMCPServer("lldb-debug",
   "1.0.0", ...)`); the Rust port renames it to the backend-neutral `"debug"` — an
   intentional deviation from the parity oracle (see [Resolved Decisions](#resolved-decisions)).
   (The DAP-handshake `clientID` sent to lldb-dap stays `"lldb-debug-mcp"` per FR-4.4.8 —
   that is an lldb-dap-facing identifier below the seam, not the MCP server name.)
2. Transport MUST be stdio (Go: `server.ServeStdio`). Requests are read from
   stdin; responses written to stdout.
3. Tool-capabilities `listChanged` MUST be advertised as `false`
   (Go: `WithToolCapabilities(false)`).
4. On a fatal server error the process MUST print `Server error: <err>` to
   **stderr** and exit with code `1` (Go: `main.go:25-26`).
5. The server MUST read no CLI flags. The only environment variable consulted is
   `LLDB_DAP_PATH` (see FR-15), read lazily at launch/attach time, not at startup.
6. The lldb-dap subprocess MUST be spawned lazily on the first `launch`/`attach`,
   never at server startup.
7. Tool dispatch MUST allow concurrency: while a blocking tool (`continue`,
   `step_*`) is waiting for a stop, a separate `pause` (or `status`) call MUST be
   able to execute. (Go relies on mcp-go's worker pool; the Rust server MUST not
   serialize all tool calls onto a single task such that `pause` cannot interrupt
   a blocked `continue`.)

#### FR-2 — Tool inventory (exactly 21 tools)

The server MUST register exactly these tools, with these names and descriptions
(verbatim). No more, no fewer.

| # | Name | Description (verbatim) |
|---|------|------------------------|
| 1 | `launch` | `Launch a program under the debugger` |
| 2 | `attach` | `Attach the debugger to a running process` |
| 3 | `disconnect` | `Disconnect from the debug session` |
| 4 | `set_breakpoint` | `Set a source-line breakpoint` |
| 5 | `set_function_breakpoint` | `Set a breakpoint on a function by name` |
| 6 | `remove_breakpoint` | `Remove a breakpoint by ID` |
| 7 | `list_breakpoints` | `List all current breakpoints` |
| 8 | `continue` | `Continue execution of the paused program` |
| 9 | `step_over` | `Step over the current line or instruction` |
| 10 | `step_into` | `Step into the current line or instruction` |
| 11 | `step_out` | `Step out of the current function` |
| 12 | `pause` | `Pause all threads in the running program` |
| 13 | `status` | `Get the current debug session status` |
| 14 | `backtrace` | `Get the call stack for a thread` |
| 15 | `threads` | `List all threads in the debugged process` |
| 16 | `variables` | `List variables in the current scope` |
| 17 | `evaluate` | `Evaluate an expression in the debugger` |
| 18 | `read_output` | `Read captured program output (stdout, stderr, console)` |
| 19 | `read_memory` | `Read raw memory at a given address` |
| 20 | `disassemble` | `Disassemble instructions at an address or the current PC` |
| 21 | `run_command` | `Run an LLDB command directly via the debug console` |

#### FR-3 — Tool result & error conventions

1. Every tool handler MUST return a successful MCP tool result whose single text
   content is a JSON object string, **except** the two documented plain-text
   early-exits (FR-4.8, FR-5.7) and error results.
2. User/operational errors MUST be returned as MCP **error** results (Go:
   `mcp.NewToolResultError`, which sets `IsError = true`) — NOT as protocol-level
   errors. The Go handlers never return a non-nil transport error in any tested
   path; the Rust handlers MUST likewise surface domain errors via the tool
   result, not via the `Result<_, Error>` transport channel.
3. Numeric arguments arrive as JSON numbers (Go reads them as `float64`). The
   Rust port MUST accept JSON numbers and coerce to integers where the Go code
   does `int(...)`.
4. A missing required string/number parameter MUST produce the error text
   `missing required parameter: <detail>`, where `<detail>` is the underlying
   validation message (Go uses mcp-go `RequireString`/`RequireInt`). Only the
   prefix `missing required parameter:` is literal; `<detail>` is whatever the
   parameter extractor reports (e.g. `missing required parameter: required
   argument "program" not found`). The Go tests assert only the substring
   `missing required parameter`, so parity here is the **prefix**, not the tail.
5. On JSON marshal failure of a success payload, the error text MUST be
   `failed to marshal result: <err>` (and `failed to marshal output: <err>` for
   `read_output` specifically). These paths are effectively unreachable in Rust
   with `serde`, but the contract is recorded for completeness.
6. JSON parity is structural (per product decision): same keys, same value types,
   same presence/omission rules. A field that Go omits (Go `omitempty` or a
   conditional map insert) MUST be absent in Rust; a field Go always includes
   MUST always be present.

#### FR-4 — State machine & guards

1. The session has exactly five states with these string renderings
   (Go `session.go:21-45`):
   `idle`, `configuring`, `stopped`, `running`, `terminated`. An unknown state
   renders as `unknown(<n>)`.
2. State guards are read-only checks (no transition validation). The guard helper
   (Go `CheckState(allowed...)`) MUST produce these exact error strings:
   - Current state `idle`: `no debug session active. Use 'launch' or 'attach' first.`
   - Current state `running`: `process is running. Use 'pause' first.`
   - Otherwise: `invalid state: <current>, expected one of: <a, b, ...>` where the
     list is the allowed states' string names joined by `", "`. State names are
     **unquoted**, e.g. `invalid state: running, expected one of: idle, stopped`
     (Go `session.go:218`).
3. Per-tool guards (allowed states):

   | Tool | Allowed states |
   |------|----------------|
   | `status` | any state (no guard) |
   | `list_breakpoints` | any state (no guard) |
   | `launch`, `attach` | `idle` only |
   | `disconnect` | any state **except** `idle` |
   | `set_breakpoint`, `set_function_breakpoint` | `idle` (pending buffer) or `stopped` |
   | `remove_breakpoint` | `stopped` only |
   | `continue`, `step_over`, `step_into`, `step_out` | `stopped` only |
   | `pause` | `running` only |
   | `backtrace`, `threads`, `variables`, `evaluate`, `read_memory`, `disassemble`, `run_command` | `stopped` only |
   | `read_output` | any state **except** `idle` |

4. State transitions occur only at the points the Go code performs them: `launch`/
   `attach` set `configuring` then `stopped` or `running`; `continue`/`step_*` set
   `running` then (via the stop result) `stopped`/`terminated`; a stopped/exited/
   terminated event drives the corresponding state; `disconnect` resets to `idle`;
   `pause` does **not** change state.

#### FR-4 (cont.) — `launch` tool

1. Parameters:

   | name | type | required | default | notes |
   |------|------|----------|---------|-------|
   | `program` | string | yes | — | description `Path to the executable to debug` |
   | `args` | string | no | unset | JSON **array string**, e.g. `["--flag","value"]`; description `JSON array of command-line arguments` |
   | `cwd` | string | no | `""` | description `Working directory for the launched program` |
   | `env` | string | no | unset | JSON **object string**, e.g. `{"KEY":"value"}`; description `JSON object of environment variables` |
   | `stop_on_entry` | boolean | no | `true` | description `Stop at program entry point (default true)` |

2. Guard: `idle` only.
3. Validation error strings (verbatim):
   - missing `program`: `missing required parameter: <err>`
   - `args` present but not a string: `'args' must be a JSON array string, e.g. '["--flag", "value"]'`
   - `args` JSON parse failure: `failed to parse 'args' as JSON array: <err>`
   - `env` present but not a string: `'env' must be a JSON object string, e.g. '{"KEY": "value"}'`
   - `env` JSON parse failure: `failed to parse 'env' as JSON object: <err>`
4. Handshake order (MUST be exactly this sequence — Go `launch.go:75-324`):
   1. set state `configuring`;
   2. detect lldb-dap (FR-15) → on failure reset session, error `failed to find lldb-dap: <err>`;
   3. spawn subprocess (FR-16) → on failure reset session, error `failed to spawn lldb-dap: <err>`;
   4. record subprocess and the repl-mode flag (`replModeCommand = isLLDBDAP`);
   5. create the DAP client over the subprocess stdio;
   6. **register event callbacks BEFORE starting the read loop** (output → buffer,
      stopped → cache last-stopped-event, exit → record exit code, terminated →
      set state `terminated`);
   7. start the read loop;
   8. send `InitializeRequest` and wait for the response. Arguments:
      `clientID="lldb-debug-mcp"`, `adapterID="lldb-dap"`, `pathFormat="path"`,
      `linesStartAt1=true`, `columnsStartAt1=true`, `supportsVariableType=true`,
      `supportsRunInTerminalRequest=false`. Errors (each performs subprocess
      cleanup): `initialize request failed: <err>`,
      `unexpected initialize response type: <type>`, `initialize failed: <message>`;
   9. build launch arguments (FR-17) and send `LaunchRequest`. Then **wait for BOTH
      the launch response AND the `InitializedEvent`, in either order** (order is
      version-dependent across lldb-dap releases). Errors: `launch request failed:
      <err>`, `launch timed out: <err>` (on context cancellation),
      `unexpected launch response type: <type>`, `launch failed: <message>`;
   10. flush pending breakpoints (FR-7.5): for each source file send a
       `SetBreakpointsRequest` with the file's full breakpoint list; if any
       function breakpoints are pending send one `SetFunctionBreakpointsRequest`.
       Record each resulting breakpoint (id, type, file/line or function, verified,
       condition). Errors mirror the breakpoint-tool error strings
       (`setBreakpoints failed for <file>: <…>`, etc.);
   11. send `SetExceptionBreakpointsRequest` with empty filters (even when there
       are none). Error: `setExceptionBreakpoints failed: <err>`;
   12. if `stop_on_entry`, register the stop waiter **before** configurationDone;
   13. send `ConfigurationDoneRequest`. Error: `configurationDone failed: <err>`;
   14. record program path and PID.
5. Success when `stop_on_entry=true`: block on the stop waiter.
   - If the result indicates the program exited/terminated during launch: set state
     `terminated` and return the **plain text** `Program exited during launch`.
   - Otherwise set state `stopped` and return JSON
     `{"status":"launched","program":<program>,"pid":<pid>,"state":"stopped"}`,
     plus `stop_reason` and `stopped_thread_id` when a stopped event is present.
   - Context cancellation: cleanup subprocess, error
     `timed out waiting for stop on entry: <err>`.
6. Success when `stop_on_entry=false`: set state `running`, return JSON
   `{"status":"launched","program":<program>,"pid":<pid>,"state":"running"}`.
7. The `stopOnEntry` field is omitted from the DAP launch arguments when `false`
   (Go `omitempty`). The Rust serialization MUST omit it when false (this affects
   what lldb-dap receives, so it is intentional, not incidental).
8. `Program exited during launch` is plain text, not JSON (see FR-3.1 exception).

#### FR-5 — `attach` tool

1. Parameters: `pid` (number, optional, `Process ID to attach to`), `wait_for`
   (string, optional, `Process name to wait for`). At least one is required;
   `pid` takes precedence over `wait_for`.
2. Guard: `idle` only.
3. Validation error strings (verbatim):
   - neither given: `either 'pid' or 'wait_for' must be provided`
   - `pid` present but not a number: `'pid' must be a number`
   - `pid <= 0`: `'pid' must be a positive integer`
   - `wait_for` present but empty: `'wait_for' must be a non-empty string`
4. Handshake mirrors `launch` steps 1–8 with identical error strings, then sends
   an `AttachRequest` and waits for BOTH the attach response and `InitializedEvent`
   (order-independent). Attach arguments always set `stopOnEntry=true`; set `pid`
   when `pid>0`, otherwise set `waitFor=true` and `program=<wait_for name>`.
   Errors: `attach request failed: <err>`, `attach timed out: <err>`,
   `unexpected attach response type: <type>`, `attach failed: <message>`.
5. `attach` does **NOT** flush pending breakpoints (unlike `launch`). It sends
   `SetExceptionBreakpointsRequest` (empty filters) then `ConfigurationDoneRequest`,
   with the same error strings as launch.
6. The program label is `pid:<n>` when attaching by PID, otherwise the wait-for
   name. PID is the supplied pid when `pid>0`, otherwise the subprocess PID.
7. `attach` always behaves as stop-on-entry: register the stop waiter, block, and:
   - on exit/terminate: set state `terminated`, return **plain text**
     `Process exited during attach`;
   - otherwise: set state `stopped`, return JSON
     `{"status":"attached","program":<label>,"pid":<pid>,"state":"stopped"}`
     plus `stop_reason`/`stopped_thread_id` when a stopped event is present;
   - context cancellation: cleanup, error `timed out waiting for stop on entry: <err>`.

#### FR-6 — `disconnect` tool

1. Parameter: `terminate` (boolean, optional, default `true`, description
   `Terminate the debuggee (default true)`).
2. Guard: any state except `idle` (idle → the no-session error).
3. If a DAP client exists, send `DisconnectRequest{terminateDebuggee=terminate}`
   under a **5-second** timeout; **ignore any error**. Then cancel the stop waiter.
4. If a subprocess exists, close its stdin, then wait for **graceful** exit in the
   background; if it has not exited within **5 seconds**, kill it and drain the
   wait. The two 5-second timeouts are sequential: the disconnect-request timeout
   bounds the DAP graceful-shutdown attempt, then the subprocess-exit timeout
   bounds waiting for the process to actually exit before force-killing.
5. Reset the session to the idle baseline (clears client, subprocess, program,
   pid, exit code, last-stopped-event, repl-mode flag, frame mapping, all
   breakpoint tracking, and the output buffer).
6. `disconnect` MUST always succeed once past the guard, returning JSON
   `{"status":"disconnected"}`. There is no error path other than the idle guard.

#### FR-7 — Breakpoint tools & tracking

1. `set_breakpoint` parameters: `file` (string, required, `Source file path`),
   `line` (number, required, `Line number`), `condition` (string, optional,
   default `""`, `Conditional expression for the breakpoint`). Guard: `idle` or
   `stopped`.
   - In `idle` (pending mode): buffer the breakpoint and return JSON
     `{"status":"pending","file":<file>,"line":<line>,"condition":<condition>,
     "message":"Breakpoint will be set when program is launched"}`. No DAP sent.
   - In `stopped`: append to the file's tracked list, send one
     `SetBreakpointsRequest` for that file. Select the response breakpoint whose
     `line` equals the requested line; if none matches, use the **last** breakpoint
     in the response; if the response has none, error
     `setBreakpoints response contained no breakpoints`. Record it and return JSON
     `{"breakpoint_id":<id>,"verified":<bool>,"file":<file>,"line":<matched line>}`
     plus `message` when the matched breakpoint carries a non-empty message.
   - Error strings: `setBreakpoints request failed: <err>`,
     `unexpected setBreakpoints response type: <type>`,
     `setBreakpoints failed: <message>`.
2. `set_function_breakpoint` parameters: `name` (string, required, `Function name`),
   `condition` (string, optional, `Conditional expression for the breakpoint`).
   Guard: `idle` or `stopped`.
   - In `idle`: buffer and return JSON
     `{"status":"pending","function":<name>,"condition":<condition>,
     "message":"Function breakpoint will be set when program is launched"}`.
   - In `stopped`: append to the function-breakpoint list, send one
     `SetFunctionBreakpointsRequest` with the full list, take the **last** response
     breakpoint as the new one, record it, and return JSON
     `{"breakpoint_id":<id>,"verified":<bool>,"function":<name>,"message":<msg>}`.
     When the response message is empty, synthesize it: verified →
     `Breakpoint set on function '<name>'`; not verified →
     `Breakpoint on function '<name>' pending verification`.
   - Error strings: `setFunctionBreakpoints request failed: <err>`,
     `unexpected response type: <type>`, `setFunctionBreakpoints failed: <message>`.
3. `remove_breakpoint` parameter: `breakpoint_id` (number, required,
   `Breakpoint ID to remove`). Guard: `stopped` only.
   - Look up the tracked breakpoint by id; if absent, the underlying error is
     `breakpoint ID <id> not found`, surfaced as
     `failed to remove breakpoint: <err>`.
   - For a function breakpoint, re-send `SetFunctionBreakpointsRequest` with the
     remaining function breakpoints; for a source breakpoint, re-send
     `SetBreakpointsRequest` for the file with its remaining breakpoints. Error
     strings match the corresponding set-breakpoint errors.
   - Return JSON `{"removed":true,"breakpoint_id":<id>}`.
   - Removal matching: source breakpoints are matched by **line only** (first
     match removed); function breakpoints by **name only** (first match removed).
     "First match" means the first entry in the session's **active tracking list**
     (not DAP response order), so removal is deterministic. Then the
     response-tracking entry is deleted.
4. `list_breakpoints`: no parameters, no guard, no DAP. Returns JSON
   `{"breakpoints":[...],"count":<n>}`, where the list is **sorted ascending by id**
   and each entry always includes `id`, `type`, `verified`, and conditionally
   includes `file` (non-empty), `line` (>0), `function` (non-empty), `condition`
   (non-empty). The empty list MUST serialize as `[]`, not `null`.
5. Pending-breakpoint flush (used only by `launch`): pending source breakpoints
   (keyed by file) and pending function breakpoints are appended to the active
   tracking structures and the pending buffers cleared. Flushing twice MUST NOT
   duplicate active breakpoints (idempotent). Breakpoint IDs are assigned by the
   debugger (DAP responses), never by the server.

#### FR-8 — Execution tools

Common contract for `continue`, `step_over`, `step_into`, `step_out`:
1. Guard: `stopped` only.
2. Thread id resolution: default `1`; if a `thread_id` argument is present and
   numeric, use it; otherwise if a last-stopped event exists, use its thread id.
3. Register the stop waiter **before** sending the DAP request; set state
   `running`; send the request; on a send error revert state to `stopped` and
   return the send error; otherwise block on the stop waiter (or context
   cancellation).
4. Stop-result handling (`handleStopResult`) produces the response:
   - Stopped event: set state `stopped`, drain the output buffer, return JSON
     `{"status":"stopped","reason":<reason>,"thread_id":<tid>,
     "description":<desc>}` plus `hit_breakpoint_ids` when non-empty, then merge
     the formatted output entries (FR-12) into the object.
   - Exited: set state `terminated`, drain output, return JSON `{"status":"exited"}`
     plus `exit_code` when known, merged with formatted output.
   - Terminated: set state `terminated`, return JSON
     `{"status":"terminated","message":"Debug session ended"}` (no output drain).
   - Otherwise: error `unexpected stop result`.

Per-tool specifics:

| Tool | Extra params | DAP request | Send-error string | Timeout string |
|------|--------------|-------------|-------------------|----------------|
| `continue` | `thread_id` (number, opt, `Thread ID to continue (optional)`) | `ContinueRequest` | `continue request failed: <err>` | `continue timed out; process still running, use 'pause' to stop it` |
| `step_over` | `thread_id`; `granularity` (string, opt, enum `line`\|`instruction`, `Step granularity`) | `NextRequest` (+granularity) | `step over request failed: <err>` | `step over timed out; process still running, use 'pause' to stop it` |
| `step_into` | `thread_id`; `granularity` (enum `line`\|`instruction`) | `StepInRequest` (+granularity) | `step into request failed: <err>` | `step into timed out; process still running, use 'pause' to stop it` |
| `step_out` | `thread_id` (no granularity) | `StepOutRequest` | `step out request failed: <err>` | `step out timed out; process still running, use 'pause' to stop it` |

`granularity` is applied to the DAP request only when a non-empty value is given.

`pause`:
1. No parameters. Guard: `running` only.
2. Send `PauseRequest` for all threads (Go uses thread id `0`). Errors:
   `pause request failed: <err>`, `unexpected pause response type: <type>`,
   `pause failed: <message>`.
3. Does **not** change session state. Returns JSON
   `{"status":"pause_requested","message":"Pause request sent. The running
   continue/step operation will return when the process stops."}`. The blocked
   `continue`/`step_*` call returns when the resulting stop event arrives.

#### FR-9 — `status` tool

1. No parameters, no guard (valid in any state).
2. Returns JSON beginning with `{"state":<state string>}` and adding fields per
   state:
   - `idle`: `message="No active debug session"`.
   - `configuring`: `message="Debug session is being configured"`.
   - `stopped`: `program`, `pid`; plus `stop_reason`, `stopped_thread_id` when a
     last-stopped event exists; plus `stop_description` when the event text is
     non-empty; plus `hit_breakpoint_ids` when non-empty.
   - `running`: `program`, `pid`.
   - `terminated`: `program`; plus `exit_code` when known.
3. `status` MUST use only cached session data (no live DAP calls).

#### FR-10 — Inspection tools (`backtrace`, `threads`, `variables`, `evaluate`)

1. `backtrace` parameters: `thread_id` (number, opt, `Thread ID (uses stopped
   thread if omitted)`), `levels` (number, opt, default `20`, `Maximum number of
   stack frames to return`). Guard: `stopped`.
   - Thread id resolution: explicit `thread_id` wins; else the last-stopped
     thread; else `1`. `levels` overridden only when present and `> 0`.
   - Sends one `StackTraceRequest{startFrame=0, levels}`. Rebuilds and stores the
     frame mapping (frame index → frame id) for all returned frames.
   - Returns JSON `{"frames":[...],"total_frames":<n>,"thread_id":<tid>}`. Each
     frame always has `index`, `name`, `id`; adds `file` and `line` when source
     path is present; adds `address` when an instruction-pointer reference exists.
   - Errors: `stackTrace request failed: <err>`,
     `unexpected stackTrace response type: <type>`, `stackTrace failed: <message>`.
2. `threads`: no parameters. Guard: `stopped`. Sends one `ThreadsRequest`. Returns
   JSON `{"threads":[...],"count":<n>}` plus `stopped_thread_id` when one of the
   threads matches the last-stopped event. Each thread has `id`, `name`, and
   (for the stopped thread) `is_stopped=true` and `is_current=true`. Errors:
   `threads request failed: <err>`, `unexpected threads response type: <type>`,
   `threads failed: <message>`.
3. `variables` parameters: `frame_index` (number, opt, default `0`, `Stack frame
   index (default 0)`), `scope` (string, opt, default `local`, enum
   `local`\|`global`\|`register`, `Variable scope`), `depth` (number, opt,
   `Maximum depth for nested structures`), `filter` (string, opt, default `""`,
   `Filter variables by name pattern`). Guard: `stopped`.
   - Default depth is **2**, except **1** when `scope=global`. An explicit `depth`
     argument that is present and `>= 0` overrides (so explicit `0` is honored).
   - Frame resolution: look up the frame id in the cached frame mapping; on a miss,
     issue an implicit `StackTraceRequest{levels=20}` for the resolved thread,
     rebuild the mapping, and use it. Out of range →
     `frame index <n> out of range (stack has <m> frames)`; surfaced in the
     handler as `failed to resolve frame: <err>`.
   - Sends a `ScopesRequest`, matches the scope by case-insensitive name
     (`Locals`/`Local`, `Globals`/`Global`, `Registers`/`Register`), then runs the
     flattening algorithm (FR-11) with a hard cap of **100** variables.
   - Returns JSON `{"variables":[...],"count":<n>,"scope":<scope>,
     "truncated":<bool>}`.
   - Errors: `scopes request failed: <err>`,
     `unexpected scopes response type: <type>`, `scopes failed: <message>`,
     `scope '<scope>' not found in frame <n>`, `failed to fetch variables: <err>`,
     and the frame-resolution errors above.
4. `evaluate` parameters: `expression` (string, required, `Expression to
   evaluate`), `frame_index` (number, opt, default `0`, `Stack frame index for
   evaluation context`). Guard: `stopped`. Resolves the frame id (same as
   `variables`), sends an `EvaluateRequest{context="variables"}`, and returns JSON
   `{"result":<result>,"type":<type>}` plus `has_children=true` and
   `variables_reference=<ref>` when the result has children. Errors:
   `evaluate request failed: <err>`, `unexpected evaluate response type: <type>`,
   `evaluate failed: <message>`.

#### FR-11 — Variable flattening algorithm

The flattening function takes `(variables_reference, depth, max_count, filter)`
and returns `(flat_list, truncated)`. It MUST behave exactly as the Go
`FlattenVariables` (Go `variables_util.go`):

1. Fetch the variables for the reference (one `VariablesRequest`). Error strings:
   `variables request failed: <err>` (both for the send failure and a
   `!success` response), `unexpected variables response type: <type>`.
2. Each flat variable serializes as: `name` (always), `value` (always),
   `type` (omitted when empty), `has_children` (omitted when false),
   `children_count` (omitted when 0).
3. **Filter applies to the top level only**, as a case-insensitive substring match
   on the variable name. An empty filter matches all. Children of an included
   parent are always included regardless of the filter.
4. Names: top-level entries use the bare name; nested entries use a dotted path
   `parent.child.grandchild` built by concatenation at each level.
5. Variables (and children) are emitted in **the order the backend returns them**
   (DAP response order) — no sorting.
6. For each variable, in order:
   - If it has children (`variablesReference > 0`):
     - if `depth > 0`: emit the parent node (no `has_children` marker), then check
       the cap, then recurse with `depth-1`;
     - if `depth == 0`: emit the node with `has_children=true` and
       `children_count = named + indexed` children, then check the cap.
   - If it is a leaf: emit it, then check the cap.
7. The cap is checked after **every** emitted node. The counted length is the
   **total number of emitted flat entries** — parent/container nodes AND leaves,
   at every nesting level — not just leaves. When that length reaches `max_count`,
   return immediately with `truncated=true`. The result length never exceeds
   `max_count`.
8. A container node is always emitted **before** its expanded children.

#### FR-12 — Output capture & `read_output`

1. The session owns an output buffer that the DAP output-event callback appends to
   as `(category, text)` pairs.
2. The buffer is capped at **1,048,576 bytes** (1 MiB). The byte count of each
   entry is `len(category) + len(text)`. When the total **exceeds** the cap,
   oldest entries are dropped (FIFO) until at/under the cap, and a truncation flag
   is set.
3. Draining returns all entries and clears the buffer. If the truncation flag is
   set, a marker entry `(category="console", text="[output truncated]")` is
   **prepended** and the flag reset. Draining an empty, non-truncated buffer
   returns nothing. Draining is idempotent (a second drain returns nothing).
4. `read_output`: no parameters. Guard: any state except `idle`. Returns the
   formatted drained output (below). Marshal-error string is
   `failed to marshal output: <err>`.
5. Output formatting (`formatOutputEntries`, shared with execution responses):
   - Empty input → `{"output":"","count":0}`.
   - Non-empty → group entries by category into three buckets: `stdout`,
     `stderr`, and `console` (the `console` bucket receives every category that is
     not exactly `stdout` or `stderr`). The result always includes
     `count=<n entries>` and includes a `stdout`/`stderr`/`console` key only when
     that bucket has content. (Note: the non-empty form has no `output` key.)

#### FR-13 — `read_memory` & `disassemble`

1. `read_memory` parameters: `address` (string, required, `Memory address (hex
   string, e.g. 0x1000)`), `count` (number, required, `Number of bytes to read`).
   Guard: `stopped`.
   - If `address` does not begin with `0x`/`0X`, prepend `0x`.
   - Send `ReadMemoryRequest{memoryReference=address, count}`.
   - If the response data is empty: return JSON
     `{"address":<response address>,"bytes_read":0}` (no `hex_dump`).
   - Otherwise base64-decode the data (error `failed to decode memory data: <err>`),
     parse the normalized request address as hex (error `failed to parse address:
     <err>`), format a hex dump, and return JSON
     `{"address":<response address>,"bytes_read":<n>,"hex_dump":<dump>}`.
   - Errors: `readMemory request failed: <err>`,
     `unexpected readMemory response type: <type>`, `readMemory failed: <message>`.
   - **Hex dump format (exact):** 16 bytes per row. Each row:
     `0x%08x: ` (lowercase, 8-digit zero-padded address + colon + space), then 16
     byte columns where each present byte is `%02x ` (lowercase, 2 digits, trailing
     space) and each missing byte is three spaces, with **one extra space inserted
     before column 8** (the 8/8 group separator); then ` |`, then the ASCII gutter
     where printable bytes (`0x20..0x7e`) render as themselves and others as `.`,
     missing positions render as a single space; then `|`. Rows are separated by
     `\n` with no trailing newline. Empty data → empty string. (Verified against
     Go `formatHexDump` test vectors.)
2. `disassemble` parameters: `address` (string, opt, default `""` → current PC,
   `Start address (hex string, uses current PC if omitted)`), `instruction_count`
   (number, opt, `Number of instructions to disassemble`). Guard: `stopped`.
   - `instruction_count` default is **20** (the documented intent in the design
     doc and README). The Go *code* currently defaults to **10** (`memory.go:173`),
     which is treated as a latent bug; the Rust port aligns to intent. See the
     resolved note in [Decisions](#resolved-decisions). (If strict Go-code parity
     is later preferred, this flips to 10 in one place plus its parity test.)
   - When `address` is empty, resolve the current PC via a
     `StackTraceRequest{levels=1}` and use the top frame's instruction-pointer
     reference; if there is no frame or no IP reference, error
     `no instruction pointer available for current frame`.
   - Normalize `address` (and the current PC) with a `0x` prefix as in `read_memory`.
   - Send `DisassembleRequest{memoryReference=address, instructionCount}`. Returns
     JSON `{"instructions":[...],"count":<n>,"start_address":<normalized address>}`.
     Each instruction always has `address`, `instruction`; adds `bytes`, `symbol`,
     `file`+`line` when present; adds `is_current_pc=true` when the address matches
     the current PC.
   - Errors: `disassemble request failed: <err>`,
     `unexpected disassemble response type: <type>`, `disassemble failed: <message>`,
     plus the stackTrace errors for the current-PC path.

#### FR-14 — `run_command` (escape hatch)

1. Parameter: `command` (string, required, `LLDB command string to execute`).
   Guard: `stopped`.
2. The command is sent as an `EvaluateRequest{context="repl"}` (no frame id). When
   the repl-mode-command flag is **false** (legacy `lldb-vscode`, or before a
   successful lldb-dap launch), the command MUST be prefixed with a single
   backtick to force command interpretation. When the flag is true (lldb-dap
   launched with `--repl-mode=command`), no prefix is added.
3. Returns JSON `{"result":<result>,"type":<type>}` (no `has_children`). Errors:
   `run_command request failed: <err>`, `unexpected evaluate response type: <type>`,
   `command failed: <message>`.

#### FR-15 — lldb-dap detection

1. Detection MUST search in this order and return the first hit, along with a flag
   indicating whether the binary supports `--repl-mode=command`:
   1. `LLDB_DAP_PATH` env var — resolve via PATH lookup, else as an absolute path.
      The repl-mode-capable flag is set when the basename **contains** the
      substring `lldb-dap`.
   2. `lldb-dap` on PATH (flag true).
   3. `lldb-dap-<N>` on PATH for `N` from **20 down to 15** inclusive (prefers
      higher versions; flag true).
   4. `lldb-vscode` on PATH (flag **false**).
   5. macOS only: `xcrun --find lldb-dap` (flag true).
2. On no match, error `lldb-dap binary not found; searched: <comma-separated list
   of every candidate tried>`.

#### FR-16 — Subprocess management

1. Spawn the detected binary. Pass the single argument `--repl-mode=command`
   **only** when the repl-mode-capable flag is true; otherwise pass no arguments.
2. Wire stdin (DAP requests), stdout (DAP responses, buffered reader), and stderr
   (drained in the background to avoid pipe-buffer deadlock).
3. Capture the last **4096 bytes** of stderr in a ring buffer (keep-last-N
   semantics; a write larger than the capacity keeps only its last N bytes; writes
   never error and always report the full input length). The default ring size is
   4096 when constructed with a non-positive size.
4. Subprocess lifecycle (kill/wait/EOF detection) is driven by the session/
   disconnect logic and the DAP read loop reaching EOF, not by the spawn function.

#### FR-17 — DAP client behavior

1. **Framing:** DAP base protocol — `Content-Length: <N>\r\n\r\n` header followed
   by exactly N bytes of UTF-8 JSON. The Rust client MUST read and write this
   framing and decode messages into typed DAP values.
2. **Sequence numbers:** each outgoing request gets a unique, strictly increasing
   sequence number. The first is **1**. (The exact starting value is observable on
   the wire but lldb-dap only requires uniqueness/correlation; preserving start=1
   is acceptable and simplest.)
3. **Correlation:** responses are matched to requests by the response's
   `request_seq` against the seq used when sending. Pending requests are tracked in
   a map keyed by request seq; each waiter is a single-capacity channel/oneshot so
   dispatch never blocks. On write failure the pending entry is rolled back.
4. **Send (blocking):** assign seq, register the waiter, write the framed request,
   then await the response or context cancellation. There is no internal timeout —
   timeouts come from the caller's context/deadline. On cancellation the pending
   entry is removed.
5. **Send-and-await-both:** the launch/attach flows MUST be able to await a request
   response and the `InitializedEvent` concurrently, in any order. (Go wraps the
   blocking send in a goroutine and selects over the response channel, the
   initialized channel, and the context; the Rust port may use any equivalent —
   e.g. `tokio::select!` over two futures.)
6. **Read loop:** continuously read messages and dispatch by type:
   - `StoppedEvent`: invoke the on-stopped callback (which caches the event), then
     deliver to the stop waiter.
   - `InitializedEvent`: signal the single-capacity initialized channel
     (non-blocking; a second signal is dropped).
   - `OutputEvent`: invoke the output callback (appends to the buffer).
   - `ExitedEvent`: invoke the on-exit callback (records the exit code), then
     deliver an "exited" stop result with the exit code.
   - `TerminatedEvent`: invoke the on-terminated callback (sets state terminated),
     then cancel the stop waiter.
   - `ThreadEvent`, `BreakpointEvent`, `ProcessEvent`, `ContinuedEvent`: log only —
     no callback, no stop-waiter effect.
   - Any response message: dispatch to the pending waiter (no-op with a log if no
     waiter is registered for that seq; MUST NOT panic).
   - Any other message: log as unhandled.
7. **EOF / read error:** on any read error (including EOF), in order: record the
   error and mark the client closed exactly once; cancel all pending requests with
   a wrapped "read loop terminated" error (so every blocked request unblocks with
   an error); cancel the stop waiter (so a blocked `continue`/`step_*` unblocks as
   "terminated"); invoke the on-terminated callback (so the session transitions to
   `terminated` even when the subprocess was killed externally); then exit the loop.
8. **Stop waiter:** a single-waiter primitive. `Register` returns a fresh
   single-capacity channel, replacing any previous waiter. The producers
   (`Deliver` a stopped event, `DeliverExit` with an exit code, `Cancel` →
   terminated) each no-op when no waiter is registered and clear the waiter after
   delivering, so exactly one result is delivered per registration. The stop result
   carries exactly one of: a stopped event, an exited flag + optional exit code, or
   a terminated flag.
9. **Backend launch arguments** are serialized with these JSON field names and
   omission rules (these reach lldb-dap and are intentional):
   - launch: `program` (always), and `args`, `cwd`, `env`, `stopOnEntry`,
     `initCommands`, `preRunCommands`, `postRunCommands`, `stopCommands`,
     `exitCommands`, `terminateCommands` — each omitted when empty/false;
   - attach: `pid`, `program`, `waitFor`, `stopOnEntry`, `attachCommands`,
     `coreFile` — each omitted when empty/false.

#### FR-18 — `DebuggerBackend` abstraction (the seam)

1. The MCP tool handlers and the session manager MUST depend on a debugger-neutral
   trait, `DebuggerBackend` (name not normative), rather than on a DAP client type
   directly. The DAP/lldb-dap implementation is one concrete type behind this trait.
2. The trait MUST be sufficient to express **every** operation the current tool set
   needs, so a future WinDbg backend can implement it without changes above the
   seam. At minimum it MUST cover:
   - lifecycle: initialize, launch (with the launch arguments of FR-4),
     attach (FR-5), configuration-done, disconnect/terminate;
   - breakpoints: set source breakpoints for a file, set function breakpoints,
     set exception breakpoints;
   - execution: continue, step over/into/out (with granularity), pause;
   - inspection: threads, stack trace (with start frame / levels), scopes,
     variables (by reference), evaluate (with a context/mode distinction between
     expression evaluation and raw command/repl execution), read memory, disassemble;
   - an asynchronous **event stream** delivering at least: stopped (reason, thread,
     description, hit-breakpoint-ids), output (category, text), exited (exit code),
     terminated, and initialized;
   - a backend capability/flag equivalent to "supports raw command mode without
     backtick-prefixing" (the repl-mode flag) and a detection/spawn step that
     produces a ready backend instance.
3. Backend-neutral types: the trait's request/response/event types MUST be
   debugger-neutral (not raw DAP structs leaking through the seam), so the tool
   layer formats responses without knowing which backend produced them. (The DAP
   backend translates to/from these types.)
4. The DAP-specific concerns — wire framing, sequence correlation, the
   `InitializedEvent` ordering quirk, `--repl-mode=command`, lldb-dap detection —
   MUST live inside the DAP backend, below the seam.
5. This version ships exactly one backend (DAP/lldb-dap). No WinDbg code is
   required; the abstraction's adequacy for WinDbg is a review criterion, not a
   deliverable.
6. **Event/response payload policy (resolves OQ-2):** for this version the neutral
   types carry the current values **as opaque pass-through strings/ints** — they
   are NOT normalized into backend-specific enums. Concretely: stop `reason` stays
   a free-form string (`"breakpoint"`, `"exception"`, `"signal"`, `"step"`, …); a
   frame's instruction-pointer reference stays a string; a variable carries its
   `variables_reference`, `named`/`indexed` child counts as integers. This keeps
   the tool-layer output identical to Go. Normalizing these into neutral enums is
   deferred until the WinDbg backend lands (it can introduce a mapping then).
7. **Indicative trait surface** (names/signatures are not normative; they fix the
   *shape* and neutrality the design must satisfy). The neutral types are plain
   data the DAP backend translates to/from DAP wire types:

   ```rust
   // Neutral data (no DAP structs leak above the seam):
   struct StopInfo { reason: String, thread_id: i64, description: String,
                     hit_breakpoint_ids: Vec<i64> }
   enum BackendEvent { Initialized, Stopped(StopInfo),
                       Output { category: String, text: String },
                       Exited { code: i64 }, Terminated }
   struct Frame { index: i64, id: i64, name: String,
                  source_path: Option<String>, line: i64,
                  instruction_pointer: Option<String> }
   struct Variable { name: String, value: String, ty: String,
                     variables_reference: i64, named: i64, indexed: i64 }
   struct BreakpointResult { id: i64, verified: bool, line: i64, message: String }
   struct EvalResult { result: String, ty: String, variables_reference: i64 }
   struct MemoryReadResult { address: String, data: Vec<u8> }
   struct Instruction { address: String, instruction: String, bytes: String,
                        symbol: String, source_path: Option<String>, line: i64 }

   #[async_trait]
   trait DebuggerBackend: Send + Sync {
       // lifecycle
       async fn initialize(&self) -> Result<()>;
       async fn launch(&self, args: LaunchArgs) -> Result<()>;
       async fn attach(&self, args: AttachArgs) -> Result<()>;
       async fn configuration_done(&self) -> Result<()>;
       async fn disconnect(&self, terminate: bool) -> Result<()>;
       // breakpoints
       async fn set_source_breakpoints(&self, file: &str, bps: &[SourceBp])
           -> Result<Vec<BreakpointResult>>;
       async fn set_function_breakpoints(&self, bps: &[FunctionBp])
           -> Result<Vec<BreakpointResult>>;
       async fn set_exception_breakpoints(&self, filters: &[String]) -> Result<()>;
       // execution
       async fn cont(&self, thread_id: i64) -> Result<()>;
       async fn step_over(&self, thread_id: i64, gran: Option<Granularity>) -> Result<()>;
       async fn step_into(&self, thread_id: i64, gran: Option<Granularity>) -> Result<()>;
       async fn step_out(&self, thread_id: i64) -> Result<()>;
       async fn pause(&self) -> Result<()>;
       // inspection
       async fn threads(&self) -> Result<Vec<ThreadInfo>>;
       async fn stack_trace(&self, thread_id: i64, start: i64, levels: i64)
           -> Result<Vec<Frame>>;
       async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>>;
       async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>>;
       async fn evaluate(&self, expr: &str, frame_id: Option<i64>, mode: EvalMode)
           -> Result<EvalResult>;                 // EvalMode = Expression | Repl
       async fn read_memory(&self, address: &str, count: i64) -> Result<MemoryReadResult>;
       async fn disassemble(&self, address: &str, count: i64) -> Result<Vec<Instruction>>;
       // capability + events
       fn supports_command_repl_mode(&self) -> bool; // the repl-mode flag
       fn events(&self) -> EventStream<BackendEvent>; // async stream of BackendEvent
   }
   ```

   The DAP backend owns wire framing, seq correlation, the `InitializedEvent`
   ordering quirk, `--repl-mode=command`, lldb-dap detection/spawn, and the
   translation between these neutral types and DAP messages. `EvalMode::Repl`
   is where the backtick-prefix decision (FR-14.2) lives — the tool layer asks for
   "repl/command execution" and the backend applies the prefix when
   `supports_command_repl_mode()` is false.

### Non-Functional Requirements

1. **Language/runtime:** Rust (stable toolchain), `rmcp` for the MCP server,
   `serde`/`serde_json` for JSON, and an async runtime (e.g. `tokio`) for the
   subprocess, read loop, and concurrent tool dispatch. A DAP types crate may be
   used or DAP types may be defined locally; either is acceptable provided the
   wire behavior matches.
2. **Parity oracle:** the Go tests are the behavioral oracle. The Rust port MUST
   replicate every behavior the Go tests pin (FR sections above enumerate them).
3. **JSON parity:** structural/semantic (FR-3.6). Field presence/omission MUST
   match; key order and whitespace need not.
4. **Concurrency safety:** shared session/DAP state MUST be safe under concurrent
   tool dispatch (Go validates with `-race`; Rust gets this from the type system,
   but the design MUST still permit a concurrent `pause` during a blocked
   `continue`). The Rust build MUST be warning-clean and pass `cargo clippy`
   without suppressions.
5. **Platforms:** Linux and macOS MUST work (matching Go). macOS detection
   includes the `xcrun` fallback. Windows is not required in this version (it
   becomes relevant only with the future WinDbg backend).
6. **Single session:** at most one active debug session/subprocess at a time.
7. **No new runtime dependencies on the user beyond what Go required:** an
   `lldb-dap`/`lldb-vscode` binary at runtime; a C compiler only for building test
   fixtures.
8. **Performance:** interactive latency MUST be comparable to the Go version; no
   added per-call timeouts beyond those Go uses (5 s disconnect; per-tool blocking
   bounded only by the MCP client's context).

## User Stories

- As an **AI agent**, I want the Rust server to expose the same tool names and
  parameters as the Go server, so my existing debugging workflows work unchanged
  after swapping the binary.
- As an **AI agent**, I want `continue` to block until the next stop and return the
  stop reason, location, and any program output, so I can reason about the program
  without extra round-trips.
- As a **maintainer**, I want a `DebuggerBackend` trait with the lldb-dap logic
  behind it, so I can later add a WinDbg backend without rewriting the tool layer.
- As a **maintainer**, I want a Rust test suite that pins the same behaviors as the
  Go tests, so I can prove parity and catch regressions.
- As a **user on macOS**, I want auto-detection to find `lldb-dap` via `xcrun`,
  so the server works against Xcode's toolchain without configuration.

## Acceptance Criteria

Functional parity:
- [ ] The server registers exactly the 21 tools of FR-2 with the verbatim names and
      descriptions.
- [ ] Every tool's parameters, defaults, enums, and state guards match FR-4…FR-14.
- [ ] Every error string enumerated in this spec is produced verbatim on the
      corresponding failure path.
- [ ] `launch` performs the FR-4.4 handshake and succeeds with the order-independent
      `InitializedEvent`/launch-response wait; `stop_on_entry` true/false both behave
      as specified, including the plain-text `Program exited during launch` early-exit.
- [ ] `attach` behaves per FR-5, including not flushing pending breakpoints and the
      plain-text `Process exited during attach` early-exit.
- [ ] `disconnect` always succeeds past the guard, applies the two 5-second timeouts,
      and resets the session to idle.
- [ ] Breakpoint tools behave per FR-7, including pending mode, matched-breakpoint
      selection, id-sorted `list_breakpoints`, `[]`-not-`null` empty list, and
      line-only/name-only removal matching.
- [ ] Execution tools behave per FR-8, including stop-waiter-before-send ordering,
      state revert on send error, output merging into stop responses, and `pause`
      not changing state.
- [ ] `variables` honors the dynamic depth default (2; 1 for global), explicit
      `depth=0`, the 100-variable cap (counting container nodes), top-level-only
      case-insensitive substring filtering, dotted nested names, and the
      `has_children`/`children_count` markers — verified against the Go
      `FlattenVariables` test vectors.
- [ ] `read_memory` produces hex dumps byte-identical to the Go `formatHexDump`
      test vectors (full row, partial row, multi-row, empty).
- [ ] `read_output` and the execution-response output merge group categories per
      FR-12, with the `[output truncated]` marker behavior and 1 MiB cap.
- [ ] `run_command` backtick-prefixes when the repl-mode flag is false and not when
      true; uses `context="repl"`.
- [ ] lldb-dap detection follows the FR-15 order (including the 20→15 versioned
      search and macOS `xcrun`) and the not-found error lists all searched candidates.
- [ ] The DAP client matches FR-17: framing, seq correlation, the full event
      dispatch table, EOF recovery (all pending unblocked + stop waiter cancelled +
      on-terminated fired), and the single-waiter stop primitive.

Architecture:
- [ ] The tool handlers and session manager depend only on the `DebuggerBackend`
      trait (FR-18); no DAP types leak above the seam.
- [ ] The DAP/lldb-dap backend is the only implementation and lives entirely below
      the seam.
- [ ] A reviewer confirms the trait could host a WinDbg backend (no DAP-only
      assumptions in the trait surface).

Testing & quality:
- [ ] Every Go unit test has a Rust equivalent pinning the same behavior:
      state machine & guards; breakpoint tracking (add/remove/list/pending/flush);
      output buffer append/drain/truncation/concurrency; detection order & not-found;
      stderr ring buffer; DAP send/correlate/concurrent/cancel; read-loop per-event;
      stop waiter (deliver/exit/cancel/no-waiter/concurrent); DAP framing round-trip
      and malformed-input errors; variable flattening (all listed cases); hex dump
      formatting; `formatOutputEntries`; every tool's state-guard and
      missing-parameter cases.
- [ ] Integration tests (behind a feature flag analogous to the Go `integration`
      build tag) reproduce the Go scenarios against the same C fixtures
      (`simple.c`, `loop.c`, `crash.c`, plus `structs.c`/`multithread.c` for future
      use): process exit + exit code, stdout capture, crash/signal stop with a
      backtrace frame at `crash.c:7`, lldb-dap crash recovery (kill subprocess →
      terminated → relaunch), crash during a blocked `continue` (returns within the
      bound, no hang), and the full 13-step end-to-end workflow (breakpoints at
      `loop.c:6` and `:9`, continue, backtrace finds `main`, variables include `i`
      and `sum`, three step-overs, evaluate `i + 1`, `run_command` `register read`,
      hit second breakpoint within 20 continues, remove first breakpoint, list shows
      1, continue to exit 0).
- [ ] `cargo test` is green; `cargo clippy` is clean with no `#[allow]`
      suppressions; the build emits no warnings.

## Constraints

- `rmcp` is the mandated MCP framework. Its tool-registration and stdio-serving
  APIs differ from mcp-go; the **observable** MCP behavior (tool names, params,
  result/error shapes) MUST match regardless of API differences.
- The lldb-dap/`lldb-vscode` runtime dependency and its version quirks
  (`--repl-mode=command` only on lldb-dap; backtick fallback on lldb-vscode) are
  inherited unchanged.
- The `InitializedEvent`-vs-launch-response ordering varies across lldb-dap
  versions and MUST be handled order-independently (do not assume one order).
- Single session, stdio transport, and lazy subprocess start are fixed
  architectural constraints carried over from Go.

## Dependencies

- `rmcp` — Rust MCP SDK (server, stdio transport, tool registration).
- `serde` / `serde_json` — JSON (de)serialization.
- An async runtime (`tokio` or equivalent) — subprocess management, read loop,
  concurrent tool dispatch.
- A DAP types source — an existing crate or locally-defined types; must match the
  wire format go-dap produces/consumes.
- `lldb-dap` (LLVM 18+) or `lldb-vscode` (older) at runtime.
- `gcc`/`clang` for compiling integration-test fixtures.

## Resolved Decisions

- **Server identity rename → `debug` (intentional deviation).** The published binary is
  renamed `lldb-debug-mcp` → `debug-mcp` and the advertised MCP server name
  `lldb-debug` → `debug`, reflecting that backends are now pluggable (lldb is one of
  several possible). The `lldb` prefix is reserved for genuinely lldb-bound pieces
  (the lldb backend crate/types and lldb-dap detection). This is the second intentional
  deviation from the Go oracle (after OQ-1); it changes the server's protocol identity,
  so MCP clients that namespace tools by server name update that key. The DAP `clientID`
  remains `"lldb-debug-mcp"` (below the seam).
- **OQ-1 — `disassemble` default `instruction_count` → 20 (intent).** The Go code
  defaults to **10** (`memory.go:173`); the design doc and README document **20**.
  Per the product steer ("aim for intent"), the Rust port uses **20** and treats
  the Go code value as a latent bug (the Go code and docs should be reconciled to
  20 as a follow-up). This is the **only** intentional behavioral deviation from
  the current Go code in this spec; it is isolated to one default and its parity
  test, and is trivially reversible to 10 if strict code-parity is later chosen.
- **OQ-2 — Backend event/response payloads → opaque pass-through.** Resolved in
  FR-18.6: neutral types carry current values as opaque strings/ints, not
  normalized enums, preserving exact tool output. Normalization is deferred to the
  WinDbg backend.
- **Numeric-validation policy → minimal tool-boundary guards (intentional deviation).**
  Go is permissive (coerces `float64 → int` and forwards clearly-invalid values to
  lldb-dap). Because the tool surface is exposed to agents, the Rust port rejects a
  *minimal* set of clearly-invalid values at the boundary with predictable errors, while
  keeping Go's truncation for valid values (and adding no caps): `read_memory` `count`
  must be a positive integer (`'count' must be a positive integer`); an explicit, numeric
  `thread_id` on `continue`/`step_*`/`backtrace` must be positive
  (`'thread_id' must be a positive integer`) — an absent/non-numeric `thread_id` still
  falls back to last-stopped → `1` (Go parity); `set_breakpoint` `line` must be a positive
  integer after truncation (`'line' must be a positive integer`). This is the third
  intentional deviation (after the server rename and OQ-1).
- **Breakpoint mutation is now transactional (robustness; success-path parity preserved).**
  The stopped-state breakpoint handlers (`set_breakpoint`, `set_function_breakpoint`,
  `remove_breakpoint`) build the proposed breakpoint list locally and commit the session's
  tracked state only after lldb-dap confirms the change. A backend rejection therefore
  leaves the tracked lists unchanged (the Go order mutated first, then re-sent, leaving
  stale/untracked entries on failure). This is an error-path robustness improvement only —
  the success-path observable behavior is identical to the Go oracle.

## Open Questions

- **OQ-3 — Repl-mode flag default semantics.** In Go the flag defaults to `false`,
  so `run_command` backtick-prefixes before a successful lldb-dap launch; in
  practice a successful lldb-dap launch sets it true. Confirm the Rust default is
  likewise "false until a repl-mode-capable backend is launched," so behavior is
  identical. (Believed to be intent; flagged for confirmation.)
- **OQ-4 — `xcrun` detection on non-macOS.** The Go code only attempts `xcrun` on
  macOS. Confirm the Rust port gates this on the target/host OS identically (no
  `xcrun` attempt on Linux).
- **OQ-5 — Module/crate layout.** Not behavior-affecting, but worth settling in
  design: where the seam, the DAP backend, the session manager, and the tool
  handlers live as crates/modules.

---

## Appendix A — Language-Difference Determinations

Per the product decision to favor *intent* over literal transcription, these Go
behaviors are judged **incidental** (language artifacts) and MAY be adapted in
Rust without breaking parity, because observable behavior is unchanged:

| Go behavior | Classification | Rust determination |
|-------------|----------------|--------------------|
| `encoding/json` marshals map keys in sorted order | Incidental | Use `serde`; key order need not match (structural parity chosen). |
| `FrameMapping()` returns the internal map by reference (aliasing hazard) | Incidental | Return an owned/cloned map; no observable difference. |
| `nil` vs empty slices (`functionBreakpoints`, `pendingFunctionBPs` start `nil`) | Incidental | Use empty `Vec`; identical observable behavior. |
| Launch/attach use a blocking `Send` wrapped in a goroutine to await response + event | Incidental | Use `tokio::select!`/futures over two awaits; same order-independent wait. |
| `StopWaiter` uses a buffered(1) channel; `pending` uses `map[int]chan` | Incidental | Use `oneshot`/`mpsc` of capacity 1 and a `HashMap<i64, oneshot::Sender>`. |
| seq is a mutex-guarded counter starting at 1 | Incidental (start value) | Any unique increasing seq is fine; preserving start=1 is acceptable and simplest. |
| Goroutine-drained stderr ring buffer | Incidental | Use an async task / shared buffer; keep the 4096-byte keep-last-N semantics. |
| `mcp-go` worker pool enables concurrent `pause` during `continue` | **Behavioral** | MUST preserve: the Rust server must allow concurrent tool dispatch so `pause` can interrupt a blocked `continue`. |
| `stopOnEntry`/`args`/`env`/etc. `omitempty` in DAP args | **Behavioral** | MUST preserve: these reach lldb-dap; omit when empty/false. |
| Backtick-prefix when repl-mode flag is false | **Behavioral** | MUST preserve (legacy `lldb-vscode` support). |
| Hex-dump byte layout, 1 MiB output cap, 100-variable cap, depth defaults | **Behavioral** | MUST preserve exactly. |

Behaviors marked **Behavioral** are NOT incidental and MUST be reproduced exactly.

## Appendix B — C Test Fixtures (parity oracle)

Built with `gcc -g -O0 -fno-omit-frame-pointer` (`-pthread` for `multithread`):
- `simple.c` — prints `hello from simple\n`, returns 0. (exit/output tests)
- `loop.c` — `for (i=0;i<10;i++) sum += i;` with `sum += i;` at **line 6** and
  `printf("final sum=%d\n", sum);` at **line 9**; locals `i`, `sum`. (primary
  workflow fixture)
- `crash.c` — prints `about to crash\n`, then `*p = 42;` (NULL deref) at **line 7**.
  (crash/signal tests; backtrace must show `crash.c:7`)
- `structs.c` — nested `Person`/`Address` structs. (future structured-variable
  inspection; not yet referenced by tests)
- `multithread.c` — one pthread worker, joined. (future multithread scenarios;
  not yet referenced by tests)
