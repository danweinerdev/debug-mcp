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
