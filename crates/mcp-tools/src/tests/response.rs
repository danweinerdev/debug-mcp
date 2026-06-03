//! `ToolOutcome` + `RespBuilder` behavior.

use serde_json::json;

use crate::{RespBuilder, ToolOutcome};

#[test]
fn tool_outcome_variants_and_is_error() {
    assert!(!ToolOutcome::json(json!({ "ok": true })).is_error());
    assert!(!ToolOutcome::text("Program exited during launch").is_error());
    assert!(ToolOutcome::error("boom").is_error());
}

#[test]
fn tool_outcome_text_holds_plain_string() {
    let out = ToolOutcome::text("Program exited during launch");
    assert_eq!(
        out,
        ToolOutcome::Text("Program exited during launch".into())
    );
}

#[test]
fn resp_builder_conditional_inserts() {
    // Mirror a Go `map[string]any` with conditional inserts: present keys included,
    // omitted keys absent.
    let out = RespBuilder::new()
        .set("status", "launched")
        .set("program", "/bin/ls")
        .set("pid", 1234)
        .set_if(true, "state", "stopped")
        .set_if(false, "should_be_absent", "x")
        .set_opt("stop_reason", Some("breakpoint"))
        .set_opt("absent_opt", Option::<&str>::None)
        .into_outcome();

    let expected = ToolOutcome::Json(json!({
        "status": "launched",
        "program": "/bin/ls",
        "pid": 1234,
        "state": "stopped",
        "stop_reason": "breakpoint"
    }));
    assert_eq!(out, expected);
}

#[test]
fn resp_builder_insert_in_loop() {
    let mut b = RespBuilder::new();
    b.insert("count", 3);
    for (k, v) in [("a", 1), ("b", 2)] {
        b.insert(k, v);
    }
    assert_eq!(b.build(), json!({ "count": 3, "a": 1, "b": 2 }));
}

#[test]
fn resp_builder_contains_key() {
    let b = RespBuilder::new().set("x", 1);
    assert!(b.contains_key("x"));
    assert!(!b.contains_key("y"));
}

#[test]
fn resp_builder_keys_serialize_sorted() {
    // serde_json::Map is a BTreeMap → sorted keys, matching Go's map JSON ordering.
    let v = RespBuilder::new()
        .set("zebra", 1)
        .set("apple", 2)
        .set("mango", 3)
        .build();
    let s = serde_json::to_string(&v).unwrap();
    assert_eq!(s, r#"{"apple":2,"mango":3,"zebra":1}"#);
}
