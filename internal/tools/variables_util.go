package tools

import (
	"context"
	"fmt"
	"strings"

	godap "github.com/google/go-dap"

	"github.com/danweinerdev/lldb-debug-mcp/internal/dap"
)

// FlatVariable is a simplified representation of a DAP variable, suitable
// for JSON serialization in tool responses. Nested variable names use
// dot-separated paths (e.g. "parent.child.grandchild").
type FlatVariable struct {
	Name          string `json:"name"`
	Value         string `json:"value"`
	Type          string `json:"type,omitempty"`
	HasChildren   bool   `json:"has_children,omitempty"`
	ChildrenCount int    `json:"children_count,omitempty"`
}

// FlattenVariables fetches variables from a DAP variables reference and
// returns them as a flat list. It recursively expands children up to the
// given depth, prefixing child names with their parent path. The filter
// parameter applies a case-insensitive substring match on variable names
// at the top level only. If the result count reaches maxCount, the
// remaining variables are skipped and truncated is set to true.
func FlattenVariables(ctx context.Context, client *dap.Client, variablesReference int, depth int, maxCount int, filter string) ([]FlatVariable, bool, error) {
	// Send the VariablesRequest to the DAP adapter.
	req := &godap.VariablesRequest{}
	req.Type = "request"
	req.Command = "variables"
	req.Arguments = godap.VariablesArguments{
		VariablesReference: variablesReference,
	}

	resp, err := client.Send(ctx, req)
	if err != nil {
		return nil, false, fmt.Errorf("variables request failed: %w", err)
	}

	varResp, ok := resp.(*godap.VariablesResponse)
	if !ok {
		return nil, false, fmt.Errorf("unexpected variables response type: %T", resp)
	}
	if !varResp.Success {
		return nil, false, fmt.Errorf("variables request failed: %s", varResp.Message)
	}

	var result []FlatVariable
	filterLower := strings.ToLower(filter)

	for _, v := range varResp.Body.Variables {
		// Apply filter at the top level only.
		if filter != "" && !strings.Contains(strings.ToLower(v.Name), filterLower) {
			continue
		}

		flat := FlatVariable{
			Name:  v.Name,
			Value: v.Value,
			Type:  v.Type,
		}

		if v.VariablesReference > 0 {
			if depth > 0 {
				// Recurse into children.
				result = append(result, flat)
				if len(result) >= maxCount {
					return result, true, nil
				}
				truncated, err := flattenRecursive(ctx, client, v.VariablesReference, v.Name, depth-1, &result, maxCount)
				if err != nil {
					return nil, false, err
				}
				if truncated {
					return result, true, nil
				}
			} else {
				// No more depth: mark as having children.
				flat.HasChildren = true
				flat.ChildrenCount = v.NamedVariables + v.IndexedVariables
				result = append(result, flat)
				if len(result) >= maxCount {
					return result, true, nil
				}
			}
		} else {
			result = append(result, flat)
			if len(result) >= maxCount {
				return result, true, nil
			}
		}
	}

	return result, false, nil
}

// flattenRecursive is the recursive helper for FlattenVariables. It fetches
// children of the given variable reference, prefixes their names with the
// parent path, and appends them to result. No filter is applied at child
// levels. Returns true if the result was truncated due to reaching maxCount.
func flattenRecursive(ctx context.Context, client *dap.Client, varRef int, prefix string, depth int, result *[]FlatVariable, maxCount int) (bool, error) {
	req := &godap.VariablesRequest{}
	req.Type = "request"
	req.Command = "variables"
	req.Arguments = godap.VariablesArguments{
		VariablesReference: varRef,
	}

	resp, err := client.Send(ctx, req)
	if err != nil {
		return false, fmt.Errorf("variables request failed: %w", err)
	}

	varResp, ok := resp.(*godap.VariablesResponse)
	if !ok {
		return false, fmt.Errorf("unexpected variables response type: %T", resp)
	}
	if !varResp.Success {
		return false, fmt.Errorf("variables request failed: %s", varResp.Message)
	}

	for _, v := range varResp.Body.Variables {
		childName := prefix + "." + v.Name
		flat := FlatVariable{
			Name:  childName,
			Value: v.Value,
			Type:  v.Type,
		}

		if v.VariablesReference > 0 {
			if depth > 0 {
				*result = append(*result, flat)
				if len(*result) >= maxCount {
					return true, nil
				}
				truncated, err := flattenRecursive(ctx, client, v.VariablesReference, childName, depth-1, result, maxCount)
				if err != nil {
					return false, err
				}
				if truncated {
					return true, nil
				}
			} else {
				flat.HasChildren = true
				flat.ChildrenCount = v.NamedVariables + v.IndexedVariables
				*result = append(*result, flat)
				if len(*result) >= maxCount {
					return true, nil
				}
			}
		} else {
			*result = append(*result, flat)
			if len(*result) >= maxCount {
				return true, nil
			}
		}
	}

	return false, nil
}
