package tools

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"testing"
	"time"

	godap "github.com/google/go-dap"

	"github.com/danweinerdev/lldb-debug-mcp/internal/dap"
)

// makeVariablesResponse creates a VariablesResponse with the given
// sequence numbers and variables.
func makeVariablesResponse(seq, requestSeq int, vars []godap.Variable) *godap.VariablesResponse {
	resp := &godap.VariablesResponse{}
	resp.Seq = seq
	resp.Type = "response"
	resp.Command = "variables"
	resp.RequestSeq = requestSeq
	resp.Success = true
	resp.Body = godap.VariablesResponseBody{
		Variables: vars,
	}
	return resp
}

// fakeVarServer reads VariablesRequests from clientWritePR and responds using
// the provided handler function. The handler receives the VariablesArguments
// and returns the variables to include in the response.
func fakeVarServer(
	t *testing.T,
	clientWritePR *io.PipeReader,
	clientReadPW *io.PipeWriter,
	handler func(args godap.VariablesArguments) []godap.Variable,
) {
	t.Helper()
	reader := bufio.NewReader(clientWritePR)
	seq := 0
	for {
		msg, err := godap.ReadProtocolMessage(reader)
		if err != nil {
			return
		}
		req, ok := msg.(*godap.VariablesRequest)
		if !ok {
			t.Errorf("fakeVarServer: expected *VariablesRequest, got %T", msg)
			return
		}
		seq++
		vars := handler(req.Arguments)
		resp := makeVariablesResponse(seq, req.GetRequest().Seq, vars)
		if err := godap.WriteProtocolMessage(clientReadPW, resp); err != nil {
			t.Errorf("fakeVarServer: failed to write response: %v", err)
			return
		}
	}
}

// setupTestClient creates a DAP client connected via pipes and a cleanup
// function. The caller must start fakeVarServer in a goroutine using the
// returned PipeReader and PipeWriter.
func setupTestClient(t *testing.T) (client *dap.Client, clientWritePR *io.PipeReader, clientReadPW *io.PipeWriter) {
	t.Helper()
	clientReadPR, clientReadPW := io.Pipe()
	clientWritePR, clientWritePW := io.Pipe()
	t.Cleanup(func() {
		clientReadPR.Close()
		clientReadPW.Close()
		clientWritePR.Close()
		clientWritePW.Close()
	})

	client = dap.NewClient(bufio.NewReader(clientReadPR), clientWritePW)
	go client.ReadLoop()

	return client, clientWritePR, clientReadPW
}

func TestFlatVariableJSONMarshal(t *testing.T) {
	tests := []struct {
		name     string
		input    FlatVariable
		expected map[string]any
	}{
		{
			name: "basic variable",
			input: FlatVariable{
				Name:  "x",
				Value: "42",
				Type:  "int",
			},
			expected: map[string]any{
				"name":  "x",
				"value": "42",
				"type":  "int",
			},
		},
		{
			name: "variable with children indicator",
			input: FlatVariable{
				Name:          "myStruct",
				Value:         "{...}",
				Type:          "MyStruct",
				HasChildren:   true,
				ChildrenCount: 3,
			},
			expected: map[string]any{
				"name":           "myStruct",
				"value":          "{...}",
				"type":           "MyStruct",
				"has_children":   true,
				"children_count": float64(3),
			},
		},
		{
			name: "omit empty type",
			input: FlatVariable{
				Name:  "val",
				Value: "hello",
			},
			expected: map[string]any{
				"name":  "val",
				"value": "hello",
			},
		},
		{
			name: "omit false has_children and zero children_count",
			input: FlatVariable{
				Name:  "leaf",
				Value: "true",
				Type:  "bool",
			},
			expected: map[string]any{
				"name":  "leaf",
				"value": "true",
				"type":  "bool",
			},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			data, err := json.Marshal(tc.input)
			if err != nil {
				t.Fatalf("json.Marshal failed: %v", err)
			}

			var got map[string]any
			if err := json.Unmarshal(data, &got); err != nil {
				t.Fatalf("json.Unmarshal failed: %v", err)
			}

			for key, want := range tc.expected {
				if got[key] != want {
					t.Errorf("key %q: got %v, want %v", key, got[key], want)
				}
			}

			// Check no unexpected keys.
			for key := range got {
				if _, ok := tc.expected[key]; !ok {
					t.Errorf("unexpected key in JSON: %q = %v", key, got[key])
				}
			}
		})
	}
}

func TestFlattenVariablesBasic(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{Name: "x", Value: "42", Type: "int", VariablesReference: 0},
			{Name: "y", Value: "3.14", Type: "float64", VariablesReference: 0},
			{Name: "name", Value: `"hello"`, Type: "string", VariablesReference: 0},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 3 {
		t.Fatalf("expected 3 variables, got %d", len(result))
	}

	if result[0].Name != "x" || result[0].Value != "42" || result[0].Type != "int" {
		t.Errorf("result[0]: got %+v", result[0])
	}
	if result[1].Name != "y" || result[1].Value != "3.14" || result[1].Type != "float64" {
		t.Errorf("result[1]: got %+v", result[1])
	}
	if result[2].Name != "name" || result[2].Value != `"hello"` || result[2].Type != "string" {
		t.Errorf("result[2]: got %+v", result[2])
	}
}

func TestFlattenVariablesFilter(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{Name: "count", Value: "10", Type: "int", VariablesReference: 0},
			{Name: "Counter", Value: "20", Type: "int", VariablesReference: 0},
			{Name: "name", Value: `"test"`, Type: "string", VariablesReference: 0},
			{Name: "myCount", Value: "30", Type: "int", VariablesReference: 0},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "count")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 3 {
		t.Fatalf("expected 3 variables matching 'count', got %d", len(result))
	}

	// Verify case-insensitive matching.
	if result[0].Name != "count" {
		t.Errorf("result[0].Name: got %q, want %q", result[0].Name, "count")
	}
	if result[1].Name != "Counter" {
		t.Errorf("result[1].Name: got %q, want %q", result[1].Name, "Counter")
	}
	if result[2].Name != "myCount" {
		t.Errorf("result[2].Name: got %q, want %q", result[2].Name, "myCount")
	}
}

func TestFlattenVariablesHasChildrenAtDepthZero(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{Name: "x", Value: "1", Type: "int", VariablesReference: 0},
			{Name: "myStruct", Value: "{...}", Type: "MyStruct", VariablesReference: 5, NamedVariables: 3},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 2 {
		t.Fatalf("expected 2 variables, got %d", len(result))
	}

	// The leaf variable should not have children.
	if result[0].HasChildren {
		t.Error("result[0].HasChildren: expected false for leaf variable")
	}

	// The struct variable should indicate children at depth 0.
	if !result[1].HasChildren {
		t.Error("result[1].HasChildren: expected true for struct variable at depth 0")
	}
	if result[1].ChildrenCount != 3 {
		t.Errorf("result[1].ChildrenCount: got %d, want 3", result[1].ChildrenCount)
	}
}

func TestFlattenVariablesRecursive(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		switch args.VariablesReference {
		case 1: // top level
			return []godap.Variable{
				{Name: "x", Value: "1", Type: "int", VariablesReference: 0},
				{Name: "point", Value: "{x:10, y:20}", Type: "Point", VariablesReference: 2, NamedVariables: 2},
			}
		case 2: // point's children
			return []godap.Variable{
				{Name: "x", Value: "10", Type: "int", VariablesReference: 0},
				{Name: "y", Value: "20", Type: "int", VariablesReference: 0},
			}
		default:
			return nil
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 1, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 4 {
		t.Fatalf("expected 4 variables (x, point, point.x, point.y), got %d", len(result))
	}

	// Check flat leaf.
	if result[0].Name != "x" || result[0].Value != "1" {
		t.Errorf("result[0]: got %+v", result[0])
	}

	// Check parent.
	if result[1].Name != "point" || result[1].Value != "{x:10, y:20}" {
		t.Errorf("result[1]: got %+v", result[1])
	}

	// Check children with dot-separated names.
	if result[2].Name != "point.x" || result[2].Value != "10" {
		t.Errorf("result[2]: got %+v, want name=point.x value=10", result[2])
	}
	if result[3].Name != "point.y" || result[3].Value != "20" {
		t.Errorf("result[3]: got %+v, want name=point.y value=20", result[3])
	}
}

func TestFlattenVariablesDeepRecursion(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	// Three levels: top -> child -> grandchild
	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		switch args.VariablesReference {
		case 1: // top level
			return []godap.Variable{
				{Name: "root", Value: "{...}", Type: "Root", VariablesReference: 2, NamedVariables: 1},
			}
		case 2: // root's children
			return []godap.Variable{
				{Name: "child", Value: "{...}", Type: "Child", VariablesReference: 3, NamedVariables: 1},
			}
		case 3: // child's children
			return []godap.Variable{
				{Name: "leaf", Value: "42", Type: "int", VariablesReference: 0},
			}
		default:
			return nil
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 2, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 3 {
		t.Fatalf("expected 3 variables, got %d", len(result))
	}

	if result[0].Name != "root" {
		t.Errorf("result[0].Name: got %q, want %q", result[0].Name, "root")
	}
	if result[1].Name != "root.child" {
		t.Errorf("result[1].Name: got %q, want %q", result[1].Name, "root.child")
	}
	if result[2].Name != "root.child.leaf" || result[2].Value != "42" {
		t.Errorf("result[2]: got %+v, want name=root.child.leaf value=42", result[2])
	}
}

func TestFlattenVariablesDepthLimitStopsRecursion(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	// Three levels available, but depth=1 should stop after one level.
	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		switch args.VariablesReference {
		case 1: // top level
			return []godap.Variable{
				{Name: "root", Value: "{...}", Type: "Root", VariablesReference: 2, NamedVariables: 1},
			}
		case 2: // root's children
			return []godap.Variable{
				{Name: "nested", Value: "{...}", Type: "Nested", VariablesReference: 3, NamedVariables: 2},
			}
		case 3: // should not be reached at depth=1
			t.Error("should not recurse to depth 3 when depth=1")
			return nil
		default:
			return nil
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 1, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 2 {
		t.Fatalf("expected 2 variables, got %d", len(result))
	}

	// The child at depth limit should have HasChildren set.
	if result[1].Name != "root.nested" {
		t.Errorf("result[1].Name: got %q, want %q", result[1].Name, "root.nested")
	}
	if !result[1].HasChildren {
		t.Error("result[1].HasChildren: expected true at depth limit")
	}
	if result[1].ChildrenCount != 2 {
		t.Errorf("result[1].ChildrenCount: got %d, want 2", result[1].ChildrenCount)
	}
}

func TestFlattenVariablesTruncation(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{Name: "a", Value: "1", Type: "int", VariablesReference: 0},
			{Name: "b", Value: "2", Type: "int", VariablesReference: 0},
			{Name: "c", Value: "3", Type: "int", VariablesReference: 0},
			{Name: "d", Value: "4", Type: "int", VariablesReference: 0},
			{Name: "e", Value: "5", Type: "int", VariablesReference: 0},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 3, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if !truncated {
		t.Error("expected truncated=true when maxCount exceeded")
	}
	if len(result) != 3 {
		t.Fatalf("expected 3 variables (maxCount), got %d", len(result))
	}
	if result[0].Name != "a" || result[1].Name != "b" || result[2].Name != "c" {
		t.Errorf("got variables %q, %q, %q; want a, b, c", result[0].Name, result[1].Name, result[2].Name)
	}
}

func TestFlattenVariablesTruncationDuringRecursion(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		switch args.VariablesReference {
		case 1:
			return []godap.Variable{
				{Name: "a", Value: "1", Type: "int", VariablesReference: 0},
				{Name: "s", Value: "{...}", Type: "S", VariablesReference: 2, NamedVariables: 3},
			}
		case 2:
			return []godap.Variable{
				{Name: "f1", Value: "10", Type: "int", VariablesReference: 0},
				{Name: "f2", Value: "20", Type: "int", VariablesReference: 0},
				{Name: "f3", Value: "30", Type: "int", VariablesReference: 0},
			}
		default:
			return nil
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// maxCount=3: should get a, s, s.f1 and then truncate.
	result, truncated, err := FlattenVariables(ctx, client, 1, 1, 3, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if !truncated {
		t.Error("expected truncated=true when maxCount exceeded during recursion")
	}
	if len(result) != 3 {
		t.Fatalf("expected 3 variables, got %d", len(result))
	}
	if result[0].Name != "a" {
		t.Errorf("result[0].Name: got %q, want %q", result[0].Name, "a")
	}
	if result[1].Name != "s" {
		t.Errorf("result[1].Name: got %q, want %q", result[1].Name, "s")
	}
	if result[2].Name != "s.f1" {
		t.Errorf("result[2].Name: got %q, want %q", result[2].Name, "s.f1")
	}
}

func TestFlattenVariablesFilterNotAppliedToChildren(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		switch args.VariablesReference {
		case 1:
			return []godap.Variable{
				{Name: "point", Value: "{...}", Type: "Point", VariablesReference: 2, NamedVariables: 2},
				{Name: "count", Value: "5", Type: "int", VariablesReference: 0},
			}
		case 2:
			return []godap.Variable{
				{Name: "x", Value: "10", Type: "int", VariablesReference: 0},
				{Name: "y", Value: "20", Type: "int", VariablesReference: 0},
			}
		default:
			return nil
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Filter "point" should match the parent, and children should be
	// included regardless of whether they match the filter.
	result, truncated, err := FlattenVariables(ctx, client, 1, 1, 100, "point")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	// Should get: point, point.x, point.y (count is filtered out at top level).
	if len(result) != 3 {
		t.Fatalf("expected 3 variables, got %d: %+v", len(result), result)
	}
	if result[0].Name != "point" {
		t.Errorf("result[0].Name: got %q, want %q", result[0].Name, "point")
	}
	if result[1].Name != "point.x" {
		t.Errorf("result[1].Name: got %q, want %q", result[1].Name, "point.x")
	}
	if result[2].Name != "point.y" {
		t.Errorf("result[2].Name: got %q, want %q", result[2].Name, "point.y")
	}
}

func TestFlattenVariablesEmptyResult(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 0 {
		t.Errorf("expected 0 variables, got %d", len(result))
	}
}

func TestFlattenVariablesFilterNoMatch(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{Name: "x", Value: "1", Type: "int", VariablesReference: 0},
			{Name: "y", Value: "2", Type: "int", VariablesReference: 0},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "zzz")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 0 {
		t.Errorf("expected 0 variables with filter 'zzz', got %d", len(result))
	}
}

func TestFlattenVariablesChildrenCountIndexedAndNamed(t *testing.T) {
	client, clientWritePR, clientReadPW := setupTestClient(t)

	go fakeVarServer(t, clientWritePR, clientReadPW, func(args godap.VariablesArguments) []godap.Variable {
		return []godap.Variable{
			{
				Name:               "arr",
				Value:              "[...]",
				Type:               "[]int",
				VariablesReference: 5,
				NamedVariables:     1,
				IndexedVariables:   10,
			},
		}
	})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	result, truncated, err := FlattenVariables(ctx, client, 1, 0, 100, "")
	if err != nil {
		t.Fatalf("FlattenVariables returned error: %v", err)
	}
	if truncated {
		t.Error("expected truncated=false")
	}
	if len(result) != 1 {
		t.Fatalf("expected 1 variable, got %d", len(result))
	}

	if !result[0].HasChildren {
		t.Error("expected HasChildren=true")
	}
	// ChildrenCount should be NamedVariables + IndexedVariables = 11.
	if result[0].ChildrenCount != 11 {
		t.Errorf("ChildrenCount: got %d, want 11", result[0].ChildrenCount)
	}
}
