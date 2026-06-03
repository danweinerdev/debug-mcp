//! In-process harness for the live integration suite (Phase 6.1).
//!
//! Mirrors the Go `internal/tools/integration_test.go` helpers (`newTestTools`,
//! `launchFixture`, `disconnectCleanup`, `callContinue`, `parseToolResult`,
//! `testFixturePath`) but drives the **real** Rust handlers in-process: a [`ToolServer`]
//! over a real [`SessionManager`] + the real [`LldbFactory`], invoked via
//! [`ToolServer::call`]. No stdio round-trip — exactly the structure of the Go tests,
//! which call `tools.handleX(ctx, req)` directly.
//!
//! Every live test runs only when lldb-dap **and** the compiled fixtures are present; the
//! harness exposes [`live_prereqs`] so a missing prerequisite **skips cleanly** (logs +
//! returns) rather than failing — matching the plan's "skip, not fail, when lldb-dap or
//! the fixtures are absent."
//!
//! Per-call timeouts: every tool call goes through [`Harness::call`], which bounds the
//! handler with `tokio::time::timeout`. A timeout is a hard test failure (a hang is a
//! parity bug), except where a test explicitly probes the no-hang behavior.
//!
//! This lives in the crate's library (not a `tests/common/` module) so the helpers are
//! `pub` library API consumed by the test binaries — which keeps them out of the
//! dead-code lint without any `#[allow]` (a `tests/common/` module compiled into every
//! test binary would otherwise need `#![allow(dead_code)]`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lldb_backend::LldbFactory;
use mcp_session::{SessionManager, State};
use mcp_tools::{ToolOutcome, ToolServer};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// A generous default per-call bound. Individual calls override it (launch/continue use
/// 30 s like Go; the e2e workflow uses 60 s overall). A call exceeding its bound is a
/// hang — a hard failure.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The in-process test harness: the real server, the shared session (so tests can read
/// state + the recorded lldb-dap subprocess pid), and a never-cancelled token.
pub struct Harness {
    pub server: ToolServer,
    pub session: Arc<SessionManager>,
    ct: CancellationToken,
}

impl Harness {
    /// Build a server over a fresh session + the **real** lldb factory.
    pub fn new() -> Harness {
        let session = Arc::new(SessionManager::new());
        let factory = Arc::new(LldbFactory::new());
        let server = ToolServer::new(Arc::clone(&session), factory);
        Harness {
            server,
            session,
            ct: CancellationToken::new(),
        }
    }

    /// The current session state.
    pub fn state(&self) -> State {
        self.session.state()
    }

    /// The recorded lldb-dap subprocess pid (0 until a launch/attach records it).
    pub fn pid(&self) -> i64 {
        self.session.pid()
    }

    /// Invoke a tool by name with the given args, bounded by `timeout`. Panics (fails the
    /// test) if the handler does not return within `timeout` — a hang is a parity bug.
    pub async fn call(
        &self,
        name: &str,
        args: Map<String, Value>,
        timeout: Duration,
    ) -> ToolOutcome {
        match tokio::time::timeout(timeout, self.server.call(name, &args, &self.ct)).await {
            Ok(outcome) => outcome,
            Err(_) => panic!("tool '{name}' did not return within {timeout:?} (handler hung)"),
        }
    }

    /// Invoke a tool with the default 30 s bound.
    pub async fn call_default(&self, name: &str, args: Map<String, Value>) -> ToolOutcome {
        self.call(name, args, DEFAULT_CALL_TIMEOUT).await
    }

    /// Launch a fixture with `stop_on_entry=true`, asserting success, and return the parsed
    /// JSON object (Go `launchFixture`).
    pub async fn launch_fixture(&self, fixture: &Path) -> Map<String, Value> {
        let args = obj(&[
            ("program", Value::String(fixture.display().to_string())),
            ("stop_on_entry", Value::Bool(true)),
        ]);
        let out = self.call("launch", args, Duration::from_secs(30)).await;
        expect_json_obj("launch", &out)
    }

    /// Continue, asserting non-error, and return the parsed JSON object (Go `callContinue`).
    pub async fn continue_(&self) -> Map<String, Value> {
        let out = self
            .call("continue", Map::new(), Duration::from_secs(30))
            .await;
        expect_json_obj("continue", &out)
    }

    /// Best-effort disconnect for cleanup (Go `disconnectCleanup`): ignore the outcome.
    pub async fn disconnect_cleanup(&self) {
        let args = obj(&[("terminate", Value::Bool(true))]);
        let _ = self.call("disconnect", args, Duration::from_secs(10)).await;
    }
}

impl Default for Harness {
    fn default() -> Self {
        Harness::new()
    }
}

/// Build an arguments object from `(key, value)` pairs.
pub fn obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// `parse_tool_result` (Go `parseToolResult` / the inline `json.Unmarshal` of the text
/// content): assert the outcome is a JSON-success and return the object. A `Text`/`Error`
/// outcome (or a non-object JSON) panics with context.
pub fn expect_json_obj(label: &str, outcome: &ToolOutcome) -> Map<String, Value> {
    match outcome {
        ToolOutcome::Json(Value::Object(map)) => map.clone(),
        ToolOutcome::Json(other) => panic!("{label}: expected JSON object, got {other:?}"),
        ToolOutcome::Text(t) => panic!("{label}: expected JSON success, got plain text {t:?}"),
        ToolOutcome::Error(e) => panic!("{label}: expected JSON success, got tool error {e:?}"),
    }
}

/// Assert the outcome is a tool **error** and return its message.
pub fn expect_error<'a>(label: &str, outcome: &'a ToolOutcome) -> &'a str {
    match outcome {
        ToolOutcome::Error(m) => m,
        other => panic!("{label}: expected a tool error, got {other:?}"),
    }
}

/// The repository root (where the workspace `Cargo.toml` and `testdata/` live),
/// derived from this crate's manifest dir.
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/integration-tests → up two levels = <root>.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("workspace root above crates/integration-tests")
        .to_path_buf()
}

/// The absolute path to a fixture in `testdata/` (Go `testFixturePath`). Does **not**
/// assert existence — callers gate on [`live_prereqs`] first.
pub fn fixture_path(name: &str) -> PathBuf {
    workspace_root().join("testdata").join(name)
}

/// Whether lldb-dap can be detected (so the live suite can run) — uses the same detection
/// the production backend uses (`LLDB_DAP_PATH` / PATH / versioned / `xcrun`).
pub fn lldb_dap_available() -> bool {
    lldb_backend::find_lldb_dap().is_ok()
}

/// The live-suite prerequisites: lldb-dap detectable **and** the named fixtures all built.
/// Returns `Ok(())` to run or `Err(reason)` to skip cleanly. Tests log the reason and
/// return (a skip), never fail, when this is `Err` (plan 6.1/6.2).
pub fn live_prereqs(fixtures: &[&str]) -> Result<(), String> {
    if !lldb_dap_available() {
        return Err("lldb-dap not found (set LLDB_DAP_PATH or install lldb-dap)".to_string());
    }
    for name in fixtures {
        let p = fixture_path(name);
        if !p.exists() {
            return Err(format!(
                "fixture {p:?} missing — run `make -C testdata` to build the C fixtures"
            ));
        }
    }
    Ok(())
}

/// The skip-or-run check for a live test: returns `true` (and logs a clear skip message)
/// when the prerequisites are not met, so the test can `if should_skip(...) { return; }`.
/// `test_name` is the calling test's name, for a readable skip log.
pub fn should_skip(test_name: &str, fixtures: &[&str]) -> bool {
    match live_prereqs(fixtures) {
        Ok(()) => false,
        Err(reason) => {
            eprintln!("SKIP {test_name}: {reason}");
            true
        }
    }
}
