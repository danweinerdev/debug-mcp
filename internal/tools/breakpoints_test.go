package tools

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleSetBreakpointStateGuardRejectsInvalidStates(t *testing.T) {
	tests := []struct {
		name  string
		state session.State
	}{
		{"running", session.StateRunning},
		{"terminated", session.StateTerminated},
		{"configuring", session.StateConfiguring},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			sm := session.NewSessionManager()
			sm.SetState(tc.state)
			tools := New(sm)

			req := mcp.CallToolRequest{}
			req.Params.Arguments = map[string]any{
				"file": "/src/main.c",
				"line": float64(10),
			}

			result, err := tools.handleSetBreakpoint(context.Background(), req)
			if err != nil {
				t.Fatalf("handleSetBreakpoint returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleSetBreakpoint should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleSetBreakpointPendingMode(t *testing.T) {
	sm := session.NewSessionManager()
	// Default state is idle.
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"file": "/src/main.c",
		"line": float64(42),
	}

	result, err := tools.handleSetBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetBreakpoint returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleSetBreakpoint returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "pending" {
		t.Errorf("status: got %q, want %q", data["status"], "pending")
	}
	if data["file"] != "/src/main.c" {
		t.Errorf("file: got %q, want %q", data["file"], "/src/main.c")
	}
	if line, ok := data["line"].(float64); !ok || int(line) != 42 {
		t.Errorf("line: got %v, want 42", data["line"])
	}
	if data["message"] != "Breakpoint will be set when program is launched" {
		t.Errorf("message: got %q, want %q", data["message"], "Breakpoint will be set when program is launched")
	}
}

func TestHandleSetBreakpointPendingModeWithCondition(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"file":      "/src/main.c",
		"line":      float64(10),
		"condition": "i > 5",
	}

	result, err := tools.handleSetBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetBreakpoint returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleSetBreakpoint returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "pending" {
		t.Errorf("status: got %q, want %q", data["status"], "pending")
	}
	if data["condition"] != "i > 5" {
		t.Errorf("condition: got %q, want %q", data["condition"], "i > 5")
	}
}

func TestHandleSetBreakpointMissingFile(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"line": float64(10),
	}

	result, err := tools.handleSetBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetBreakpoint returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleSetBreakpoint should return tool error when file is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text == "" {
		t.Error("error message should not be empty")
	}
}

func TestHandleSetBreakpointMissingLine(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"file": "/src/main.c",
	}

	result, err := tools.handleSetBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetBreakpoint returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleSetBreakpoint should return tool error when line is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text == "" {
		t.Error("error message should not be empty")
	}
}

func TestHandleRemoveBreakpointStateGuardRejectsNonStopped(t *testing.T) {
	tests := []struct {
		name  string
		state session.State
	}{
		{"idle", session.StateIdle},
		{"running", session.StateRunning},
		{"terminated", session.StateTerminated},
		{"configuring", session.StateConfiguring},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			sm := session.NewSessionManager()
			sm.SetState(tc.state)
			tools := New(sm)

			req := mcp.CallToolRequest{}
			req.Params.Arguments = map[string]any{
				"breakpoint_id": float64(1),
			}

			result, err := tools.handleRemoveBreakpoint(context.Background(), req)
			if err != nil {
				t.Fatalf("handleRemoveBreakpoint returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleRemoveBreakpoint should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleListBreakpointsEmptyList(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}

	result, err := tools.handleListBreakpoints(context.Background(), req)
	if err != nil {
		t.Fatalf("handleListBreakpoints returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleListBreakpoints returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	count, ok := data["count"].(float64)
	if !ok || int(count) != 0 {
		t.Errorf("count: got %v, want 0", data["count"])
	}

	bps, ok := data["breakpoints"].([]any)
	if !ok {
		t.Fatalf("breakpoints: expected array, got %T", data["breakpoints"])
	}
	if len(bps) != 0 {
		t.Errorf("breakpoints: got %d items, want 0", len(bps))
	}
}

func TestHandleListBreakpointsWithBreakpoints(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	// Add some breakpoint responses directly to the session manager.
	sm.AddBreakpointResponse(session.BreakpointInfo{
		ID:       1,
		Type:     "source",
		File:     "/src/main.c",
		Line:     42,
		Verified: true,
	})
	sm.AddBreakpointResponse(session.BreakpointInfo{
		ID:        2,
		Type:      "function",
		Function:  "main",
		Condition: "argc > 1",
		Verified:  false,
	})

	req := mcp.CallToolRequest{}

	result, err := tools.handleListBreakpoints(context.Background(), req)
	if err != nil {
		t.Fatalf("handleListBreakpoints returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleListBreakpoints returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	count, ok := data["count"].(float64)
	if !ok || int(count) != 2 {
		t.Errorf("count: got %v, want 2", data["count"])
	}

	bps, ok := data["breakpoints"].([]any)
	if !ok {
		t.Fatalf("breakpoints: expected array, got %T", data["breakpoints"])
	}
	if len(bps) != 2 {
		t.Fatalf("breakpoints: got %d items, want 2", len(bps))
	}

	// Breakpoints should be sorted by ID.
	bp1 := bps[0].(map[string]any)
	if id, ok := bp1["id"].(float64); !ok || int(id) != 1 {
		t.Errorf("bp1 id: got %v, want 1", bp1["id"])
	}
	if bp1["type"] != "source" {
		t.Errorf("bp1 type: got %v, want source", bp1["type"])
	}
	if bp1["file"] != "/src/main.c" {
		t.Errorf("bp1 file: got %v, want /src/main.c", bp1["file"])
	}
	if line, ok := bp1["line"].(float64); !ok || int(line) != 42 {
		t.Errorf("bp1 line: got %v, want 42", bp1["line"])
	}
	if bp1["verified"] != true {
		t.Errorf("bp1 verified: got %v, want true", bp1["verified"])
	}
	// Condition not set, should not be present.
	if _, exists := bp1["condition"]; exists {
		t.Errorf("bp1 condition: should not be present, got %v", bp1["condition"])
	}

	bp2 := bps[1].(map[string]any)
	if id, ok := bp2["id"].(float64); !ok || int(id) != 2 {
		t.Errorf("bp2 id: got %v, want 2", bp2["id"])
	}
	if bp2["type"] != "function" {
		t.Errorf("bp2 type: got %v, want function", bp2["type"])
	}
	if bp2["function"] != "main" {
		t.Errorf("bp2 function: got %v, want main", bp2["function"])
	}
	if bp2["condition"] != "argc > 1" {
		t.Errorf("bp2 condition: got %v, want 'argc > 1'", bp2["condition"])
	}
	if bp2["verified"] != false {
		t.Errorf("bp2 verified: got %v, want false", bp2["verified"])
	}
	// File and line not set for function breakpoint, should not be present.
	if _, exists := bp2["file"]; exists {
		t.Errorf("bp2 file: should not be present, got %v", bp2["file"])
	}
	if _, exists := bp2["line"]; exists {
		t.Errorf("bp2 line: should not be present, got %v", bp2["line"])
	}
}
