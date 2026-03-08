package tools

import (
	"context"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleAttachStateGuardNotIdle(t *testing.T) {
	tests := []struct {
		name  string
		state session.State
	}{
		{"configuring", session.StateConfiguring},
		{"stopped", session.StateStopped},
		{"running", session.StateRunning},
		{"terminated", session.StateTerminated},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			sm := session.NewSessionManager()
			sm.SetState(tc.state)
			tools := New(sm)

			result, err := tools.handleAttach(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleAttach returned error: %v", err)
			}
			if !result.IsError {
				t.Fatal("handleAttach should return tool error when not idle")
			}
		})
	}
}

func TestHandleAttachMissingParameters(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	// No pid or wait_for provided.
	result, err := tools.handleAttach(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleAttach returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleAttach should return tool error when neither pid nor wait_for is provided")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text != "either 'pid' or 'wait_for' must be provided" {
		t.Errorf("error message: got %q, want %q", text, "either 'pid' or 'wait_for' must be provided")
	}
}

func TestHandleAttachInvalidPid(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"pid": "not-a-number",
	}

	result, err := tools.handleAttach(context.Background(), req)
	if err != nil {
		t.Fatalf("handleAttach returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleAttach should return tool error for invalid pid type")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text != "'pid' must be a number" {
		t.Errorf("error message: got %q, want %q", text, "'pid' must be a number")
	}
}

func TestHandleAttachNegativePid(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"pid": float64(-1),
	}

	result, err := tools.handleAttach(context.Background(), req)
	if err != nil {
		t.Fatalf("handleAttach returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleAttach should return tool error for negative pid")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text != "'pid' must be a positive integer" {
		t.Errorf("error message: got %q, want %q", text, "'pid' must be a positive integer")
	}
}

func TestHandleAttachZeroPid(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"pid": float64(0),
	}

	result, err := tools.handleAttach(context.Background(), req)
	if err != nil {
		t.Fatalf("handleAttach returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleAttach should return tool error for zero pid")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text != "'pid' must be a positive integer" {
		t.Errorf("error message: got %q, want %q", text, "'pid' must be a positive integer")
	}
}

func TestHandleAttachEmptyWaitFor(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"wait_for": "",
	}

	result, err := tools.handleAttach(context.Background(), req)
	if err != nil {
		t.Fatalf("handleAttach returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleAttach should return tool error for empty wait_for")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if text != "'wait_for' must be a non-empty string" {
		t.Errorf("error message: got %q, want %q", text, "'wait_for' must be a non-empty string")
	}
}
