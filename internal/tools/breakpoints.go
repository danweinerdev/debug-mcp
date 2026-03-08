package tools

import (
	"context"
	"encoding/json"
	"fmt"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleSetBreakpoint(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: allow idle (pending) or stopped.
	if err := t.session.CheckState(session.StateIdle, session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	file, err := request.RequireString("file")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	line, err := request.RequireInt("line")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	condition := request.GetString("condition", "")

	// 3. If idle, buffer as pending breakpoint.
	if t.session.State() == session.StateIdle {
		t.session.AddPendingSourceBreakpoint(file, line, condition)

		result := map[string]any{
			"status":    "pending",
			"file":      file,
			"line":      line,
			"condition": condition,
			"message":   "Breakpoint will be set when program is launched",
		}
		resultJSON, err := json.Marshal(result)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil
	}

	// 4. State is stopped: send SetBreakpoints DAP request.
	t.session.AddSourceBreakpoint(file, line, condition)
	bps := t.session.SourceBreakpointsForFile(file)

	req := &godap.SetBreakpointsRequest{}
	req.Type = "request"
	req.Command = "setBreakpoints"
	req.Arguments = godap.SetBreakpointsArguments{
		Source:      godap.Source{Path: file},
		Breakpoints: bps,
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("setBreakpoints request failed: %s", err)), nil
	}

	sbResp, ok := resp.(*godap.SetBreakpointsResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected setBreakpoints response type: %T", resp)), nil
	}
	if !sbResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("setBreakpoints failed: %s", sbResp.Message)), nil
	}

	// Find the breakpoint in the response that matches our line.
	var matchedBP *godap.Breakpoint
	for i := range sbResp.Body.Breakpoints {
		bp := &sbResp.Body.Breakpoints[i]
		if bp.Line == line {
			matchedBP = bp
			break
		}
	}
	// If no exact line match, use the last breakpoint in the response
	// (which corresponds to the one we just added).
	if matchedBP == nil && len(sbResp.Body.Breakpoints) > 0 {
		matchedBP = &sbResp.Body.Breakpoints[len(sbResp.Body.Breakpoints)-1]
	}

	if matchedBP == nil {
		return mcp.NewToolResultError("setBreakpoints response contained no breakpoints"), nil
	}

	// Store the breakpoint info.
	t.session.AddBreakpointResponse(session.BreakpointInfo{
		ID:        matchedBP.Id,
		Type:      "source",
		File:      file,
		Line:      matchedBP.Line,
		Condition: condition,
		Verified:  matchedBP.Verified,
	})

	// Build result.
	result := map[string]any{
		"breakpoint_id": matchedBP.Id,
		"verified":      matchedBP.Verified,
		"file":          file,
		"line":          matchedBP.Line,
	}
	if matchedBP.Message != "" {
		result["message"] = matchedBP.Message
	}

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
