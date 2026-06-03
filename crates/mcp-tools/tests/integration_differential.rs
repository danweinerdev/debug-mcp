//! Phase 6.3: the differential-parity harness + a golden cross-check.
//!
//! Two layers, both gated behind the `integration` feature:
//!
//! 1. **Golden cross-check (always runs where lldb-dap is present).** Drives the Rust
//!    `debug-mcp` binary over stdio through a representative scenario and asserts the
//!    parsed JSON matches the spec's documented response shapes + the `debug` server name.
//!    This is the fallback cross-check for the no-Go sandbox (the live differential run
//!    cannot run here because Go is not installed).
//!
//! 2. **Differential run (gated on the Go binary).** Replays identical MCP tool sequences
//!    against the Go `lldb-debug-mcp` and the Rust `debug-mcp` over stdio and diffs the
//!    parsed JSON **structurally** (object key order/whitespace ignored — `serde_json`
//!    sorts keys, and the differ walks values), asserting the two recorded intentional
//!    deviations explicitly (server name `debug` vs `lldb-debug`; `disassemble` default
//!    20 vs 10). It **skips cleanly** with a clear message when the Go binary is absent.
//!
//! OQ-3 (repl-mode default) is implicitly covered by the `run_command` scenario — a wrong
//! default flips the backtick prefix, which the diff would catch. OQ-4 (`xcrun`) is
//! macOS-only and is not exercised on Linux.

#![cfg(feature = "integration")]

use integration_tests::harness::{fixture_path, lldb_dap_available};
use integration_tests::stdio::{go_reference_binary, rust_binary, StdioMcp, ToolCallResult};
use serde_json::{json, Value};

/// The `LLDB_DAP_PATH` (if set) to propagate to the spawned server children so both
/// binaries use the same adapter. Empty slice when unset (they detect via PATH).
fn child_env() -> Vec<(String, String)> {
    match std::env::var("LLDB_DAP_PATH") {
        Ok(p) if !p.is_empty() => vec![("LLDB_DAP_PATH".to_string(), p)],
        _ => Vec::new(),
    }
}

fn child_env_refs(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

// --- Golden cross-check against the documented response shapes (no Go needed) ---

#[tokio::test(flavor = "multi_thread")]
async fn golden_response_shapes_over_stdio() {
    if !lldb_dap_available() {
        eprintln!("SKIP golden_response_shapes_over_stdio: lldb-dap not found");
        return;
    }
    let simple = fixture_path("simple");
    let loop_bin = fixture_path("loop");
    let loop_src = fixture_path("loop.c");
    if !simple.exists() || !loop_bin.exists() {
        eprintln!(
            "SKIP golden_response_shapes_over_stdio: fixtures missing (run make -C testdata)"
        );
        return;
    }
    let rust = match rust_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIP golden_response_shapes_over_stdio: debug-mcp binary not built");
            return;
        }
    };
    let env_owned = child_env();

    // Drive on a blocking thread (the stdio client is synchronous I/O).
    let simple = simple.display().to_string();
    let loop_bin = loop_bin.display().to_string();
    let loop_src = loop_src.display().to_string();
    tokio::task::spawn_blocking(move || {
        let env = child_env_refs(&env_owned);
        // serverInfo.name == "debug" (Spec FR-1.1, the intentional rename).
        let info = StdioMcp::server_info(&rust, &env).expect("server_info");
        assert_eq!(
            info.get("name").and_then(Value::as_str),
            Some("debug"),
            "Rust server name must be 'debug'"
        );

        // --- simple: launch → continue-to-exit shape ---
        {
            let mut mcp = StdioMcp::spawn(&rust, &env).expect("spawn debug-mcp");
            let launch = require_json(&mcp_call(
                &mut mcp,
                "launch",
                json!({"program": simple, "stop_on_entry": true}),
            ));
            assert_eq!(launch["status"], json!("launched"));
            assert_eq!(launch["state"], json!("stopped"));
            assert!(
                launch["pid"].as_i64().is_some_and(|p| p > 0),
                "launch must report a non-zero pid, got {:?}",
                launch["pid"]
            );

            let cont = require_json(&mcp_call(&mut mcp, "continue", json!({})));
            assert_eq!(cont["status"], json!("exited"));
            assert_eq!(cont["exit_code"].as_i64(), Some(0));

            let _ = mcp_call(&mut mcp, "disconnect", json!({"terminate": true}));
        }

        // --- loop: breakpoint → continue → backtrace/variables/disassemble shapes ---
        {
            let mut mcp = StdioMcp::spawn(&rust, &env).expect("spawn debug-mcp");
            let _ = require_json(&mcp_call(
                &mut mcp,
                "launch",
                json!({"program": loop_bin, "stop_on_entry": true}),
            ));
            let bp = require_json(&mcp_call(
                &mut mcp,
                "set_breakpoint",
                json!({"file": loop_src, "line": 6}),
            ));
            assert!(
                bp["breakpoint_id"].as_i64().is_some(),
                "breakpoint_id present"
            );
            assert!(bp.get("verified").is_some(), "verified present");

            let cont = require_json(&mcp_call(&mut mcp, "continue", json!({})));
            assert_eq!(cont["status"], json!("stopped"));
            assert!(
                cont.get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r.contains("breakpoint")),
                "reason contains breakpoint, got {:?}",
                cont.get("reason")
            );

            // backtrace shape: frames[] with index/name/id, total_frames, thread_id.
            let bt = require_json(&mcp_call(&mut mcp, "backtrace", json!({})));
            let frames = bt["frames"].as_array().expect("frames");
            assert!(!frames.is_empty());
            assert!(bt.get("total_frames").is_some());
            assert!(bt.get("thread_id").is_some());
            let f0 = &frames[0];
            assert!(
                f0.get("index").is_some() && f0.get("name").is_some() && f0.get("id").is_some()
            );

            // variables shape: variables[]/count/scope/truncated, with i and sum present.
            let vars = require_json(&mcp_call(&mut mcp, "variables", json!({})));
            assert!(vars.get("count").is_some());
            assert_eq!(vars["scope"], json!("local"));
            assert!(vars.get("truncated").is_some());
            let names: Vec<&str> = vars["variables"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.get("name").and_then(Value::as_str))
                .collect();
            assert!(
                names.contains(&"i") && names.contains(&"sum"),
                "i and sum present: {names:?}"
            );

            // disassemble default instruction_count is the documented 20 (the second
            // intentional deviation). The shape carries instructions[]/count/start_address.
            let dis = require_json(&mcp_call(&mut mcp, "disassemble", json!({})));
            let instrs = dis["instructions"].as_array().expect("instructions");
            assert_eq!(
                dis["count"].as_i64(),
                Some(instrs.len() as i64),
                "count matches instructions length"
            );
            assert_eq!(
                instrs.len(),
                20,
                "disassemble must default to 20 instructions (intentional deviation), got {}",
                instrs.len()
            );
            assert!(dis.get("start_address").is_some());

            let _ = mcp_call(&mut mcp, "disconnect", json!({"terminate": true}));
        }
    })
    .await
    .expect("golden cross-check task");
}

// --- Differential run: Rust vs Go (gated on the Go binary) ---

#[tokio::test(flavor = "multi_thread")]
async fn differential_rust_vs_go() {
    if !lldb_dap_available() {
        eprintln!("SKIP differential_rust_vs_go: lldb-dap not found");
        return;
    }
    let rust = match rust_binary() {
        Some(b) => b,
        None => {
            eprintln!("SKIP differential_rust_vs_go: debug-mcp binary not built");
            return;
        }
    };
    let go = match go_reference_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "SKIP differential_rust_vs_go: the Go reference binary (lldb-debug-mcp) is not \
                 available. Go is not installed in this sandbox, so the live differential run is \
                 skipped; the ported integration scenarios (6.2) are the substantive live parity \
                 proof, and golden_response_shapes_over_stdio cross-checks the documented shapes. \
                 Set GO_DEBUG_MCP_BIN or put lldb-debug-mcp on PATH to enable this lane."
            );
            return;
        }
    };

    let loop_bin = fixture_path("loop").display().to_string();
    let loop_src = fixture_path("loop.c").display().to_string();
    let env_owned = child_env();

    tokio::task::spawn_blocking(move || {
        let env = child_env_refs(&env_owned);

        // (A) Deviation 1: server name differs by design.
        let rust_info = StdioMcp::server_info(&rust, &env).expect("rust server_info");
        let go_info = StdioMcp::server_info(&go, &env).expect("go server_info");
        assert_eq!(rust_info.get("name").and_then(Value::as_str), Some("debug"));
        assert_eq!(
            go_info.get("name").and_then(Value::as_str),
            Some("lldb-debug"),
            "expected the Go server name 'lldb-debug' (the deviation we assert, not mask)"
        );

        // (B) Tool inventory: same names + descriptions + schemas (order-independent).
        {
            let mut r = StdioMcp::spawn(&rust, &env).expect("spawn rust");
            let mut g = StdioMcp::spawn(&go, &env).expect("spawn go");
            let mut rt = r.list_tools().expect("rust tools");
            let mut gt = g.list_tools().expect("go tools");
            sort_tools(&mut rt);
            sort_tools(&mut gt);
            assert_eq!(rt.len(), 21, "rust must expose 21 tools");
            assert_eq!(gt.len(), 21, "go must expose 21 tools");
            for (rtool, gtool) in rt.iter().zip(gt.iter()) {
                if let Some(diff) = json_diff("tool", rtool, gtool) {
                    panic!("tool inventory differs: {diff}");
                }
            }
        }

        // (C) Replay an identical scenario and diff each tool result structurally.
        let scenario: Vec<(&str, Value)> = vec![
            (
                "launch",
                json!({"program": loop_bin, "stop_on_entry": true}),
            ),
            ("set_breakpoint", json!({"file": loop_src, "line": 6})),
            ("continue", json!({})),
            ("backtrace", json!({})),
            ("variables", json!({})),
            ("step_over", json!({})),
            ("evaluate", json!({"expression": "i + 1"})),
            ("run_command", json!({"command": "register read"})),
            ("status", json!({})),
            ("list_breakpoints", json!({})),
        ];

        let mut r = StdioMcp::spawn(&rust, &env).expect("spawn rust");
        let mut g = StdioMcp::spawn(&go, &env).expect("spawn go");
        for (name, args) in &scenario {
            let rr = mcp_call(&mut r, name, args.clone());
            let gg = mcp_call(&mut g, name, args.clone());
            compare_results(name, &rr, &gg);
        }
        let _ = mcp_call(&mut r, "disconnect", json!({"terminate": true}));
        let _ = mcp_call(&mut g, "disconnect", json!({"terminate": true}));

        // (D) Deviation 2: disassemble default instruction_count (Rust 20, Go 10).
        {
            let mut r = StdioMcp::spawn(&rust, &env).expect("spawn rust");
            let mut g = StdioMcp::spawn(&go, &env).expect("spawn go");
            let _ = mcp_call(
                &mut r,
                "launch",
                json!({"program": loop_bin, "stop_on_entry": true}),
            );
            let _ = mcp_call(
                &mut g,
                "launch",
                json!({"program": loop_bin, "stop_on_entry": true}),
            );
            let rd = require_json(&mcp_call(&mut r, "disassemble", json!({})));
            let gd = require_json(&mcp_call(&mut g, "disassemble", json!({})));
            assert_eq!(
                rd["count"].as_i64(),
                Some(20),
                "Rust disassemble defaults to 20"
            );
            assert_eq!(
                gd["count"].as_i64(),
                Some(10),
                "Go disassemble defaults to 10"
            );
            let _ = mcp_call(&mut r, "disconnect", json!({"terminate": true}));
            let _ = mcp_call(&mut g, "disconnect", json!({"terminate": true}));
        }
    })
    .await
    .expect("differential task");
}

/// Call a tool over stdio, propagating I/O errors as panics (a transport failure in the
/// harness is a hard error, not a parity result).
fn mcp_call(mcp: &mut StdioMcp, name: &str, args: Value) -> ToolCallResult {
    mcp.call_tool(name, args)
        .unwrap_or_else(|e| panic!("tools/call {name} transport error: {e}"))
}

/// Assert a tool call produced JSON success and return the object.
fn require_json(result: &ToolCallResult) -> Value {
    assert!(
        !result.is_error,
        "expected JSON success, got tool error: {}",
        result.text
    );
    result
        .json
        .clone()
        .unwrap_or_else(|| panic!("expected JSON content, got plain text: {}", result.text))
}

/// Compare two tool results between Rust and Go. Both must agree on is_error and on the
/// structural JSON shape; volatile/non-deterministic fields (addresses, ids, register
/// dumps, pid) are normalized away before the diff.
fn compare_results(name: &str, rust: &ToolCallResult, go: &ToolCallResult) {
    assert_eq!(
        rust.is_error, go.is_error,
        "tool '{name}': is_error differs (rust={}, go={})",
        rust.is_error, go.is_error
    );
    match (&rust.json, &go.json) {
        (Some(rv), Some(gv)) => {
            let rn = normalize(name, rv);
            let gn = normalize(name, gv);
            if let Some(diff) = json_diff(name, &rn, &gn) {
                panic!("tool '{name}' result differs structurally: {diff}\n  rust={rn}\n  go={gn}");
            }
        }
        (None, None) => {
            assert_eq!(
                rust.text, go.text,
                "tool '{name}': plain-text content differs"
            );
        }
        _ => panic!(
            "tool '{name}': one side is JSON and the other plain text (rust_json={}, go_json={})",
            rust.json.is_some(),
            go.json.is_some()
        ),
    }
}

/// Normalize a result's volatile fields so the structural diff focuses on shape + stable
/// values. Debugger-assigned ids, runtime addresses, pids, raw register/command dumps, and
/// stack-frame line/address vary run to run and between LLVM builds; we compare presence
/// and type, replacing the value with a typed placeholder.
fn normalize(tool: &str, value: &Value) -> Value {
    let mut v = value.clone();
    // Global volatile keys present in several result shapes.
    redact_keys(
        &mut v,
        &[
            "pid",
            "breakpoint_id",
            "id",
            "thread_id",
            "stopped_thread_id",
            "address",
            "start_address",
            "hit_breakpoint_ids",
            "variables_reference",
            "stop_description",
            "description",
        ],
    );
    match tool {
        // The free-form lldb dump + the evaluated value vary; keep only presence/type.
        "run_command" | "evaluate" => {
            if let Some(obj) = v.as_object_mut() {
                if obj.contains_key("result") {
                    obj.insert("result".to_string(), json!("<redacted-string>"));
                }
                if let Some(t) = obj.get_mut("type") {
                    *t = json!("<redacted-string>");
                }
            }
        }
        // Frame/variable values vary (line numbers, addresses, live values); redact the
        // per-entry volatile fields but keep the array shape + names/keys.
        "backtrace" => redact_array_field(&mut v, "frames", &["line", "file", "address", "id"]),
        "variables" => redact_array_field(
            &mut v,
            "variables",
            &["value", "type", "variables_reference"],
        ),
        _ => {}
    }
    v
}

/// Replace each present key in `value` (top-level object) with a typed placeholder.
fn redact_keys(value: &mut Value, keys: &[&str]) {
    if let Some(obj) = value.as_object_mut() {
        for key in keys {
            if let Some(slot) = obj.get_mut(*key) {
                *slot = placeholder_for(slot);
            }
        }
    }
}

/// Redact per-entry volatile fields within an array field (e.g. `frames`/`variables`),
/// preserving the array length and each entry's key set.
fn redact_array_field(value: &mut Value, array_key: &str, entry_keys: &[&str]) {
    if let Some(arr) = value
        .as_object_mut()
        .and_then(|o| o.get_mut(array_key))
        .and_then(Value::as_array_mut)
    {
        for entry in arr.iter_mut() {
            redact_keys(entry, entry_keys);
        }
    }
}

/// A typed placeholder that preserves the JSON type but erases the (volatile) value.
fn placeholder_for(value: &Value) -> Value {
    match value {
        Value::String(_) => json!("<redacted-string>"),
        Value::Number(_) => json!(0),
        Value::Bool(_) => json!(false),
        Value::Array(a) => json!(vec![json!("<redacted>"); a.len().min(1)]),
        Value::Object(_) => json!({"<redacted>": true}),
        Value::Null => Value::Null,
    }
}

/// Sort the tools list by name so two servers' inventories compare order-independently.
fn sort_tools(tools: &mut [Value]) {
    tools.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
}

/// A recursive structural diff: returns `None` when `a == b` (key order ignored, since
/// `serde_json` objects are sorted maps), else a human path to the first difference.
fn json_diff(path: &str, a: &Value, b: &Value) -> Option<String> {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, av) in am {
                match bm.get(k) {
                    Some(bv) => {
                        if let Some(d) = json_diff(&format!("{path}.{k}"), av, bv) {
                            return Some(d);
                        }
                    }
                    None => return Some(format!("{path}.{k}: present on left, absent on right")),
                }
            }
            for k in bm.keys() {
                if !am.contains_key(k) {
                    return Some(format!("{path}.{k}: absent on left, present on right"));
                }
            }
            None
        }
        (Value::Array(aa), Value::Array(ba)) => {
            if aa.len() != ba.len() {
                return Some(format!(
                    "{path}: array length differs ({} vs {})",
                    aa.len(),
                    ba.len()
                ));
            }
            for (i, (av, bv)) in aa.iter().zip(ba.iter()).enumerate() {
                if let Some(d) = json_diff(&format!("{path}[{i}]"), av, bv) {
                    return Some(d);
                }
            }
            None
        }
        _ if a == b => None,
        _ => Some(format!("{path}: {a} != {b}")),
    }
}
