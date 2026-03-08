package tools

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleDisconnectIdleReturnsError(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleDisconnect should return tool error when idle")
	}
}

func TestHandleDisconnectFromConfiguring(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateConfiguring)
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleDisconnect returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "disconnected" {
		t.Errorf("status: got %q, want %q", data["status"], "disconnected")
	}

	// Session should be reset to idle.
	if sm.State() != session.StateIdle {
		t.Errorf("state after disconnect: got %v, want idle", sm.State())
	}
}

func TestHandleDisconnectFromStopped(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleDisconnect returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "disconnected" {
		t.Errorf("status: got %q, want %q", data["status"], "disconnected")
	}

	if sm.State() != session.StateIdle {
		t.Errorf("state after disconnect: got %v, want idle", sm.State())
	}
}

func TestHandleDisconnectFromRunning(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateRunning)
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleDisconnect returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "disconnected" {
		t.Errorf("status: got %q, want %q", data["status"], "disconnected")
	}

	if sm.State() != session.StateIdle {
		t.Errorf("state after disconnect: got %v, want idle", sm.State())
	}
}

func TestHandleDisconnectFromTerminated(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateTerminated)
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleDisconnect returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["status"] != "disconnected" {
		t.Errorf("status: got %q, want %q", data["status"], "disconnected")
	}

	if sm.State() != session.StateIdle {
		t.Errorf("state after disconnect: got %v, want idle", sm.State())
	}
}

func TestHandleDisconnectResetsSessionState(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateRunning)
	sm.SetProgram("/usr/bin/test")
	sm.SetPID(12345)
	tools := New(sm)

	result, err := tools.handleDisconnect(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleDisconnect returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleDisconnect returned tool error: %v", result)
	}

	// Verify session is fully reset.
	if sm.State() != session.StateIdle {
		t.Errorf("state: got %v, want idle", sm.State())
	}
	if sm.Program() != "" {
		t.Errorf("program: got %q, want empty", sm.Program())
	}
	if sm.PID() != 0 {
		t.Errorf("pid: got %d, want 0", sm.PID())
	}
	if sm.Client() != nil {
		t.Error("client should be nil after disconnect")
	}
	if sm.Subprocess() != nil {
		t.Error("subprocess should be nil after disconnect")
	}
}
