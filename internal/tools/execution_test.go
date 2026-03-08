package tools

import (
	"context"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleContinueStateGuardRejectsIdle(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	result, err := tools.handleContinue(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleContinue should return tool error when idle")
	}
}

func TestHandleContinueStateGuardRejectsRunning(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateRunning)
	tools := New(sm)

	result, err := tools.handleContinue(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleContinue should return tool error when running")
	}
}

func TestHandleContinueStateGuardRejectsTerminated(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateTerminated)
	tools := New(sm)

	result, err := tools.handleContinue(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleContinue should return tool error when terminated")
	}
}

func TestHandleContinueStateGuardRejectsConfiguring(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateConfiguring)
	tools := New(sm)

	result, err := tools.handleContinue(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleContinue should return tool error when configuring")
	}
}

func TestHandleContinueStateGuardRejectsNonStopped(t *testing.T) {
	tests := []struct {
		name  string
		state session.State
	}{
		{"idle", session.StateIdle},
		{"configuring", session.StateConfiguring},
		{"running", session.StateRunning},
		{"terminated", session.StateTerminated},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			sm := session.NewSessionManager()
			sm.SetState(tc.state)
			tools := New(sm)

			result, err := tools.handleContinue(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleContinue returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleContinue should return tool error when state is %s", tc.name)
			}
		})
	}
}
