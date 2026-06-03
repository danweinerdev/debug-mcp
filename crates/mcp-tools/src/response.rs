//! Response intent — an rmcp-free description of what a handler wants to return.
//!
//! Go handlers return `*mcp.CallToolResult` built three ways: `NewToolResultText`
//! holding a marshaled JSON object (the common success path), `NewToolResultText`
//! holding a plain string (the two documented early-exits, FR-4.8/FR-5.7), and
//! `NewToolResultError` which sets `IsError=true` (FR-3.2). [`ToolOutcome`] captures
//! that intent so the rmcp-aware server layer (Phase 5.5) can map it to a
//! `CallToolResult` in one place — keeping these modules trivially unit-testable.
//!
//! [`RespBuilder`] mirrors Go's `map[string]any` success payloads: build up an object
//! with conditional/omit-empty inserts, then [`RespBuilder::into_outcome`] serializes
//! it into a [`ToolOutcome::Json`].

use serde_json::{Map, Value};

/// What a tool handler wants to return, free of any rmcp type.
///
/// The server layer maps these as:
/// - `Json(v)` → success result whose single text content is `v` serialized to a
///   JSON string (FR-3.1).
/// - `Text(s)` → success result whose single text content is the plain string `s`
///   (the `Program exited during launch` / `Process exited during attach`
///   early-exits, FR-4.8/FR-5.7).
/// - `Error(s)` → a tool **error** result (`IsError=true`) carrying `s` (FR-3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    /// Success: serialize this value to a JSON string for the text content.
    Json(Value),
    /// Success: a plain-text body (not JSON).
    Text(String),
    /// A tool error result; the string is the user-facing message.
    Error(String),
}

impl ToolOutcome {
    /// A success outcome carrying a JSON object/value.
    pub fn json(value: Value) -> Self {
        ToolOutcome::Json(value)
    }

    /// A success outcome carrying plain text.
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutcome::Text(s.into())
    }

    /// A tool error outcome.
    pub fn error(s: impl Into<String>) -> Self {
        ToolOutcome::Error(s.into())
    }

    /// `true` for the error variant (the eventual `CallToolResult.is_error`).
    pub fn is_error(&self) -> bool {
        matches!(self, ToolOutcome::Error(_))
    }
}

/// A small builder over `serde_json::Map` for the conditional/omit-empty key inserts
/// the Go handlers do when assembling their `map[string]any` responses.
///
/// `serde_json::Map` is backed by a `BTreeMap`, so keys serialize sorted — which
/// happens to match Go's `encoding/json` map-key ordering. Parity is structural
/// (FR-3.6); the ordering match is a bonus.
#[derive(Debug, Default, Clone)]
pub struct RespBuilder {
    map: Map<String, Value>,
}

impl RespBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Unconditionally insert `key => value` (consuming-builder style).
    pub fn set(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.map.insert(key.to_string(), value.into());
        self
    }

    /// Insert `key => value` directly (by `&mut`), for building inside a loop.
    pub fn insert(&mut self, key: &str, value: impl Into<Value>) {
        self.map.insert(key.to_string(), value.into());
    }

    /// Insert only when `value` is `Some` (Go's conditional `if x != nil` insert).
    pub fn set_opt(mut self, key: &str, value: Option<impl Into<Value>>) -> Self {
        if let Some(v) = value {
            self.map.insert(key.to_string(), v.into());
        }
        self
    }

    /// Insert only when `cond` holds (Go's `if cond { m[key] = value }`).
    pub fn set_if(mut self, cond: bool, key: &str, value: impl Into<Value>) -> Self {
        if cond {
            self.map.insert(key.to_string(), value.into());
        }
        self
    }

    /// Whether `key` is already present.
    pub fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// The finished object as a `serde_json::Value`.
    pub fn build(self) -> Value {
        Value::Object(self.map)
    }

    /// The finished object wrapped as a success [`ToolOutcome::Json`].
    pub fn into_outcome(self) -> ToolOutcome {
        ToolOutcome::Json(self.build())
    }
}
