package tools

import (
	"context"
	"encoding/json"
	"fmt"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/dap"
	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleContinue(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Get thread ID from parameter, last stopped event, or default to 1.
	threadID := 1
	if raw, ok := request.GetArguments()["thread_id"]; ok && raw != nil {
		if tid, ok := raw.(float64); ok {
			threadID = int(tid)
		}
	} else if lastEvent := t.session.LastStoppedEvent(); lastEvent != nil {
		threadID = lastEvent.Body.ThreadId
	}

	// 3. Register StopWaiter BEFORE sending request (race-free).
	client := t.session.Client()
	waiterCh := client.StopWaiter().Register()

	// 4. Set state to running.
	t.session.SetState(session.StateRunning)

	// 5. Send ContinueRequest.
	req := &godap.ContinueRequest{}
	req.Type = "request"
	req.Command = "continue"
	req.Arguments = godap.ContinueArguments{
		ThreadId: threadID,
	}

	_, err := client.Send(ctx, req)
	if err != nil {
		// Send failed — revert to stopped state.
		t.session.SetState(session.StateStopped)
		return mcp.NewToolResultError(fmt.Sprintf("continue request failed: %s", err)), nil
	}

	// 6. Block on StopWaiter with context cancellation.
	select {
	case result := <-waiterCh:
		return t.handleStopResult(result)
	case <-ctx.Done():
		return mcp.NewToolResultError("continue timed out; process still running, use 'pause' to stop it"), nil
	}
}

// handleStopResult processes a StopResult from the StopWaiter and returns
// the appropriate MCP tool result. It updates session state and drains
// any buffered output.
func (t *Tools) handleStopResult(result dap.StopResult) (*mcp.CallToolResult, error) {
	switch {
	case result.Event != nil:
		// StoppedEvent — hit breakpoint, step completed, etc.
		t.session.SetState(session.StateStopped)

		entries := t.session.OutputBuffer().Drain()

		resultMap := map[string]any{
			"status":      "stopped",
			"reason":      result.Event.Body.Reason,
			"thread_id":   result.Event.Body.ThreadId,
			"description": result.Event.Body.Description,
		}
		if len(result.Event.Body.HitBreakpointIds) > 0 {
			resultMap["hit_breakpoint_ids"] = result.Event.Body.HitBreakpointIds
		}

		// Merge output entries into result.
		for k, v := range formatOutputEntries(entries) {
			resultMap[k] = v
		}

		resultJSON, err := json.Marshal(resultMap)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil

	case result.Exited:
		// Process exited.
		t.session.SetState(session.StateTerminated)

		entries := t.session.OutputBuffer().Drain()

		resultMap := map[string]any{
			"status": "exited",
		}
		if result.ExitCode != nil {
			resultMap["exit_code"] = *result.ExitCode
		}

		// Merge output entries into result.
		for k, v := range formatOutputEntries(entries) {
			resultMap[k] = v
		}

		resultJSON, err := json.Marshal(resultMap)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil

	case result.Terminated:
		// Connection lost.
		t.session.SetState(session.StateTerminated)

		resultMap := map[string]any{
			"status":  "terminated",
			"message": "Debug session ended",
		}

		resultJSON, err := json.Marshal(resultMap)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil

	default:
		return mcp.NewToolResultError("unexpected stop result"), nil
	}
}
