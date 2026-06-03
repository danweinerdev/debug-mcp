//! The 21 hand-built tool definitions: verbatim names + descriptions (Spec FR-2) and
//! input schemas matching the Go mcp-go schemas (types/required/enum/descriptions,
//! design Decision 3 / R2). Runtime parsing goes through [`Args`](crate::Args); these
//! schemas are advertisement-level (Spec FR-2, design §"MCP tool surface").
//!
//! mcp-go's `WithString`/`WithNumber`/`WithBoolean` emit JSON-Schema `{"type": …,
//! "description": …}` properties, `Required()` adds the name to the schema's `required`
//! array, and `Enum(...)` adds an `enum` array. Every tool's schema is a top-level
//! `{"type":"object","properties":{…},"required":[…]}`.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool};
use serde_json::{json, Map, Value};

/// One property's `(name, schema-fragment, required)`. The fragment is the JSON-Schema for
/// that property (type + description + optional enum).
struct Prop {
    name: &'static str,
    schema: Value,
    required: bool,
}

fn string_prop(name: &'static str, description: &str, required: bool) -> Prop {
    Prop {
        name,
        schema: json!({ "type": "string", "description": description }),
        required,
    }
}

fn string_enum_prop(
    name: &'static str,
    description: &str,
    enum_values: &[&str],
    required: bool,
) -> Prop {
    Prop {
        name,
        schema: json!({
            "type": "string",
            "description": description,
            "enum": enum_values,
        }),
        required,
    }
}

fn number_prop(name: &'static str, description: &str, required: bool) -> Prop {
    Prop {
        name,
        schema: json!({ "type": "number", "description": description }),
        required,
    }
}

fn boolean_prop(name: &'static str, description: &str) -> Prop {
    Prop {
        name,
        schema: json!({ "type": "boolean", "description": description }),
        required: false,
    }
}

/// Assemble a `{"type":"object","properties":{…},"required":[…]}` schema object. The
/// `required` array is included only when non-empty (mcp-go omits it for paramless tools).
fn object_schema(props: &[Prop]) -> Arc<JsonObject> {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for p in props {
        properties.insert(p.name.to_string(), p.schema.clone());
        if p.required {
            required.push(Value::String(p.name.to_string()));
        }
    }

    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }
    Arc::new(schema)
}

fn tool(name: &'static str, description: &'static str, props: &[Prop]) -> Tool {
    Tool::new_with_raw(name, Some(description.into()), object_schema(props))
}

/// The 21 tools in Go registration order (Spec FR-2). Names and descriptions are verbatim.
pub fn all_tools() -> Vec<Tool> {
    vec![
        tool(
            "launch",
            "Launch a program under the debugger",
            &[
                string_prop("program", "Path to the executable to debug", true),
                string_prop("args", "JSON array of command-line arguments", false),
                string_prop("cwd", "Working directory for the launched program", false),
                string_prop("env", "JSON object of environment variables", false),
                boolean_prop(
                    "stop_on_entry",
                    "Stop at program entry point (default true)",
                ),
            ],
        ),
        tool(
            "attach",
            "Attach the debugger to a running process",
            &[
                number_prop("pid", "Process ID to attach to", false),
                string_prop("wait_for", "Process name to wait for", false),
            ],
        ),
        tool(
            "disconnect",
            "Disconnect from the debug session",
            &[boolean_prop(
                "terminate",
                "Terminate the debuggee (default true)",
            )],
        ),
        tool(
            "set_breakpoint",
            "Set a source-line breakpoint",
            &[
                string_prop("file", "Source file path", true),
                number_prop("line", "Line number", true),
                string_prop(
                    "condition",
                    "Conditional expression for the breakpoint",
                    false,
                ),
            ],
        ),
        tool(
            "set_function_breakpoint",
            "Set a breakpoint on a function by name",
            &[
                string_prop("name", "Function name", true),
                string_prop(
                    "condition",
                    "Conditional expression for the breakpoint",
                    false,
                ),
            ],
        ),
        tool(
            "remove_breakpoint",
            "Remove a breakpoint by ID",
            &[number_prop(
                "breakpoint_id",
                "Breakpoint ID to remove",
                true,
            )],
        ),
        tool("list_breakpoints", "List all current breakpoints", &[]),
        tool(
            "continue",
            "Continue execution of the paused program",
            &[number_prop(
                "thread_id",
                "Thread ID to continue (optional)",
                false,
            )],
        ),
        tool(
            "step_over",
            "Step over the current line or instruction",
            &[
                number_prop("thread_id", "Thread ID to step (optional)", false),
                string_enum_prop(
                    "granularity",
                    "Step granularity",
                    &["line", "instruction"],
                    false,
                ),
            ],
        ),
        tool(
            "step_into",
            "Step into the current line or instruction",
            &[
                number_prop("thread_id", "Thread ID to step (optional)", false),
                string_enum_prop(
                    "granularity",
                    "Step granularity",
                    &["line", "instruction"],
                    false,
                ),
            ],
        ),
        tool(
            "step_out",
            "Step out of the current function",
            &[number_prop(
                "thread_id",
                "Thread ID to step out (optional)",
                false,
            )],
        ),
        tool("pause", "Pause all threads in the running program", &[]),
        tool("status", "Get the current debug session status", &[]),
        tool(
            "backtrace",
            "Get the call stack for a thread",
            &[
                number_prop(
                    "thread_id",
                    "Thread ID (uses stopped thread if omitted)",
                    false,
                ),
                number_prop("levels", "Maximum number of stack frames to return", false),
            ],
        ),
        tool("threads", "List all threads in the debugged process", &[]),
        tool(
            "variables",
            "List variables in the current scope",
            &[
                number_prop("frame_index", "Stack frame index (default 0)", false),
                string_enum_prop(
                    "scope",
                    "Variable scope",
                    &["local", "global", "register"],
                    false,
                ),
                number_prop("depth", "Maximum depth for nested structures", false),
                string_prop("filter", "Filter variables by name pattern", false),
            ],
        ),
        tool(
            "evaluate",
            "Evaluate an expression in the debugger",
            &[
                string_prop("expression", "Expression to evaluate", true),
                number_prop(
                    "frame_index",
                    "Stack frame index for evaluation context",
                    false,
                ),
            ],
        ),
        tool(
            "read_output",
            "Read captured program output (stdout, stderr, console)",
            &[],
        ),
        tool(
            "read_memory",
            "Read raw memory at a given address",
            &[
                string_prop("address", "Memory address (hex string, e.g. 0x1000)", true),
                number_prop("count", "Number of bytes to read", true),
            ],
        ),
        tool(
            "disassemble",
            "Disassemble instructions at an address or the current PC",
            &[
                string_prop(
                    "address",
                    "Start address (hex string, uses current PC if omitted)",
                    false,
                ),
                number_prop(
                    "instruction_count",
                    "Number of instructions to disassemble",
                    false,
                ),
            ],
        ),
        tool(
            "run_command",
            "Run an LLDB command directly via the debug console",
            &[string_prop(
                "command",
                "LLDB command string to execute",
                true,
            )],
        ),
    ]
}
