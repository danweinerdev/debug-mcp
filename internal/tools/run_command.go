package tools

import (
	"context"
	"encoding/json"
	"fmt"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleRunCommand(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse required command parameter.
	command, err := request.RequireString("command")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	// 3. Build expression based on repl mode.
	expression := command
	if !t.session.ReplModeCommand() {
		// Legacy lldb-vscode: prefix with backtick to force command mode.
		expression = "`" + command
	}

	// 4. Send EvaluateRequest via the DAP client.
	req := &godap.EvaluateRequest{}
	req.Type = "request"
	req.Command = "evaluate"
	req.Arguments = godap.EvaluateArguments{
		Expression: expression,
		Context:    "repl",
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("run_command request failed: %s", err)), nil
	}

	// 5. Parse response.
	evalResp, ok := resp.(*godap.EvaluateResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected evaluate response type: %T", resp)), nil
	}
	if !evalResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("command failed: %s", evalResp.Message)), nil
	}

	result := map[string]any{
		"result": evalResp.Body.Result,
		"type":   evalResp.Body.Type,
	}
	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
