package tools

import (
	"context"
	"encoding/json"
	"testing"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleStatusIdle(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "idle" {
		t.Errorf("state: got %q, want %q", data["state"], "idle")
	}
	if data["message"] != "No active debug session" {
		t.Errorf("message: got %q, want %q", data["message"], "No active debug session")
	}
}

func TestHandleStatusConfiguring(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateConfiguring)
	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "configuring" {
		t.Errorf("state: got %q, want %q", data["state"], "configuring")
	}
	if data["message"] != "Debug session is being configured" {
		t.Errorf("message: got %q, want %q", data["message"], "Debug session is being configured")
	}
}

func TestHandleStatusStopped(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	sm.SetProgram("/usr/bin/test")
	sm.SetPID(12345)

	event := &godap.StoppedEvent{}
	event.Body = godap.StoppedEventBody{
		Reason:            "breakpoint",
		ThreadId:          1,
		Text:              "stopped at breakpoint 3",
		AllThreadsStopped: true,
		HitBreakpointIds:  []int{3},
	}
	sm.SetLastStoppedEvent(event)

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "stopped" {
		t.Errorf("state: got %q, want %q", data["state"], "stopped")
	}
	if data["program"] != "/usr/bin/test" {
		t.Errorf("program: got %q, want %q", data["program"], "/usr/bin/test")
	}
	if pid, ok := data["pid"].(float64); !ok || int(pid) != 12345 {
		t.Errorf("pid: got %v, want 12345", data["pid"])
	}
	if data["stop_reason"] != "breakpoint" {
		t.Errorf("stop_reason: got %q, want %q", data["stop_reason"], "breakpoint")
	}
	if threadId, ok := data["stopped_thread_id"].(float64); !ok || int(threadId) != 1 {
		t.Errorf("stopped_thread_id: got %v, want 1", data["stopped_thread_id"])
	}
	if data["stop_description"] != "stopped at breakpoint 3" {
		t.Errorf("stop_description: got %q, want %q", data["stop_description"], "stopped at breakpoint 3")
	}

	bpIds, ok := data["hit_breakpoint_ids"].([]any)
	if !ok || len(bpIds) != 1 {
		t.Fatalf("hit_breakpoint_ids: got %v, want [3]", data["hit_breakpoint_ids"])
	}
	if id, ok := bpIds[0].(float64); !ok || int(id) != 3 {
		t.Errorf("hit_breakpoint_ids[0]: got %v, want 3", bpIds[0])
	}
}

func TestHandleStatusStoppedNoEvent(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	sm.SetProgram("/usr/bin/test")
	sm.SetPID(42)
	// No LastStoppedEvent set.

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "stopped" {
		t.Errorf("state: got %q, want %q", data["state"], "stopped")
	}
	if data["program"] != "/usr/bin/test" {
		t.Errorf("program: got %q, want %q", data["program"], "/usr/bin/test")
	}
	if pid, ok := data["pid"].(float64); !ok || int(pid) != 42 {
		t.Errorf("pid: got %v, want 42", data["pid"])
	}
	// stop_reason should not be present when there's no event.
	if _, ok := data["stop_reason"]; ok {
		t.Errorf("stop_reason should not be present without a stopped event")
	}
}

func TestHandleStatusStoppedMinimalEvent(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	sm.SetProgram("/usr/bin/test")
	sm.SetPID(99)

	// Event with empty Text and no HitBreakpointIds.
	event := &godap.StoppedEvent{}
	event.Body = godap.StoppedEventBody{
		Reason:   "step",
		ThreadId: 2,
	}
	sm.SetLastStoppedEvent(event)

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["stop_reason"] != "step" {
		t.Errorf("stop_reason: got %q, want %q", data["stop_reason"], "step")
	}
	// Text is empty, so stop_description should be absent.
	if _, ok := data["stop_description"]; ok {
		t.Errorf("stop_description should not be present when Text is empty")
	}
	// No hit breakpoints, so hit_breakpoint_ids should be absent.
	if _, ok := data["hit_breakpoint_ids"]; ok {
		t.Errorf("hit_breakpoint_ids should not be present when empty")
	}
}

func TestHandleStatusRunning(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateRunning)
	sm.SetProgram("/usr/bin/myapp")
	sm.SetPID(5678)

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "running" {
		t.Errorf("state: got %q, want %q", data["state"], "running")
	}
	if data["program"] != "/usr/bin/myapp" {
		t.Errorf("program: got %q, want %q", data["program"], "/usr/bin/myapp")
	}
	if pid, ok := data["pid"].(float64); !ok || int(pid) != 5678 {
		t.Errorf("pid: got %v, want 5678", data["pid"])
	}
}

func TestHandleStatusTerminatedWithExitCode(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateTerminated)
	sm.SetProgram("/usr/bin/done")
	sm.SetExitCode(42)

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleStatus returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "terminated" {
		t.Errorf("state: got %q, want %q", data["state"], "terminated")
	}
	if data["program"] != "/usr/bin/done" {
		t.Errorf("program: got %q, want %q", data["program"], "/usr/bin/done")
	}
	if exitCode, ok := data["exit_code"].(float64); !ok || int(exitCode) != 42 {
		t.Errorf("exit_code: got %v, want 42", data["exit_code"])
	}
}

func TestHandleStatusTerminatedNoExitCode(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateTerminated)
	sm.SetProgram("/usr/bin/crashed")
	// No exit code set.

	tools := New(sm)

	result, err := tools.handleStatus(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleStatus returned error: %v", err)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["state"] != "terminated" {
		t.Errorf("state: got %q, want %q", data["state"], "terminated")
	}
	if _, ok := data["exit_code"]; ok {
		t.Errorf("exit_code should not be present when not set")
	}
}
