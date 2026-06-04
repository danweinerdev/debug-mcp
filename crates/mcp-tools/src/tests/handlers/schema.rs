//! Tool-registration tests: the 21 tools register with verbatim names/descriptions
//! (Spec FR-2), correct schema types/required/enums, and the server identity is
//! name `"debug"` v`"1.0.0"` with tool-capabilities listChanged=false (R2 / Spec FR-1).

use rmcp::ServerHandler;
use serde_json::Value;

use crate::tests::handlers::support::Harness;

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
    // The default test harness registers only the all-false-capability `fake` factory, so
    // the advertised set is exactly the 21 base tools.
    let tools = Harness::new().server.tools();
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
    let tools = Harness::new().server.tools();
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
    let tools = Harness::new().server.tools();
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
    let tools = Harness::new().server.tools();
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
    let tools = Harness::new().server.tools();
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

// ---- task 1.3: capability-gated listing + the backend selector property ----

/// The four capability-gated tool names, in `all_tools` append order.
const GATED: [&str; 4] = [
    "open_crash_dump",
    "attach_kernel",
    "analyze_crash",
    "get_modules",
];

#[test]
fn all_false_caps_yield_exactly_the_21_base_tools() {
    use debugger_core::BackendCapabilities;
    let tools = crate::schema::all_tools(BackendCapabilities::default());
    assert_eq!(tools.len(), 21, "no capabilities ⇒ the 21 base tools");
    for gated in GATED {
        assert!(
            !tools.iter().any(|t| t.name == gated),
            "{gated} must be absent under all-false caps"
        );
    }
}

#[test]
fn all_true_caps_append_the_four_gated_tools() {
    use debugger_core::BackendCapabilities;
    let caps = BackendCapabilities {
        crash_dump: true,
        kernel: true,
        analyze: true,
        modules: true,
    };
    let tools = crate::schema::all_tools(caps);
    assert_eq!(tools.len(), 25, "all capabilities ⇒ 21 + 4 gated tools");
    for gated in GATED {
        assert!(
            tools.iter().any(|t| t.name == gated),
            "{gated} must be present under all-true caps"
        );
    }
    // The base 21 still lead, unchanged in name/order.
    for (i, (name, _)) in INVENTORY.iter().enumerate() {
        assert_eq!(tools[i].name, *name, "base tool {i} unchanged");
    }
}

#[test]
fn each_capability_gates_only_its_own_tool() {
    use debugger_core::BackendCapabilities;
    let cases: [(BackendCapabilities, &str); 4] = [
        (
            BackendCapabilities {
                crash_dump: true,
                ..BackendCapabilities::default()
            },
            "open_crash_dump",
        ),
        (
            BackendCapabilities {
                kernel: true,
                ..BackendCapabilities::default()
            },
            "attach_kernel",
        ),
        (
            BackendCapabilities {
                analyze: true,
                ..BackendCapabilities::default()
            },
            "analyze_crash",
        ),
        (
            BackendCapabilities {
                modules: true,
                ..BackendCapabilities::default()
            },
            "get_modules",
        ),
    ];
    for (caps, expected) in cases {
        let tools = crate::schema::all_tools(caps);
        assert_eq!(tools.len(), 22, "exactly one gated tool appended");
        assert!(
            tools.iter().any(|t| t.name == expected),
            "{expected} present for its flag"
        );
        for other in GATED.iter().filter(|g| **g != expected) {
            assert!(
                !tools.iter().any(|t| t.name == *other),
                "{other} absent when only its sibling flag is set"
            );
        }
    }
}

#[test]
fn gated_tool_schemas_match_the_design_surface() {
    use debugger_core::BackendCapabilities;
    let caps = BackendCapabilities {
        crash_dump: true,
        kernel: true,
        analyze: true,
        modules: true,
    };
    let tools = crate::schema::all_tools(caps);
    let by_name = |name: &str| {
        let t = tools.iter().find(|t| t.name == name).unwrap();
        Value::Object((*t.input_schema).clone())
    };

    // open_crash_dump: required dump_path (string) + optional backend enum.
    let ocd = by_name("open_crash_dump");
    assert_eq!(ocd["properties"]["dump_path"]["type"], "string");
    assert_eq!(ocd["properties"]["backend"]["type"], "string");
    let req = ocd["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v == "dump_path"));
    assert!(!req.iter().any(|v| v == "backend"));

    // attach_kernel: required connection (string) + optional backend enum.
    let ak = by_name("attach_kernel");
    assert_eq!(ak["properties"]["connection"]["type"], "string");
    let req = ak["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v == "connection"));
    assert!(!req.iter().any(|v| v == "backend"));

    // analyze_crash / get_modules: no args ⇒ no required array.
    for paramless in ["analyze_crash", "get_modules"] {
        assert!(
            by_name(paramless).get("required").is_none(),
            "{paramless} takes no args"
        );
    }
}

#[test]
fn launch_and_attach_gained_optional_backend_enum() {
    use debugger_core::BackendCapabilities;
    let tools = crate::schema::all_tools(BackendCapabilities::default());
    for name in ["launch", "attach"] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = Value::Object((*tool.input_schema).clone());
        assert_eq!(
            schema["properties"]["backend"]["type"], "string",
            "{name} backend is a string"
        );
        let enum_vals = schema["properties"]["backend"]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} backend has an enum"));
        assert_eq!(
            enum_vals,
            &[Value::from("lldb"), Value::from("windbg")],
            "{name} backend enum values"
        );
        // backend is NOT required. `attach` has no required props at all (its `required`
        // array is omitted entirely), so absence-or-not-present both satisfy "optional".
        let backend_required = schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|req| req.iter().any(|v| v == "backend"));
        assert!(!backend_required, "{name} backend must be optional");
    }
}

#[test]
fn the_19_other_base_tool_schemas_are_unchanged() {
    use debugger_core::BackendCapabilities;
    // Snapshot guard: the base tool-name set (sorted) is exactly the existing 21. Only
    // `launch`/`attach` gained the optional `backend` prop; every other tool keeps its
    // exact property set, so the 19 untouched tools cannot drift. We assert no tool other
    // than launch/attach exposes a `backend` property.
    let tools = crate::schema::all_tools(BackendCapabilities::default());

    let mut got: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    got.sort_unstable();
    let mut expected: Vec<&str> = INVENTORY.iter().map(|(n, _)| *n).collect();
    expected.sort_unstable();
    assert_eq!(got, expected, "the base tool-name set is exactly the 21");

    for tool in &tools {
        if tool.name == "launch" || tool.name == "attach" {
            continue;
        }
        let schema = Value::Object((*tool.input_schema).clone());
        assert!(
            schema["properties"].get("backend").is_none(),
            "{} must not gain a backend property",
            tool.name
        );
    }
}
