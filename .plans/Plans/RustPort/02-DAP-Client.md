---
title: "DAP Transport — dap-client"
type: phase
plan: RustPort
phase: 2
status: complete
created: 2026-06-02
updated: 2026-06-02
deliverable: "A generic, debugger-agnostic DAP client over async stdio: Content-Length framing, the ~20 DAP message types used, seq/pending correlation, the read loop with full event dispatch, the stop-waiter, and EOF/crash recovery — all tested against scripted peers over tokio::io::duplex."
tasks:
  - id: "2.1"
    title: "DAP wire types + Content-Length framing"
    status: complete
    verification: "Frame round-trip (write→read) preserves seq/type/command/arguments for Initialize/Launch/Attach/Continue/Evaluate; malformed input errors on truncated body, invalid JSON body, missing Content-Length header, and empty reader — mirrors dap/types_test.go."
  - id: "2.2"
    title: "Client core — seq, pending map, Send/SendAsync, cancel"
    status: complete
    depends_on: ["2.1"]
    verification: "Send returns the correlated response; 3 concurrent sends answered in reversed order each get their own response (correlation by request_seq); next_seq yields 1..N; a cancelled/dropped request removes its pending entry (AbortGuard); a concurrent AbortGuard drop racing cancel_all_pending neither panics nor double-frees (idempotent delete — R5); dispatch to a non-existent waiter does not panic — mirrors dap/client_test.go."
  - id: "2.3"
    title: "Read loop, event dispatch, stop-waiter, EOF recovery"
    status: complete
    depends_on: ["2.2"]
    verification: "Per-event dispatch verified (Stopped→onStopped+deliver; Initialized→signal; Output→sink; Exited→deliver exit code; Terminated→cancel; Thread/Breakpoint/Process/Continued→log only); EOF fails all pending with a wrapped error AND delivers Terminated to the stop-waiter AND fires the terminated signal; stop-waiter Register/Deliver/DeliverExit/Cancel are single-shot and no-op without a waiter; concurrent register+deliver is race-clean under ThreadSanitizer — mirrors dap/client_test.go read-loop set + dap/stopwaiter_test.go."
---

# Phase 2: DAP Transport — dap-client

## Overview

The generic DAP transport, with no lldb knowledge. It owns wire framing, request/response
correlation, the read loop, the event-dispatch table, and the single-slot stop-waiter.
`lldb-backend` (Phase 3) drives it. Mirrors design §"DAP Client internals", §FR-17 of the
spec, and Go `internal/dap/{client,stopwaiter,types}.go`. All tests use a scripted DAP
peer over `tokio::io::duplex()` — the Rust analog of the Go pipe fakes.

## 2.1: DAP wire types + Content-Length framing

### Subtasks
- [ ] Implement `read_message`/`write_message`: `Content-Length: <N>\r\n\r\n` + N bytes UTF-8 JSON, over a buffered async reader/writer.
- [ ] Define the ~20 DAP message structs used (serde): requests Initialize, Launch, Attach, ConfigurationDone, SetBreakpoints, SetFunctionBreakpoints, SetExceptionBreakpoints, Continue, Next, StepIn, StepOut, Pause, Threads, StackTrace, Scopes, Variables, Evaluate, ReadMemory, Disassemble, Disconnect; responses for each; events Initialized, Stopped, Output, Exited, Terminated, plus the informational Thread/Breakpoint/Process/Continued.
- [ ] Implement a message enum + discriminator decode (type + command/event) so the read loop can match concrete types.
- [ ] Decide DAP type source (R3): local structs (leaning) vs a crate; record the choice.
- [ ] Tests: round-trip each exercised message; malformed-input errors (truncated body, invalid JSON, no `Content-Length`, empty reader).

### Notes
Match Go's `google/go-dap` wire shape exactly (CRLF header, byte-count body). Request
`Arguments` for launch/attach are carried as raw JSON (`serde_json::Value` / `RawValue`)
since the lldb-specific arg shapes live in Phase 3 — `dap-client` stays debugger-agnostic.
Preserve the exact JSON field names DAP uses. Mirror the malformed-input cases from
`types_test.go` precisely.

## 2.2: Client core — seq, pending map, Send/SendAsync, cancel

### Subtasks
- [ ] `Client` struct: write half behind `tokio::sync::Mutex`, `AtomicI64` seq (first = 1), `Mutex<HashMap<i64, oneshot::Sender<Result<Msg, BackendError>>>>` pending map, plus the read-side handles.
- [ ] `next_seq()` pre-increment; assign + stamp the request's `seq` before writing.
- [ ] `send_async(req) -> oneshot::Receiver`: register pending(seq)→sender, write the frame; roll back the pending entry on write error.
- [ ] `send(req).await`: await the receiver; an `AbortGuard` (Drop removes the pending entry) provides cancel-safety when the future is dropped.
- [ ] `cancel_all_pending(err)`: drain the map, send `Err(err.clone-ish)` to every waiter.
- [ ] `send_and_await_both`: a primitive (or document the pattern) that awaits a response and the Initialized signal concurrently, order-independent.
- [ ] Tests: send/await; 3 concurrent sends answered reversed; `next_seq` 1..N; drop-on-cancel removes pending; dispatch-no-waiter no panic.

### Notes
Correlation key is the response's `request_seq`. Channels are capacity-1 (`oneshot`) so
dispatch never blocks. There is no internal timeout — timeouts come from the caller's
context (Phase 5 wraps with `select!`). The `AbortGuard` must be `Send`, must not hold
the pending-map lock across its body, and must tolerate the entry already being gone
(idempotent — `cancel_all_pending` may have drained it). See design Decision 4 + risk R5.

## 2.3: Read loop, event dispatch, stop-waiter, EOF recovery

### Subtasks
- [ ] `StopWaiter`: `Mutex<Option<oneshot::Sender<StopOutcome>>>`; `register()` replaces any prior slot; `deliver`/`deliver_exit`/`cancel` each no-op when empty and clear the slot after sending; ignore `send` error on a dropped receiver.
- [ ] Read-loop task: loop `read_message`; on success `match` the concrete type and dispatch:
  - [ ] `Stopped` → call on-stopped hook, then `stop_waiter.deliver`.
  - [ ] `Initialized` → non-blocking send on the capacity-1 initialized channel.
  - [ ] `Output` → forward `(category, text)` to the output sink.
  - [ ] `Exited` → call on-exit hook, then `stop_waiter.deliver_exit(code)`.
  - [ ] `Terminated` → call on-terminated hook, then `stop_waiter.cancel()`.
  - [ ] `Thread`/`Breakpoint`/`Process`/`Continued` → log only.
  - [ ] response message → dispatch to the pending waiter (log + no panic if absent).
- [ ] EOF/read-error path: record closed-once + raw error; `cancel_all_pending(wrapped)`; `stop_waiter.cancel()`; fire the terminated signal; exit.
- [ ] Expose the output + terminated signals as channels/streams `lldb-backend` will adapt into `BackendEvent`.
- [ ] Tests: each event type's effect; EOF recovery (pending err + stop-waiter terminated + signal); stop-waiter deliver/exit/cancel/no-waiter/concurrent.

### Notes
The concrete event types must be matched **before** the generic response case (events are
matched by concrete type; only non-event responses fall through). EOF ordering matters —
close-once, then unblock pending, then stop-waiter, then terminated signal (Go
`client.go:171-185`). Run the concurrency tests under ThreadSanitizer (the `-race`
analog). This is the trickiest async code in the project; lean on the scripted-peer
fakes to make every event ordering deterministic.

## Acceptance Criteria
- [ ] Framing round-trips and rejects all four malformed-input cases.
- [ ] Send/correlation works under concurrency (reversed-order responses); seq starts at 1.
- [ ] The full event-dispatch table behaves per spec FR-17.6; informational events are log-only.
- [ ] EOF recovery unblocks every pending request, terminates the stop-waiter, and fires the lifecycle signal.
- [ ] The stop-waiter is single-shot, no-ops without a waiter, and is race-clean under TSan.
- [ ] `cargo clippy -- -D warnings` clean; tests in dedicated folders; zero `unsafe`.
