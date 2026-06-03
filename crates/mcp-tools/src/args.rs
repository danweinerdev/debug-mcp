//! `Args` — an rmcp-free accessor over a tool call's argument object.
//!
//! This is the parity surface for Go's permissive argument handling (Spec FR-3,
//! design Decision 3). It wraps the `serde_json::Map` rmcp hands a handler and
//! reproduces mcp-go's `RequireString`/`RequireInt`/`GetString`/`GetBool` semantics
//! plus the exact `launch.go` wording for the `args`/`env` JSON-string parameters.
//!
//! Every method returns `Result<T, String>` where `Err(String)` is the **exact**
//! user-facing error text the Go handler would surface (FR-3.4). The handler layer
//! (Phase 5.3/5.4) maps that string straight into a tool error result.

use serde_json::{Map, Value};

/// A borrowed view over a tool call's arguments. Construct via [`Args::new`] with the
/// `serde_json::Map` rmcp parses from `tools/call`. All accessors are read-only.
pub struct Args<'a> {
    map: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    /// Wrap a reference to the arguments map.
    pub fn new(map: &'a Map<String, Value>) -> Self {
        Self { map }
    }

    /// The raw `Value` for `key`, if present (mirrors Go's
    /// `request.GetArguments()[key]`). Returns `None` for an absent key.
    pub fn get_raw(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key)
    }

    /// A required string parameter (mcp-go `RequireString`). On a missing key the
    /// `Err` is `missing required parameter: required argument "<key>" not found`;
    /// on a present-but-non-string value it is
    /// `missing required parameter: argument "<key>" is not of type string`.
    pub fn require_string(&self, key: &str) -> Result<String, String> {
        match self.map.get(key) {
            None => Err(missing_required(&not_found(key))),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(missing_required(&not_of_type(key, "string"))),
        }
    }

    /// A required integer parameter (mcp-go `RequireInt`). Go reads the JSON number
    /// as `float64` and coerces with `int(...)` (truncation toward zero, FR-3.3). On
    /// a missing key the `Err` is
    /// `missing required parameter: required argument "<key>" not found`; on a
    /// present-but-non-number value it is
    /// `missing required parameter: argument "<key>" is not of type int`.
    pub fn require_int(&self, key: &str) -> Result<i64, String> {
        match self.map.get(key) {
            None => Err(missing_required(&not_found(key))),
            Some(v) => match value_as_f64(v) {
                Some(f) => Ok(f as i64),
                None => Err(missing_required(&not_of_type(key, "int"))),
            },
        }
    }

    /// An optional string parameter with a fallback (mcp-go `GetString`). A missing
    /// key or a non-string value yields `default`.
    pub fn get_string(&self, key: &str, default: &str) -> String {
        match self.map.get(key) {
            Some(Value::String(s)) => s.clone(),
            _ => default.to_string(),
        }
    }

    /// An optional boolean parameter with a fallback (mcp-go `GetBool`). A missing
    /// key or a non-boolean value yields `default`.
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.map.get(key) {
            Some(Value::Bool(b)) => *b,
            _ => default,
        }
    }

    /// An optional number parameter read as `f64` (Go reads JSON numbers as
    /// `float64`). `None` for a missing key or non-number value — callers apply the
    /// `> 0` / `int(...)` checks the Go handlers do inline (e.g. `disassemble`).
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.map.get(key).and_then(value_as_f64)
    }

    /// Parse a JSON-array-of-strings passed as a **string** parameter (Go
    /// `launch.go`'s `args`). Absent/`null` → `Ok(vec![])`. A present non-string
    /// value → `'<key>' must be a JSON array string, e.g. '["--flag", "value"]'`. A
    /// string that fails to parse as `[]string` →
    /// `failed to parse '<key>' as JSON array: <err>`.
    pub fn parse_json_array(&self, key: &str) -> Result<Vec<String>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s)
                .map_err(|e| format!("failed to parse '{key}' as JSON array: {e}")),
            Some(_) => Err(format!(
                "'{key}' must be a JSON array string, e.g. '[\"--flag\", \"value\"]'"
            )),
        }
    }

    /// Parse a JSON-object-of-strings passed as a **string** parameter (Go
    /// `launch.go`'s `env`). Absent/`null` → `Ok(vec![])`. A present non-string
    /// value → `'<key>' must be a JSON object string, e.g. '{"KEY": "value"}'`. A
    /// string that fails to parse as `map[string]string` →
    /// `failed to parse '<key>' as JSON object: <err>`.
    ///
    /// Returned as a `Vec<(String, String)>` to mirror [`debugger_core::LaunchSpec`]'s
    /// `env` shape; insertion order is `serde_json`'s object order.
    pub fn parse_json_object(&self, key: &str) -> Result<Vec<(String, String)>, String> {
        match self.map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::String(s)) => {
                let parsed: Map<String, Value> = serde_json::from_str(s)
                    .map_err(|e| format!("failed to parse '{key}' as JSON object: {e}"))?;
                let mut out = Vec::with_capacity(parsed.len());
                for (k, v) in parsed {
                    match v {
                        Value::String(val) => out.push((k, val)),
                        other => {
                            return Err(format!(
                                "failed to parse '{key}' as JSON object: cannot unmarshal {} into Go value of type string",
                                json_type_name(&other)
                            ));
                        }
                    }
                }
                Ok(out)
            }
            Some(_) => Err(format!(
                "'{key}' must be a JSON object string, e.g. '{{\"KEY\": \"value\"}}'"
            )),
        }
    }
}

/// Wrap a validation detail with the required-parameter prefix (FR-3.4). Only the
/// prefix is contractual; `<detail>` mirrors mcp-go's extractor wording.
fn missing_required(detail: &str) -> String {
    format!("missing required parameter: {detail}")
}

fn not_found(key: &str) -> String {
    format!("required argument \"{key}\" not found")
}

fn not_of_type(key: &str, ty: &str) -> String {
    format!("argument \"{key}\" is not of type {ty}")
}

/// Read a JSON value as `f64` the way Go's mcp-go does — only an actual JSON number
/// coerces; strings/bools/etc. do not.
fn value_as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// A Go-`encoding/json`-flavored type name for an object value's element, used in the
/// `env` non-string-value error to echo Go's `cannot unmarshal <kind>` wording.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
