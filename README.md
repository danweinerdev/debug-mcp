# lldb-debug-mcp

An MCP (Model Context Protocol) server that gives AI agents interactive LLDB debugging capabilities. Built in Go, communicates with `lldb-dap` via the Debug Adapter Protocol.

## Requirements

- Go 1.22+
- `lldb-dap` (or `lldb-vscode`) binary

### Installing lldb-dap

**macOS** (Xcode Command Line Tools):
```bash
# lldb-dap ships with Xcode
xcode-select --install
```

**Ubuntu/Debian**:
```bash
sudo apt install lldb
```

**Fedora**:
```bash
sudo dnf install lldb
```

**Arch Linux**:
```bash
sudo pacman -S lldb
```

The server auto-detects the `lldb-dap` binary using this fallback chain:

1. `LLDB_DAP_PATH` environment variable (if set, use directly)
2. `lldb-dap` in PATH
3. `lldb-dap-{20,19,18,17,16,15}` in PATH (versioned binaries)
4. `lldb-vscode` in PATH (older LLVM versions)
5. macOS only: `xcrun --find lldb-dap`

Set the `LLDB_DAP_PATH` environment variable to specify an explicit path to the binary if auto-detection does not find it.

## Installation

```bash
go install github.com/danweinerdev/lldb-debug-mcp/cmd/lldb-debug-mcp@latest
```

Or build from source:
```bash
go build -o lldb-debug-mcp ./cmd/lldb-debug-mcp
```

## MCP Client Configuration

### Claude Code

Add to your Claude Code MCP settings:
```json
{
  "mcpServers": {
    "lldb-debug": {
      "command": "/path/to/lldb-debug-mcp"
    }
  }
}
```

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%/Claude/claude_desktop_config.json` (Windows):
```json
{
  "mcpServers": {
    "lldb-debug": {
      "command": "/path/to/lldb-debug-mcp"
    }
  }
}
```

## Tools

### Session Management
| Tool | Description |
|------|-------------|
| `launch` | Launch a program under the debugger |
| `attach` | Attach to a running process |
| `disconnect` | Disconnect from the debug session |

### Breakpoints
| Tool | Description |
|------|-------------|
| `set_breakpoint` | Set a source-line breakpoint |
| `set_function_breakpoint` | Set a breakpoint on a function by name |
| `remove_breakpoint` | Remove a breakpoint by ID |
| `list_breakpoints` | List all current breakpoints |

### Execution Control
| Tool | Description |
|------|-------------|
| `continue` | Continue execution |
| `step_over` | Step over the current line |
| `step_into` | Step into a function call |
| `step_out` | Step out of the current function |
| `pause` | Pause all threads |

### Inspection
| Tool | Description |
|------|-------------|
| `status` | Get debug session status |
| `backtrace` | Get call stack for a thread |
| `threads` | List all threads |
| `variables` | List variables in scope |
| `evaluate` | Evaluate an expression |
| `read_output` | Read captured program output |

### Advanced
| Tool | Description |
|------|-------------|
| `read_memory` | Read raw memory at an address |
| `disassemble` | Disassemble instructions |
| `run_command` | Run arbitrary LLDB commands |
