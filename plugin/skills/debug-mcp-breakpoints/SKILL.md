---
name: debug-mcp-breakpoints
description: Set, list, and remove breakpoints in a native debug session via the debug MCP server (lldb-dap) — source-line breakpoints, function-name breakpoints, and conditional breakpoints. Use when the user wants to stop execution at a specific file:line or function, break only when a condition holds (e.g. on the 42nd iteration or when a pointer is null), or manage existing breakpoints. Assumes a session is active (see the debug-mcp-debugging skill).
---

# debug-mcp — breakpoints

Breakpoints are where you decide *where the program stops* so you can look around.
They live in the active session and persist across `continue`/`step` calls until
removed. Set them after `launch` (with `stop_on_entry=true`, you get a chance to
place them before any code runs) or any time the program is stopped.

**Precondition:** an active session (see **debug-mcp-debugging**). Breakpoints
need debug info (`-g`/debug build) to resolve file:line and function names.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| Stop at a file & line | `mcp__debug__set_breakpoint` | `file` (required), `line` (required), `condition` |
| Stop when a function is entered | `mcp__debug__set_function_breakpoint` | `name` (required), `condition` |
| See all breakpoints | `mcp__debug__list_breakpoints` | — |
| Delete one | `mcp__debug__remove_breakpoint` | `breakpoint_id` (required) |

## Source-line breakpoints

`set_breakpoint(file=..., line=...)` stops just before that line executes.
- `file` — the source path. Prefer the path as the build recorded it; an absolute
  path is safest.
- `line` — 1-based line number.
- The tool returns a **breakpoint id** — keep it; that's what
  `remove_breakpoint(breakpoint_id=...)` takes.

If lldb can't bind the line (no code there, or the file isn't in the debug info),
the breakpoint may resolve to a nearby line or stay unverified — check
`list_breakpoints` after setting if a stop doesn't happen where expected.

## Function breakpoints

`set_function_breakpoint(name="parse_header")` stops on entry to every function
matching that name — handy when you know *what* but not *which line*, or the line
moves between builds. For overloaded/templated names, more than one location may
bind.

## Conditional breakpoints

Both breakpoint tools take an optional `condition`: an expression evaluated in the
breakpoint's frame each time it's hit; the program only stops when it's true.
- `set_breakpoint(file="loop.c", line=14, condition="i == 42")` — stop on the
  iteration where `i` is 42, not all 1000.
- `set_function_breakpoint(name="free_node", condition="node == NULL")` — stop
  only on the null-pointer call.

Conditions are LLDB/native expressions in the target language's syntax. Keep them
cheap (comparisons of locals); a condition that itself crashes will disrupt the
run.

## Managing breakpoints

- `list_breakpoints` → every breakpoint with its id, location, condition, and
  verified/hit state. Use it to confirm a breakpoint actually bound.
- `remove_breakpoint(breakpoint_id=N)` → delete by the id from set/list. There's
  no "remove all" — remove ids individually (list first to collect them).

## Recipe: bisect a crash

1. `launch(program=..., stop_on_entry=true)`.
2. `set_function_breakpoint(name="suspect_fn")` (or a `set_breakpoint` near the
   suspected spot).
3. `continue` (see **debug-mcp-execution**) → it stops at the breakpoint or on the
   fault, whichever comes first.
4. Inspect (`backtrace`, `variables`, `evaluate` — see **debug-mcp-inspection**),
   then refine: tighten with a `condition`, move the breakpoint deeper, or
   `step_into` from here.
