---
name: debug-mcp-lowlevel
description: Low-level and escape-hatch debugging via the debug MCP server (lldb-dap) — read raw memory at an address, disassemble instructions at an address or the current PC, and run arbitrary LLDB commands inside the live session. Use when the user needs to inspect bytes at a pointer, look at machine code around a fault, debug without source/line info, or do something the structured tools don't cover and only a raw LLDB command will. Assumes a session is active (see the debug-mcp-debugging skill).
---

# debug-mcp — memory, disassembly & raw LLDB

When source-level inspection isn't enough — a corrupted pointer, a fault in
stripped/optimized code, a register-level question, or a one-off LLDB command the
structured tools don't expose — these drop to the machine level inside the same
live session.

**Precondition:** an active session (see **debug-mcp-debugging**); for meaningful
addresses, usually a *stopped* one.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| Raw bytes at an address | `mcp__debug__read_memory` | `address` (required), `count` (required) |
| Machine code at an address / current PC | `mcp__debug__disassemble` | `address`, `instruction_count` |
| Run any LLDB command | `mcp__debug__run_command` | `command` (required) |

## read_memory

`read_memory(address="0x...", count=N)` returns `count` raw bytes starting at the
hex `address`.
- `address` — a hex string (e.g. `"0x16fdff8a0"`). Get one from `variables`
  (a pointer's value), `evaluate("&x")`, or a `backtrace` frame.
- `count` — number of bytes.

Use it to inspect a buffer, confirm a pointer points at sane data, or read a
struct's bytes when the typed view is suspect.

## disassemble

`disassemble` returns the instructions at an address (or the **current PC** if
`address` is omitted), annotated where symbol/line info is available.
- `address` — hex start address; omit to disassemble around where execution is
  stopped.
- `instruction_count` — how many instructions (this server defaults to **20**).

Pair with **instruction-granularity stepping** (`step_over`/`step_into` with
`granularity="instruction"`, see **debug-mcp-execution**) and
`variables(scope="register")` to debug at the asm level — the right move when a
fault lands in code with no line info, or when you need to see exactly which
instruction trapped.

## run_command — the escape hatch

`run_command(command="...")` runs an arbitrary LLDB command **inside the same live
session**, so its effects and any state it sets persist (unlike a one-shot `lldb`
in Bash, which is a throwaway process). Reach for it when the structured tools
don't cover what you need:
- watchpoints — `command="watchpoint set variable counter"`;
- richer formatting / `type summary add`;
- `image lookup`, `register read`, `memory region`, `frame select`, etc.

Notes:
- The command runs against the current session/frame/thread, so set up state
  (stop, frame) first with the structured tools, then issue the command.
- On lldb-dap (LLVM 18+) the server runs in `--repl-mode=command`; on older
  `lldb-vscode` a backtick fallback is used below the seam — you don't manage
  that, just pass the command text.
- Prefer the structured tools when one fits (they return parsed results);
  `run_command` is for the gaps and one-offs.

## Recipe: fault with no source

1. Stopped on the fault → `disassemble()` (no address → around the PC) to see the
   trapping instruction.
2. `variables(scope="register")` to read the registers feeding it.
3. `read_memory(address="<the suspect register/pointer>", count=...)` to inspect
   what it points at.
4. `run_command("image lookup --address <pc>")` (or similar) for any detail the
   structured tools didn't surface.
