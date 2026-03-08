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

func TestAddSourceBreakpointsMultipleFiles(t *testing.T) {
	sm := NewSessionManager()

	// Add breakpoints to two different files.
	bp1 := sm.AddSourceBreakpoint("/src/main.go", 10, "")
	bp2 := sm.AddSourceBreakpoint("/src/main.go", 25, "x > 5")
	bp3 := sm.AddSourceBreakpoint("/src/util.go", 42, "")

	if bp1.Line != 10 || bp1.Condition != "" {
		t.Errorf("bp1: got Line=%d Condition=%q, want Line=10 Condition=\"\"", bp1.Line, bp1.Condition)
	}
	if bp2.Line != 25 || bp2.Condition != "x > 5" {
		t.Errorf("bp2: got Line=%d Condition=%q, want Line=25 Condition=\"x > 5\"", bp2.Line, bp2.Condition)
	}
	if bp3.Line != 42 {
		t.Errorf("bp3: got Line=%d, want 42", bp3.Line)
	}

	// Verify per-file lists via accessor.
	mainBPs := sm.SourceBreakpointsForFile("/src/main.go")
	if len(mainBPs) != 2 {
		t.Fatalf("main.go breakpoints: got %d, want 2", len(mainBPs))
	}
	if mainBPs[0].Line != 10 || mainBPs[1].Line != 25 {
		t.Errorf("main.go lines: got [%d, %d], want [10, 25]", mainBPs[0].Line, mainBPs[1].Line)
	}

	utilBPs := sm.SourceBreakpointsForFile("/src/util.go")
	if len(utilBPs) != 1 || utilBPs[0].Line != 42 {
		t.Errorf("util.go breakpoints: got %v, want [{Line:42}]", utilBPs)
	}

	// File with no breakpoints returns nil.
	noBPs := sm.SourceBreakpointsForFile("/src/other.go")
	if noBPs != nil {
		t.Errorf("other.go breakpoints: got %v, want nil", noBPs)
	}
}

func TestRemoveSourceBreakpointByID(t *testing.T) {
	sm := NewSessionManager()

	// Set up source breakpoints and their responses.
	sm.AddSourceBreakpoint("/src/main.go", 10, "")
	sm.AddSourceBreakpoint("/src/main.go", 25, "")
	sm.AddBreakpointResponse(BreakpointInfo{
		ID: 1, Type: "source", File: "/src/main.go", Line: 10, Verified: true,
	})
	sm.AddBreakpointResponse(BreakpointInfo{
		ID: 2, Type: "source", File: "/src/main.go", Line: 25, Verified: true,
	})

	// Remove breakpoint 1 (line 10).
	filePath, wasFunc, err := sm.RemoveBreakpointByID(1)
	if err != nil {
		t.Fatalf("RemoveBreakpointByID(1): unexpected error: %v", err)
	}
	if filePath != "/src/main.go" {
		t.Errorf("filePath: got %q, want %q", filePath, "/src/main.go")
	}
	if wasFunc {
		t.Error("wasFunction: got true, want false")
	}

	// Verify only line 25 remains.
	bps := sm.SourceBreakpointsForFile("/src/main.go")
	if len(bps) != 1 || bps[0].Line != 25 {
		t.Errorf("remaining breakpoints: got %v, want [{Line:25}]", bps)
	}

	// Verify breakpoint 1 is gone from responses.
	list := sm.ListBreakpoints()
	if len(list) != 1 || list[0].ID != 2 {
		t.Errorf("ListBreakpoints after remove: got %v, want [ID=2]", list)
	}
}

func TestRemoveBreakpointByIDNotFound(t *testing.T) {
	sm := NewSessionManager()

	_, _, err := sm.RemoveBreakpointByID(999)
	if err == nil {
		t.Fatal("RemoveBreakpointByID(999): expected error, got nil")
	}
	if err.Error() != "breakpoint ID 999 not found" {
		t.Errorf("error message: got %q, want %q", err.Error(), "breakpoint ID 999 not found")
	}
}

func TestListBreakpointsSorted(t *testing.T) {
	sm := NewSessionManager()

	// Add responses out of order.
	sm.AddBreakpointResponse(BreakpointInfo{ID: 5, Type: "source", File: "/a.go", Line: 1})
	sm.AddBreakpointResponse(BreakpointInfo{ID: 2, Type: "function", Function: "main"})
	sm.AddBreakpointResponse(BreakpointInfo{ID: 8, Type: "source", File: "/b.go", Line: 10})

	list := sm.ListBreakpoints()
	if len(list) != 3 {
		t.Fatalf("ListBreakpoints: got %d items, want 3", len(list))
	}
	if list[0].ID != 2 || list[1].ID != 5 || list[2].ID != 8 {
		t.Errorf("ListBreakpoints order: got IDs [%d, %d, %d], want [2, 5, 8]",
			list[0].ID, list[1].ID, list[2].ID)
	}
}

func TestFunctionBreakpoints(t *testing.T) {
	sm := NewSessionManager()

	bp1 := sm.AddFunctionBreakpoint("main", "")
	bp2 := sm.AddFunctionBreakpoint("handleRequest", "count > 3")

	if bp1.Name != "main" || bp1.Condition != "" {
		t.Errorf("bp1: got Name=%q Condition=%q, want Name=\"main\" Condition=\"\"", bp1.Name, bp1.Condition)
	}
	if bp2.Name != "handleRequest" || bp2.Condition != "count > 3" {
		t.Errorf("bp2: got Name=%q Condition=%q", bp2.Name, bp2.Condition)
	}

	all := sm.AllFunctionBreakpoints()
	if len(all) != 2 {
		t.Fatalf("AllFunctionBreakpoints: got %d, want 2", len(all))
	}
	if all[0].Name != "main" || all[1].Name != "handleRequest" {
		t.Errorf("AllFunctionBreakpoints: got [%q, %q], want [\"main\", \"handleRequest\"]",
			all[0].Name, all[1].Name)
	}

	// Verify returned slice is a copy.
	all[0].Name = "modified"
	original := sm.AllFunctionBreakpoints()
	if original[0].Name != "main" {
		t.Errorf("AllFunctionBreakpoints returned non-copy: mutation was visible")
	}
}

func TestRemoveFunctionBreakpointByID(t *testing.T) {
	sm := NewSessionManager()

	sm.AddFunctionBreakpoint("main", "")
	sm.AddFunctionBreakpoint("handler", "")
	sm.AddBreakpointResponse(BreakpointInfo{ID: 1, Type: "function", Function: "main"})
	sm.AddBreakpointResponse(BreakpointInfo{ID: 2, Type: "function", Function: "handler"})

	filePath, wasFunc, err := sm.RemoveBreakpointByID(1)
	if err != nil {
		t.Fatalf("RemoveBreakpointByID(1): unexpected error: %v", err)
	}
	if filePath != "" {
		t.Errorf("filePath: got %q, want empty", filePath)
	}
	if !wasFunc {
		t.Error("wasFunction: got false, want true")
	}

	// Verify only "handler" remains.
	all := sm.AllFunctionBreakpoints()
	if len(all) != 1 || all[0].Name != "handler" {
		t.Errorf("remaining function breakpoints: got %v, want [{Name:\"handler\"}]", all)
	}
}

func TestPendingBreakpointFlush(t *testing.T) {
	sm := NewSessionManager()

	// Add pending breakpoints.
	sm.AddPendingSourceBreakpoint("/src/main.go", 10, "")
	sm.AddPendingSourceBreakpoint("/src/main.go", 20, "x > 0")
	sm.AddPendingSourceBreakpoint("/src/util.go", 5, "")
	sm.AddPendingFunctionBreakpoint("main", "")
	sm.AddPendingFunctionBreakpoint("init", "")

	// Flush moves to active and returns pending.
	sourceFiles, funcBPs := sm.FlushPendingBreakpoints()

	// Verify returned pending breakpoints.
	if len(sourceFiles) != 2 {
		t.Fatalf("sourceFiles: got %d files, want 2", len(sourceFiles))
	}
	if len(sourceFiles["/src/main.go"]) != 2 {
		t.Errorf("sourceFiles[main.go]: got %d bps, want 2", len(sourceFiles["/src/main.go"]))
	}
	if len(sourceFiles["/src/util.go"]) != 1 {
		t.Errorf("sourceFiles[util.go]: got %d bps, want 1", len(sourceFiles["/src/util.go"]))
	}
	if len(funcBPs) != 2 {
		t.Errorf("funcBPs: got %d, want 2", len(funcBPs))
	}

	// Verify breakpoints are now active.
	mainBPs := sm.SourceBreakpointsForFile("/src/main.go")
	if len(mainBPs) != 2 {
		t.Errorf("active source bps for main.go: got %d, want 2", len(mainBPs))
	}
	allFunc := sm.AllFunctionBreakpoints()
	if len(allFunc) != 2 {
		t.Errorf("active function bps: got %d, want 2", len(allFunc))
	}

	// Verify pending buffers are cleared (second flush returns empty).
	sourceFiles2, funcBPs2 := sm.FlushPendingBreakpoints()
	if len(sourceFiles2) != 0 {
		t.Errorf("second flush sourceFiles: got %d, want 0", len(sourceFiles2))
	}
	if len(funcBPs2) != 0 {
		t.Errorf("second flush funcBPs: got %d, want 0", len(funcBPs2))
	}

	// Verify active state was NOT duplicated by second flush.
	mainBPs = sm.SourceBreakpointsForFile("/src/main.go")
	if len(mainBPs) != 2 {
		t.Errorf("active source bps after second flush: got %d, want 2", len(mainBPs))
	}
}

func TestSourceBreakpointsForFileCopy(t *testing.T) {
	sm := NewSessionManager()

	sm.AddSourceBreakpoint("/src/main.go", 10, "")

	// Modify the returned slice and verify the original is unchanged.
	bps := sm.SourceBreakpointsForFile("/src/main.go")
	bps[0].Line = 999

	original := sm.SourceBreakpointsForFile("/src/main.go")
	if original[0].Line != 10 {
		t.Errorf("SourceBreakpointsForFile returned non-copy: mutation was visible")
	}
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
