package tools

import (
	"context"
	"encoding/json"
	"fmt"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

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
