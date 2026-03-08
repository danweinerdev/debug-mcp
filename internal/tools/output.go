package tools

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

// formatOutputEntries groups output entries by category and returns a map
// suitable for JSON serialization. If entries is empty, returns a map with
// count 0 and an empty output string. Otherwise, returns count and the
// category-keyed output strings (stdout, stderr, console) that are present.
func formatOutputEntries(entries []session.OutputEntry) map[string]any {
	if len(entries) == 0 {
		return map[string]any{
			"output": "",
			"count":  0,
		}
	}

	var stdout, stderr, console strings.Builder
	for _, e := range entries {
		switch e.Category {
		case "stdout":
			stdout.WriteString(e.Text)
		case "stderr":
			stderr.WriteString(e.Text)
		default:
			console.WriteString(e.Text)
		}
	}

	result := map[string]any{
		"count": len(entries),
	}
	if stdout.Len() > 0 {
		result["stdout"] = stdout.String()
	}
	if stderr.Len() > 0 {
		result["stderr"] = stderr.String()
	}
	if console.Len() > 0 {
		result["console"] = console.String()
	}

	return result
}

func (t *Tools) handleReadOutput(_ context.Context, _ mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// State guard: allow any state except idle (need at least a session).
	if err := t.session.CheckState(session.StateConfiguring, session.StateStopped, session.StateRunning, session.StateTerminated); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	entries := t.session.OutputBuffer().Drain()
	result := formatOutputEntries(entries)

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal output: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
