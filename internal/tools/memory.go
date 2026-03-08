package tools

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleDisassemble(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse parameters.
	address := ""
	if raw, ok := request.GetArguments()["address"]; ok && raw != nil {
		if a, ok := raw.(string); ok && a != "" {
			address = a
		}
	}

	instructionCount := 10
	if raw, ok := request.GetArguments()["instruction_count"]; ok && raw != nil {
		if ic, ok := raw.(float64); ok && ic > 0 {
			instructionCount = int(ic)
		}
	}

	// 3. If no address provided, get current PC from stack trace.
	currentPC := ""
	if address == "" {
		threadID := 1
		if lastEvent := t.session.LastStoppedEvent(); lastEvent != nil {
			threadID = lastEvent.Body.ThreadId
		}

		stReq := &godap.StackTraceRequest{}
		stReq.Type = "request"
		stReq.Command = "stackTrace"
		stReq.Arguments = godap.StackTraceArguments{
			ThreadId: threadID,
			Levels:   1,
		}

		resp, err := t.session.Client().Send(ctx, stReq)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("stackTrace request failed: %s", err)), nil
		}

		stackResp, ok := resp.(*godap.StackTraceResponse)
		if !ok {
			return mcp.NewToolResultError(fmt.Sprintf("unexpected stackTrace response type: %T", resp)), nil
		}
		if !stackResp.Success {
			return mcp.NewToolResultError(fmt.Sprintf("stackTrace failed: %s", stackResp.Message)), nil
		}

		if len(stackResp.Body.StackFrames) == 0 || stackResp.Body.StackFrames[0].InstructionPointerReference == "" {
			return mcp.NewToolResultError("no instruction pointer available for current frame"), nil
		}

		address = stackResp.Body.StackFrames[0].InstructionPointerReference
		currentPC = address
	}

	// 4. Normalize address: ensure "0x" prefix.
	if !strings.HasPrefix(address, "0x") && !strings.HasPrefix(address, "0X") {
		address = "0x" + address
	}
	if currentPC != "" && !strings.HasPrefix(currentPC, "0x") && !strings.HasPrefix(currentPC, "0X") {
		currentPC = "0x" + currentPC
	}

	// 5. Send DisassembleRequest.
	req := &godap.DisassembleRequest{}
	req.Type = "request"
	req.Command = "disassemble"
	req.Arguments = godap.DisassembleArguments{
		MemoryReference:  address,
		InstructionCount: instructionCount,
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("disassemble request failed: %s", err)), nil
	}

	// 6. Parse response.
	disResp, ok := resp.(*godap.DisassembleResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected disassemble response type: %T", resp)), nil
	}
	if !disResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("disassemble failed: %s", disResp.Message)), nil
	}

	// 7. Format instructions.
	instructions := make([]map[string]any, 0, len(disResp.Body.Instructions))
	for _, i := range disResp.Body.Instructions {
		inst := map[string]any{
			"address":     i.Address,
			"instruction": i.Instruction,
		}
		if i.InstructionBytes != "" {
			inst["bytes"] = i.InstructionBytes
		}
		if i.Symbol != "" {
			inst["symbol"] = i.Symbol
		}
		if i.Location != nil && i.Location.Path != "" {
			inst["file"] = i.Location.Path
			inst["line"] = i.Line
		}
		// Mark current PC.
		if currentPC != "" && i.Address == currentPC {
			inst["is_current_pc"] = true
		}
		instructions = append(instructions, inst)
	}

	// 8. Return JSON.
	result := map[string]any{
		"instructions":  instructions,
		"count":         len(instructions),
		"start_address": address,
	}

	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}
