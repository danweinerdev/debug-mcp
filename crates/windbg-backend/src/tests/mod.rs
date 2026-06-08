//! Unit tests for `windbg-backend` (dedicated `src/tests/` module per CLAUDE.md — never inline
//! `#[cfg(test)]` next to the code). These use [`FakeEngine`](fake::FakeEngine) and need **no
//! live engine** (and no COM on the calling thread), so they run on any Windows host.

mod breakpoints;
mod execution;
mod extras;
mod fake;
mod inspection;
mod lifecycle;
mod marshal;

use tokio::sync::oneshot;

use crate::backend::WinDbgBackend;
use crate::engine_ops::EngineOps;
use crate::error::EngineError;
use crate::thread::spawn_engine_thread;
use fake::FakeEngine;

/// Spawn the engine thread over a fake constructor and assemble a [`WinDbgBackend`], awaiting the
/// readiness signal exactly as `WinDbgFactory::connect` does (but with no COM/Engine::create).
/// Returns the backend plus the terminated receiver so a test can assert teardown.
///
/// This is the test analog of `connect()`'s spawn+wire, proving the thread-spawn + readiness path
/// with the injected fake constructor — `connect()` itself needs a live engine and is not used.
async fn backend_over_fake(
    make_engine: impl FnOnce() -> Result<Box<dyn EngineOps>, EngineError> + Send + 'static,
) -> Result<(WinDbgBackend, oneshot::Receiver<Option<i64>>), EngineError> {
    let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (cmd_tx, ready_rx, engine_thread) = spawn_engine_thread(make_engine, 8, Some(term_tx));
    let interrupt = match ready_rx.await {
        Ok(result) => result?,
        // The thread died before signaling readiness — surface as an engine error for the test.
        Err(_) => return Err(EngineError::engine("engine thread exited before readiness")),
    };
    Ok((
        WinDbgBackend::new(cmd_tx, interrupt, None, engine_thread),
        term_rx,
    ))
}

/// The production-shaped fake constructor: build a default [`FakeEngine`].
fn ok_constructor() -> Result<Box<dyn EngineOps>, EngineError> {
    Ok(Box::new(FakeEngine::default()))
}
