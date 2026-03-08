# CLAUDE.md

## Project: lldb-debug-mcp

MCP server that wraps lldb-dap via the Debug Adapter Protocol, exposing LLDB debugging capabilities as MCP tools for AI agents.

## Build

```bash
go build -o lldb-debug-mcp ./cmd/lldb-debug-mcp
```

## Test

```bash
# Unit tests
go test -race ./...

# Integration tests (requires lldb-dap)
make -C testdata
go test -tags integration -race ./internal/tools/ -v
```

## Architecture

```
AI Agent <-stdio/MCP-> [Go MCP Server (mcp-go)] <-stdio/DAP-> [lldb-dap] <-SB API-> [Target]
```

Three internal layers:
1. **MCP Tool Handlers** (`internal/tools/`) -- parameter validation, state guards, response formatting
2. **Session Manager** (`internal/session/`) -- session state machine, breakpoint tracking, output buffering
3. **DAP Client** (`internal/dap/`) -- message framing, request/response correlation, async event dispatch

## Code Conventions

- All tool handlers return `(*mcp.CallToolResult, error)` -- use `mcp.NewToolResultError()` for user errors
- State guards check session state before executing; handlers must call `t.session.CheckState(...)` first
- StopWaiter pattern: register before sending DAP request, block on channel after
- `godap` import alias for `github.com/google/go-dap`
- Integration tests use `//go:build integration` tag
