//! Phase 6.1 smoke test: launch `testdata/simple` and assert it reaches stopped.
//!
//! Gated behind the `integration` feature; without it this file is empty. The test skips
//! cleanly (logs + returns) when lldb-dap or the fixtures are absent.

#![cfg(feature = "integration")]

use integration_tests::harness::{fixture_path, should_skip, Harness};
use mcp_session::State;

#[tokio::test]
async fn smoke_launch_simple_reaches_stopped() {
    if should_skip("smoke_launch_simple_reaches_stopped", &["simple"]) {
        return;
    }

    let h = Harness::new();
    let fixture = fixture_path("simple");

    let launch = h.launch_fixture(&fixture).await;
    assert_eq!(launch["status"], serde_json::json!("launched"));
    assert_eq!(launch["state"], serde_json::json!("stopped"));
    assert_eq!(h.state(), State::Stopped);

    // The pid fix: launch records the lldb-dap subprocess pid (non-zero), not 0.
    let pid = launch["pid"].as_i64().expect("pid is an integer");
    assert!(
        pid > 0,
        "expected a non-zero lldb-dap subprocess pid, got {pid}"
    );
    assert_eq!(h.pid(), pid, "session pid matches the reported launch pid");

    h.disconnect_cleanup().await;
    assert_eq!(h.state(), State::Idle);
}
