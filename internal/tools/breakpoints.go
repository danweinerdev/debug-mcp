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

func (t *Tools) handleRemoveBreakpoint(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: only allow when stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse required breakpoint_id.
	id, err := request.RequireInt("breakpoint_id")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	// 3. Remove from session tracking.
	filePath, wasFunction, err := t.session.RemoveBreakpointByID(id)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to remove breakpoint: %s", err)), nil
	}

	// 4. Send updated breakpoint list to DAP.
	if wasFunction {
		bps := t.session.AllFunctionBreakpoints()
		req := &godap.SetFunctionBreakpointsRequest{}
		req.Type = "request"
		req.Command = "setFunctionBreakpoints"
		req.Arguments = godap.SetFunctionBreakpointsArguments{
			Breakpoints: bps,
		}

		resp, err := t.session.Client().Send(ctx, req)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("setFunctionBreakpoints request failed: %s", err)), nil
		}
		fbpResp, ok := resp.(*godap.SetFunctionBreakpointsResponse)
		if !ok {
			return mcp.NewToolResultError(fmt.Sprintf("unexpected response type: %T", resp)), nil
		}
		if !fbpResp.Success {
			return mcp.NewToolResultError(fmt.Sprintf("setFunctionBreakpoints failed: %s", fbpResp.Message)), nil
		}
	} else {
		bps := t.session.SourceBreakpointsForFile(filePath)
		req := &godap.SetBreakpointsRequest{}
		req.Type = "request"
		req.Command = "setBreakpoints"
		req.Arguments = godap.SetBreakpointsArguments{
			Source:      godap.Source{Path: filePath},
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
	}

	// 5. Return success result.
	result := map[string]any{
		"removed":       true,
		"breakpoint_id": id,
	}
	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

func (t *Tools) handleListBreakpoints(_ context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// No state guard — valid in any state.
	breakpoints := t.session.ListBreakpoints()

	items := make([]map[string]any, 0, len(breakpoints))
	for _, bp := range breakpoints {
		entry := map[string]any{
			"id":       bp.ID,
			"type":     bp.Type,
			"verified": bp.Verified,
		}
		if bp.File != "" {
			entry["file"] = bp.File
		}
		if bp.Line > 0 {
			entry["line"] = bp.Line
		}
		if bp.Function != "" {
			entry["function"] = bp.Function
		}
		if bp.Condition != "" {
			entry["condition"] = bp.Condition
		}
		items = append(items, entry)
	}

	result := map[string]any{
		"breakpoints": items,
		"count":       len(items),
	}
	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

func (t *Tools) handleSetFunctionBreakpoint(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: allow in idle (pending) or stopped.
	if err := t.session.CheckState(session.StateIdle, session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	name, err := request.RequireString("name")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}
	condition := request.GetString("condition", "")

	// 3. If state is idle, add as pending.
	if t.session.State() == session.StateIdle {
		t.session.AddPendingFunctionBreakpoint(name, condition)

		result := map[string]any{
			"status":    "pending",
			"function":  name,
			"condition": condition,
			"message":   "Function breakpoint will be set when program is launched",
		}
		resultJSON, err := json.Marshal(result)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil
	}

	// 4. State is stopped: add and send to DAP.
	t.session.AddFunctionBreakpoint(name, condition)
	bps := t.session.AllFunctionBreakpoints()

	req := &godap.SetFunctionBreakpointsRequest{}
	req.Type = "request"
	req.Command = "setFunctionBreakpoints"
	req.Arguments = godap.SetFunctionBreakpointsArguments{
		Breakpoints: bps,
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("setFunctionBreakpoints request failed: %s", err)), nil
	}

	fbpResp, ok := resp.(*godap.SetFunctionBreakpointsResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected response type: %T", resp)), nil
	}
	if !fbpResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("setFunctionBreakpoints failed: %s", fbpResp.Message)), nil
	}

	// The response breakpoints correspond positionally to the request breakpoints.
	// Our new breakpoint is the last one added.
	var id int
	var verified bool
	var message string

	if len(fbpResp.Body.Breakpoints) > 0 {
		bp := fbpResp.Body.Breakpoints[len(fbpResp.Body.Breakpoints)-1]
		id = bp.Id
		verified = bp.Verified
		message = bp.Message

		t.session.AddBreakpointResponse(session.BreakpointInfo{
			ID:        id,
			Type:      "function",
			Function:  name,
			Condition: condition,
			Verified:  verified,
		})
	}

	if message == "" {
		if verified {
			message = fmt.Sprintf("Breakpoint set on function '%s'", name)
		} else {
			message = fmt.Sprintf("Breakpoint on function '%s' pending verification", name)
		}
	}

	result := map[string]any{
		"breakpoint_id": id,
		"verified":      verified,
		"function":      name,
		"message":       message,
	}
	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
