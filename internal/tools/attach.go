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

// attachResult is used to receive the attach response from a goroutine.
type attachResult struct {
	msg godap.Message
	err error
}

func (t *Tools) handleAttach(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be idle.
	if err := t.session.CheckState(session.StateIdle); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters: pid (number, optional) or wait_for (string, optional).
	//    At least one must be provided. If both, pid takes precedence.
	var pid int
	var waitForName string
	var programName string

	pidRaw, pidPresent := request.GetArguments()["pid"]
	waitForRaw, waitForPresent := request.GetArguments()["wait_for"]

	if pidPresent && pidRaw != nil {
		pidFloat, ok := pidRaw.(float64)
		if !ok {
			return mcp.NewToolResultError("'pid' must be a number"), nil
		}
		pid = int(pidFloat)
		if pid <= 0 {
			return mcp.NewToolResultError("'pid' must be a positive integer"), nil
		}
		programName = fmt.Sprintf("pid:%d", pid)
	} else if waitForPresent && waitForRaw != nil {
		waitForName, _ = waitForRaw.(string)
		if waitForName == "" {
			return mcp.NewToolResultError("'wait_for' must be a non-empty string"), nil
		}
		programName = waitForName
	} else {
		return mcp.NewToolResultError("either 'pid' or 'wait_for' must be provided"), nil
	}

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

	// 10. Build LLDBDAPAttachArgs.
	attachArgs := dap.LLDBDAPAttachArgs{
		StopOnEntry: true,
	}
	if pid > 0 {
		attachArgs.PID = pid
	} else {
		attachArgs.WaitFor = true
		attachArgs.Program = waitForName
	}

	argsJSON, err := json.Marshal(attachArgs)
	if err != nil {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal attach args: %s", err)), nil
	}

	// 11. Send AttachRequest via goroutine (to wait for both response and event).
	attachReq := &godap.AttachRequest{}
	attachReq.Type = "request"
	attachReq.Command = "attach"
	attachReq.Arguments = json.RawMessage(argsJSON)

	attachCh := make(chan attachResult, 1)
	go func() {
		msg, err := client.Send(ctx, attachReq)
		attachCh <- attachResult{msg: msg, err: err}
	}()

	// 12. Wait for BOTH AttachResponse AND InitializedEvent (order-independent).
	var attachResp godap.Message
	var gotInitialized bool
	for !gotInitialized || attachResp == nil {
		select {
		case r := <-attachCh:
			if r.err != nil {
				t.cleanupSubprocess()
				return mcp.NewToolResultError(fmt.Sprintf("attach request failed: %s", r.err)), nil
			}
			attachResp = r.msg
			attachCh = nil // prevent re-receive
		case <-client.InitializedChan():
			gotInitialized = true
		case <-ctx.Done():
			t.cleanupSubprocess()
			return mcp.NewToolResultError(fmt.Sprintf("attach timed out: %s", ctx.Err())), nil
		}
	}

	// Check AttachResponse success.
	ar, ok := attachResp.(*godap.AttachResponse)
	if !ok {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("unexpected attach response type: %T", attachResp)), nil
	}
	if !ar.Success {
		t.cleanupSubprocess()
		return mcp.NewToolResultError(fmt.Sprintf("attach failed: %s", ar.Message)), nil
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

	// 15. Set program and PID info.
	t.session.SetProgram(programName)
	if pid > 0 {
		t.session.SetPID(pid)
	} else if sub.Cmd.Process != nil {
		t.session.SetPID(sub.Cmd.Process.Pid)
	}

	// 16. Wait for StoppedEvent (attach always uses stop_on_entry=true).
	waiterCh := client.StopWaiter().Register()
	select {
	case result := <-waiterCh:
		if result.Exited || result.Terminated {
			t.session.SetState(session.StateTerminated)
			return mcp.NewToolResultText("Process exited during attach"), nil
		}
		t.session.SetState(session.StateStopped)

		// Build result with stop info.
		resultMap := map[string]any{
			"status":  "attached",
			"program": programName,
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
