# debug-mcp — portable agent plugin (OpenCode / Codex)

Skills that teach a coding agent to debug native (C/C++/Rust/Go/Swift) programs
**live** through the `debug` MCP server — breakpoints, stepping, stack and
variable inspection, expression evaluation, memory/disassembly, and crash-dump
analysis — instead of print-debugging or driving `lldb`/`gdb`/`windbg` by hand.

This tree is **generated** from the canonical Claude Code plugin at `plugin/` in
the debug-mcp repository (`scripts/plugins-sync.sh`, run via `make plugins`).
Never edit it in place — changes here are overwritten on the next sync.

## Skills

| Skill | Covers |
|---|---|
| `debug-mcp-debugging` | Session lifecycle: launch, attach, disconnect, status, backend selection |
| `debug-mcp-breakpoints` | set_breakpoint, set_function_breakpoint, conditions, list/remove |
| `debug-mcp-execution` | continue, step_over/into/out, pause, granularity, threads |
| `debug-mcp-inspection` | backtrace, threads, variables, evaluate, read_output |
| `debug-mcp-lowlevel` | read_memory, disassemble, run_command (raw lldb/WinDbg escape hatch) |
| `debug-mcp-windbg` | open_crash_dump, analyze_crash (!analyze -v), attach_kernel, get_modules (Windows) |

All six are model-loaded, description-matched skills — no slash commands, no
hooks. (The Claude Code variant additionally ships a print-debugging nudge hook;
that part is Claude-specific and is not ported.)

## Requirements

1. **The debug-mcp server binary.** Build it from the repository:

   ```bash
   cargo build --release -p debug-mcp
   ```

2. **A native debugger for your OS.** macOS/Linux: `lldb-dap` discoverable on
   `PATH` (or via `LLDB_DAP_PATH`) — it ships with lldb. Windows: nothing to
   install; the OS-bundled DbgEng drives core debugging (the full Debugging
   Tools for Windows enrich `analyze_crash`).

3. **Register the server under the MCP name `debug`.** The skills refer to the
   server's own tool names (`launch`, `set_breakpoint`, `backtrace`, …); hosts
   prefix them with the server name (e.g. `debug_launch` in OpenCode), so a
   different server name breaks the references.

   OpenCode (`opencode.json`):

   ```json
   { "mcp": { "debug": { "type": "local",
     "command": ["/path/to/debug-mcp"], "enabled": true } } }
   ```

   Codex (`~/.codex/config.toml`):

   ```toml
   [mcp_servers.debug]
   command = "/path/to/debug-mcp"
   ```

## Install the skills

- **OpenCode** — expose this tree's `skills/` through OpenCode's skill
  discovery, e.g. symlink it into your agent skill root
  (`ln -s /path/to/debug-mcp/.opencode-plugin/skills/* ~/.agents/skills/`) or a
  per-project `.agents/skills/`. Skills load automatically when a task matches
  their description.
- **Codex** — install the `.codex-plugin/` tree via a marketplace that carries
  it; the manifest is the `plugin.json` at the tree root.

## Using it

Ask naturally — the skills are selected by description match:

- "Debug why `./target/debug/server` segfaults" → launch under the debugger,
  breakpoints, inspect at the stop.
- "Attach to pid 4321 and find where it hangs" → attach, pause, backtrace.
- "Open `app.dmp` and tell me the faulting frame" → crash-dump postmortem
  (Windows).
