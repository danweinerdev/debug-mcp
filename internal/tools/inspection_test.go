package tools

import (
	"context"
	"strings"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleBacktraceStateGuardRejectsNonStopped(t *testing.T) {
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

			result, err := tools.handleBacktrace(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleBacktrace returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleBacktrace should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleEvaluateStateGuardRejectsNonStopped(t *testing.T) {
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
				"expression": "x + 1",
			}

			result, err := tools.handleEvaluate(context.Background(), req)
			if err != nil {
				t.Fatalf("handleEvaluate returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleEvaluate should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleEvaluateMissingExpressionParameter(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	// No arguments at all.

	result, err := tools.handleEvaluate(context.Background(), req)
	if err != nil {
		t.Fatalf("handleEvaluate returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleEvaluate should return tool error when expression is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}

func TestHandleEvaluateMissingExpressionParameterEmptyArgs(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{}

	result, err := tools.handleEvaluate(context.Background(), req)
	if err != nil {
		t.Fatalf("handleEvaluate returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleEvaluate should return tool error when expression is missing from arguments")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}

func TestHandleThreadsStateGuardRejectsNonStopped(t *testing.T) {
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

			result, err := tools.handleThreads(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleThreads returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleThreads should return tool error when state is %s", tc.name)
			}
		})
	}
}
