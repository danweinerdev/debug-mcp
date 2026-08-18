---
name: debug-mcp-execution
description: Drive execution of a stopped native program via the debug MCP server (lldb-dap) — continue, step over/into/out at line or instruction granularity, and pause a running program. Use when the user wants to run to the next breakpoint, single-step through code line by line, step into or out of a function, advance one machine instruction, or interrupt a running/hung program. Assumes a session is active (see the debug-mcp-debugging skill).
---

# debug-mcp — drive execution

> Tool names below are the `debug` MCP server's own tool names. Your host
> prefixes them with the server name (e.g. `debug_launch` in OpenCode) —
> register the server under the name `debug` so they resolve.

Once the program is stopped (at entry, a breakpoint, or a fault), these tools move
it forward — and each one **returns the next stop**: the reason it stopped, the
thread, and the frame. That returned stop is your cue to inspect (see
**debug-mcp-inspection**). The session is stateful, so the "current thread/frame"
carries between calls.

**Precondition:** an active, *stopped* session (see **debug-mcp-debugging**).
Stepping needs the program paused; if it's running, `pause` first.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| Run until the next stop | `continue` | `thread_id` (optional) |
| Execute the current line, not descending into calls | `step_over` | `thread_id`, `granularity` |
| Descend into the call on the current line | `step_into` | `thread_id`, `granularity` |
| Finish the current function and stop at the caller | `step_out` | `thread_id` |
| Interrupt a running program | `pause` | — |

## continue

`continue` resumes until the next breakpoint, signal/fault, or program exit. If
the program exits, the result reflects that (read leftover output with
`read_output`). Pass `thread_id` to resume a specific thread; omit it
to resume the process.

## Stepping

- **`step_over`** — run the current source line (or instruction) to completion,
  *without* stopping inside any function it calls. The workhorse for walking down
  a function.
- **`step_into`** — if the current line calls a function, stop at its first line;
  otherwise behaves like `step_over`. Use to descend into the callee you suspect.
- **`step_out`** — run until the current function returns, then stop in the
  caller. Use to escape a function you've seen enough of.

`step_over` and `step_into` accept `granularity`:
- `"line"` (default) — step by source line.
- `"instruction"` — step a single machine instruction. Pair with
  `disassemble` (see **debug-mcp-lowlevel**) when debugging at the asm
  level or when there's no line info.

Each step returns the new stop location; read it to decide the next move rather
than assuming where you landed.

## pause

`pause` interrupts a program that's running (e.g. after a `continue` into an
infinite loop or a hang) and stops all threads, so you can `backtrace` to see
*where* it's spinning. This is the tool for "it's stuck — where?".

## Patterns

- **Walk a function:** repeated `step_over`, checking `variables` after each (see
  **debug-mcp-inspection**), until the wrong value appears.
- **Follow a suspect call:** at the call site, `step_into`; if it's not the
  culprit, `step_out` and `step_over` it next time.
- **Diagnose a hang:** `continue` → (it hangs) → `pause` → `backtrace` on the
  busy thread; inspect the loop's `variables`.
- **Multi-threaded:** target a specific thread with `thread_id` (get ids from
  `threads`); otherwise the stopped/current thread is used.
