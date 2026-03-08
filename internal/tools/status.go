package tools

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleStatus(_ context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// No state guard -- valid in any state.

	state := t.session.State()
	result := map[string]any{
		"state": state.String(),
	}

	switch state {
	case session.StateIdle:
		result["message"] = "No active debug session"

	case session.StateConfiguring:
		result["message"] = "Debug session is being configured"

	case session.StateStopped:
		result["program"] = t.session.Program()
		result["pid"] = t.session.PID()
		if event := t.session.LastStoppedEvent(); event != nil {
			result["stop_reason"] = event.Body.Reason
			result["stopped_thread_id"] = event.Body.ThreadId
			if event.Body.Text != "" {
				result["stop_description"] = event.Body.Text
			}
			if len(event.Body.HitBreakpointIds) > 0 {
				result["hit_breakpoint_ids"] = event.Body.HitBreakpointIds
			}
		}

	case session.StateRunning:
		result["program"] = t.session.Program()
		result["pid"] = t.session.PID()

	case session.StateTerminated:
		result["program"] = t.session.Program()
		if exitCode := t.session.ExitCode(); exitCode != nil {
			result["exit_code"] = *exitCode
		}
	}

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal status: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
