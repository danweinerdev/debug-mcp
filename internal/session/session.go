// Package session manages the debug session state for the MCP server.
// It provides a thread-safe SessionManager that tracks the current debug
// state, DAP client, subprocess, breakpoints, and output buffering.
package session

import (
	"fmt"
	"sort"
	"strings"
	"sync"

	godap "github.com/google/go-dap"

	"github.com/danweinerdev/lldb-debug-mcp/internal/dap"
	"github.com/danweinerdev/lldb-debug-mcp/internal/detect"
)

// State represents the current state of a debug session.
type State int

const (
	StateIdle        State = iota // No active debug session.
	StateConfiguring              // Session is being set up (launch/attach in progress).
	StateStopped                  // Process is stopped (e.g., at a breakpoint).
	StateRunning                  // Process is running.
	StateTerminated               // Process has terminated.
)

// String returns a human-readable name for the state.
func (s State) String() string {
	switch s {
	case StateIdle:
		return "idle"
	case StateConfiguring:
		return "configuring"
	case StateStopped:
		return "stopped"
	case StateRunning:
		return "running"
	case StateTerminated:
		return "terminated"
	default:
		return fmt.Sprintf("unknown(%d)", int(s))
	}
}

// BreakpointInfo holds metadata about a resolved breakpoint.
type BreakpointInfo struct {
	ID        int
	Type      string // "source" or "function"
	File      string
	Line      int
	Function  string
	Condition string
	Verified  bool
}

// OutputEntry represents a single captured output line with its category.
type OutputEntry struct {
	Category string // "stdout", "stderr", "console"
	Text     string
}

// OutputBuffer is a thread-safe buffer that captures debug output entries.
// It enforces a maximum total size (1MB by default) by dropping oldest
// entries when the limit is exceeded.
type OutputBuffer struct {
	mu        sync.Mutex
	entries   []OutputEntry
	size      int // total bytes across all entries
	maxSize   int // maximum total bytes before truncation
	truncated bool
}

// NewOutputBuffer creates a new OutputBuffer with a 1MB size limit.
func NewOutputBuffer() *OutputBuffer {
	return &OutputBuffer{
		maxSize: 1048576, // 1MB
	}
}

// Append adds an output entry to the buffer. If the total size exceeds the
// maximum, oldest entries are dropped and a truncation marker is recorded.
func (b *OutputBuffer) Append(category, text string) {
	b.mu.Lock()
	defer b.mu.Unlock()

	entrySize := len(category) + len(text)
	b.entries = append(b.entries, OutputEntry{Category: category, Text: text})
	b.size += entrySize

	// Drop oldest entries until we are under the limit.
	for b.size > b.maxSize && len(b.entries) > 0 {
		dropped := b.entries[0]
		droppedSize := len(dropped.Category) + len(dropped.Text)
		b.entries = b.entries[1:]
		b.size -= droppedSize
		b.truncated = true
	}
}

// Drain returns all buffered entries and clears the buffer. If entries were
// dropped due to size limits, a "[output truncated]" marker is prepended.
func (b *OutputBuffer) Drain() []OutputEntry {
	b.mu.Lock()
	defer b.mu.Unlock()

	if len(b.entries) == 0 && !b.truncated {
		return nil
	}

	result := b.entries
	if b.truncated {
		marker := OutputEntry{Category: "console", Text: "[output truncated]"}
		result = append([]OutputEntry{marker}, result...)
		b.truncated = false
	}

	b.entries = nil
	b.size = 0
	return result
}

// SessionManager holds all state for an active debug session. All public
// methods are thread-safe via an internal RWMutex.
type SessionManager struct {
	mu sync.RWMutex

	state State

	// Process info
	program string
	pid     int

	// Exit info
	exitCode *int

	// DAP client and subprocess
	client     *dap.Client
	subprocess *detect.SubprocessResult

	// repl mode flag
	replModeCommand bool

	// Last stop info (cached from StoppedEvent)
	lastStoppedEvent *godap.StoppedEvent

	// Frame mapping: frame index -> DAP frame ID
	frameMapping map[int]int

	// Breakpoint state (placeholder maps for Phase 3)
	sourceBreakpoints   map[string][]godap.SourceBreakpoint
	functionBreakpoints []godap.FunctionBreakpoint
	breakpointResponses map[int]BreakpointInfo

	// Pending breakpoints (set before launch, flushed on InitializedEvent)
	pendingSourceBPs   map[string][]godap.SourceBreakpoint
	pendingFunctionBPs []godap.FunctionBreakpoint

	// Output buffer
	outputBuffer *OutputBuffer
}

// NewSessionManager creates a new SessionManager in the idle state with
// all maps initialized.
func NewSessionManager() *SessionManager {
	return &SessionManager{
		state:               StateIdle,
		sourceBreakpoints:   make(map[string][]godap.SourceBreakpoint),
		breakpointResponses: make(map[int]BreakpointInfo),
		pendingSourceBPs:    make(map[string][]godap.SourceBreakpoint),
		frameMapping:        make(map[int]int),
		outputBuffer:        NewOutputBuffer(),
	}
}

// State returns the current session state.
func (s *SessionManager) State() State {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.state
}

// SetState updates the session state.
func (s *SessionManager) SetState(state State) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.state = state
}

// CheckState returns nil if the current state is one of the allowed states,
// or an error with a descriptive message including the current state and
// what is allowed.
func (s *SessionManager) CheckState(allowed ...State) error {
	s.mu.RLock()
	current := s.state
	s.mu.RUnlock()

	for _, a := range allowed {
		if current == a {
			return nil
		}
	}

	// Build user-friendly error messages for common cases.
	if current == StateIdle {
		return fmt.Errorf("no debug session active. Use 'launch' or 'attach' first.")
	}
	if current == StateRunning {
		return fmt.Errorf("process is running. Use 'pause' first.")
	}

	// Generic message for other cases.
	names := make([]string, len(allowed))
	for i, a := range allowed {
		names[i] = a.String()
	}
	return fmt.Errorf("invalid state: %s, expected one of: %s", current.String(), strings.Join(names, ", "))
}

// Reset clears all session state and returns to idle.
func (s *SessionManager) Reset() {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.state = StateIdle
	s.client = nil
	s.subprocess = nil
	s.program = ""
	s.pid = 0
	s.exitCode = nil
	s.lastStoppedEvent = nil
	s.replModeCommand = false
	s.frameMapping = make(map[int]int)
	s.sourceBreakpoints = make(map[string][]godap.SourceBreakpoint)
	s.functionBreakpoints = nil
	s.breakpointResponses = make(map[int]BreakpointInfo)
	s.pendingSourceBPs = make(map[string][]godap.SourceBreakpoint)
	s.pendingFunctionBPs = nil
	s.outputBuffer = NewOutputBuffer()
}

// --- Accessor methods ---

// Program returns the program path for the current session.
func (s *SessionManager) Program() string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.program
}

// PID returns the process ID for the current session.
func (s *SessionManager) PID() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.pid
}

// ExitCode returns the exit code if the process has exited, or nil.
func (s *SessionManager) ExitCode() *int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.exitCode
}

// Client returns the DAP client for the current session.
func (s *SessionManager) Client() *dap.Client {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.client
}

// Subprocess returns the subprocess result for the current session.
func (s *SessionManager) Subprocess() *detect.SubprocessResult {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.subprocess
}

// ReplModeCommand returns whether REPL mode is set to "command".
func (s *SessionManager) ReplModeCommand() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.replModeCommand
}

// LastStoppedEvent returns the most recent StoppedEvent, or nil if none.
func (s *SessionManager) LastStoppedEvent() *godap.StoppedEvent {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.lastStoppedEvent
}

// FrameMapping returns a copy of the frame index to DAP frame ID mapping.
func (s *SessionManager) FrameMapping() map[int]int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.frameMapping
}

// OutputBuffer returns the output buffer.
func (s *SessionManager) OutputBuffer() *OutputBuffer {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.outputBuffer
}

// --- Setter methods ---

// SetClient sets the DAP client for the current session.
func (s *SessionManager) SetClient(client *dap.Client) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.client = client
}

// SetSubprocess sets the subprocess result for the current session.
func (s *SessionManager) SetSubprocess(sub *detect.SubprocessResult) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.subprocess = sub
}

// SetProgram sets the program path for the current session.
func (s *SessionManager) SetProgram(program string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.program = program
}

// SetPID sets the process ID for the current session.
func (s *SessionManager) SetPID(pid int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.pid = pid
}

// SetExitCode sets the exit code for the current session.
func (s *SessionManager) SetExitCode(code int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.exitCode = &code
}

// SetReplModeCommand sets whether REPL mode is "command".
func (s *SessionManager) SetReplModeCommand(v bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.replModeCommand = v
}

// SetLastStoppedEvent caches the most recent StoppedEvent.
func (s *SessionManager) SetLastStoppedEvent(event *godap.StoppedEvent) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.lastStoppedEvent = event
}

// SetFrameMapping replaces the frame index to DAP frame ID mapping.
func (s *SessionManager) SetFrameMapping(mapping map[int]int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.frameMapping = mapping
}

// --- Breakpoint methods ---

// AddSourceBreakpoint creates a source breakpoint and adds it to the
// tracked breakpoints for the given file.
func (s *SessionManager) AddSourceBreakpoint(file string, line int, condition string) godap.SourceBreakpoint {
	s.mu.Lock()
	defer s.mu.Unlock()

	bp := godap.SourceBreakpoint{Line: line, Condition: condition}
	s.sourceBreakpoints[file] = append(s.sourceBreakpoints[file], bp)
	return bp
}

// AddFunctionBreakpoint creates a function breakpoint and adds it to
// the tracked function breakpoints.
func (s *SessionManager) AddFunctionBreakpoint(name string, condition string) godap.FunctionBreakpoint {
	s.mu.Lock()
	defer s.mu.Unlock()

	bp := godap.FunctionBreakpoint{Name: name, Condition: condition}
	s.functionBreakpoints = append(s.functionBreakpoints, bp)
	return bp
}

// AddBreakpointResponse stores resolved breakpoint metadata by ID.
func (s *SessionManager) AddBreakpointResponse(info BreakpointInfo) {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.breakpointResponses[info.ID] = info
}

// RemoveBreakpointByID removes a tracked breakpoint by its DAP ID.
// It returns the file path for source breakpoints, wasFunction=true for
// function breakpoints, or an error if the ID is not found.
func (s *SessionManager) RemoveBreakpointByID(id int) (filePath string, wasFunction bool, err error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	info, ok := s.breakpointResponses[id]
	if !ok {
		return "", false, fmt.Errorf("breakpoint ID %d not found", id)
	}

	switch info.Type {
	case "source":
		filePath = info.File
		bps := s.sourceBreakpoints[filePath]
		for i, bp := range bps {
			if bp.Line == info.Line {
				s.sourceBreakpoints[filePath] = append(bps[:i], bps[i+1:]...)
				break
			}
		}
	case "function":
		wasFunction = true
		for i, bp := range s.functionBreakpoints {
			if bp.Name == info.Function {
				s.functionBreakpoints = append(s.functionBreakpoints[:i], s.functionBreakpoints[i+1:]...)
				break
			}
		}
	}

	delete(s.breakpointResponses, id)
	return filePath, wasFunction, nil
}

// ListBreakpoints returns all tracked breakpoints sorted by ID.
func (s *SessionManager) ListBreakpoints() []BreakpointInfo {
	s.mu.RLock()
	defer s.mu.RUnlock()

	result := make([]BreakpointInfo, 0, len(s.breakpointResponses))
	for _, info := range s.breakpointResponses {
		result = append(result, info)
	}
	sort.Slice(result, func(i, j int) bool {
		return result[i].ID < result[j].ID
	})
	return result
}

// SourceBreakpointsForFile returns a copy of the source breakpoints for a file.
func (s *SessionManager) SourceBreakpointsForFile(file string) []godap.SourceBreakpoint {
	s.mu.RLock()
	defer s.mu.RUnlock()

	bps := s.sourceBreakpoints[file]
	if len(bps) == 0 {
		return nil
	}
	cp := make([]godap.SourceBreakpoint, len(bps))
	copy(cp, bps)
	return cp
}

// AllFunctionBreakpoints returns a copy of the function breakpoint list.
func (s *SessionManager) AllFunctionBreakpoints() []godap.FunctionBreakpoint {
	s.mu.RLock()
	defer s.mu.RUnlock()

	if len(s.functionBreakpoints) == 0 {
		return nil
	}
	cp := make([]godap.FunctionBreakpoint, len(s.functionBreakpoints))
	copy(cp, s.functionBreakpoints)
	return cp
}

// AddPendingSourceBreakpoint adds a source breakpoint to the pending buffer.
// Pending breakpoints are flushed to active state on InitializedEvent.
func (s *SessionManager) AddPendingSourceBreakpoint(file string, line int, condition string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	bp := godap.SourceBreakpoint{Line: line, Condition: condition}
	s.pendingSourceBPs[file] = append(s.pendingSourceBPs[file], bp)
}

// AddPendingFunctionBreakpoint adds a function breakpoint to the pending buffer.
// Pending breakpoints are flushed to active state on InitializedEvent.
func (s *SessionManager) AddPendingFunctionBreakpoint(name string, condition string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	bp := godap.FunctionBreakpoint{Name: name, Condition: condition}
	s.pendingFunctionBPs = append(s.pendingFunctionBPs, bp)
}

// FlushPendingBreakpoints moves pending breakpoints to active state and
// returns them so the caller can send DAP requests. Pending buffers are
// cleared after the flush.
func (s *SessionManager) FlushPendingBreakpoints() (sourceFiles map[string][]godap.SourceBreakpoint, funcBPs []godap.FunctionBreakpoint) {
	s.mu.Lock()
	defer s.mu.Unlock()

	sourceFiles = s.pendingSourceBPs
	funcBPs = s.pendingFunctionBPs

	// Move pending source breakpoints to active.
	for file, bps := range sourceFiles {
		s.sourceBreakpoints[file] = append(s.sourceBreakpoints[file], bps...)
	}

	// Move pending function breakpoints to active.
	s.functionBreakpoints = append(s.functionBreakpoints, funcBPs...)

	// Clear pending buffers.
	s.pendingSourceBPs = make(map[string][]godap.SourceBreakpoint)
	s.pendingFunctionBPs = nil

	return sourceFiles, funcBPs
}
