package tools

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleReadOutputStateGuardRejectsIdle(t *testing.T) {
	sm := session.NewSessionManager()
	tools := New(sm)

	result, err := tools.handleReadOutput(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleReadOutput returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleReadOutput should return tool error when idle")
	}
}

func TestHandleReadOutputEmptyBuffer(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	result, err := tools.handleReadOutput(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleReadOutput returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleReadOutput returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if data["output"] != "" {
		t.Errorf("output: got %q, want empty string", data["output"])
	}
	if count, ok := data["count"].(float64); !ok || int(count) != 0 {
		t.Errorf("count: got %v, want 0", data["count"])
	}
}

func TestHandleReadOutputWithEntries(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)

	buf := sm.OutputBuffer()
	buf.Append("stdout", "hello world\n")
	buf.Append("stderr", "warning: something\n")
	buf.Append("stdout", "more output\n")

	tools := New(sm)

	result, err := tools.handleReadOutput(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("handleReadOutput returned error: %v", err)
	}
	if result.IsError {
		t.Fatalf("handleReadOutput returned tool error: %v", result)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal result: %v", err)
	}

	if count, ok := data["count"].(float64); !ok || int(count) != 3 {
		t.Errorf("count: got %v, want 3", data["count"])
	}
	if data["stdout"] != "hello world\nmore output\n" {
		t.Errorf("stdout: got %q, want %q", data["stdout"], "hello world\nmore output\n")
	}
	if data["stderr"] != "warning: something\n" {
		t.Errorf("stderr: got %q, want %q", data["stderr"], "warning: something\n")
	}
	// No console entries, so console key should be absent.
	if _, ok := data["console"]; ok {
		t.Errorf("console should not be present when there are no console entries")
	}
}

func TestHandleReadOutputDrainIsIdempotent(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateRunning)

	buf := sm.OutputBuffer()
	buf.Append("stdout", "first read\n")

	tools := New(sm)

	// First call should return the entry.
	result1, err := tools.handleReadOutput(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("first handleReadOutput returned error: %v", err)
	}
	if result1.IsError {
		t.Fatalf("first handleReadOutput returned tool error: %v", result1)
	}

	var data1 map[string]any
	text1 := result1.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text1), &data1); err != nil {
		t.Fatalf("failed to unmarshal first result: %v", err)
	}
	if count, ok := data1["count"].(float64); !ok || int(count) != 1 {
		t.Errorf("first call count: got %v, want 1", data1["count"])
	}

	// Second call should return empty (drain clears the buffer).
	result2, err := tools.handleReadOutput(context.Background(), mcp.CallToolRequest{})
	if err != nil {
		t.Fatalf("second handleReadOutput returned error: %v", err)
	}
	if result2.IsError {
		t.Fatalf("second handleReadOutput returned tool error: %v", result2)
	}

	var data2 map[string]any
	text2 := result2.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text2), &data2); err != nil {
		t.Fatalf("failed to unmarshal second result: %v", err)
	}
	if count, ok := data2["count"].(float64); !ok || int(count) != 0 {
		t.Errorf("second call count: got %v, want 0", data2["count"])
	}
	if data2["output"] != "" {
		t.Errorf("second call output: got %q, want empty string", data2["output"])
	}
}

func TestFormatOutputEntriesEmpty(t *testing.T) {
	result := formatOutputEntries(nil)
	if result["output"] != "" {
		t.Errorf("output: got %q, want empty string", result["output"])
	}
	if result["count"] != 0 {
		t.Errorf("count: got %v, want 0", result["count"])
	}
}

func TestFormatOutputEntriesGroupsByCategory(t *testing.T) {
	entries := []session.OutputEntry{
		{Category: "stdout", Text: "line1\n"},
		{Category: "stderr", Text: "err1\n"},
		{Category: "console", Text: "dbg1\n"},
		{Category: "stdout", Text: "line2\n"},
	}

	result := formatOutputEntries(entries)

	if result["count"] != 4 {
		t.Errorf("count: got %v, want 4", result["count"])
	}
	if result["stdout"] != "line1\nline2\n" {
		t.Errorf("stdout: got %q, want %q", result["stdout"], "line1\nline2\n")
	}
	if result["stderr"] != "err1\n" {
		t.Errorf("stderr: got %q, want %q", result["stderr"], "err1\n")
	}
	if result["console"] != "dbg1\n" {
		t.Errorf("console: got %q, want %q", result["console"], "dbg1\n")
	}
}

func TestFormatOutputEntriesOmitsMissingCategories(t *testing.T) {
	entries := []session.OutputEntry{
		{Category: "stdout", Text: "only stdout\n"},
	}

	result := formatOutputEntries(entries)

	if result["count"] != 1 {
		t.Errorf("count: got %v, want 1", result["count"])
	}
	if result["stdout"] != "only stdout\n" {
		t.Errorf("stdout: got %q, want %q", result["stdout"], "only stdout\n")
	}
	if _, ok := result["stderr"]; ok {
		t.Error("stderr should not be present when there are no stderr entries")
	}
	if _, ok := result["console"]; ok {
		t.Error("console should not be present when there are no console entries")
	}
}
