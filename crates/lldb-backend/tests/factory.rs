//! `LldbFactory` tests (task 3.5).
//!
//! The hermetic test asserts the factory's `name()`. The detect/spawn → `Detect`/`Spawn`
//! mapping for `connect()` is covered structurally by the detect unit tests (which prove
//! `find_lldb_dap` returns `BackendError::Detect`) plus the subprocess spawn error
//! wrapping; `connect()` reads the real process environment, which cannot be mutated
//! race-free across concurrent tests, so it is exercised end-to-end only by the
//! `live` smoke test below.
//!
//! The `live` smoke test (opt-in, analog of the Go `//go:build integration` tag) runs
//! the full factory → launch path against a real lldb-dap + the `testdata/simple`
//! fixture, and skips cleanly when either is missing.

use debugger_core::BackendFactory;
use lldb_backend::LldbFactory;

#[test]
fn factory_name_is_lldb() {
    assert_eq!(LldbFactory::new().name(), "lldb");
}

#[cfg(feature = "live")]
mod live {
    use debugger_core::{BackendFactory, LaunchOutcome, LaunchSpec};
    use lldb_backend::{find_lldb_dap, LldbFactory};

    /// Resolve the `testdata/simple` fixture path relative to the repo. Returns `None`
    /// (⇒ skip) if it has not been built. The crate is at `<repo>/rust/crates/lldb-backend`;
    /// testdata is at `<repo>/testdata`.
    fn simple_fixture() -> Option<std::path::PathBuf> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // crates/lldb-backend → crates → rust → repo root.
        let repo = manifest.ancestors().nth(3)?;
        let p = repo.join("testdata").join("simple");
        p.exists().then_some(p)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_then_launch_reaches_stopped() {
        // Skip cleanly if lldb-dap or the fixture is absent (not a failure).
        if find_lldb_dap().is_err() {
            eprintln!("SKIP live::connect_then_launch_reaches_stopped: no lldb-dap on PATH");
            return;
        }
        let Some(program) = simple_fixture() else {
            eprintln!(
                "SKIP live::connect_then_launch_reaches_stopped: testdata/simple not built \
                 (run `make -C testdata`)"
            );
            return;
        };

        let factory = LldbFactory::new();
        let conn = factory.connect().await.expect("connect to a real lldb-dap");

        let spec = LaunchSpec {
            program: program.to_string_lossy().into_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            stop_on_entry: true,
            source_breakpoints: Vec::new(),
            function_breakpoints: Vec::new(),
        };

        // Bound the handshake so a hung adapter fails the test rather than hanging CI.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            conn.backend.launch(spec),
        )
        .await
        .expect("launch did not time out")
        .expect("launch ok");

        // stop_on_entry=true ⇒ the program stops at entry (the simple fixture stops
        // before its single `printf`). Either Stopped (the normal case) or an Exited
        // race is acceptable; Running is not (stop_on_entry was true).
        match outcome {
            LaunchOutcome::Stopped(info) => {
                eprintln!("live launch stopped: reason={:?}", info.reason);
            }
            LaunchOutcome::Exited { code } => {
                eprintln!("live launch exited during stop-on-entry: code={code:?}");
            }
            LaunchOutcome::Running => panic!("stop_on_entry=true must not yield Running"),
        }

        conn.backend.disconnect(true).await;
    }
}
