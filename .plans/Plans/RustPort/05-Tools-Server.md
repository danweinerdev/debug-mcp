---
title: "Tools + Server — mcp-tools + debug-mcp"
type: phase
plan: RustPort
phase: 5
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "All 21 MCP tools (handlers + exact schemas + Args accessor + response/format/flatten helpers) wired into an rmcp stdio server named 'debug', shipped as the debug-mcp binary that registers LldbFactory."
tasks:
  - id: "5.1"
    title: "Args accessor + response builders"
    status: complete
    verification: "Args reproduces mcp-go semantics + exact errors: required-missing → 'missing required parameter: …' prefix; JSON-number→int coercion; `args` non-string → \"'args' must be a JSON array string, …\" and parse-failure string; `env` equivalents; get_string/get_bool/get_int defaults. RespBuilder emits conditional keys (omitempty) and the CallToolResult text/error shapes (is_error) matching Go's NewToolResultText/Error."
  - id: "5.2"
    title: "format + flatten helpers"
    status: complete
    depends_on: ["5.1"]
    verification: "format_hex_dump byte-exact for full row, partial row, multi-row, empty; format_output_entries empty→{output:'',count:0}, grouping into stdout/stderr/console, omit-missing; flatten_variables passes every variables_util_test.go vector (basic, top-level-only case-insensitive filter, has_children at depth 0, recursive/deep/depth-limit, truncation incl. mid-recursion, children_count=named+indexed, empty/no-match), cap counts all emitted nodes, dotted names, DAP order."
  - id: "5.3"
    title: "Lifecycle + breakpoint + execution handlers"
    status: complete
    depends_on: ["5.1", "5.2"]
    verification: "Per-tool state guards reject disallowed states with exact messages; launch/attach produce the success JSON (+stop_reason/stopped_thread_id) and the plain-text exit-early strings, and the launch handler spawns the event-pump BEFORE awaiting backend.launch(spec) (a terminated-during-handshake event reaches the session, not dropped); disconnect always succeeds past the guard, applies the TWO SEQUENTIAL 5s timeouts (DAP DisconnectRequest, then subprocess graceful-exit before SIGKILL), and returns {status:disconnected}+resets even when both expire; set_breakpoint/set_function_breakpoint pending vs stopped paths + matched-breakpoint selection; remove (stopped-only)/list (id-sorted, []-not-null); continue/step set running→stopped/terminated with output merged, revert state on send error, generation-guarded post-call write; pause (running-only) returns pause_requested without changing state and can interrupt a blocked continue."
  - id: "5.4"
    title: "Inspection + memory + run_command handlers"
    status: complete
    depends_on: ["5.1", "5.2"]
    verification: "status per-state fields (no live DAP); backtrace (levels default 20, thread resolution, frame rebuild) / threads (stopped-thread flags) / variables (scope enum + case-insensitive Locals/Globals/Registers match, depth default 2 / global 1 / explicit≥0, 100-cap, top-level filter, resolveFrameID implicit levels=20) / evaluate (context=variables, has_children when ref>0); read_memory (0x normalization, base64 decode, hex_dump, empty→bytes_read 0); disassemble (default 20, current-PC path, is_current_pc, start_address); run_command (context=repl, backtick via backend, no has_children); resolveFrameID's implicit-stackTrace errors use the INNER prefixes 'implicit stackTrace request failed:' / 'implicit stackTrace failed:' (Go inspection.go), surfaced to the user as 'failed to resolve frame: implicit stackTrace …' — tests pin the full combined string, not just the outer wrapper."
  - id: "5.5"
    title: "rmcp server wiring + tool registration + binary"
    status: complete
    depends_on: ["5.3", "5.4"]
    verification: "All 21 tools registered with verbatim names + descriptions + input schemas matching the Go mcp-go schemas (types/required/enum/descriptions); server name 'debug' v1.0.0, WithToolCapabilities(false); `debug-mcp` boots over stdio and registers LldbFactory; a concurrent pause during a blocked continue succeeds (rmcp dispatches tool calls concurrently — R1); server-error path prints 'Server error: …' to stderr + exit 1."
---

# Phase 5: Tools + Server — mcp-tools + debug-mcp

## Overview

The user-facing layer: the 21 MCP tools and the rmcp stdio server. Handlers depend on
`debugger-core` + `mcp-session` + `rmcp` only. This phase reproduces every Go tool
contract (params, defaults, guards, response keys, error strings) from Spec FR-2…FR-14.
The binary `debug-mcp` wires the session, tools, and `LldbFactory`. Mirrors design §"MCP
tool surface" and Decisions 3, 6, 7.

## 5.1: Args accessor + response builders

### Subtasks
- [ ] `Args` wrapper over the rmcp arguments object (`serde_json::Map`): `require_string`, `get_string(default)`, `get_bool(default)`, `require_int`, `get_f64`, `get_raw` — each reproducing mcp-go behavior + the exact Go error strings.
- [ ] `args`/`env` helpers: require a string, parse JSON array/object, with the exact "must be a JSON array string"/"must be a JSON object string" + parse-failure messages.
- [ ] `RespBuilder` over `serde_json::Map` with conditional inserts; `ok_json(value)`, `ok_text(str)`, `err_text(msg)` producing `CallToolResult` (success text / `is_error` error) matching Go's `NewToolResultText`/`NewToolResultError`.
- [ ] Tests: required/missing prefix; number→int coercion; `args`/`env` wrong-type + parse errors; default getters; response conditional-key omission.

### Notes
This module centralizes Go's permissive arg handling so all 21 handlers stay faithful
(design Decision 3). Numbers arrive as JSON numbers (Go float64) — coerce to int where Go
does. `serde_json::Map` (BTree) serializes keys sorted, coincidentally matching Go's map
output (structural parity is the requirement; this is a bonus).

## 5.2: format + flatten helpers

### Subtasks
- [ ] `format_hex_dump(data, start_addr)`: 16/row, `0x%08x: ` prefix, 2-digit lowercase bytes, the extra space before column 8, 3-space padding for missing bytes, ` |` + ASCII gutter (printable 0x20–0x7e else `.`, space for missing) + `|`, `\n` between rows (none trailing), empty→"".
- [ ] `format_output_entries(entries)`: empty→`{output:"",count:0}`; else group by category into stdout/stderr/console (default bucket = console), always `count`, include a bucket key only when non-empty.
- [ ] `flatten_variables(backend, ref, depth, max_count, filter)`: one variables fetch per level; top-level-only case-insensitive substring filter; dotted nested names; container-before-children; `has_children`+`children_count=named+indexed` at depth 0; expand with depth-1 when depth>0; cap checked after every emitted node (counts containers + leaves), return `truncated`.
- [ ] `FlatVariable` serde with omitempty (`type`/`has_children`/`children_count`).
- [ ] Tests: hex-dump 4 vectors byte-exact; output grouping/empty/omit-missing; all flatten vectors from `variables_util_test.go`.

### Notes
`flatten_variables` calls the backend's `variables(ref)` per level — the depth defaults
(2; global 1; explicit≥0) are applied by the `variables` handler (5.4) before calling in.
Cap counts ALL emitted nodes; result length never exceeds `max_count`. This is the
trickiest parity surface — port the Go algorithm structurally and pin it with the exact
test vectors.

## 5.3: Lifecycle + breakpoint + execution handlers

### Subtasks
- [ ] `launch`: guard idle; parse program/args/cwd/env/stop_on_entry; flush pending breakpoints into `LaunchSpec`; `factory.connect()` + store backend + spawn event-pump **(before** `backend.launch`**)**; `backend.launch(spec)`; map `LaunchOutcome` to the success JSON or the plain-text "Program exited during launch"; cancellation → "launch timed out…"/"timed out waiting for stop on entry…".
- [ ] `attach`: guard idle; pid-precedence validation + exact errors; `backend.attach`; map `AttachOutcome` incl. plain-text "Process exited during attach".
- [ ] `disconnect`: guard non-idle; `backend.disconnect(terminate)` (5s) ignore errors; drop backend (kill, 5s graceful then kill); reset; always `{"status":"disconnected"}`.
- [ ] `set_breakpoint`/`set_function_breakpoint`: guard idle|stopped; pending-mode JSON vs stopped-mode (call backend, matched-breakpoint selection: exact-line then last; record; build response + synthesized function message).
- [ ] `remove_breakpoint` (stopped) → backend re-send remaining + `{removed,breakpoint_id}`; `list_breakpoints` (no guard) id-sorted, `[]` not null.
- [ ] `continue`/`step_over`/`step_into`/`step_out`: guard stopped; thread resolution; set running; `select!(backend.cont/step, ct.cancelled→timeout string)`; on send error revert to stopped; map `StopOutcome` via shared formatter (set state generation-guarded; drain+merge output); granularity for over/into.
- [ ] `pause`: guard running; `backend.pause()`; no state change; `{status:"pause_requested",message:…}`.
- [ ] Tests: per-tool guard rejections; success/early-exit shapes; bp pending/stopped + selection; execution transitions + output merge + send-error revert; pause-concurrency.

### Notes
The shared stop-outcome formatter is Go's `handleStopResult` (stopped/exited/terminated +
output merge). Set the post-call state under the session lock with a generation check
(design Decision 6). Thread-id resolution: explicit arg → last-stopped thread → 1.
Cancellation strings are per-tool (continue/step over/into/out). `launch` flushes pending
breakpoints; `attach` does not.

**Ordering constraints (do not reorder):**
- Spawn the event-pump task **before** awaiting `backend.launch(spec)` — a
  `Terminated` event during the handshake must reach the session, not be dropped.
- `disconnect` applies **two sequential** 5 s timeouts: first the DAP `DisconnectRequest`
  (errors ignored), then close stdin and wait for graceful exit before `SIGKILL`. It
  returns `{status:disconnected}` and resets even when both expire and a kill is needed.

## 5.4: Inspection + memory + run_command handlers

### Subtasks
- [ ] `status` (no guard): per-state fields from cached session data only — idle/configuring messages; stopped (program,pid,+stop_reason/stopped_thread_id/stop_description/hit_breakpoint_ids); running (program,pid); terminated (program,+exit_code).
- [ ] `backtrace` (stopped): levels default 20 (override if >0); thread resolution; `backend.stack_trace`; rebuild + store frame mapping; frames with conditional file/line/address; `total_frames`,`thread_id`.
- [ ] `threads` (stopped): `backend.threads`; mark stopped/current thread; `stopped_thread_id` when matched.
- [ ] `variables` (stopped): frame_index/scope/depth/filter parsing (depth default 2, global 1, explicit≥0); `resolve_frame_id` (frame-map hit, else implicit `stack_trace(levels=20)` rebuild, else out-of-range error); `backend.scopes` + case-insensitive Locals/Globals/Registers match; `flatten_variables(...,100,filter)`; `{variables,count,scope,truncated}`.
- [ ] `evaluate` (stopped): expression required; resolve frame; `backend.evaluate(Expression)`; `{result,type}` + `has_children`/`variables_reference` when ref>0.
- [ ] `read_memory` (stopped): 0x-normalize; `backend.read_memory`; empty→`{address,bytes_read:0}`; else parse addr + `format_hex_dump`; `{address(=response),bytes_read,hex_dump}`.
- [ ] `disassemble` (stopped): default count 20; current-PC path (`stack_trace levels=1` → top frame IP, error if none); normalize; `backend.disassemble`; instructions with conditional bytes/symbol/file/line + `is_current_pc`; `{instructions,count,start_address}`.
- [ ] `run_command` (stopped): `backend.evaluate(cmd, None, Repl)`; `{result,type}` (discard variables_reference — no has_children).
- [ ] Tests: each tool's response shape, defaults, scope/depth, normalization, current-PC, error strings.

### Notes
`disassemble` default is **20** (Spec OQ-1 — intentional deviation; Go code uses 10).
`resolve_frame_id` implicit stack trace always uses `levels=20` regardless of any
`backtrace` levels arg, and its error variants carry the inner `"implicit stackTrace
request failed: …"` / `"unexpected stackTrace response type: …"` / `"implicit stackTrace
failed: …"` messages, wrapped by the handler as `"failed to resolve frame: …"`.
`evaluate` uses `context="variables"`; `run_command` uses
`context="repl"` and the backtick/has-children differences live in the backend + handler
(design §LldbBackend notes). `status` makes NO live backend calls.

## 5.5: rmcp server wiring + tool registration + binary

### Subtasks
- [ ] Build the 21 tool definitions with hand-written input schemas matching the Go mcp-go schemas (string/number/boolean, required, enum line|instruction / local|global|register, exact descriptions).
- [ ] Implement the rmcp `ServerHandler`/router dispatching by tool name to the handlers; server identity name `"debug"`, version `"1.0.0"`, `WithToolCapabilities(false)` equivalent.
- [ ] `debug-mcp/src/main.rs`: `#[tokio::main]`; construct `SessionManager` + `LldbFactory`; `serve(stdio())`; on fatal error print `Server error: <e>` to stderr + exit 1.
- [ ] Resolve R2 (rmcp manual schema/registration API) — manual `Tool`/router if available, else macro-for-schema + Args-for-parsing fallback.
- [ ] Verify R1: confirm rmcp dispatches `tools/call` concurrently; if it serializes, spawn-and-await the blocking backend call with the request token.
- [ ] Tests: tool list (names/descriptions/schemas) snapshot vs Go; server boots over an in-memory transport; concurrent pause-during-continue; server-error/exit path.

### Notes
Schema parity is advertisement-level; runtime parsing goes through `Args` (design Decision
3). Server name is `"debug"` (Spec FR-1.1 — renamed from Go's `"lldb-debug"`). Handlers
must never hold the session lock across an `.await` (design Decision 7) so `pause` can
interrupt a blocked `continue`. The DAP `clientID` sent to lldb-dap stays
`"lldb-debug-mcp"` (Phase 3, below the seam).

## Acceptance Criteria
- [ ] `Args` + response builders reproduce Go's exact validation strings, numeric coercion, JSON-string `args`/`env`, and conditional response keys.
- [ ] `format_hex_dump`, `format_output_entries`, and `flatten_variables` pass every Go test vector.
- [ ] All 21 handlers reproduce their Go contract (guards, defaults, response keys, error strings) incl. the intentional `disassemble`=20 deviation.
- [ ] 21 tools register with verbatim names/descriptions/schemas; server name is `"debug"`; `debug-mcp` boots over stdio.
- [ ] Concurrent `pause` interrupts a blocked `continue` (R1 resolved); server-error path prints to stderr + exits 1.
- [ ] `cargo clippy -- -D warnings` clean; tests in dedicated folders.
