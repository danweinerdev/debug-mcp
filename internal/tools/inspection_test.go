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
