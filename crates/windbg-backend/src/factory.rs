//! [`WinDbgFactory`] — the [`BackendFactory`] that produces a connected WinDbg backend.
//!
//! `connect()` is the analog of `lldb-backend`'s spawn-and-wire: instead of spawning a
//! subprocess + read loop, it spawns the **dedicated engine thread** (which does the COM init and
//! `Engine::create` *on the thread*), awaits its readiness signal, wires the output sink, and
//! assembles the neutral [`BackendEvent`] stream from an output `mpsc` + a single `Terminated`
//! `oneshot` — mirroring [`build_event_stream`].
//!
//! **Invariant: `connect()` makes ZERO COM calls on the calling thread.** All COM work (apartment
//! init + `Engine::create`) happens on the spawned engine thread, which signals back through the
//! readiness `oneshot`; `connect()` only spawns the thread, awaits readiness, and builds channels.
//! This is load-bearing — the calling thread (a tokio worker) is not COM-initialized, and the
//! confinement invariant ("every DbgEng/COM call runs on the engine thread") admits no exception.

use std::sync::Arc;

use async_trait::async_trait;
use dbgeng_sys::{Engine, OutputKind};
use debugger_core::{
    BackendCapabilities, BackendError, BackendEvent, BackendFactory, Connection, DebuggerBackend,
};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::{mpsc, oneshot};

use crate::backend::WinDbgBackend;
use crate::engine_ops::EngineOps;
use crate::error::EngineError;
use crate::thread::{spawn_engine_thread, EngineCmd};

/// The command-channel buffer for the engine thread. The backend awaits each reply before the
/// next caller proceeds, so a small buffer never backs up; a handful of slots absorbs any
/// transient overlap.
const CMD_BUFFER: usize = 8;

/// The WinDbg (DbgEng) backend factory. Registered by the binary (task 3.5); `connect()` is
/// called lazily on the first `launch`/`attach`, never at server startup.
#[derive(Debug, Default, Clone)]
pub struct WinDbgFactory;

impl WinDbgFactory {
    /// Construct the factory.
    pub fn new() -> Self {
        WinDbgFactory
    }
}

#[async_trait]
impl BackendFactory for WinDbgFactory {
    fn name(&self) -> &'static str {
        "windbg"
    }

    fn capabilities(&self) -> BackendCapabilities {
        // WinDbg supports every capability-gated verb (the C++ windbg-mcp plugin's surface).
        BackendCapabilities {
            crash_dump: true,
            kernel: true,
            analyze: true,
            modules: true,
        }
    }

    async fn connect(&self) -> Result<Connection, BackendError> {
        // INVARIANT: not one COM call below runs on this (the caller's) thread. The engine thread
        // does the apartment init + Engine::create and signals back over `ready_rx`.

        // Output channel: the engine's output sink (installed below, running ON the engine thread)
        // sends each line here; `build_event_stream` turns them into BackendEvent::Output.
        let (out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
        // Terminated channel: the engine thread fires this once on teardown (its read-loop-ended
        // analog); it becomes the single BackendEvent::Terminated.
        let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();

        // Spawn the engine thread with the PRODUCTION constructor: build the real Engine on the
        // thread and box it as a dyn EngineOps. A create failure travels back over the readiness
        // oneshot (mapped to Detect below) — connect() never touches the engine directly.
        let make_engine =
            || -> Result<Box<dyn EngineOps>, EngineError> { Engine::create().map(boxed_engine) };
        let (cmd_tx, ready_rx, engine_thread) =
            spawn_engine_thread(make_engine, CMD_BUFFER, Some(term_tx));

        // Await readiness. A creation failure → BackendError::Detect with a "failed to initialize
        // DbgEng" message (the engine could not be stood up); a dropped sender (the thread died
        // before signaling) → Spawn.
        let interrupt = match ready_rx.await {
            Ok(Ok(handle)) => handle,
            Ok(Err(e)) => {
                return Err(BackendError::Detect(format!(
                    "failed to initialize DbgEng: {e}"
                )))
            }
            Err(_) => {
                return Err(BackendError::Spawn(
                    "failed to initialize DbgEng: engine thread exited before readiness".into(),
                ))
            }
        };

        // Wire the output sink: a closure that ONLY sends (OutputKind, text) to the output mpsc —
        // no await, no direct buffer write (Decision 6). It runs on the engine thread; the
        // unbounded send is non-blocking. Installed fire-and-forget via SetOutputSink (no reply).
        //
        // LIMITATION (debuggee stdout): this sink carries only DbgEng's `IDebugOutputCallbacks` —
        // *engine* output (ModLoad, break notices, symbol diagnostics) — NOT the debuggee's own
        // stdout/stderr. `dbgeng-sys::Engine::launch` uses `CREATE_NO_WINDOW`, so the child's
        // `printf` console writes go nowhere observable and never reach this channel. So the
        // `read_output` MCP tool returns engine output, not program output, under WinDbg — a known
        // difference from the lldb/DAP backend, which pipes the launched process's stdout through
        // the event stream. Surfacing real debuggee stdout needs a `dbgeng-sys` launch-path change
        // (stdout pipe redirection / console handling), tracked for a later phase.
        let sink_tx = out_tx.clone();
        let sink = Box::new(move |kind: OutputKind, text: &str| {
            // The receiver may already be gone (the event stream was dropped); ignore that.
            let _ = sink_tx.send((kind, text.to_string()));
        });
        // Dropping our own out_tx clone here would NOT close the channel (the sink holds one); we
        // keep none beyond the sink, so the output stream ends when the engine drops the sink.
        drop(out_tx);
        if cmd_tx
            .send(EngineCmd::SetOutputSink { sink })
            .await
            .is_err()
        {
            return Err(BackendError::Spawn(
                "failed to initialize DbgEng: engine thread closed before sink install".into(),
            ));
        }

        // Assemble the neutral BackendEvent stream (mirror lldb_backend::build_event_stream).
        let events = build_event_stream(out_rx, term_rx);

        // DbgEng runs in-process — there is no debugger subprocess pid to report.
        let backend: Arc<dyn DebuggerBackend> =
            Arc::new(WinDbgBackend::new(cmd_tx, interrupt, None, engine_thread));
        Ok(Connection { backend, events })
    }
}

/// Box a created [`Engine`] as a `dyn EngineOps` (the production constructor's tail). Factored out
/// so the closure type stays simple.
fn boxed_engine(engine: Engine) -> Box<dyn EngineOps> {
    Box::new(engine)
}

/// Map a DbgEng [`OutputKind`] to the neutral output `category` string carried on
/// [`BackendEvent::Output`] (opaque pass-through, Spec FR-18.6). Normal output is `"stdout"`,
/// error/warning is `"stderr"`, the prompt is `"console"`, and any other category falls back to
/// `"console"`.
fn category_for(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Normal => "stdout",
        OutputKind::Error | OutputKind::Warning => "stderr",
        OutputKind::Prompt => "console",
        OutputKind::Other(_) => "console",
    }
}

/// Adapt the engine thread's output (`mpsc`) + terminated (`oneshot`) channels into one neutral
/// [`BackendEvent`] stream (design Decision 5), mirroring `lldb_backend::build_event_stream`:
/// every output line becomes `Output{category,text}`; the single terminated signal becomes
/// `Terminated{code}` and (with the output stream's end) terminates the merged stream. The stream
/// is `'static` + `Send` so the session's event-pump can own it after `connect()` returns.
pub(crate) fn build_event_stream(
    out_rx: mpsc::UnboundedReceiver<(OutputKind, String)>,
    term_rx: oneshot::Receiver<Option<i64>>,
) -> BoxStream<'static, BackendEvent> {
    // Output lines → BackendEvent::Output, in arrival order.
    let output_stream = stream::unfold(out_rx, |mut rx| async move {
        rx.recv().await.map(|(kind, text)| {
            (
                BackendEvent::Output {
                    category: category_for(kind).to_string(),
                    text,
                },
                rx,
            )
        })
    });

    // The terminated signal → a single Terminated event (a dropped sender yields no event — the
    // output stream's end then terminates the merged stream). Modeled as a 0-or-1 stream.
    let terminated_stream = stream::once(async move {
        match term_rx.await {
            Ok(code) => Some(BackendEvent::Terminated { code }),
            Err(_) => None,
        }
    })
    .filter_map(|e| async move { e });

    stream::select(output_stream, terminated_stream).boxed()
}
