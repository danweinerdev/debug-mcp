---
name: debug-mcp-debugging
description: Start, attach to, and tear down an interactive native debug session using the debug MCP server (lldb-dap under the hood) instead of running a binary bare or driving lldb/gdb by hand. Use when the user wants to debug a crash, segfault, hang, or wrong-value bug in a native (C/C++/Rust/Go/Swift) executable, attach to a running process, or step through a program live. This is the entry-point skill — it covers launch/attach/disconnect/status; breakpoints, stepping, and inspection have their own skills.
---

# debug-mcp — start & control a debug session

The `debug` MCP server wraps **lldb-dap** as live, stateful debug tools. A session
is persistent: breakpoints, the stopped thread, the current frame, and program
output all survive across tool calls — unlike a one-shot `lldb`/`gdb` invocation
in Bash, where every call is a fresh process. Reach for these whenever the goal is
"why does this crash / hang / produce the wrong value", not just "what does it
print".

**Precondition:** `lldb-dap` must be discoverable (on `PATH`, or via
`LLDB_DAP_PATH`). The server detects it lazily at `launch`/`attach`; if detection
fails the tool returns an error naming every path it searched.

## The session lifecycle

```
launch | attach  ──►  (running/stopped)  ──►  disconnect
                          ▲     │
                  step/continue │ breakpoint hit, signal, or pause
                          └──────┘
```

One session at a time. `launch`/`attach` start it; `status` reports where it is;
`disconnect` ends it. Most tools below require an active session and a *stopped*
program — drive execution with the **debug-mcp-execution** skill, inspect with
**debug-mcp-inspection**.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| Run a program under the debugger | `mcp__debug__launch` | `program` (required), `args`, `cwd`, `env`, `stop_on_entry` |
| Attach to a live process | `mcp__debug__attach` | `pid` **or** `wait_for` |
| End the session | `mcp__debug__disconnect` | `terminate` (default true) |
| Where is the session right now? | `mcp__debug__status` | — |

## launch

- `program` — path to the executable (required). Build with debug info first
  (`-g`, or a Cargo `dev`/debug profile) or symbols/line info will be missing.
- `args` — a **JSON-array string** of argv, e.g. `"[\"--port\",\"8080\"]"`.
- `cwd` — working directory for the debuggee.
- `env` — a **JSON-object string** of environment variables.
- `stop_on_entry` — defaults **true**: the program stops before `main`, giving you
  a chance to set breakpoints before anything runs. Set `false` to run straight to
  the first breakpoint or to completion.

If the program exits during launch (e.g. it ran to completion with
`stop_on_entry=false` and no breakpoint), the tool returns the exit result rather
than a stopped state — read captured output with `mcp__debug__read_output`.

## attach

Provide exactly one of:
- `pid` — attach to that process id, **or**
- `wait_for` — a process *name*; the debugger waits for the next process with that
  name to start, then attaches. Useful for catching short-lived or
  relaunch-on-crash processes.

Attaching needs ptrace permission. In this repo's dev container that's already
arranged (`--cap-add=SYS_PTRACE`, `seccomp=unconfined`); on a host a hardened
`kernel.yama.ptrace_scope` can block it.

## disconnect

- `terminate` (default **true**) — kill the debuggee on disconnect. Pass `false`
  to detach and leave an *attached* process running.

Always disconnect when done so lldb-dap and the target are cleaned up before
starting the next session.

## Typical openings

- **"Debug why `./bin/server --config x.toml` segfaults"** →
  `launch(program="./bin/server", args="[\"--config\",\"x.toml\"]", stop_on_entry=true)`,
  set a breakpoint near the suspected fault (**debug-mcp-breakpoints**), then
  `continue` and inspect at the stop (**debug-mcp-inspection**). On a segfault the
  session stops *on the faulting instruction* — read `backtrace` there.
- **"Attach to the running daemon (pid 4321)"** →
  `attach(pid=4321)` → `status` → set breakpoints → `continue`.
- **"Catch the worker the moment it restarts"** →
  `attach(wait_for="worker")`.

## When NOT to use the debugger

If the user only needs the program's stdout/stderr or exit code, just run it.
Use this server when you need to *stop inside* the program and look around.
