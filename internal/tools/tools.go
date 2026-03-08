// Package tools registers MCP tool definitions and their handler methods
// for the LLDB debug server.
package tools

import (
	"context"

	"github.com/mark3labs/mcp-go/mcp"
	"github.com/mark3labs/mcp-go/server"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

// Tools holds the session manager and provides MCP tool handler methods.
type Tools struct {
	session *session.SessionManager
}

// New creates a new Tools instance bound to the given SessionManager.
func New(session *session.SessionManager) *Tools {
	return &Tools{session: session}
}

// Register adds all debug tool definitions and their handlers to the MCP server.
func (t *Tools) Register(s *server.MCPServer) {
	// Session management
	s.AddTool(mcp.NewTool("launch",
		mcp.WithDescription("Launch a program under the debugger"),
		mcp.WithString("program", mcp.Required(), mcp.Description("Path to the executable to debug")),
		mcp.WithString("args", mcp.Description("JSON array of command-line arguments")),
		mcp.WithString("cwd", mcp.Description("Working directory for the launched program")),
		mcp.WithString("env", mcp.Description("JSON object of environment variables")),
		mcp.WithBoolean("stop_on_entry", mcp.Description("Stop at program entry point (default true)")),
	), t.handleLaunch)

	s.AddTool(mcp.NewTool("attach",
		mcp.WithDescription("Attach the debugger to a running process"),
		mcp.WithNumber("pid", mcp.Description("Process ID to attach to")),
		mcp.WithString("wait_for", mcp.Description("Process name to wait for")),
	), t.handleAttach)

	s.AddTool(mcp.NewTool("disconnect",
		mcp.WithDescription("Disconnect from the debug session"),
		mcp.WithBoolean("terminate", mcp.Description("Terminate the debuggee (default true)")),
	), t.handleDisconnect)

	// Breakpoints
	s.AddTool(mcp.NewTool("set_breakpoint",
		mcp.WithDescription("Set a source-line breakpoint"),
		mcp.WithString("file", mcp.Required(), mcp.Description("Source file path")),
		mcp.WithNumber("line", mcp.Required(), mcp.Description("Line number")),
		mcp.WithString("condition", mcp.Description("Conditional expression for the breakpoint")),
	), t.handleSetBreakpoint)

	s.AddTool(mcp.NewTool("set_function_breakpoint",
		mcp.WithDescription("Set a breakpoint on a function by name"),
		mcp.WithString("name", mcp.Required(), mcp.Description("Function name")),
		mcp.WithString("condition", mcp.Description("Conditional expression for the breakpoint")),
	), t.handleSetFunctionBreakpoint)

	s.AddTool(mcp.NewTool("remove_breakpoint",
		mcp.WithDescription("Remove a breakpoint by ID"),
		mcp.WithNumber("breakpoint_id", mcp.Required(), mcp.Description("Breakpoint ID to remove")),
	), t.handleRemoveBreakpoint)

	s.AddTool(mcp.NewTool("list_breakpoints",
		mcp.WithDescription("List all current breakpoints"),
	), t.handleListBreakpoints)

	// Execution control
	s.AddTool(mcp.NewTool("continue",
		mcp.WithDescription("Continue execution of the paused program"),
		mcp.WithNumber("thread_id", mcp.Description("Thread ID to continue (optional)")),
	), t.handleContinue)

	s.AddTool(mcp.NewTool("step_over",
		mcp.WithDescription("Step over the current line or instruction"),
		mcp.WithNumber("thread_id", mcp.Description("Thread ID to step (optional)")),
		mcp.WithString("granularity", mcp.Description("Step granularity"), mcp.Enum("line", "instruction")),
	), t.handleStepOver)

	s.AddTool(mcp.NewTool("step_into",
		mcp.WithDescription("Step into the current line or instruction"),
		mcp.WithNumber("thread_id", mcp.Description("Thread ID to step (optional)")),
		mcp.WithString("granularity", mcp.Description("Step granularity"), mcp.Enum("line", "instruction")),
	), t.handleStepInto)

	s.AddTool(mcp.NewTool("step_out",
		mcp.WithDescription("Step out of the current function"),
		mcp.WithNumber("thread_id", mcp.Description("Thread ID to step out (optional)")),
	), t.handleStepOut)

	s.AddTool(mcp.NewTool("pause",
		mcp.WithDescription("Pause all threads in the running program"),
	), t.handlePause)

	// Inspection
	s.AddTool(mcp.NewTool("status",
		mcp.WithDescription("Get the current debug session status"),
	), t.handleStatus)

	s.AddTool(mcp.NewTool("backtrace",
		mcp.WithDescription("Get the call stack for a thread"),
		mcp.WithNumber("thread_id", mcp.Description("Thread ID (uses stopped thread if omitted)")),
		mcp.WithNumber("levels", mcp.Description("Maximum number of stack frames to return")),
	), t.handleBacktrace)

	s.AddTool(mcp.NewTool("threads",
		mcp.WithDescription("List all threads in the debugged process"),
	), t.handleThreads)

	s.AddTool(mcp.NewTool("variables",
		mcp.WithDescription("List variables in the current scope"),
		mcp.WithNumber("frame_index", mcp.Description("Stack frame index (default 0)")),
		mcp.WithString("scope", mcp.Description("Variable scope"), mcp.Enum("local", "global", "register")),
		mcp.WithNumber("depth", mcp.Description("Maximum depth for nested structures")),
		mcp.WithString("filter", mcp.Description("Filter variables by name pattern")),
	), t.handleVariables)

	s.AddTool(mcp.NewTool("evaluate",
		mcp.WithDescription("Evaluate an expression in the debugger"),
		mcp.WithString("expression", mcp.Required(), mcp.Description("Expression to evaluate")),
		mcp.WithNumber("frame_index", mcp.Description("Stack frame index for evaluation context")),
	), t.handleEvaluate)

	s.AddTool(mcp.NewTool("read_output",
		mcp.WithDescription("Read captured program output (stdout, stderr, console)"),
	), t.handleReadOutput)

	// Advanced
	s.AddTool(mcp.NewTool("read_memory",
		mcp.WithDescription("Read raw memory at a given address"),
		mcp.WithString("address", mcp.Required(), mcp.Description("Memory address (hex string, e.g. 0x1000)")),
		mcp.WithNumber("count", mcp.Required(), mcp.Description("Number of bytes to read")),
	), t.handleReadMemory)

	s.AddTool(mcp.NewTool("disassemble",
		mcp.WithDescription("Disassemble instructions at an address or the current PC"),
		mcp.WithString("address", mcp.Description("Start address (hex string, uses current PC if omitted)")),
		mcp.WithNumber("instruction_count", mcp.Description("Number of instructions to disassemble")),
	), t.handleDisassemble)

	s.AddTool(mcp.NewTool("run_command",
		mcp.WithDescription("Run an LLDB command directly via the debug console"),
		mcp.WithString("command", mcp.Required(), mcp.Description("LLDB command string to execute")),
	), t.handleRunCommand)
}

// --- Session management handlers ---

// handleAttach is implemented in attach.go.

// --- Breakpoint handlers ---

// handleSetBreakpoint is implemented in breakpoints.go.
// handleSetFunctionBreakpoint is implemented in breakpoints.go.

// handleRemoveBreakpoint is implemented in breakpoints.go.
// handleListBreakpoints is implemented in breakpoints.go.

// --- Execution control handlers ---

// handleContinue is implemented in execution.go.
// handleStepOver is implemented in execution.go.
// handleStepInto is implemented in execution.go.
// handleStepOut is implemented in execution.go.
// handlePause is implemented in execution.go.

// --- Inspection handlers ---

// handleThreads is implemented in inspection.go.
// handleBacktrace is implemented in inspection.go.
// handleVariables is implemented in inspection.go.
// handleEvaluate is implemented in inspection.go.

// handleReadOutput is implemented in output.go.

// --- Advanced handlers ---

func (t *Tools) handleReadMemory(_ context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	return mcp.NewToolResultError("not implemented"), nil
}

func (t *Tools) handleDisassemble(_ context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	return mcp.NewToolResultError("not implemented"), nil
}

// handleRunCommand is implemented in run_command.go.
