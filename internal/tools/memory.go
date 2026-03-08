package tools

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	godap "github.com/google/go-dap"
	"github.com/mark3labs/mcp-go/mcp"

	"github.com/danweinerdev/lldb-debug-mcp/internal/session"
)

func (t *Tools) handleReadMemory(ctx context.Context, request mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	// 1. State guard: must be stopped.
	if err := t.session.CheckState(session.StateStopped); err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	// 2. Parse required parameters.
	address, err := request.RequireString("address")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	count, err := request.RequireInt("count")
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("missing required parameter: %s", err)), nil
	}

	// 3. Normalize address: ensure "0x" prefix for DAP.
	if !strings.HasPrefix(address, "0x") && !strings.HasPrefix(address, "0X") {
		address = "0x" + address
	}

	// 4. Send ReadMemoryRequest.
	req := &godap.ReadMemoryRequest{}
	req.Type = "request"
	req.Command = "readMemory"
	req.Arguments = godap.ReadMemoryArguments{
		MemoryReference: address,
		Count:           count,
	}

	resp, err := t.session.Client().Send(ctx, req)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("readMemory request failed: %s", err)), nil
	}

	// 5. Parse response.
	memResp, ok := resp.(*godap.ReadMemoryResponse)
	if !ok {
		return mcp.NewToolResultError(fmt.Sprintf("unexpected readMemory response type: %T", resp)), nil
	}
	if !memResp.Success {
		return mcp.NewToolResultError(fmt.Sprintf("readMemory failed: %s", memResp.Message)), nil
	}

	// 6. Handle empty data.
	if memResp.Body.Data == "" {
		result := map[string]any{
			"address":    memResp.Body.Address,
			"bytes_read": 0,
		}
		resultJSON, err := json.Marshal(result)
		if err != nil {
			return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
		}
		return mcp.NewToolResultText(string(resultJSON)), nil
	}

	// 7. Decode base64 data.
	data, err := base64.StdEncoding.DecodeString(memResp.Body.Data)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to decode memory data: %s", err)), nil
	}

	// 8. Parse the starting address for hex dump formatting.
	addrStr := strings.TrimPrefix(strings.TrimPrefix(address, "0x"), "0X")
	startAddr, err := strconv.ParseUint(addrStr, 16, 64)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to parse address: %s", err)), nil
	}

	// 9. Format as hex dump (rows of 16 bytes).
	hexDump := formatHexDump(data, startAddr)

	// 10. Return JSON.
	result := map[string]any{
		"address":    memResp.Body.Address,
		"bytes_read": len(data),
		"hex_dump":   hexDump,
	}
	resultJSON, err := json.Marshal(result)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf("failed to marshal result: %s", err)), nil
	}
	return mcp.NewToolResultText(string(resultJSON)), nil
}

// formatHexDump formats raw bytes as a hex dump with 16 bytes per row.
// Each row contains an address column, hex bytes (split into two groups of 8),
// and an ASCII representation. Non-printable characters are shown as '.'.
//
// Example output:
//
//	0x7fff5000: 48 65 6c 6c 6f 20 57 6f  72 6c 64 21 00 00 00 00  |Hello World!....|
func formatHexDump(data []byte, startAddr uint64) string {
	var sb strings.Builder
	for offset := 0; offset < len(data); offset += 16 {
		// Address column.
		fmt.Fprintf(&sb, "0x%08x: ", startAddr+uint64(offset))

		// Hex bytes.
		end := offset + 16
		if end > len(data) {
			end = len(data)
		}
		row := data[offset:end]

		for i := 0; i < 16; i++ {
			if i == 8 {
				sb.WriteByte(' ')
			}
			if i < len(row) {
				fmt.Fprintf(&sb, "%02x ", row[i])
			} else {
				sb.WriteString("   ")
			}
		}

		// ASCII representation.
		sb.WriteString(" |")
		for i := 0; i < 16; i++ {
			if i < len(row) {
				b := row[i]
				if b >= 0x20 && b <= 0x7e {
					sb.WriteByte(b)
				} else {
					sb.WriteByte('.')
				}
			} else {
				sb.WriteByte(' ')
			}
		}
		sb.WriteByte('|')

		// Add newline between rows (not after last).
		if offset+16 < len(data) {
			sb.WriteByte('\n')
		}
	}
	return sb.String()
}

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
