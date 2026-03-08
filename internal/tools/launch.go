package tools

import (
	"context"
	"encoding/json"
	"fmt"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/dap"
	"github.com/danweinerdev/lldb-debug-mcp/internal/detect"
	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

// launchResult is used to receive the launch response from a goroutine.
type launchResult struct {
	msg godap.Message
	err error
}

// cleanupSubprocess kills the lldb-dap subprocess and resets the session.
func (t *Tools) cleanupSubprocess() {
	sub := t.session.Subprocess()
	if sub != nil && sub.Cmd.Process != nil {
		sub.Stdin.Close()
		sub.Cmd.Process.Kill()
		sub.Cmd.Wait()
	}
	t.session.Reset()
}

func (t *Tools) handleLaunch(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be idle.
	if err := t.session.CheckState(session.StateIdle); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	program, err := request.RequireString("program")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	// Parse optional args: JSON array of strings passed as a string parameter.
	var args []string
	if argsRaw, ok := request.GetArguments()["args"]; ok && argsRaw != nil {
		argsStr, ok := argsRaw.(string)
		if !ok {
			return mcp.NewToolResultError("'args' must be a JSON array string, e.g. '[\"--flag\", \"value\"]'"), nil
		}
		if err := json.Unmarshal([]byte(argsStr), &args); err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to parse 'args' as JSON array: %s", err)), nil
		}
	}

	// Parse optional cwd.
	cwd := request.GetString("cwd", "")

	// Parse optional env: JSON object string.
	var env map[string]string
	if envRaw, ok := request.GetArguments()["env"]; ok && envRaw != nil {
		envStr, ok := envRaw.(string)
		if !ok {
			return mcp.NewToolResultError("'env' must be a JSON object string, e.g. '{\"KEY\": \"value\"}'"), nil
		}
		if err := json.Unmarshal([]byte(envStr), &env); err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to parse 'env' as JSON object: %s", err)), nil
		}
	}

	// Parse optional stop_on_entry (default true).
	stopOnEntry := request.GetBool("stop_on_entry", true)

	// 3. Set state to configuring.
	t.session.SetState(session.StateConfiguring)

	// 4. Find lldb-dap binary.
	dapPath, isLLDBDAP, err := detect.FindLLDBDAP()
	if err != nil {
		t.session.Reset()
		return mcp.NewToolResultError(fmt.Sprintf("failed to find lldb-dap: %s", err)), nil
	}

	// 5. Spawn subprocess.
	sub, err := detect.SpawnLLDBDAP(dapPath, isLLDBDAP)
	if err != nil {
		t.session.Reset()
		return mcp.NewToolResultError(fmt.Sprintf("failed to spawn lldb-dap: %s", err)), nil
	}
	t.session.SetSubprocess(sub)
	t.session.SetReplModeCommand(isLLDBDAP)

	// 6. Create DAP client with subprocess stdin/stdout.
	client := dap.NewClient(sub.Stdout, sub.Stdin)
	t.session.SetClient(client)

	// 7. Set up event callbacks BEFORE starting ReadLoop.
	client.SetOutputHandler(func(event *godap.OutputEvent) {
		t.session.OutputBuffer().Append(event.Body.Category, event.Body.Output)
	})
	client.SetOnStopped(func(event *godap.StoppedEvent) {
		t.session.SetLastStoppedEvent(event)
	})
	client.SetOnExit(func(exitCode int) {
		t.session.SetExitCode(exitCode)
	})
	client.SetOnTerminated(func() {
		t.session.SetState(session.StateTerminated)
	})

	// 8. Start read loop.
	go client.ReadLoop()

	// 9. Send InitializeRequest.
	initReq := &godap.InitializeRequest{}
	initReq.Type = "request"
	initReq.Command = "initialize"
	initReq.Arguments = godap.InitializeRequestArguments{
		ClientID:                     "lldb-debug-mcp",
		AdapterID:                    "lldb-dap",
		PathFormat:                   "path",
		LinesStartAt1:               true,
		ColumnsStartAt1:             true,
		SupportsVariableType:         true,
		SupportsRunInTerminalRequest: false,
	}

	initResp, err := client.Send(ctx, initReq)
	if err != nil {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("initialize request failed: %s", err)), nil
	}
	initResponse, ok := initResp.(*godap.InitializeResponse)
	if !ok {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("unexpected initialize response type: %T", initResp)), nil
	}
	if !initResponse.Success {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("initialize failed: %s", initResponse.Message)), nil
	}

	// 10. Build LLDBDAPLaunchArgs.
	launchArgs := dap.LLDBDAPLaunchArgs{
		Program:     program,
		StopOnEntry: stopOnEntry,
	}
	if len(args) > 0 {
		launchArgs.Args = args
	}
	if cwd != "" {
		launchArgs.Cwd = cwd
	}
	if len(env) > 0 {
		launchArgs.Env = env
	}

	argsJSON, err := json.Marshal(launchArgs)
	if err != nil {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal launch args: %s", err)), nil
	}

	// 11. Send LaunchRequest via goroutine (to wait for both response and event).
	launchReq := &godap.LaunchRequest{}
	launchReq.Type = "request"
	launchReq.Command = "launch"
	launchReq.Arguments = json.RawMessage(argsJSON)

	launchCh := make(chan launchResult, 1)
	go func() {
		msg, err := client.Send(ctx, launchReq)
		launchCh <- launchResult{msg: msg, err: err}
	}()

	// 12. Wait for BOTH LaunchResponse AND InitializedEvent (order-independent).
	var launchResp godap.Message
	var gotInitialized bool
	for !gotInitialized || launchResp == nil {
		select {
		case r := <-launchCh:
			if r.err != nil {
				t.cleanupSubprocess()
				return mcp.NewToolResultError(fmt.Sprintf("launch request failed: %s", r.err)), nil
			}
			launchResp = r.msg
			launchCh = nil // prevent re-receive
		case <-client.InitializedChan():
			gotInitialized = true
		case <-ctx.Done():
			t.cleanupSubprocess()
			return mcp.NewToolResultError(fmt.Sprintf("launch timed out: %s", ctx.Err())), nil
		}
	}

	// Check LaunchResponse success.
	lr, ok := launchResp.(*godap.LaunchResponse)
	if !ok {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("unexpected launch response type: %T", launchResp)), nil
	}
	if !lr.Success {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("launch failed: %s", lr.Message)), nil
	}

	// 13. Send SetExceptionBreakpointsRequest (empty filters).
	exBpReq := &godap.SetExceptionBreakpointsRequest{}
	exBpReq.Type = "request"
	exBpReq.Command = "setExceptionBreakpoints"
	exBpReq.Arguments = godap.SetExceptionBreakpointsArguments{}

	_, err = client.Send(ctx, exBpReq)
	if err != nil {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("setExceptionBreakpoints failed: %s", err)), nil
	}

	// 14. Send ConfigurationDoneRequest.
	configReq := &godap.ConfigurationDoneRequest{}
	configReq.Type = "request"
	configReq.Command = "configurationDone"

	_, err = client.Send(ctx, configReq)
	if err != nil {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("configurationDone failed: %s", err)), nil
	}

	// 15. Set program and PID.
	t.session.SetProgram(program)
	if sub.Cmd.Process != nil {
		t.session.SetPID(sub.Cmd.Process.Pid)
	}

	// 16. Handle stop_on_entry.
	if stopOnEntry {
		waiterCh := client.StopWaiter().Register()
		select {
		case result := <-waiterCh:
			if result.Exited || result.Terminated {
				t.session.SetState(session.StateTerminated)
				return mcp.NewToolResultText("Program exited during launch"), nil
			}
			t.session.SetState(session.StateStopped)

			// Build result with stop info.
			resultMap := map[string]any{
				"status":  "launched",
				"program": program,
				"pid":     t.session.PID(),
				"state":   "stopped",
			}
			if result.Event != nil {
				resultMap["stop_reason"] = result.Event.Body.Reason
				resultMap["stopped_thread_id"] = result.Event.Body.ThreadId
			}
			resultJSON, err := json.Marshal(resultMap)
			if err != nil {
				return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
			}
			return mcp.NewToolResultText(string(resultJSON)), nil

		case <-ctx.Done():
			t.cleanupSubprocess()
			return mcp.NewToolResultError(fmt.Sprintf("timed out waiting for stop on entry: %s", ctx.Err())), nil
		}
	}

	// Not stopping on entry: set state to running.
	t.session.SetState(session.StateRunning)

	// 17. Return JSON result.
	resultMap := map[string]any{
		"status":  "launched",
		"program": program,
		"pid":     t.session.PID(),
		"state":   "running",
	}
	resultJSON, err := json.Marshal(resultMap)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
