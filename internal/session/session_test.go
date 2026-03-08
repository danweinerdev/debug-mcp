package session

import (
	"strings"
	"sync"
	"testing"

	godap "github.com/google/go-dap"
)

func TestStateTransitions(t *testing.T) {
	sm := NewSessionManager()

	// idle -> configuring
	if sm.State() != StateIdle {
		t.Fatalf("initial state: got %v, want %v", sm.State(), StateIdle)
	}

	sm.SetState(StateConfiguring)
	if sm.State() != StateConfiguring {
		t.Fatalf("after SetState(configuring): got %v, want %v", sm.State(), StateConfiguring)
	}

	// configuring -> stopped
	sm.SetState(StateStopped)
	if sm.State() != StateStopped {
		t.Fatalf("after SetState(stopped): got %v, want %v", sm.State(), StateStopped)
	}

	// stopped -> running
	sm.SetState(StateRunning)
	if sm.State() != StateRunning {
		t.Fatalf("after SetState(running): got %v, want %v", sm.State(), StateRunning)
	}

	// running -> stopped
	sm.SetState(StateStopped)
	if sm.State() != StateStopped {
		t.Fatalf("after SetState(stopped) again: got %v, want %v", sm.State(), StateStopped)
	}

	// stopped -> terminated
	sm.SetState(StateTerminated)
	if sm.State() != StateTerminated {
		t.Fatalf("after SetState(terminated): got %v, want %v", sm.State(), StateTerminated)
	}

	// terminated -> idle (via Reset)
	sm.Reset()
	if sm.State() != StateIdle {
		t.Fatalf("after Reset: got %v, want %v", sm.State(), StateIdle)
	}
}

func TestCheckStateAllowed(t *testing.T) {
	sm := NewSessionManager()
	sm.SetState(StateStopped)

	// Stopped is allowed.
	if err := sm.CheckState(StateStopped); err != nil {
		t.Errorf("CheckState(stopped) with state=stopped: unexpected error: %v", err)
	}

	// Running is not allowed.
	if err := sm.CheckState(StateRunning); err == nil {
		t.Error("CheckState(running) with state=stopped: expected error, got nil")
	}
}

func TestCheckStateErrorMessages(t *testing.T) {
	sm := NewSessionManager()
	// State is idle.

	err := sm.CheckState(StateStopped, StateRunning)
	if err == nil {
		t.Fatal("CheckState(stopped, running) with state=idle: expected error, got nil")
	}

	msg := err.Error()
	// The idle-specific message should mention launching/attaching.
	if !strings.Contains(msg, "no debug session active") {
		t.Errorf("expected message to mention 'no debug session active', got: %q", msg)
	}

	// Now test the running-specific message.
	sm.SetState(StateRunning)
	err = sm.CheckState(StateStopped)
	if err == nil {
		t.Fatal("CheckState(stopped) with state=running: expected error, got nil")
	}
	msg = err.Error()
	if !strings.Contains(msg, "process is running") {
		t.Errorf("expected message to mention 'process is running', got: %q", msg)
	}

	// Test the generic message for other states (e.g., terminated).
	sm.SetState(StateTerminated)
	err = sm.CheckState(StateStopped, StateRunning)
	if err == nil {
		t.Fatal("CheckState(stopped, running) with state=terminated: expected error, got nil")
	}
	msg = err.Error()
	if !strings.Contains(msg, "terminated") {
		t.Errorf("expected message to mention 'terminated', got: %q", msg)
	}
	if !strings.Contains(msg, "stopped") || !strings.Contains(msg, "running") {
		t.Errorf("expected message to mention expected states, got: %q", msg)
	}
}

func TestReset(t *testing.T) {
	sm := NewSessionManager()

	// Set various fields.
	sm.SetState(StateStopped)
	sm.SetProgram("/usr/bin/test")
	sm.SetPID(12345)
	sm.SetExitCode(1)
	sm.SetReplModeCommand(true)

	event := &godap.StoppedEvent{}
	event.Body = godap.StoppedEventBody{Reason: "breakpoint"}
	sm.SetLastStoppedEvent(event)

	sm.SetFrameMapping(map[int]int{0: 100, 1: 200})
	sm.OutputBuffer().Append("stdout", "hello")

	// Reset and verify everything is cleared.
	sm.Reset()

	if sm.State() != StateIdle {
		t.Errorf("State after reset: got %v, want %v", sm.State(), StateIdle)
	}
	if sm.Program() != "" {
		t.Errorf("Program after reset: got %q, want empty", sm.Program())
	}
	if sm.PID() != 0 {
		t.Errorf("PID after reset: got %d, want 0", sm.PID())
	}
	if sm.ExitCode() != nil {
		t.Errorf("ExitCode after reset: got %v, want nil", sm.ExitCode())
	}
	if sm.Client() != nil {
		t.Error("Client after reset: expected nil")
	}
	if sm.Subprocess() != nil {
		t.Error("Subprocess after reset: expected nil")
	}
	if sm.ReplModeCommand() {
		t.Error("ReplModeCommand after reset: expected false")
	}
	if sm.LastStoppedEvent() != nil {
		t.Error("LastStoppedEvent after reset: expected nil")
	}
	if len(sm.FrameMapping()) != 0 {
		t.Errorf("FrameMapping after reset: got %v, want empty", sm.FrameMapping())
	}
	entries := sm.OutputBuffer().Drain()
	if len(entries) != 0 {
		t.Errorf("OutputBuffer after reset: got %d entries, want 0", len(entries))
	}
}

func TestOutputBufferAppendDrain(t *testing.T) {
	buf := NewOutputBuffer()

	buf.Append("stdout", "line 1\n")
	buf.Append("stderr", "error\n")
	buf.Append("console", "info\n")

	entries := buf.Drain()
	if len(entries) != 3 {
		t.Fatalf("Drain: got %d entries, want 3", len(entries))
	}

	expected := []OutputEntry{
		{Category: "stdout", Text: "line 1\n"},
		{Category: "stderr", Text: "error\n"},
		{Category: "console", Text: "info\n"},
	}
	for i, want := range expected {
		got := entries[i]
		if got.Category != want.Category || got.Text != want.Text {
			t.Errorf("entries[%d]: got {%q, %q}, want {%q, %q}",
				i, got.Category, got.Text, want.Category, want.Text)
		}
	}

	// Second drain returns empty.
	entries = buf.Drain()
	if len(entries) != 0 {
		t.Errorf("second Drain: got %d entries, want 0", len(entries))
	}
}

func TestOutputBufferTruncation(t *testing.T) {
	buf := NewOutputBuffer()

	// Append entries totaling > 1MB.
	// Each entry is ~1000 bytes of text + category overhead.
	// 1100 entries * ~1006 bytes/entry > 1MB.
	chunk := strings.Repeat("x", 1000)
	for i := 0; i < 1100; i++ {
		buf.Append("stdout", chunk)
	}

	entries := buf.Drain()

	if len(entries) == 0 {
		t.Fatal("Drain after truncation: got 0 entries")
	}

	// First entry should be the truncation marker.
	if entries[0].Text != "[output truncated]" {
		t.Errorf("first entry Text: got %q, want %q", entries[0].Text, "[output truncated]")
	}
	if entries[0].Category != "console" {
		t.Errorf("first entry Category: got %q, want %q", entries[0].Category, "console")
	}

	// Verify total size (excluding marker) is under the limit.
	totalSize := 0
	for _, e := range entries[1:] {
		totalSize += len(e.Category) + len(e.Text)
	}
	if totalSize > 1048576 {
		t.Errorf("total size after truncation: %d bytes, want <= 1048576", totalSize)
	}

	// Subsequent drain should be empty and no truncation marker.
	entries = buf.Drain()
	if len(entries) != 0 {
		t.Errorf("second Drain after truncation: got %d entries, want 0", len(entries))
	}
}

func TestOutputBufferConcurrent(t *testing.T) {
	buf := NewOutputBuffer()

	var wg sync.WaitGroup
	const writers = 10
	const entriesPerWriter = 100

	// Concurrent writers.
	for i := 0; i < writers; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			for j := 0; j < entriesPerWriter; j++ {
				buf.Append("stdout", "data")
			}
		}(i)
	}

	// Concurrent drainers.
	const drainers = 3
	for i := 0; i < drainers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 50; j++ {
				buf.Drain()
			}
		}()
	}

	wg.Wait()

	// If we get here without a race detector failure, the test passes.
}

func TestStateString(t *testing.T) {
	tests := []struct {
		state State
		want  string
	}{
		{StateIdle, "idle"},
		{StateConfiguring, "configuring"},
		{StateStopped, "stopped"},
		{StateRunning, "running"},
		{StateTerminated, "terminated"},
	}

	for _, tt := range tests {
		got := tt.state.String()
		if got != tt.want {
			t.Errorf("State(%d).String(): got %q, want %q", int(tt.state), got, tt.want)
		}
	}
}
