//go:build integration

package tools

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

// --- Test helpers ---

// testFixturePath returns the absolute path to a test fixture binary in the
// testdata directory. It calls t.Fatal if the binary does not exist.
func testFixturePath(t *testing.T, name string) string {
	t.Helper()
	_, thisFile, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("failed to determine test file path")
	}
	// internal/tools/integration_test.go -> project root
	projectRoot := filepath.Dir(filepath.Dir(filepath.Dir(thisFile)))
	p := filepath.Join(projectRoot, "testdata", name)
	if _, err := os.Stat(p); err != nil {
		t.Fatalf("test fixture %q not found: %v", p, err)
	}
	return p
}

// newTestTools creates a fresh SessionManager and Tools instance for testing.
func newTestTools(t *testing.T) (*Tools, *session.SessionManager) {
	t.Helper()
	sm := session.NewSessionManager()
	tools := New(sm)
	return tools, sm
}

// makeCallToolRequest creates a mcp.CallToolRequest with the given arguments map.
func makeCallToolRequest(args map[string]any) mcp.CallToolRequest {
	req := mcp.CallToolRequest{}
	req.Params.Arguments = args
	return req
}

// launchFixture launches a test fixture binary with stop_on_entry=true and
// returns the parsed JSON result. It fails the test if the launch fails.
func launchFixture(t *testing.T, tools *Tools, fixture string) map[string]any {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	req := makeCallToolRequest(map[string]any{
		"program":       fixture,
		"stop_on_entry": true,
	})

	result, err := tools.handleLaunch(ctx, req)
	if err != nil {
		t.Fatalf("handleLaunch returned error: %v", err)
	}
	if result.IsError {
		text := result.Content[0].(mcp.TextContent).Text
		t.Fatalf("handleLaunch returned tool error: %s", text)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal launch result: %v", err)
	}
	return data
}

// disconnectCleanup sends a disconnect request to cleanly tear down the
// debug session. It should be called via t.Cleanup or deferred.
func disconnectCleanup(t *testing.T, tools *Tools) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	req := makeCallToolRequest(map[string]any{
		"terminate": true,
	})

	// Ignore errors -- we are cleaning up.
	tools.handleDisconnect(ctx, req)
}

// callContinue sends a continue request and returns the parsed JSON result.
func callContinue(t *testing.T, tools *Tools) map[string]any {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	req := makeCallToolRequest(nil)
	result, err := tools.handleContinue(ctx, req)
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}
	if result.IsError {
		text := result.Content[0].(mcp.TextContent).Text
		t.Fatalf("handleContinue returned tool error: %s", text)
	}

	var data map[string]any
	text := result.Content[0].(mcp.TextContent).Text
	if err := json.Unmarshal([]byte(text), &data); err != nil {
		t.Fatalf("failed to unmarshal continue result: %v", err)
	}
	return data
}

// --- Process exit handling tests ---

func TestProcessExitHandling(t *testing.T) {
	fixture := testFixturePath(t, "simple")
	tools, sm := newTestTools(t)
	t.Cleanup(func() { disconnectCleanup(t, tools) })

	// 1. Launch simple.c with stop_on_entry=true.
	launchData := launchFixture(t, tools, fixture)
	if launchData["status"] != "launched" {
		t.Fatalf("launch status: got %q, want %q", launchData["status"], "launched")
	}
	if launchData["state"] != "stopped" {
		t.Fatalf("launch state: got %q, want %q", launchData["state"], "stopped")
	}

	// Verify session is stopped.
	if s := sm.State(); s != session.StateStopped {
		t.Fatalf("session state after launch: got %v, want %v", s, session.StateStopped)
	}

	// 2. Continue -- program should exit normally.
	continueData := callContinue(t, tools)

	// 3. Verify continue result has status "exited" and exit_code 0.
	if continueData["status"] != "exited" {
		t.Fatalf("continue status: got %q, want %q", continueData["status"], "exited")
	}
	exitCode, ok := continueData["exit_code"].(float64)
	if !ok {
		t.Fatalf("exit_code: expected float64, got %T (%v)", continueData["exit_code"], continueData["exit_code"])
	}
	if int(exitCode) != 0 {
		t.Fatalf("exit_code: got %d, want 0", int(exitCode))
	}

	// 4. Verify session state is terminated.
	if s := sm.State(); s != session.StateTerminated {
		t.Fatalf("session state after exit: got %v, want %v", s, session.StateTerminated)
	}

	// 5. Try calling variables -- should get state error (process terminated).
	{
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		req := makeCallToolRequest(nil)
		result, err := tools.handleVariables(ctx, req)
		if err != nil {
			t.Fatalf("handleVariables returned error: %v", err)
		}
		if !result.IsError {
			t.Fatal("handleVariables should return tool error when process is terminated")
		}
		errText := result.Content[0].(mcp.TextContent).Text
		if errText == "" {
			t.Fatal("handleVariables error message should not be empty")
		}
	}

	// 6. Try calling backtrace -- should get state error.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		req := makeCallToolRequest(nil)
		result, err := tools.handleBacktrace(ctx, req)
		if err != nil {
			t.Fatalf("handleBacktrace returned error: %v", err)
		}
		if !result.IsError {
			t.Fatal("handleBacktrace should return tool error when process is terminated")
		}
		errText := result.Content[0].(mcp.TextContent).Text
		if errText == "" {
			t.Fatal("handleBacktrace error message should not be empty")
		}
	}

	// 7. Disconnect and launch again -- verify session reuse works.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()

		req := makeCallToolRequest(map[string]any{"terminate": true})
		result, err := tools.handleDisconnect(ctx, req)
		if err != nil {
			t.Fatalf("handleDisconnect returned error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("handleDisconnect returned tool error: %s", text)
		}
	}

	// Verify session is back to idle after disconnect.
	if s := sm.State(); s != session.StateIdle {
		t.Fatalf("session state after disconnect: got %v, want %v", s, session.StateIdle)
	}

	// Re-launch to verify session reuse.
	launchData2 := launchFixture(t, tools, fixture)
	if launchData2["status"] != "launched" {
		t.Fatalf("re-launch status: got %q, want %q", launchData2["status"], "launched")
	}
	if launchData2["state"] != "stopped" {
		t.Fatalf("re-launch state: got %q, want %q", launchData2["state"], "stopped")
	}
}

func TestProcessExitWithOutput(t *testing.T) {
	fixture := testFixturePath(t, "simple")
	tools, _ := newTestTools(t)
	t.Cleanup(func() { disconnectCleanup(t, tools) })

	// 1. Launch simple.c with stop_on_entry=true.
	launchData := launchFixture(t, tools, fixture)
	if launchData["status"] != "launched" {
		t.Fatalf("launch status: got %q, want %q", launchData["status"], "launched")
	}

	// 2. Continue to exit.
	continueData := callContinue(t, tools)

	// 3. Verify the continue result indicates exit.
	if continueData["status"] != "exited" {
		t.Fatalf("continue status: got %q, want %q", continueData["status"], "exited")
	}

	exitCode, ok := continueData["exit_code"].(float64)
	if !ok {
		t.Fatalf("exit_code: expected float64, got %T (%v)", continueData["exit_code"], continueData["exit_code"])
	}
	if int(exitCode) != 0 {
		t.Fatalf("exit_code: got %d, want 0", int(exitCode))
	}

	// 4. Check that the continue result includes stdout output.
	// The output may be in the continue result or may need to be read
	// separately. Check the continue result first.
	stdout, hasStdout := continueData["stdout"].(string)
	if hasStdout {
		if !strings.Contains(stdout, "hello from simple") {
			t.Errorf("stdout output: got %q, want it to contain %q", stdout, "hello from simple")
		}
	} else {
		// Output may have been captured but not yet drained into the
		// continue result. Try reading output explicitly.
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		req := makeCallToolRequest(nil)
		result, err := tools.handleReadOutput(ctx, req)
		if err != nil {
			t.Fatalf("handleReadOutput returned error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("handleReadOutput returned tool error: %s", text)
		}

		var outputData map[string]any
		text := result.Content[0].(mcp.TextContent).Text
		if err := json.Unmarshal([]byte(text), &outputData); err != nil {
			t.Fatalf("failed to unmarshal output result: %v", err)
		}

		stdout2, hasStdout2 := outputData["stdout"].(string)
		if !hasStdout2 || !strings.Contains(stdout2, "hello from simple") {
			t.Errorf("expected stdout to contain %q, got continue data: %v, output data: %v",
				"hello from simple", continueData, outputData)
		}
	}
}

// --- Subprocess crash recovery tests ---

// TestLLDBDAPCrashRecovery verifies that when the lldb-dap subprocess is
// killed externally, the server transitions to a terminated state and can
// successfully launch a new session afterward.
func TestLLDBDAPCrashRecovery(t *testing.T) {
	fixture := testFixturePath(t, "loop")

	// Step 1: Launch loop with stop_on_entry=true.
	tools, sm := newTestTools(t)

	launchData := launchFixture(t, tools, fixture)
	if launchData["state"] != "stopped" {
		t.Fatalf("expected state 'stopped' after launch, got %v", launchData["state"])
	}

	// Step 2: Verify session is in stopped state via status tool.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		result, err := tools.handleStatus(ctx, makeCallToolRequest(nil))
		if err != nil {
			t.Fatalf("handleStatus returned error: %v", err)
		}
		var data map[string]any
		text := result.Content[0].(mcp.TextContent).Text
		if err := json.Unmarshal([]byte(text), &data); err != nil {
			t.Fatalf("failed to unmarshal status: %v", err)
		}
		if data["state"] != "stopped" {
			t.Fatalf("expected status state 'stopped', got %q", data["state"])
		}
	}

	// Step 3: Kill the lldb-dap subprocess.
	sub := sm.Subprocess()
	if sub == nil || sub.Cmd.Process == nil {
		t.Fatal("subprocess or process is nil after launch")
	}

	if err := sub.Cmd.Process.Kill(); err != nil {
		t.Fatalf("failed to kill lldb-dap subprocess: %v", err)
	}

	// Let EOF propagate through the DAP client read loop so that
	// onTerminated fires and the state transitions to terminated.
	time.Sleep(200 * time.Millisecond)

	// Step 4: Call status -- should report terminated state.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		result, err := tools.handleStatus(ctx, makeCallToolRequest(nil))
		if err != nil {
			t.Fatalf("handleStatus after crash returned error: %v", err)
		}
		var data map[string]any
		text := result.Content[0].(mcp.TextContent).Text
		if err := json.Unmarshal([]byte(text), &data); err != nil {
			t.Fatalf("failed to unmarshal status after crash: %v", err)
		}
		if data["state"] != "terminated" {
			t.Fatalf("expected state 'terminated' after killing subprocess, got %q", data["state"])
		}
	}

	// Step 5: Disconnect (cleanup after crash).
	disconnectCleanup(t, tools)

	// Verify state is back to idle after disconnect.
	if s := sm.State(); s != session.StateIdle {
		t.Fatalf("expected state idle after disconnect, got %s", s)
	}

	// Step 6: Launch again -- verify a fresh session works.
	launchData = launchFixture(t, tools, fixture)
	if launchData["state"] != "stopped" {
		t.Fatalf("expected state 'stopped' after second launch, got %v", launchData["state"])
	}

	// Verify the new session reports stopped via status.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()

		result, err := tools.handleStatus(ctx, makeCallToolRequest(nil))
		if err != nil {
			t.Fatalf("handleStatus after re-launch returned error: %v", err)
		}
		var data map[string]any
		text := result.Content[0].(mcp.TextContent).Text
		if err := json.Unmarshal([]byte(text), &data); err != nil {
			t.Fatalf("failed to unmarshal status after re-launch: %v", err)
		}
		if data["state"] != "stopped" {
			t.Fatalf("expected state 'stopped' from status after second launch, got %q", data["state"])
		}
	}

	// Clean up the second session.
	disconnectCleanup(t, tools)
}

// TestLLDBDAPCrashDuringContinue verifies that killing the lldb-dap
// subprocess while a continue operation is blocked waiting for a stop event
// does not cause the server to hang. The continue call should return
// promptly (either with a terminated result or an error), and a new session
// should be launchable afterward.
func TestLLDBDAPCrashDuringContinue(t *testing.T) {
	fixture := testFixturePath(t, "loop")
	loopSource := testFixturePath(t, "loop.c")

	// Step 1: Launch loop with stop_on_entry=true.
	tools, sm := newTestTools(t)

	launchData := launchFixture(t, tools, fixture)
	if launchData["state"] != "stopped" {
		t.Fatalf("expected state 'stopped' after launch, got %v", launchData["state"])
	}

	// Step 2: Set breakpoint at line 6 (inside the loop: sum += i).
	{
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()

		req := makeCallToolRequest(map[string]any{
			"file": loopSource,
			"line": float64(6),
		})

		result, err := tools.handleSetBreakpoint(ctx, req)
		if err != nil {
			t.Fatalf("handleSetBreakpoint returned error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("handleSetBreakpoint returned tool error: %s", text)
		}
	}

	// Step 3: Start continue in a goroutine.
	type continueOutcome struct {
		result *mcp.CallToolResult
		err    error
	}

	continueCh := make(chan continueOutcome, 1)
	continueCtx, continueCancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer continueCancel()

	go func() {
		r, e := tools.handleContinue(continueCtx, makeCallToolRequest(nil))
		continueCh <- continueOutcome{result: r, err: e}
	}()

	// Step 4: Kill lldb-dap subprocess after a short delay to let the
	// continue request be sent and the program start running.
	time.Sleep(200 * time.Millisecond)

	sub := sm.Subprocess()
	if sub == nil || sub.Cmd.Process == nil {
		t.Fatal("subprocess or process is nil")
	}

	if err := sub.Cmd.Process.Kill(); err != nil {
		t.Fatalf("failed to kill lldb-dap subprocess: %v", err)
	}

	// Step 5: The continue call should return (not hang forever).
	// Acceptable outcomes after a crash:
	// a) A result with status "terminated" (StopWaiter delivered a cancel)
	// b) A tool error (client.Send failed on broken pipe/EOF)
	// c) A Go-level error (should not happen, but not a hang)
	// The critical assertion is that the call returns promptly.
	select {
	case cr := <-continueCh:
		if cr.err != nil {
			t.Logf("continue returned Go error (acceptable after crash): %v", cr.err)
		} else if cr.result != nil {
			text := cr.result.Content[0].(mcp.TextContent).Text
			t.Logf("continue returned: isError=%v text=%s", cr.result.IsError, text)

			if !cr.result.IsError {
				var data map[string]any
				if err := json.Unmarshal([]byte(text), &data); err == nil {
					status, _ := data["status"].(string)
					// After a crash, the StopWaiter should deliver a
					// terminated result, so status should be "terminated".
					if status != "terminated" && status != "stopped" && status != "exited" {
						t.Errorf("unexpected status %q in continue result after crash", status)
					}
				}
			}
		}

	case <-time.After(10 * time.Second):
		t.Fatal("continue call did not return within 10 seconds after killing subprocess -- the server is hanging")
	}

	// Step 6: Verify session is in terminated state.
	// Allow a brief moment for state propagation through callbacks.
	time.Sleep(100 * time.Millisecond)

	state := sm.State()
	// After a crash, the state should be terminated (set by the
	// onTerminated callback or handleStopResult). If the Send call
	// failed before the stop waiter registered, the continue handler
	// may have reverted state to stopped. Both are acceptable.
	if state != session.StateTerminated && state != session.StateStopped {
		t.Fatalf("expected state terminated or stopped after crash, got %s", state)
	}
	t.Logf("state after crash: %s", state)

	// Step 7: Disconnect and verify a new launch works.
	{
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()

		result, err := tools.handleDisconnect(ctx, makeCallToolRequest(map[string]any{"terminate": true}))
		if err != nil {
			t.Fatalf("handleDisconnect returned error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Logf("disconnect returned tool error (may be acceptable after crash): %s", text)
			// If disconnect could not proceed, force reset to idle.
			if sm.State() != session.StateIdle {
				sm.Reset()
			}
		}
	}

	// Ensure state is idle before re-launch.
	if sm.State() != session.StateIdle {
		t.Logf("state after disconnect: %s (forcing reset)", sm.State())
		sm.Reset()
	}

	// Launch again to verify the server is not in a broken state.
	launchData = launchFixture(t, tools, fixture)
	if launchData["state"] != "stopped" {
		t.Fatalf("expected state 'stopped' after re-launch, got %v", launchData["state"])
	}

	// Verify the fresh session is functional.
	if sm.State() != session.StateStopped {
		t.Fatalf("expected state stopped after re-launch, got %s", sm.State())
	}

	// Clean up.
	disconnectCleanup(t, tools)
}

// --- Crash handling tests ---

// parseToolResult extracts and parses the JSON text from a CallToolResult.
func parseToolResult(t *testing.T, result *mcp.CallToolResult) map[string]any {
	t.Helper()
	if len(result.Content) == 0 {
		t.Fatal("tool result has no content")
	}
	text, ok := result.Content[0].(mcp.TextContent)
	if !ok {
		t.Fatalf("tool result content is not TextContent, got %T", result.Content[0])
	}
	var data map[string]any
	if err := json.Unmarshal([]byte(text.Text), &data); err != nil {
		t.Fatalf("failed to parse tool result JSON: %v (raw: %s)", err, text.Text)
	}
	return data
}

// TestCrashHandling verifies that when a debugged program crashes (NULL
// pointer dereference / SIGSEGV), the server correctly:
//  1. Returns a stop result with reason "exception" or "signal" (not a tool error)
//  2. Shows the crash location in the backtrace (crash.c, line 7)
//  3. Allows run_command ("bt") to work while stopped at the crash site
func TestCrashHandling(t *testing.T) {
	fixture := testFixturePath(t, "crash")
	tools, _ := newTestTools(t)
	t.Cleanup(func() { disconnectCleanup(t, tools) })

	// 1. Launch crash program with stop_on_entry=true.
	launchData := launchFixture(t, tools, fixture)
	if launchData["status"] != "launched" {
		t.Fatalf("expected status 'launched', got %v", launchData["status"])
	}
	if launchData["state"] != "stopped" {
		t.Fatalf("expected state 'stopped', got %v", launchData["state"])
	}

	// 2. Continue -- the program will hit a NULL pointer dereference and crash.
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	continueReq := makeCallToolRequest(nil)
	continueResult, err := tools.handleContinue(ctx, continueReq)
	if err != nil {
		t.Fatalf("handleContinue returned error: %v", err)
	}

	// The continue should NOT be a tool error -- the program stopped due to
	// a signal/exception, which is a valid stop result.
	if continueResult.IsError {
		text := continueResult.Content[0].(mcp.TextContent).Text
		t.Fatalf("handleContinue returned tool error: %s", text)
	}

	continueData := parseToolResult(t, continueResult)

	// 3. Verify the stop reason is "signal" or "exception".
	stopReason, ok := continueData["reason"].(string)
	if !ok {
		t.Fatalf("expected 'reason' field in continue result, got: %v", continueData)
	}
	if stopReason != "exception" && stopReason != "signal" {
		t.Errorf("expected stop reason 'exception' or 'signal', got %q", stopReason)
	}

	// 4. Verify the status is "stopped" (not exited or terminated).
	if continueData["status"] != "stopped" {
		t.Errorf("expected status 'stopped', got %v", continueData["status"])
	}

	// 5. Verify backtrace shows crash frame referencing crash.c line 7.
	btReq := makeCallToolRequest(nil)
	btResult, err := tools.handleBacktrace(ctx, btReq)
	if err != nil {
		t.Fatalf("handleBacktrace returned error: %v", err)
	}
	if btResult.IsError {
		text := btResult.Content[0].(mcp.TextContent).Text
		t.Fatalf("handleBacktrace returned tool error: %s", text)
	}

	btData := parseToolResult(t, btResult)

	framesRaw, ok := btData["frames"].([]any)
	if !ok || len(framesRaw) == 0 {
		t.Fatalf("expected non-empty frames array, got: %v", btData["frames"])
	}

	// Look for a frame that references crash.c.
	foundCrashFrame := false
	for _, frameRaw := range framesRaw {
		frame, ok := frameRaw.(map[string]any)
		if !ok {
			continue
		}
		file, _ := frame["file"].(string)
		if strings.HasSuffix(file, "crash.c") {
			foundCrashFrame = true
			line, ok := frame["line"].(float64)
			if !ok {
				t.Errorf("expected line number in crash frame, got: %v", frame["line"])
			} else if int(line) != 7 {
				t.Errorf("expected crash at line 7, got line %d", int(line))
			}
			break
		}
	}
	if !foundCrashFrame {
		t.Errorf("no frame referencing crash.c found in backtrace: %v", framesRaw)
	}

	// 6. Verify run_command works in crash state (run "bt" command).
	rcReq := makeCallToolRequest(map[string]any{
		"command": "bt",
	})
	rcResult, err := tools.handleRunCommand(ctx, rcReq)
	if err != nil {
		t.Fatalf("handleRunCommand returned error: %v", err)
	}
	if rcResult.IsError {
		text := rcResult.Content[0].(mcp.TextContent).Text
		t.Fatalf("handleRunCommand returned tool error: %s", text)
	}

	rcData := parseToolResult(t, rcResult)

	// The "bt" command result should contain backtrace text referencing "main".
	resultText, ok := rcData["result"].(string)
	if !ok {
		t.Fatalf("expected 'result' string in run_command response, got: %v", rcData)
	}
	if !strings.Contains(resultText, "main") {
		t.Errorf("expected run_command 'bt' result to contain 'main', got: %q", resultText)
	}
}

// --- End-to-end debugging workflow test ---

// TestEndToEndDebuggingWorkflow exercises a realistic agent debugging scenario:
// launch, set breakpoint, continue, inspect, step, evaluate, run_command,
// manage breakpoints, and continue to exit.
func TestEndToEndDebuggingWorkflow(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	fixture := testFixturePath(t, "loop")
	loopSource := testFixturePath(t, "loop.c")

	tools, _ := newTestTools(t)
	t.Cleanup(func() { disconnectCleanup(t, tools) })

	// ---------------------------------------------------------------
	// Step 1: Launch loop with stop_on_entry=true.
	// ---------------------------------------------------------------
	launchData := launchFixture(t, tools, fixture)
	if launchData["status"] != "launched" {
		t.Fatalf("step 1: launch status: got %q, want %q", launchData["status"], "launched")
	}
	if launchData["state"] != "stopped" {
		t.Fatalf("step 1: launch state: got %q, want %q", launchData["state"], "stopped")
	}
	t.Log("step 1 passed: launched and stopped on entry")

	// ---------------------------------------------------------------
	// Step 2: Set breakpoint at loop.c line 6 (sum += i).
	// ---------------------------------------------------------------
	var firstBreakpointID float64
	{
		req := makeCallToolRequest(map[string]any{
			"file": loopSource,
			"line": float64(6),
		})
		result, err := tools.handleSetBreakpoint(ctx, req)
		if err != nil {
			t.Fatalf("step 2: handleSetBreakpoint error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 2: handleSetBreakpoint tool error: %s", text)
		}
		bpData := parseToolResult(t, result)
		bpID, ok := bpData["breakpoint_id"].(float64)
		if !ok {
			t.Fatalf("step 2: expected breakpoint_id in response, got: %v", bpData)
		}
		firstBreakpointID = bpID
		t.Logf("step 2 passed: set breakpoint id=%v at line 6", firstBreakpointID)
	}

	// ---------------------------------------------------------------
	// Step 3: Continue to breakpoint.
	// ---------------------------------------------------------------
	{
		continueData := callContinue(t, tools)
		status, _ := continueData["status"].(string)
		if status != "stopped" {
			t.Fatalf("step 3: expected status 'stopped', got %q", status)
		}
		reason, _ := continueData["reason"].(string)
		if !strings.Contains(reason, "breakpoint") {
			t.Fatalf("step 3: expected stop reason containing 'breakpoint', got %q", reason)
		}
		t.Logf("step 3 passed: continued and hit breakpoint (reason=%q)", reason)
	}

	// ---------------------------------------------------------------
	// Step 4: Backtrace -- verify at least 1 frame with "main".
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(nil)
		result, err := tools.handleBacktrace(ctx, req)
		if err != nil {
			t.Fatalf("step 4: handleBacktrace error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 4: handleBacktrace tool error: %s", text)
		}
		btData := parseToolResult(t, result)
		framesRaw, ok := btData["frames"].([]any)
		if !ok || len(framesRaw) == 0 {
			t.Fatalf("step 4: expected non-empty frames array, got: %v", btData["frames"])
		}
		foundMain := false
		for _, frameRaw := range framesRaw {
			frame, ok := frameRaw.(map[string]any)
			if !ok {
				continue
			}
			name, _ := frame["name"].(string)
			if strings.Contains(name, "main") {
				foundMain = true
				break
			}
		}
		if !foundMain {
			t.Fatalf("step 4: no frame with 'main' found in backtrace: %v", framesRaw)
		}
		t.Log("step 4 passed: backtrace contains 'main' frame")
	}

	// ---------------------------------------------------------------
	// Step 5: Variables -- verify "i" and "sum" exist in locals.
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(nil)
		result, err := tools.handleVariables(ctx, req)
		if err != nil {
			t.Fatalf("step 5: handleVariables error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 5: handleVariables tool error: %s", text)
		}
		varData := parseToolResult(t, result)
		varsRaw, ok := varData["variables"].([]any)
		if !ok {
			t.Fatalf("step 5: expected variables array, got: %v", varData["variables"])
		}
		foundI := false
		foundSum := false
		for _, v := range varsRaw {
			varEntry, ok := v.(map[string]any)
			if !ok {
				continue
			}
			name, _ := varEntry["name"].(string)
			if name == "i" {
				foundI = true
			}
			if name == "sum" {
				foundSum = true
			}
		}
		if !foundI {
			t.Fatalf("step 5: variable 'i' not found in locals: %v", varsRaw)
		}
		if !foundSum {
			t.Fatalf("step 5: variable 'sum' not found in locals: %v", varsRaw)
		}
		t.Log("step 5 passed: found variables 'i' and 'sum'")
	}

	// ---------------------------------------------------------------
	// Step 6: Step over 3 times, verify each succeeds (still in loop.c).
	// ---------------------------------------------------------------
	for stepNum := 1; stepNum <= 3; stepNum++ {
		req := makeCallToolRequest(nil)
		result, err := tools.handleStepOver(ctx, req)
		if err != nil {
			t.Fatalf("step 6.%d: handleStepOver error: %v", stepNum, err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 6.%d: handleStepOver tool error: %s", stepNum, text)
		}
		stepData := parseToolResult(t, result)
		status, _ := stepData["status"].(string)
		if status != "stopped" {
			t.Fatalf("step 6.%d: expected status 'stopped' after step, got %q", stepNum, status)
		}
		t.Logf("step 6.%d passed: step over completed", stepNum)
	}
	t.Log("step 6 passed: all 3 step overs completed")

	// ---------------------------------------------------------------
	// Step 7: Evaluate expression "i + 1", verify numeric result.
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(map[string]any{
			"expression": "i + 1",
		})
		result, err := tools.handleEvaluate(ctx, req)
		if err != nil {
			t.Fatalf("step 7: handleEvaluate error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 7: handleEvaluate tool error: %s", text)
		}
		evalData := parseToolResult(t, result)
		evalResult, ok := evalData["result"].(string)
		if !ok {
			t.Fatalf("step 7: expected 'result' string in evaluate response, got: %v", evalData)
		}
		// The result should contain a numeric value (e.g. "1", "2", etc.).
		hasDigit := false
		for _, ch := range evalResult {
			if ch >= '0' && ch <= '9' {
				hasDigit = true
				break
			}
		}
		if !hasDigit {
			t.Fatalf("step 7: expected numeric value in evaluate result, got %q", evalResult)
		}
		t.Logf("step 7 passed: evaluate 'i + 1' = %q", evalResult)
	}

	// ---------------------------------------------------------------
	// Step 8: Run command "register read", verify non-empty result.
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(map[string]any{
			"command": "register read",
		})
		result, err := tools.handleRunCommand(ctx, req)
		if err != nil {
			t.Fatalf("step 8: handleRunCommand error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 8: handleRunCommand tool error: %s", text)
		}
		rcData := parseToolResult(t, result)
		rcResult, ok := rcData["result"].(string)
		if !ok || rcResult == "" {
			t.Fatalf("step 8: expected non-empty 'result' from register read, got: %v", rcData)
		}
		t.Logf("step 8 passed: register read returned %d bytes of output", len(rcResult))
	}

	// ---------------------------------------------------------------
	// Step 9: Set second breakpoint at line 9 (printf("final sum=...")).
	// ---------------------------------------------------------------
	var secondBreakpointID float64
	{
		req := makeCallToolRequest(map[string]any{
			"file": loopSource,
			"line": float64(9),
		})
		result, err := tools.handleSetBreakpoint(ctx, req)
		if err != nil {
			t.Fatalf("step 9: handleSetBreakpoint error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 9: handleSetBreakpoint tool error: %s", text)
		}
		bpData := parseToolResult(t, result)
		bpID, ok := bpData["breakpoint_id"].(float64)
		if !ok {
			t.Fatalf("step 9: expected breakpoint_id in response, got: %v", bpData)
		}
		secondBreakpointID = bpID
		t.Logf("step 9 passed: set second breakpoint id=%v at line 9", secondBreakpointID)
	}

	// ---------------------------------------------------------------
	// Step 10: Continue to second breakpoint. The loop may still have
	// iterations remaining, so we may hit the first breakpoint (line 6)
	// several times before reaching the second breakpoint (line 9).
	// Keep continuing until we hit the second breakpoint.
	// ---------------------------------------------------------------
	{
		hitSecond := false
		for attempts := 0; attempts < 20; attempts++ {
			continueData := callContinue(t, tools)
			status, _ := continueData["status"].(string)
			if status != "stopped" {
				t.Fatalf("step 10: expected status 'stopped', got %q", status)
			}
			// Check if we hit the second breakpoint.
			if hitIDs, ok := continueData["hit_breakpoint_ids"].([]any); ok {
				for _, id := range hitIDs {
					if idFloat, ok := id.(float64); ok && idFloat == secondBreakpointID {
						hitSecond = true
						break
					}
				}
			}
			if hitSecond {
				break
			}
			t.Logf("step 10: hit breakpoint (not second), continuing...")
		}
		if !hitSecond {
			t.Fatal("step 10: never hit the second breakpoint after 20 continues")
		}
		t.Log("step 10 passed: continued and hit second breakpoint")
	}

	// ---------------------------------------------------------------
	// Step 11: Remove first breakpoint by ID.
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(map[string]any{
			"breakpoint_id": firstBreakpointID,
		})
		result, err := tools.handleRemoveBreakpoint(ctx, req)
		if err != nil {
			t.Fatalf("step 11: handleRemoveBreakpoint error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 11: handleRemoveBreakpoint tool error: %s", text)
		}
		rmData := parseToolResult(t, result)
		removed, ok := rmData["removed"].(bool)
		if !ok || !removed {
			t.Fatalf("step 11: expected removed=true, got: %v", rmData)
		}
		t.Logf("step 11 passed: removed breakpoint id=%v", firstBreakpointID)
	}

	// ---------------------------------------------------------------
	// Step 12: List breakpoints -- should have exactly 1 (second BP).
	// ---------------------------------------------------------------
	{
		req := makeCallToolRequest(nil)
		result, err := tools.handleListBreakpoints(ctx, req)
		if err != nil {
			t.Fatalf("step 12: handleListBreakpoints error: %v", err)
		}
		if result.IsError {
			text := result.Content[0].(mcp.TextContent).Text
			t.Fatalf("step 12: handleListBreakpoints tool error: %s", text)
		}
		bpListData := parseToolResult(t, result)
		count, ok := bpListData["count"].(float64)
		if !ok {
			t.Fatalf("step 12: expected 'count' in list breakpoints response, got: %v", bpListData)
		}
		if int(count) != 1 {
			t.Fatalf("step 12: expected 1 breakpoint remaining, got %d (data: %v)", int(count), bpListData)
		}
		// Verify the remaining breakpoint is the second one.
		bpsRaw, ok := bpListData["breakpoints"].([]any)
		if !ok || len(bpsRaw) != 1 {
			t.Fatalf("step 12: expected 1 breakpoint entry, got: %v", bpListData["breakpoints"])
		}
		bpEntry, ok := bpsRaw[0].(map[string]any)
		if !ok {
			t.Fatalf("step 12: breakpoint entry is not a map: %v", bpsRaw[0])
		}
		remainingID, ok := bpEntry["id"].(float64)
		if !ok {
			t.Fatalf("step 12: expected 'id' in breakpoint entry, got: %v", bpEntry)
		}
		if remainingID != secondBreakpointID {
			t.Fatalf("step 12: remaining breakpoint id=%v, expected %v", remainingID, secondBreakpointID)
		}
		t.Logf("step 12 passed: 1 breakpoint remaining (id=%v)", remainingID)
	}

	// ---------------------------------------------------------------
	// Step 13: Continue to exit.
	// ---------------------------------------------------------------
	{
		continueData := callContinue(t, tools)
		status, _ := continueData["status"].(string)
		if status != "exited" {
			t.Fatalf("step 13: expected status 'exited', got %q (data: %v)", status, continueData)
		}
		exitCode, ok := continueData["exit_code"].(float64)
		if !ok {
			t.Fatalf("step 13: expected exit_code, got: %v", continueData)
		}
		if int(exitCode) != 0 {
			t.Fatalf("step 13: expected exit_code=0, got %d", int(exitCode))
		}
		t.Log("step 13 passed: program exited with code 0")
	}

	t.Log("end-to-end debugging workflow completed successfully")
}
