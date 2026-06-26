---
name: debug-mcp-inspection
description: Inspect a stopped native program's state via the debug MCP server (lldb-dap) — call stack/backtrace, threads, local/global/register variables, expression evaluation, and captured program output. Use when the user wants to see the call stack at a crash, list threads, read the value of a variable or struct field, evaluate an expression in a frame's context, or read what the program printed. Assumes a session is active and stopped (see the debug-mcp-debugging skill).
---

# debug-mcp — inspect program state

When the program is stopped, these tools answer "what's true right now": the call
stack that got here, the threads, the values in scope, the result of an
expression, and everything the program has printed. This is where a crash or a
wrong value actually gets diagnosed.

**Precondition:** an active, *stopped* session (see **debug-mcp-debugging**).
`read_output` works any time a session exists; the rest read the stopped state.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| The call stack | `mcp__debug__backtrace` | `thread_id`, `levels` |
| List threads | `mcp__debug__threads` | — |
| Variables in scope | `mcp__debug__variables` | `frame_index`, `scope`, `depth`, `filter` |
| Evaluate an expression | `mcp__debug__evaluate` | `expression` (required), `frame_index` |
| What the program printed | `mcp__debug__read_output` | — |

## backtrace — start here at a stop

`backtrace` returns the call stack for the stopped thread: each frame's index,
function, source file:line, and (where known) address. On a segfault, **frame 0 is
the faulting location** — read it first.
- `thread_id` — a specific thread (default: the stopped/current thread).
- `levels` — cap the number of frames returned for deep stacks.

Frame **indices** from the backtrace are what `variables` and `evaluate` use as
`frame_index` to choose the scope to read in.

## threads

`threads` lists every thread in the process with its id and state — the map for a
multi-threaded bug. Feed a thread id to `backtrace(thread_id=...)`,
`continue(thread_id=...)`, or the steppers to act on one thread.

## variables

`variables` lists the values in scope at a frame:
- `frame_index` — which frame (default 0, the innermost). Use an index from
  `backtrace` to read a caller's locals.
- `scope` — `"local"` (default), `"global"`, or `"register"` (CPU registers).
- `depth` — how far to expand nested structs/arrays/pointers. Keep it small (1–2)
  for big aggregates; raise it to drill into a specific structure.
- `filter` — a name pattern to narrow a noisy frame to the variable(s) you care
  about.

## evaluate

`evaluate(expression=..., frame_index=...)` runs an expression in a frame's
context and returns its value — for reading something not in the plain variable
list, calling an accessor, dereferencing a pointer, or computing a comparison.
- The expression is in the target language's syntax (C/C++/Rust/…), e.g.
  `node->next->value`, `len(buf)`, `*p == 0`.
- `frame_index` selects the evaluation scope (default 0).
- Side-effecting expressions *do* execute in the live process — prefer pure reads
  unless you intend to mutate state.

## read_output

`read_output` returns the program's captured stdout, stderr, and debug console
output so far. Use it after a `continue`/exit to see what was printed, or to
correlate a log line with where execution stopped. It's the right tool when the
program "printed something then crashed" and you want both halves.

## A diagnosis flow

1. Stopped (breakpoint or fault) → `backtrace` to see *how you got here*.
2. `variables(frame_index=0)` (add a `filter`) to read the locals at the fault.
3. Don't see it directly? `evaluate("expr", frame_index=N)` to dereference /
   compute / inspect a caller's value at frame `N` from the backtrace.
4. `read_output` to fold in what the program printed.
5. Multi-threaded or unsure which thread? `threads`, then `backtrace` per thread.
