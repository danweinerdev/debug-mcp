//! `LldbFactory` — the [`BackendFactory`] that produces a connected lldb backend
//! (task 3.5). Go origin: the lazy lldb-dap spawn at launch/attach time in
//! `internal/tools/launch.go`/`attach.go`.
//!
//! `connect()` detects lldb-dap, spawns it, builds a `dap-client::Client` over the
//! pipes, starts the read loop, assembles the neutral [`BackendEvent`] stream from the
//! read loop's output/terminated channels, and returns a *not-yet-launched*
//! [`Connection`]. The Phase 5 handler then calls `backend.launch(spec)` /
//! `backend.attach(spec)`.

use std::sync::Arc;

use async_trait::async_trait;
use dap_client::{Client, ReadLoop, ReadLoopChannels};
use debugger_core::{BackendError, BackendEvent, BackendFactory, Connection, DebuggerBackend};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::process::ChildStdin;

use crate::backend::LldbBackend;
use crate::{detect, subprocess};

/// The lldb-dap backend factory. Registered by the binary; `connect()` is called lazily
/// on the first `launch`/`attach` (never at server startup).
#[derive(Debug, Default, Clone)]
pub struct LldbFactory;

impl LldbFactory {
    /// Construct the factory.
    pub fn new() -> Self {
        LldbFactory
    }
}

#[async_trait]
impl BackendFactory for LldbFactory {
    fn name(&self) -> &'static str {
        "lldb"
    }

    async fn connect(&self) -> Result<Connection, BackendError> {
        // 1. Detect (failure → BackendError::Detect, Go `failed to find lldb-dap`).
        let detected = detect::find_lldb_dap()?;

        // 2. Spawn (failure → BackendError::Spawn, Go `failed to spawn lldb-dap`).
        let sub = subprocess::spawn(&detected.path, detected.is_lldb_dap)?;
        let subprocess::Subprocess {
            child,
            stdin,
            stdout,
            stderr: _stderr,
            is_lldb_dap,
        } = sub;

        // 3. Build the DAP client over the pipes; start the read loop.
        let client: Client<ChildStdin> = Client::new(stdin);
        let (read_loop, channels) = ReadLoop::new(stdout, client.shared_for_read_loop());
        tokio::spawn(read_loop.run());

        // 4. Assemble the neutral BackendEvent stream from the read loop's channels.
        let events = build_event_stream(channels);

        // 5. Return the not-yet-launched backend + its event stream.
        let backend: Arc<dyn DebuggerBackend> =
            Arc::new(LldbBackend::new(client, is_lldb_dap, Some(child)));
        Ok(Connection { backend, events })
    }
}

/// Adapt the read loop's `output` (`mpsc`) + `terminated` (`oneshot`) channels into one
/// neutral [`BackendEvent`] stream (design Decision 5): every output chunk becomes
/// `Output{category,text}`; the single terminated signal becomes `Terminated{code}` and
/// ends the stream. The stream is `'static` + `Send` so the session's event-pump can own
/// it after `connect()` returns.
pub(crate) fn build_event_stream(channels: ReadLoopChannels) -> BoxStream<'static, BackendEvent> {
    let ReadLoopChannels { output, terminated } = channels;

    // Output chunks → BackendEvent::Output, in arrival order.
    let output_stream = stream::unfold(output, |mut rx| async move {
        rx.recv().await.map(|chunk| {
            (
                BackendEvent::Output {
                    category: chunk.category,
                    text: chunk.text,
                },
                rx,
            )
        })
    });

    // The terminated signal → a single Terminated event (a dropped sender, meaning the
    // read loop ended without firing, yields no event — the output stream's end then
    // terminates the merged stream). Modeled as a 0-or-1 stream.
    let terminated_stream = stream::once(async move {
        match terminated.await {
            Ok(code) => Some(BackendEvent::Terminated { code }),
            Err(_) => None,
        }
    })
    .filter_map(|e| async move { e });

    // Merge: forward outputs as they arrive; emit Terminated when it fires. `merge`
    // keeps draining until both sub-streams end (the output mpsc closes when the read
    // loop drops its sender on exit, and the terminated stream is single-shot).
    stream::select(output_stream, terminated_stream).boxed()
}
