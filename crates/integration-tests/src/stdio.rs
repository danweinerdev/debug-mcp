//! A minimal stdio MCP client for the differential-parity harness (Phase 6.3).
//!
//! Drives an MCP server binary (the Rust `debug-mcp` or the Go `lldb-debug-mcp`) over the
//! stdio transport: newline-delimited JSON-RPC (one JSON object per line, no
//! Content-Length framing). It performs the `initialize` / `notifications/initialized`
//! handshake, then issues `tools/list` and `tools/call` requests sequentially, correlating
//! responses by id. This is the "proven working pattern" the plan calls out — sequential
//! request/response, no concurrency.
//!
//! Lives in the crate library (not `tests/common/`) so the helpers are `pub` API consumed
//! by the test binaries — keeping them out of the dead-code lint without any `#[allow]`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// A spawned MCP server process driven over stdio.
pub struct StdioMcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl StdioMcp {
    /// Spawn `program` (with optional env) and complete the MCP `initialize` handshake.
    /// `env` is applied to the child (e.g. `LLDB_DAP_PATH`). Returns an initialized client
    /// ready for `tools/call`.
    pub fn spawn(program: &str, env: &[(&str, &str)]) -> std::io::Result<StdioMcp> {
        let mut command = Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));

        let mut client = StdioMcp {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> std::io::Result<()> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parity-harness", "version": "1.0.0"}
            }
        }))?;
        let _ = self.read_response(id)?;
        // The post-initialize notification (no response expected).
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;
        Ok(())
    }

    /// The server's advertised `serverInfo` (name/version), from a fresh `initialize`.
    /// Re-runs initialize to fetch it deterministically.
    pub fn server_info(program: &str, env: &[(&str, &str)]) -> std::io::Result<Value> {
        let mut command = Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        let mut client = StdioMcp {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let id = client.alloc_id();
        client.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parity-harness", "version": "1.0.0"}
            }
        }))?;
        let resp = client.read_response(id)?;
        let info = resp
            .get("result")
            .and_then(|r| r.get("serverInfo"))
            .cloned()
            .unwrap_or(Value::Null);
        client.shutdown();
        Ok(info)
    }

    /// Call `tools/list` and return the `tools` array.
    pub fn list_tools(&mut self) -> std::io::Result<Vec<Value>> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        }))?;
        let resp = self.read_response(id)?;
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tools)
    }

    /// Call a tool and return the parsed JSON object from its single text content (the
    /// shape every handler returns). For a tool-error result, returns the text content
    /// wrapped as `{"__is_error": true, "__text": <message>}` so the caller can compare
    /// error paths structurally too. Plain-text (non-JSON) success content is wrapped as
    /// `{"__text": <text>}`.
    pub fn call_tool(&mut self, name: &str, args: Value) -> std::io::Result<ToolCallResult> {
        let id = self.alloc_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }))?;
        let resp = self.read_response(id)?;
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let json = serde_json::from_str::<Value>(&text).ok();
        Ok(ToolCallResult {
            is_error,
            text,
            json,
        })
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let line = serde_json::to_string(message).expect("serialize JSON-RPC");
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    /// Read newline-delimited JSON-RPC messages until the response with `id` arrives,
    /// skipping notifications and unrelated messages.
    fn read_response(&mut self, id: i64) -> std::io::Result<Value> {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("server closed stdout before responding to id {id}"),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // non-JSON log line on stdout: skip.
            };
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return Ok(value);
            }
        }
    }

    /// Terminate the server (kill + reap). Idempotent.
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioMcp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A parsed `tools/call` result.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub is_error: bool,
    pub text: String,
    /// The parsed JSON object when the content is JSON (the common success path); `None`
    /// for the plain-text early-exits.
    pub json: Option<Value>,
}

/// Locate the Go reference binary for the differential harness. Order: `GO_DEBUG_MCP_BIN`
/// env var (an explicit path), then `lldb-debug-mcp` on PATH. Returns `None` when absent
/// (the harness then skips — Go is not installed in the dev sandbox).
pub fn go_reference_binary() -> Option<String> {
    if let Ok(path) = std::env::var("GO_DEBUG_MCP_BIN") {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    which("lldb-debug-mcp")
}

/// Locate the Rust `debug-mcp` binary built by cargo. Order: `DEBUG_MCP_BIN` env var, then
/// the cargo target dir (`CARGO_TARGET_DIR` or the workspace `target/`), then PATH.
pub fn rust_binary() -> Option<String> {
    if let Ok(path) = std::env::var("DEBUG_MCP_BIN") {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    // The cargo target dir holds the freshly-built binary. Honor CARGO_TARGET_DIR.
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        let candidate = format!("{target}/debug/debug-mcp");
        if std::path::Path::new(&candidate).exists() {
            return Some(candidate);
        }
    }
    which("debug-mcp")
}

/// A `which`-equivalent PATH lookup (no external process).
fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}
