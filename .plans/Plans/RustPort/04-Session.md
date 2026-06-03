---
title: "Session — mcp-session"
type: phase
plan: RustPort
phase: 4
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "mcp-session: the neutral SessionManager (state machine + guards + generation epoch), breakpoint tracking, the OutputBuffer, the frame-map cache + last-stop cache, and the event-pump task that drains BackendEvent into the buffer/state."
tasks:
  - id: "4.1"
    title: "State machine, guards, generation epoch, reset"
    status: complete
    verification: "State strings (idle/configuring/stopped/running/terminated; unknown(n)); check_state returns the exact three message forms (idle 'no debug session active…', running 'process is running…', generic 'invalid state: X, expected one of: …' unquoted); any-to-any SetState; Reset() restores the idle baseline and bumps generation — mirrors session/session_test.go state tests."
  - id: "4.2"
    title: "Breakpoint tracking"
    status: complete
    depends_on: ["4.1"]
    verification: "Add source (by file) / function; remove by id (source matched by line-only, function by name-only, first match in the active list); list sorted ascending by id with conditional fields; pending add + flush appends to active and is idempotent on a second flush; getters return defensive copies — mirrors session/session_test.go breakpoint tests."
  - id: "4.3"
    title: "OutputBuffer"
    status: complete
    depends_on: ["4.1"]
    verification: "Append pushes the entry then evicts (append-before-evict); size counts len(category)+len(text); eviction drops oldest while size > 1_048_576 (strict >); a single oversize entry is appended then immediately evicted (empty buffer, truncated=true); drain returns FIFO entries, prepends a console '[output truncated]' entry when truncated, clears, and is idempotent; concurrent append/drain is race-clean under ThreadSanitizer — mirrors session/session_test.go output tests."
  - id: "4.4"
    title: "Frame-map cache, last-stop cache, event-pump"
    status: complete
    depends_on: ["4.1"]
    verification: "frame_mapping set/replace and get-by-clone; last_stopped + exit_code setters/getters; the event-pump task drains BackendEvent (Output→OutputBuffer; Terminated{code}→record exit code + state=terminated) and the terminated transition is generation-guarded so it cannot clobber an idle state after a concurrent disconnect."
---

# Phase 4: Session — mcp-session

## Overview

The neutral session manager — everything Go's `internal/session/session.go` owned, minus
the DAP specifics. It holds the state machine, breakpoint tracking, the output buffer, the
frame-map cache, and the `Arc<dyn DebuggerBackend>` for the active session, plus the
event-pump that consumes the backend's `BackendEvent` stream. Depends only on
`debugger-core`, so it can be built in parallel with Phases 2–3. Mirrors design
§`SessionManager` and Spec FR-4/FR-7/FR-12.

## 4.1: State machine, guards, generation epoch, reset

### Subtasks
- [ ] `State` enum (Idle/Configuring/Stopped/Running/Terminated) with `Display` returning the exact lowercase strings; `unknown(n)` fallback only if a numeric repr is ever surfaced.
- [ ] `Inner` behind `RwLock`: state, generation, program, pid, exit_code, last_stopped, frame_mapping, source_bps, function_bps, bp_responses, pending_source_bps, pending_function_bps.
- [ ] `check_state(allowed)` → exact three message forms (idle / running / generic unquoted list joined by `", "`).
- [ ] `set_state`, `state`, simple getters/setters; `reset()` restoring the idle baseline AND bumping `generation`.
- [ ] Tests: state strings; the three check_state messages; any-to-any transition; reset baseline + generation bump.

### Notes
No transition validation — `check_state` is a read-only guard, transitions are
unconditional (Go parity). The `generation` epoch is the Rust-only addition (design
Decision 6) that protects the post-call state write in Phase 5 against a concurrent
`disconnect`. The Go `replModeCommand` flag is intentionally NOT tracked here — the
backend owns that decision (`supports_command_repl_mode`).

## 4.2: Breakpoint tracking

### Subtasks
- [ ] `add_source_breakpoint(file,line,cond)` / `add_function_breakpoint(name,cond)` append to active maps/vec and return the created `SourceBp`/`FunctionBp`.
- [ ] `add_breakpoint_response(BreakpointInfo)` keyed by debugger-assigned id; `source_breakpoints_for_file` / `all_function_breakpoints` return defensive copies.
- [ ] `remove_breakpoint_by_id(id)` → `(file, was_function)`: source matched by **line only**, function by **name only** (first match in the active list), then delete the response entry; unknown id → `breakpoint ID <id> not found`.
- [ ] `list_breakpoints()` sorted ascending by id.
- [ ] Pending: `add_pending_source/function`, `flush_pending() -> (source_map, func_vec)` appending to active + clearing pending; idempotent on re-flush.
- [ ] Tests: add/remove (line/name)/list-sorted/pending-flush-idempotent/copy-getters.

### Notes
IDs come from DAP responses (Phase 3/5), never assigned here. `flush_pending` is consumed
by Phase 5's launch handler to build `LaunchSpec.source_breakpoints/function_breakpoints`.
Match Go's removal semantics precisely (first-match in the active tracking list, not DAP
order) for deterministic behavior.

## 4.3: OutputBuffer

### Subtasks
- [ ] `OutputBuffer` behind its own `Mutex`: `entries: Vec<OutputEntry>`, `size`, `max_size=1_048_576`, `truncated`.
- [ ] `append(category,text)`: push the entry FIRST, add `len(category)+len(text)` to size, THEN evict oldest while `size > max_size` (strict `>`), setting `truncated`. A single entry larger than `max_size` is appended then immediately evicted (buffer empty, `truncated=true`, `size=0`) — add this as a test vector.
- [ ] `drain()`: return entries (prepending a `console`/`[output truncated]` entry when `truncated`, then clearing the flag); clear the buffer; return nothing when empty + not truncated; idempotent.
- [ ] Tests: append/drain FIFO order; truncation marker + size bound; idempotent drain; concurrent append/drain under TSan.

### Notes
Size accounting includes the category bytes (Go parity). The truncation marker is added
only at drain time and is not counted toward `size`. Keep this buffer's mutex separate
from the session `RwLock` (Go parity; avoids lock coupling).

## 4.4: Frame-map cache, last-stop cache, event-pump

### Subtasks
- [ ] `frame_mapping()` returns a clone (not the live map); `set_frame_mapping(map)` replaces it.
- [ ] `last_stopped`/`set_last_stopped`, `exit_code`/`set_exit_code` accessors.
- [ ] `spawn_event_pump(stream, session, generation)`: a task draining `BackendEvent` — `Output{category,text}` → `output_buffer.append`; `Terminated{code}` → `set_exit_code` + `set_state(Terminated)` **only if** the session generation matches (else drop, the session was reset/replaced).
- [ ] Wire the pump lifetime to the backend: dropping the backend on disconnect ends the stream and the pump task.
- [ ] Tests: frame-map store/clone; pump appends output; pump sets terminated; generation-guard drops a stale terminated after disconnect.

### Notes
The event-pump is Go's `SetOutputHandler` + `onExit`/`onTerminated`, expressed as one
stream consumer. The generation guard mirrors design Decision 6 — a `Terminated` arriving
after a concurrent `disconnect` (which bumped generation + reset to idle) must not flip
state back to terminated. The in-flight `cont`/`launch` outcome still sets state
synchronously in Phase 5 (also generation-guarded); the pump is the backstop for the
no-call-in-flight case.

## Acceptance Criteria
- [ ] State strings and the three `check_state` message forms match Go exactly.
- [ ] Breakpoint add/remove/list/pending-flush match Go (line-only/name-only removal, id-sorted list, idempotent flush, copy getters).
- [ ] OutputBuffer append/evict/drain match Go (strict `>` 1 MiB, category+text sizing, `[output truncated]` marker, idempotent drain), race-clean under TSan.
- [ ] Frame-map returns a clone; the event-pump drains output and applies the generation-guarded terminated transition.
- [ ] `cargo clippy -- -D warnings` clean; tests in dedicated folders.
