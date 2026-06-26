# debug-mcp Claude Code plugin

Steers Claude Code toward the **debug MCP server** for interactive native
debugging instead of print-debugging or driving `lldb`/`gdb` by hand.

Two parts:

1. **A `PreToolUse` hook** on `Bash` that injects a one-time, non-blocking nudge
   when a command looks like it's chasing a bug the debugger would answer better
   — driving `lldb`/`gdb`/`lldb-dap` directly, or running a native binary
   (`./bin/foo`, `target/debug/foo`, `cargo run`) to watch it crash. It **never
   blocks**: the command still runs, and version probes / unrelated commands
   (builds, tests, file ops) are left alone.
2. **Five skills** that teach the model when and how to use the debug API:

   | Skill | Covers |
   |---|---|
   | `debug-mcp-debugging` | Session lifecycle: launch, attach, disconnect, status |
   | `debug-mcp-breakpoints` | set_breakpoint, set_function_breakpoint, conditions, list/remove |
   | `debug-mcp-execution` | continue, step_over/into/out, pause, granularity, threads |
   | `debug-mcp-inspection` | backtrace, threads, variables, evaluate, read_output |
   | `debug-mcp-lowlevel` | read_memory, disassemble, run_command (raw LLDB escape hatch) |

The 21 tools are exposed under the MCP server name `debug`, i.e.
`mcp__debug__launch`, `mcp__debug__set_breakpoint`, `mcp__debug__backtrace`, …

## Requirements

The `debug` MCP server must be available to Claude Code, and `lldb-dap` must be
discoverable at runtime (on `PATH`, or via `LLDB_DAP_PATH`). In this repo's dev
container the server is baked in and wired via `/etc/claude-code/managed-mcp.json`
(`tools/claude/`), and `lldb` (which ships `lldb-dap`) is installed. On a host,
build the server (`cargo build --release -p debug-mcp`) and register it in your
MCP config, e.g.:

```json
{ "mcpServers": { "debug": { "type": "stdio",
  "command": "/path/to/debug-mcp", "args": [] } } }
```

If the server is registered under a name other than `debug`, the `mcp__debug__*`
tool names in the skills/nudge won't resolve — keep the server name `debug`.

## Enable it

Load the plugin directory directly — this activates its hooks and skills for the
session with no install step and no writes under `~/.claude`:

```bash
claude --plugin-dir ./plugin
```

In the dev container this is automatic: the image bakes the plugin at
`/opt/debug-mcp-plugin/plugin` and the launcher's default command passes
`--plugin-dir /opt/debug-mcp-plugin/plugin`, so `./claude-container.sh` starts
with the plugin loaded (`claude plugin list` → `Status: ✔ loaded`).

Alternatively, install it via the marketplace manifest at
`.claude-plugin/marketplace.json` (interactive `/plugin` menu, or
`claude plugin marketplace add <repo-root>` + `claude plugin install
debug-mcp@debug-mcp`). Note: managed-settings `enabledPlugins` /
`extraKnownMarketplaces` alone do **not** auto-install a directory-sourced plugin
headlessly — they only declare intent — so the container uses `--plugin-dir`,
which actually loads it.

## Behavior notes

- The nudge fires at most once per `(session, kind)` per cooldown
  (`DEBUG_MCP_NUDGE_COOLDOWN`, default 900s), where `kind` is `debugger`
  (driving lldb/gdb) or `runbin` (running a native binary), throttled
  independently.
- A `SessionStart` hook resets the throttle on `startup`/`resume`/`clear`/
  `compact`, so after `/clear` or a context compaction (which wipe the model's
  memory of the earlier nudge) the guidance re-injects on the next qualifying
  command.
- Both hook scripts fail open: any error path exits 0 with no output, so a
  broken hook can never wedge a command or session start.
