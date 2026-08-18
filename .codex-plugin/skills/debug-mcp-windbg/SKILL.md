---
name: debug-mcp-windbg
description: Windows-native debugging via the debug MCP server's WinDbg (DbgEng) backend — open a crash/minidump for postmortem analysis, run automated crash analysis (!analyze -v), attach to a kernel target over KDNET, and list loaded modules. Use when the user has a .dmp/minidump file to analyze, wants a WinDbg-style crash report, needs kernel debugging, or asks which modules/DLLs are loaded. Also covers WinDbg-specific behavior of the shared tools (backend selection, breakpoints, output). Windows only.
---

# debug-mcp — WinDbg: crash dumps, kernel, modules

> Tool names below are the `debug` MCP server's own tool names. Your host
> prefixes them with the server name (e.g. `debug_launch` in OpenCode) —
> register the server under the name `debug` so they resolve.

On Windows the `debug` MCP server drives **WinDbg (DbgEng)** instead of lldb-dap.
The shared tool surface (launch/attach, breakpoints, stepping, inspection) is
identical — see the other debug-mcp skills — plus four WinDbg-only tools that are
advertised only when the WinDbg backend is available.

**Backend selection:** on Windows, `launch`/`attach` default to WinDbg; they also
accept an optional `backend` arg (`"lldb"`/`"windbg"`), and `DEBUG_BACKEND` in the
environment overrides the default. `status` reports the active backend and
`available_backends`. The four tools below always use WinDbg.

## Pick the tool

| The user wants… | Tool | Key args |
|---|---|---|
| Postmortem-analyze a dump file | `open_crash_dump` | `dump_path` (required) |
| An automated crash report (`!analyze -v`) | `analyze_crash` | — |
| Kernel-debug a machine over KDNET | `attach_kernel` | `connection` (required) |
| List loaded modules/DLLs | `get_modules` | — |

## open_crash_dump — postmortem sessions

`open_crash_dump(dump_path="C:\\dumps\\app.dmp")` opens a crash/minidump as a
session that is **stopped at the faulting state**. Inspect it like any stop:
`backtrace`, `variables`, `evaluate`, `read_memory`, `disassemble`, `get_modules`.

A dump is a snapshot, not a live process: `continue` and the `step_*` tools are
rejected with `cannot continue a crash-dump session`. There is nothing to resume —
diagnose with the inspection tools, then `disconnect`.

Typical flow:
1. `open_crash_dump(dump_path=...)` → session stopped at the crash.
2. `analyze_crash` for the engine's automated verdict.
3. `backtrace` / `variables` / `evaluate` to confirm and drill in.
4. `disconnect` when done.

## analyze_crash

`analyze_crash` runs WinDbg's `!analyze -v` on the current session (live stop or
dump) and returns the report: exception code/record, faulting instruction, the
probable-cause stack, and a bucket id. Start here on any access violation or dump
— it usually names the faulting frame directly.

Requires the Debugging Tools for Windows extensions; the server discovers their
path automatically (registry → SDK default). Without them installed the engine
returns `No export analyze found` — fall back to `backtrace` + inspection, which
need only the OS-bundled DbgEng.

## attach_kernel

`attach_kernel(connection="net:port=50000,key=1.2.3.4")` attaches to a kernel
target over KDNET. The `connection` string is exactly what WinDbg's `-k` takes.
The target must have kernel debugging enabled (`bcdedit /debug on` + `/dbgsettings
net ...`). Once attached, drive it like any session (`pause`, `backtrace`,
`run_command` for kd commands like `!process`).

## get_modules

`get_modules` lists every loaded module with its base address, size, and symbol
status — the map for "which DLL owns this address" and "are symbols loaded".
Cross-reference a faulting address from `backtrace`/`disassemble` against the
module bases, or verify the right build got loaded before trusting line numbers.

## WinDbg behavior notes (shared tools)

- **Breakpoints:** a bare `0x<address>` to `set_function_breakpoint` is rejected —
  under ASLR a raw address would be misplaced on relaunch. Use a symbol
  (`module!function` is best), or `run_command("bp 0x...")` for a one-off
  address breakpoint. Conditions whose evaluation fails (out-of-scope symbol,
  typo) are silently skipped, matching DbgEng.
- **`read_output`** returns engine/event text; the debuggee's own stdout is not
  captured (DbgEng callbacks carry engine output, and the child runs windowless).
  Have the target log to a file if its prints matter.
- **`evaluate`** ignores `frame_index` and evaluates in the engine's current
  frame; select a frame first with `run_command("frame select N")` (`.frame N`).
- **`run_command`** is the raw WinDbg escape hatch: `!` extension commands
  (`!analyze`, `!heap`, `!process`), `dx`, `k`, `lm`, `bp` — anything DbgEng
  accepts, executed inside the live session.
