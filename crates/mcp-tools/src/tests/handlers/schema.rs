//! Tool-registration tests: the 21 tools register with verbatim names/descriptions
//! (Spec FR-2), correct schema types/required/enums, and the server identity is
//! name `"debug"` v`"1.0.0"` with tool-capabilities listChanged=false (R2 / Spec FR-1).

use rmcp::ServerHandler;
use serde_json::Value;

use crate::tests::handlers::support::Harness;
use crate::ToolServer;

/// The verbatim (name, description) inventory from Spec FR-2.
const INVENTORY: [(&str, &str); 21] = [
    ("launch", "Launch a program under the debugger"),
    ("attach", "Attach the debugger to a running process"),
    ("disconnect", "Disconnect from the debug session"),
    ("set_breakpoint", "Set a source-line breakpoint"),
    (
        "set_function_breakpoint",
        "Set a breakpoint on a function by name",
    ),
    ("remove_breakpoint", "Remove a breakpoint by ID"),
    ("list_breakpoints", "List all current breakpoints"),
    ("continue", "Continue execution of the paused program"),
    ("step_over", "Step over the current line or instruction"),
    ("step_into", "Step into the current line or instruction"),
    ("step_out", "Step out of the current function"),
    ("pause", "Pause all threads in the running program"),
    ("status", "Get the current debug session status"),
    ("backtrace", "Get the call stack for a thread"),
    ("threads", "List all threads in the debugged process"),
    ("variables", "List variables in the current scope"),
    ("evaluate", "Evaluate an expression in the debugger"),
    (
        "read_output",
        "Read captured program output (stdout, stderr, console)",
    ),
    ("read_memory", "Read raw memory at a given address"),
    (
        "disassemble",
        "Disassemble instructions at an address or the current PC",
    ),
    (
        "run_command",
        "Run an LLDB command directly via the debug console",
    ),
];

#[test]
fn exactly_21_tools_with_verbatim_names_and_descriptions() {
    let tools = ToolServer::tools();
    assert_eq!(tools.len(), 21, "exactly 21 tools (Spec FR-2)");
    for (i, (name, desc)) in INVENTORY.iter().enumerate() {
        assert_eq!(tools[i].name, *name, "tool {i} name");
        assert_eq!(
            tools[i].description.as_deref(),
            Some(*desc),
            "tool {i} description"
        );
    }
}

#[test]
fn launch_schema_types_and_required() {
    let tools = ToolServer::tools();
    let launch = tools.iter().find(|t| t.name == "launch").unwrap();
    let schema = Value::Object((*launch.input_schema).clone());
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["program"]["type"], "string");
    assert_eq!(
        schema["properties"]["program"]["description"],
        "Path to the executable to debug"
    );
    assert_eq!(schema["properties"]["stop_on_entry"]["type"], "boolean");
    // program is required.
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "program"));
}

#[test]
fn enums_present_on_step_and_variables() {
    let tools = ToolServer::tools();
    let step_over = tools.iter().find(|t| t.name == "step_over").unwrap();
    let schema = Value::Object((*step_over.input_schema).clone());
    let enum_vals = schema["properties"]["granularity"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(
        enum_vals,
        &[Value::from("line"), Value::from("instruction")]
    );

    let variables = tools.iter().find(|t| t.name == "variables").unwrap();
    let schema = Value::Object((*variables.input_schema).clone());
    let scope_enum = schema["properties"]["scope"]["enum"].as_array().unwrap();
    assert_eq!(
        scope_enum,
        &[
            Value::from("local"),
            Value::from("global"),
            Value::from("register")
        ]
    );
}

#[test]
fn paramless_tools_have_no_required_array() {
    let tools = ToolServer::tools();
    for name in [
        "status",
        "list_breakpoints",
        "pause",
        "threads",
        "read_output",
    ] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = Value::Object((*tool.input_schema).clone());
        assert!(
            schema.get("required").is_none(),
            "{name} has no required array"
        );
    }
}

#[test]
fn read_memory_count_is_number_and_required() {
    let tools = ToolServer::tools();
    let rm = tools.iter().find(|t| t.name == "read_memory").unwrap();
    let schema = Value::Object((*rm.input_schema).clone());
    assert_eq!(schema["properties"]["count"]["type"], "number");
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "address"));
    assert!(required.iter().any(|v| v == "count"));
}

#[test]
fn server_identity_is_debug_v1_with_tool_caps_false() {
    let h = Harness::new();
    let info = h.server.get_info();
    assert_eq!(info.server_info.name, "debug");
    assert_eq!(info.server_info.version, "1.0.0");
    // Tool capabilities advertised with listChanged=false.
    let tools_cap = info.capabilities.tools.expect("tools capability present");
    assert_eq!(tools_cap.list_changed, Some(false));
}

#[test]
fn get_tool_resolves_registered_names() {
    let h = Harness::new();
    assert!(h.server.get_tool("launch").is_some());
    assert!(h.server.get_tool("run_command").is_some());
    assert!(h.server.get_tool("nonexistent").is_none());
}
