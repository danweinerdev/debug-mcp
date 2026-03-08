package tools

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleSetFunctionBreakpointStateGuardRejectsInvalidStates(t *testing.T) {
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
				"name": "main",
			}

			result, err := tools.handleSetFunctionBreakpoint(context.Background(), req)
			if err != nil {
				t.Fatalf("handleSetFunctionBreakpoint returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleSetFunctionBreakpoint should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleSetFunctionBreakpointPendingMode(t *testing.T) {
	sm := session.NewSessionManager()
	// Default state is idle.
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"name": "main",
	}

	result, err := tools.handleSetFunctionBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetFunctionBreakpoint returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleSetFunctionBreakpoint returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "pending" {
		t.Errorf("status: got %q, want %q", data["status"], "pending")
	}
	if data["function"] != "main" {
		t.Errorf("function: got %q, want %q", data["function"], "main")
	}
	if data["condition"] != "" {
		t.Errorf("condition: got %q, want empty string", data["condition"])
	}
	if data["message"] != "Function breakpoint will be set when program is launched" {
		t.Errorf("message: got %q, want %q", data["message"], "Function breakpoint will be set when program is launched")
	}
}

func TestHandleSetFunctionBreakpointPendingModeWithCondition(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"name":      "calculate",
		"condition": "x > 10",
	}

	result, err := tools.handleSetFunctionBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetFunctionBreakpoint returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleSetFunctionBreakpoint returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "pending" {
		t.Errorf("status: got %q, want %q", data["status"], "pending")
	}
	if data["function"] != "calculate" {
		t.Errorf("function: got %q, want %q", data["function"], "calculate")
	}
	if data["condition"] != "x > 10" {
		t.Errorf("condition: got %q, want %q", data["condition"], "x > 10")
	}
}

func TestHandleSetFunctionBreakpointMissingName(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	// No name parameter provided.
	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{}

	result, err := tools.handleSetFunctionBreakpoint(context.Background(), req)
	if err != nil {
		t.Fatalf("handleSetFunctionBreakpoint returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleSetFunctionBreakpoint should return tool error when name is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text == "" {
		t.Error("error message should not be empty")
	}
}
