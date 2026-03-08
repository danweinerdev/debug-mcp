package tools

import (
	"context"
	"strings"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleRunCommandStateGuardRejectsNonStopped(t *testing.T) {
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
				"command": "bt",
			}

			result, err := tools.handleRunCommand(context.Background(), req)
			if err != nil {
				t.Fatalf("handleRunCommand returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleRunCommand should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleRunCommandMissingCommandParameter(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	// No arguments at all.

	result, err := tools.handleRunCommand(context.Background(), req)
	if err != nil {
		t.Fatalf("handleRunCommand returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleRunCommand should return tool error when command is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}

func TestHandleRunCommandMissingCommandParameterEmptyArgs(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{}

	result, err := tools.handleRunCommand(context.Background(), req)
	if err != nil {
		t.Fatalf("handleRunCommand returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleRunCommand should return tool error when command is missing from arguments")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}
