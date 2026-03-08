package tools

import (
	"context"
	"strings"
	"testing"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func TestHandleReadMemoryStateGuardRejectsNonStopped(t *testing.T) {
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

			result, err := tools.handleReadMemory(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleReadMemory returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleReadMemory should return tool error when state is %s", tc.name)
			}
		})
	}
}

func TestHandleReadMemoryMissingAddressParameter(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"count": float64(16),
	}

	result, err := tools.handleReadMemory(context.Background(), req)
	if err != nil {
		t.Fatalf("handleReadMemory returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleReadMemory should return tool error when address is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}

func TestHandleReadMemoryMissingCountParameter(t *testing.T) {
	sm := session.NewSessionManager()
	sm.SetState(session.StateStopped)
	tools := New(sm)

	req := mcp.CallToolRequest{}
	req.Params.Arguments = map[string]any{
		"address": "0x7fff5000",
	}

	result, err := tools.handleReadMemory(context.Background(), req)
	if err != nil {
		t.Fatalf("handleReadMemory returned error: %v", err)
	}
	if !result.IsError {
		t.Fatal("handleReadMemory should return tool error when count is missing")
	}

	text := result.Content[0].(mcp.TextContent).Text
	if !strings.Contains(text, "missing required parameter") {
		t.Errorf("error message should mention missing required parameter, got: %q", text)
	}
}

func TestFormatHexDumpFullRow(t *testing.T) {
	// "Hello World!...." (16 bytes)
	data := []byte("Hello World!\x00\x00\x00\x00")
	result := formatHexDump(data, 0x7fff5000)

	expected := "0x7fff5000: 48 65 6c 6c 6f 20 57 6f  72 6c 64 21 00 00 00 00  |Hello World!....|"
	if result != expected {
		t.Errorf("formatHexDump mismatch\ngot:    %q\nexpect: %q", result, expected)
	}
}

func TestFormatHexDumpPartialRow(t *testing.T) {
	// 5 bytes only
	data := []byte{0x41, 0x42, 0x43, 0x00, 0xff}
	result := formatHexDump(data, 0x1000)

	// Should pad hex and ASCII sections
	if !strings.HasPrefix(result, "0x00001000: ") {
		t.Errorf("formatHexDump should start with address, got: %q", result)
	}
	if !strings.Contains(result, "41 42 43 00 ff") {
		t.Errorf("formatHexDump should contain hex bytes, got: %q", result)
	}
	if !strings.HasSuffix(result, "|ABC..           |") {
		t.Errorf("formatHexDump ASCII section mismatch, got: %q", result)
	}
}

func TestFormatHexDumpMultipleRows(t *testing.T) {
	// 20 bytes = 1 full row + 1 partial row
	data := make([]byte, 20)
	for i := range data {
		data[i] = byte(i)
	}

	result := formatHexDump(data, 0x0)
	lines := strings.Split(result, "\n")
	if len(lines) != 2 {
		t.Fatalf("expected 2 lines, got %d: %q", len(lines), result)
	}

	if !strings.HasPrefix(lines[0], "0x00000000: ") {
		t.Errorf("first line should start with 0x00000000, got: %q", lines[0])
	}
	if !strings.HasPrefix(lines[1], "0x00000010: ") {
		t.Errorf("second line should start with 0x00000010, got: %q", lines[1])
	}
}

func TestFormatHexDumpEmpty(t *testing.T) {
	result := formatHexDump([]byte{}, 0x0)
	if result != "" {
		t.Errorf("formatHexDump of empty data should be empty, got: %q", result)
	}
}

func TestHandleDisassembleStateGuardRejectsNonStopped(t *testing.T) {
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

			result, err := tools.handleDisassemble(context.Background(), mcp.CallToolRequest{})
			if err != nil {
				t.Fatalf("handleDisassemble returned error: %v", err)
			}
			if !result.IsError {
				t.Fatalf("handleDisassemble should return tool error when state is %s", tc.name)
			}
		})
	}
}
