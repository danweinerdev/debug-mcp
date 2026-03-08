package tools

import (
	"context"
	"encoding/json"
	"time"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleDisconnect(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// State guard: must NOT be idle (any other state is valid for disconnect).
	if err := t.session.CheckState(session.StateConfiguring, session.StateStopped, session.StateRunning, session.StateTerminated); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// Parse optional terminate parameter (default true).
	terminate := request.GetBool("terminate", true)

	// Try to send DisconnectRequest if we have a client.
	client := t.session.Client()
	if client != nil {
		disconnectReq := &godap.DisconnectRequest{}
		disconnectReq.Type = "request"
		disconnectReq.Command = "disconnect"
		disconnectReq.Arguments = &godap.DisconnectArguments{
			TerminateDebuggee: terminate,
		}

		// Use a short timeout context for the disconnect request
		// (5 seconds should be more than enough).
		disconnectCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
		defer cancel()

		// Send disconnect — ignore errors (we're cleaning up anyway).
		client.Send(disconnectCtx, disconnectReq)

		// Cancel any pending StopWaiter.
		client.StopWaiter().Cancel()
	}

	// Wait for subprocess to exit (with timeout).
	sub := t.session.Subprocess()
	if sub != nil && sub.Cmd.Process != nil {
		// Close stdin to signal we're done.
		sub.Stdin.Close()

		// Wait for subprocess exit with 5-second timeout.
		done := make(chan error, 1)
		go func() {
			done <- sub.Cmd.Wait()
		}()

		select {
		case <-done:
			// Subprocess exited cleanly.
		case <-time.After(5 * time.Second):
			// Force kill.
			sub.Cmd.Process.Kill()
			<-done // wait for Wait() to complete after kill
		}
	}

	// Reset session to idle.
	t.session.Reset()

	// Return success.
	result := map[string]any{
		"status": "disconnected",
	}
	resultJSON, _ := json.Marshal(result)
	return mcp.NewToolResultText(string(resultJSON)), nil
}
