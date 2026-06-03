//! `Args` accessor parity (Spec FR-3, Go `launch.go` / mcp-go semantics).

use serde_json::{json, Map, Value};

use crate::Args;

fn args_from(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(m) => m,
        _ => panic!("test args must be a JSON object"),
    }
}

#[test]
fn require_string_present() {
    let m = args_from(json!({ "program": "/bin/ls" }));
    let args = Args::new(&m);
    assert_eq!(args.require_string("program").unwrap(), "/bin/ls");
}

#[test]
fn require_string_missing_has_prefix() {
    // Go: mcp-go RequireString → `missing required parameter: ...`.
    let m = args_from(json!({ "count": 16 }));
    let args = Args::new(&m);
    let err = args.require_string("program").unwrap_err();
    assert!(
        err.starts_with("missing required parameter:"),
        "got: {err:?}"
    );
    assert!(err.contains("program"), "got: {err:?}");
}

#[test]
fn require_string_wrong_type_has_prefix() {
    let m = args_from(json!({ "program": 42 }));
    let args = Args::new(&m);
    let err = args.require_string("program").unwrap_err();
    assert!(
        err.starts_with("missing required parameter:"),
        "got: {err:?}"
    );
}

#[test]
fn require_int_present_and_coerces() {
    let m = args_from(json!({ "count": 16 }));
    let args = Args::new(&m);
    assert_eq!(args.require_int("count").unwrap(), 16);
}

#[test]
fn require_int_truncates_like_go_int_cast() {
    // Go reads float64 and does int(...) — truncation toward zero (FR-3.3).
    let m = args_from(json!({ "count": 16.9 }));
    let args = Args::new(&m);
    assert_eq!(args.require_int("count").unwrap(), 16);

    let m2 = args_from(json!({ "count": -3.9 }));
    let args2 = Args::new(&m2);
    assert_eq!(args2.require_int("count").unwrap(), -3);
}

#[test]
fn require_int_missing_has_prefix() {
    let m = args_from(json!({ "address": "0x1000" }));
    let args = Args::new(&m);
    let err = args.require_int("count").unwrap_err();
    assert!(
        err.starts_with("missing required parameter:"),
        "got: {err:?}"
    );
    assert!(err.contains("count"), "got: {err:?}");
}

#[test]
fn require_int_wrong_type_has_prefix() {
    let m = args_from(json!({ "count": "sixteen" }));
    let args = Args::new(&m);
    let err = args.require_int("count").unwrap_err();
    assert!(
        err.starts_with("missing required parameter:"),
        "got: {err:?}"
    );
}

#[test]
fn get_string_default_and_present() {
    let m = args_from(json!({ "cwd": "/tmp" }));
    let args = Args::new(&m);
    assert_eq!(args.get_string("cwd", ""), "/tmp");
    assert_eq!(args.get_string("missing", "fallback"), "fallback");
    // Wrong type → default.
    let m2 = args_from(json!({ "cwd": 7 }));
    let args2 = Args::new(&m2);
    assert_eq!(args2.get_string("cwd", "def"), "def");
}

#[test]
fn get_bool_default_and_present() {
    let m = args_from(json!({ "stop_on_entry": false }));
    let args = Args::new(&m);
    assert!(!args.get_bool("stop_on_entry", true));
    // Missing → default.
    let m2 = args_from(json!({}));
    let args2 = Args::new(&m2);
    assert!(args2.get_bool("stop_on_entry", true));
    // Wrong type → default.
    let m3 = args_from(json!({ "stop_on_entry": "yes" }));
    let args3 = Args::new(&m3);
    assert!(args3.get_bool("stop_on_entry", true));
}

#[test]
fn get_f64_present_missing_wrong_type() {
    let m = args_from(json!({ "instruction_count": 12.0 }));
    let args = Args::new(&m);
    assert_eq!(args.get_f64("instruction_count"), Some(12.0));
    assert_eq!(args.get_f64("nope"), None);
    let m2 = args_from(json!({ "instruction_count": "x" }));
    let args2 = Args::new(&m2);
    assert_eq!(args2.get_f64("instruction_count"), None);
}

#[test]
fn get_raw_present_and_absent() {
    let m = args_from(json!({ "address": "0x1000" }));
    let args = Args::new(&m);
    assert_eq!(args.get_raw("address"), Some(&json!("0x1000")));
    assert_eq!(args.get_raw("missing"), None);
}

#[test]
fn require_positive_int_accepts_positive_and_truncates() {
    // A valid fractional value still truncates toward zero (Go parity preserved): 4.7 → 4.
    let m = args_from(json!({ "line": 4.7 }));
    let args = Args::new(&m);
    assert_eq!(args.require_positive_int("line").unwrap(), 4);

    let m2 = args_from(json!({ "count": 4096 }));
    let args2 = Args::new(&m2);
    assert_eq!(args2.require_positive_int("count").unwrap(), 4096);
}

#[test]
fn require_positive_int_rejects_zero_negative_and_fractional_to_zero() {
    for bad in [json!(0), json!(-3), json!(-0.5), json!(0.9)] {
        let m = args_from(json!({ "count": bad }));
        let args = Args::new(&m);
        assert_eq!(
            args.require_positive_int("count").unwrap_err(),
            "'count' must be a positive integer"
        );
    }
}

#[test]
fn require_positive_int_missing_keeps_required_prefix() {
    // A missing/non-number value keeps the standard `missing required parameter:` error.
    let m = args_from(json!({}));
    let args = Args::new(&m);
    assert!(args
        .require_positive_int("count")
        .unwrap_err()
        .starts_with("missing required parameter:"));
}

#[test]
fn explicit_positive_thread_id_absent_or_non_numeric_falls_back() {
    // Absent → Ok(None) (caller falls back to last-stopped → 1).
    let m = args_from(json!({}));
    assert_eq!(Args::new(&m).explicit_positive_thread_id().unwrap(), None);
    // Null → Ok(None).
    let m = args_from(json!({ "thread_id": null }));
    assert_eq!(Args::new(&m).explicit_positive_thread_id().unwrap(), None);
    // Non-numeric (string) → Ok(None) (Go parity — never errored here).
    let m = args_from(json!({ "thread_id": "1" }));
    assert_eq!(Args::new(&m).explicit_positive_thread_id().unwrap(), None);
}

#[test]
fn explicit_positive_thread_id_present_positive_and_non_positive() {
    let m = args_from(json!({ "thread_id": 5 }));
    assert_eq!(
        Args::new(&m).explicit_positive_thread_id().unwrap(),
        Some(5)
    );
    for bad in [json!(0), json!(-1), json!(-2.5)] {
        let m = args_from(json!({ "thread_id": bad }));
        assert_eq!(
            Args::new(&m).explicit_positive_thread_id().unwrap_err(),
            "'thread_id' must be a positive integer"
        );
    }
}

#[test]
fn parse_json_array_absent_is_empty() {
    let m = args_from(json!({}));
    let args = Args::new(&m);
    assert_eq!(args.parse_json_array("args").unwrap(), Vec::<String>::new());
}

#[test]
fn parse_json_array_null_is_empty() {
    let m = args_from(json!({ "args": null }));
    let args = Args::new(&m);
    assert_eq!(args.parse_json_array("args").unwrap(), Vec::<String>::new());
}

#[test]
fn parse_json_array_valid() {
    let m = args_from(json!({ "args": "[\"--flag\", \"value\"]" }));
    let args = Args::new(&m);
    assert_eq!(
        args.parse_json_array("args").unwrap(),
        vec!["--flag".to_string(), "value".to_string()]
    );
}

#[test]
fn parse_json_array_non_string_is_exact_go_message() {
    // Go launch.go:50.
    let m = args_from(json!({ "args": ["--flag"] }));
    let args = Args::new(&m);
    let err = args.parse_json_array("args").unwrap_err();
    assert_eq!(
        err,
        "'args' must be a JSON array string, e.g. '[\"--flag\", \"value\"]'"
    );
}

#[test]
fn parse_json_array_parse_failure_message_prefix() {
    // Go launch.go:53.
    let m = args_from(json!({ "args": "not json" }));
    let args = Args::new(&m);
    let err = args.parse_json_array("args").unwrap_err();
    assert!(
        err.starts_with("failed to parse 'args' as JSON array:"),
        "got: {err:?}"
    );
}

#[test]
fn parse_json_object_absent_is_empty() {
    let m = args_from(json!({}));
    let args = Args::new(&m);
    assert_eq!(
        args.parse_json_object("env").unwrap(),
        Vec::<(String, String)>::new()
    );
}

#[test]
fn parse_json_object_valid() {
    let m = args_from(json!({ "env": "{\"KEY\": \"value\"}" }));
    let args = Args::new(&m);
    assert_eq!(
        args.parse_json_object("env").unwrap(),
        vec![("KEY".to_string(), "value".to_string())]
    );
}

#[test]
fn parse_json_object_non_string_is_exact_go_message() {
    // Go launch.go:65.
    let m = args_from(json!({ "env": { "KEY": "value" } }));
    let args = Args::new(&m);
    let err = args.parse_json_object("env").unwrap_err();
    assert_eq!(
        err,
        "'env' must be a JSON object string, e.g. '{\"KEY\": \"value\"}'"
    );
}

#[test]
fn parse_json_object_parse_failure_message_prefix() {
    // Go launch.go:68.
    let m = args_from(json!({ "env": "not json" }));
    let args = Args::new(&m);
    let err = args.parse_json_object("env").unwrap_err();
    assert!(
        err.starts_with("failed to parse 'env' as JSON object:"),
        "got: {err:?}"
    );
}

#[test]
fn parse_json_object_non_string_value_fails() {
    // Go unmarshals into map[string]string; a non-string value fails to parse.
    let m = args_from(json!({ "env": "{\"KEY\": 7}" }));
    let args = Args::new(&m);
    let err = args.parse_json_object("env").unwrap_err();
    assert!(
        err.starts_with("failed to parse 'env' as JSON object:"),
        "got: {err:?}"
    );
}
