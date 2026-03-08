package tools

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleBacktrace(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Resolve thread ID from parameter, last stopped event, or default to 1.
	threadID := 1
	if raw, ok := request.GetArguments()["thread_id"]; ok && raw != nil {
		if tid, ok := raw.(float64); ok {
			threadID = int(tid)
		}
	} else if lastEvent := t.session.LastStoppedEvent(); lastEvent != nil {
		threadID = lastEvent.Body.ThreadId
	}

	// 3. Parse levels parameter (optional, default 20).
	levels := 20
	if raw, ok := request.GetArguments()["levels"]; ok && raw != nil {
		if l, ok := raw.(float64); ok && l > 0 {
			levels = int(l)
		}
	}

	// 4. Send StackTraceRequest.
	req := &godap.StackTraceRequest{}
	req.Type = "request"
	req.Command = "stackTrace"
	req.Arguments = godap.StackTraceArguments{
		ThreadId:   threadID,
		StartFrame: 0,
		Levels:     levels,
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("stackTrace request failed: %s", err)), nil
	}

	// 5. Parse response.
	stackResp, ok := resp.(*godap.StackTraceResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected stackTrace response type: %T", resp)), nil
	}
	if !stackResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("stackTrace failed: %s", stackResp.Message)), nil
	}

	// 6. Update frame mapping: store frame index -> DAP frame ID.
	frameMapping := make(map[int]int)
	for i, frame := range stackResp.Body.StackFrames {
		frameMapping[i] = frame.Id
	}
	t.session.SetFrameMapping(frameMapping)

	// 7. Format frames.
	frames := make([]map[string]any, 0, len(stackResp.Body.StackFrames))
	for i, frame := range stackResp.Body.StackFrames {
		frameInfo := map[string]any{
			"index": i,
			"name":  frame.Name,
			"id":    frame.Id,
		}
		if frame.Source != nil && frame.Source.Path != "" {
			frameInfo["file"] = frame.Source.Path
			frameInfo["line"] = frame.Line
		}
		if frame.InstructionPointerReference != "" {
			frameInfo["address"] = frame.InstructionPointerReference
		}
		frames = append(frames, frameInfo)
	}

	// 8. Return JSON.
	resultMap := map[string]any{
		"frames":       frames,
		"total_frames": stackResp.Body.TotalFrames,
		"thread_id":    threadID,
	}

	resultJSON, err := json.Marshal(resultMap)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

func (t *Tools) handleEvaluate(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	expression, err := request.RequireString("expression")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	// 3. Parse optional frame_index (default 0).
	frameIndex := 0
	if raw, ok := request.GetArguments()["frame_index"]; ok && raw != nil {
		if fi, ok := raw.(float64); ok {
			frameIndex = int(fi)
		}
	}

	// 4. Resolve frame ID from session frame mapping.
	frameMapping := t.session.FrameMapping()
	frameID, ok := frameMapping[frameIndex]
	if !ok {
		// If no mapping exists, use frameIndex as-is.
		frameID = frameIndex
	}

	// 5. Send EvaluateRequest.
	evalReq := &godap.EvaluateRequest{}
	evalReq.Type = "request"
	evalReq.Command = "evaluate"
	evalReq.Arguments = godap.EvaluateArguments{
		Expression: expression,
		FrameId:    frameID,
		Context:    "variables",
	}

	resp, err := t.session.Client().Send(ctx, evalReq)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("evaluate request failed: %s", err)), nil
	}

	// 6. Parse response.
	evalResp, ok := resp.(*godap.EvaluateResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected evaluate response type: %T", resp)), nil
	}
	if !evalResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("evaluate failed: %s", evalResp.Message)), nil
	}

	// 7. Return JSON.
	result := map[string]any{
		"result": evalResp.Body.Result,
		"type":   evalResp.Body.Type,
	}
	if evalResp.Body.VariablesReference > 0 {
		result["has_children"] = true
		result["variables_reference"] = evalResp.Body.VariablesReference
	}

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

func (t *Tools) handleVariables(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	frameIndex := 0
	if raw, ok := request.GetArguments()["frame_index"]; ok && raw != nil {
		if fi, ok := raw.(float64); ok {
			frameIndex = int(fi)
		}
	}

	scope := "local"
	if raw, ok := request.GetArguments()["scope"]; ok && raw != nil {
		if s, ok := raw.(string); ok && s != "" {
			scope = s
		}
	}

	// Default depth: 2 for local/register, 1 for global.
	depth := 2
	if scope == "global" {
		depth = 1
	}
	if raw, ok := request.GetArguments()["depth"]; ok && raw != nil {
		if d, ok := raw.(float64); ok && d >= 0 {
			depth = int(d)
		}
	}

	filter := ""
	if raw, ok := request.GetArguments()["filter"]; ok && raw != nil {
		if f, ok := raw.(string); ok {
			filter = f
		}
	}

	// 3. Resolve frame ID from session frame mapping.
	frameMapping := t.session.FrameMapping()
	frameID, ok := frameMapping[frameIndex]
	if !ok {
		frameID = frameIndex
	}

	// 4. Send ScopesRequest.
	scopesReq := &godap.ScopesRequest{}
	scopesReq.Type = "request"
	scopesReq.Command = "scopes"
	scopesReq.Arguments = godap.ScopesArguments{
		FrameId: frameID,
	}

	resp, err := t.session.Client().Send(ctx, scopesReq)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("scopes request failed: %s", err)), nil
	}

	// 5. Parse ScopesResponse.
	scopesResp, ok := resp.(*godap.ScopesResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected scopes response type: %T", resp)), nil
	}
	if !scopesResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("scopes failed: %s", scopesResp.Message)), nil
	}

	// Find the matching scope.
	var targetScope *godap.Scope
	for i := range scopesResp.Body.Scopes {
		s := &scopesResp.Body.Scopes[i]
		switch scope {
		case "local":
			if strings.EqualFold(s.Name, "Locals") || strings.EqualFold(s.Name, "Local") {
				targetScope = s
			}
		case "global":
			if strings.EqualFold(s.Name, "Globals") || strings.EqualFold(s.Name, "Global") {
				targetScope = s
			}
		case "register":
			if strings.EqualFold(s.Name, "Registers") || strings.EqualFold(s.Name, "Register") {
				targetScope = s
			}
		}
		if targetScope != nil {
			break
		}
	}

	if targetScope == nil {
		return mcp.NewToolResultError(fmt.Sprintf("scope '%s' not found in frame %d", scope, frameIndex)), nil
	}

	// 6. Call FlattenVariables.
	maxCount := 100
	vars, truncated, err := FlattenVariables(ctx, t.session.Client(), targetScope.VariablesReference, depth, maxCount, filter)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to fetch variables: %s", err)), nil
	}

	// 7. Return JSON.
	result := map[string]any{
		"variables": vars,
		"count":     len(vars),
		"scope":     scope,
		"truncated": truncated,
	}

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

func (t *Tools) handleThreads(ctx context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Send ThreadsRequest.
	req := &godap.ThreadsRequest{}
	req.Type = "request"
	req.Command = "threads"

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("threads request failed: %s", err)), nil
	}

	// 3. Parse response.
	threadsResp, ok := resp.(*godap.ThreadsResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected threads response type: %T", resp)), nil
	}
	if !threadsResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("threads failed: %s", threadsResp.Message)), nil
	}

	// 4. Get stopped thread ID from last stopped event (may be nil).
	lastEvent := t.session.LastStoppedEvent()

	// 5. Format threads.
	var stoppedThreadID *int
	threads := make([]map[string]any, 0, len(threadsResp.Body.Threads))
	for _, th := range threadsResp.Body.Threads {
		thread := map[string]any{
			"id":   th.Id,
			"name": th.Name,
		}
		if lastEvent != nil && th.Id == lastEvent.Body.ThreadId {
			thread["is_stopped"] = true
			thread["is_current"] = true
			id := th.Id
			stoppedThreadID = &id
		}
		threads = append(threads, thread)
	}

	// 6. Return JSON.
	resultMap := map[string]any{
		"threads": threads,
		"count":   len(threads),
	}
	if stoppedThreadID != nil {
		resultMap["stopped_thread_id"] = *stoppedThreadID
	}

	resultJSON, err := json.Marshal(resultMap)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
