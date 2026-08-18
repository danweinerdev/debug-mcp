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
use futures::future::FutureExt;
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::{mpsc, oneshot};

use crate::backend::WinDbgBackend;
use crate::engine_ops::EngineOps;
use crate::error::EngineError;
use crate::thread::{EngineCmd, spawn_engine_thread};

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
                )));
            }
            Err(_) => {
                return Err(BackendError::Spawn(
                    "failed to initialize DbgEng: engine thread exited before readiness".into(),
                ));
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

        // R2 — the backend-drop signal: a oneshot whose SENDER lives on the backend (never sent on)
        // and whose RECEIVER feeds `build_event_stream`. When the last backend `Arc` drops (the
        // session's `clear_backend`/disconnect path, or the cancel-cleanup path), the sender drops and
        // `drop_rx` resolves with `Err`, forcing a synthetic `Terminated` so the pump completes even
        // when an orphaned kernel-attach thread is stuck in `WaitForEvent(INFINITE)` (still holding
        // the output sink open, never firing `term_rx`). See `WinDbgBackend::_drop_signal`.
        let (drop_tx, drop_rx) = oneshot::channel::<()>();

        // Assemble the neutral BackendEvent stream (mirror lldb_backend::build_event_stream).
        let events = build_event_stream(out_rx, term_rx, drop_rx);

        // DbgEng runs in-process — there is no debugger subprocess pid to report.
        let backend: Arc<dyn DebuggerBackend> = Arc::new(WinDbgBackend::new(
            cmd_tx,
            interrupt,
            None,
            engine_thread,
            drop_tx,
        ));
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

/// The internal state threaded through the [`build_event_stream`] `unfold`: the output receiver,
/// the pinned "terminate" future (the race of `term_rx` and `drop_rx`), and whether the terminal
/// `Terminated` has already been emitted (so the stream ends after exactly one — no double, no
/// regression on the normal path).
struct StreamState {
    out_rx: mpsc::UnboundedReceiver<(OutputKind, String)>,
    /// The terminate future: resolves to the exit code (real, from `term_rx`) when EITHER the engine
    /// fires `term_rx` OR the backend drops (`drop_rx` resolves `Err` → `None` code). Pinned/boxed so
    /// it can be polled repeatedly across `unfold` iterations while output is still flowing.
    terminate: futures::future::BoxFuture<'static, Option<i64>>,
    /// Set once the single `Terminated` event has been yielded; the next poll ends the stream.
    terminated: bool,
}

/// Adapt the engine thread's output (`mpsc`) + terminated (`oneshot`) channels into one neutral
/// [`BackendEvent`] stream (design Decision 5), mirroring `lldb_backend::build_event_stream`:
/// every output line becomes `Output{category,text}`, then a single `Terminated{code}` ends the
/// stream. The stream is `'static` + `Send` so the session's event-pump can own it after
/// `connect()` returns.
///
/// R2 — the termination trigger is whichever fires FIRST: the engine `term_rx` (carrying the real
/// exit code) OR `drop_rx` (the backend was dropped → a synthetic `Terminated { code: None }`). The
/// merged stream emits `Output` events UNTIL that race resolves, then emits exactly ONE `Terminated`
/// and ENDS. Crucially the end does NOT depend on `out_rx` closing — an orphaned kernel-attach
/// thread stuck in `WaitForEvent(INFINITE)` keeps the output sink (and thus `out_rx`) open forever,
/// so relying on `out_rx`'s end would hang the pump. Two invariants hold:
/// - NORMAL path: the engine fires `term_rx` on clean exit/disconnect → exactly one
///   `Terminated{real code}`, then the stream ends. A later `drop_rx` firing is harmless (the
///   stream already ended — `terminated` is set, so no second `Terminated`).
/// - ORPHAN path: `term_rx` never fires and `out_rx` never closes → the backend drop resolves
///   `drop_rx` → synthetic `Terminated{None}` → the stream ends.
pub(crate) fn build_event_stream(
    out_rx: mpsc::UnboundedReceiver<(OutputKind, String)>,
    term_rx: oneshot::Receiver<Option<i64>>,
    drop_rx: oneshot::Receiver<()>,
) -> BoxStream<'static, BackendEvent> {
    // The terminate race: the FIRST of `term_rx` (real exit code) and `drop_rx` (backend dropped →
    // synthetic `None` code) wins. A dropped `term_rx` sender (the engine thread exited without an
    // explicit code) also resolves it with `None` — the same terminal outcome.
    let terminate = async move {
        tokio::select! {
            biased;
            // The engine's real terminated signal: `Ok(code)` carries the exit code; an `Err`
            // (sender dropped without a value) collapses to `None`.
            res = term_rx => res.unwrap_or(None),
            // The backend-drop signal: it is never sent on, so it only ever resolves `Err` on drop —
            // a synthetic termination with no exit code.
            _ = drop_rx => None,
        }
    }
    .boxed();

    let state = StreamState {
        out_rx,
        terminate,
        terminated: false,
    };

    stream::unfold(state, |mut state| async move {
        // Already emitted the one terminal event — end the stream.
        if state.terminated {
            return None;
        }
        // Race the next output line against termination. Output keeps flowing until the terminate
        // future resolves; once it does, emit the single `Terminated` and mark the stream to end on
        // the next poll. `biased` makes `out_rx.recv()` the first branch polled, so all buffered
        // output is DRAINED before `Terminated` is emitted: when output and termination are both
        // ready, we never drop a buffered line in favor of the terminal event. This is safe because
        // the `terminated` flag already guarantees `Terminated` is emitted exactly once — biasing
        // toward output only delays the (single) terminal event until the buffer is empty.
        tokio::select! {
            biased;
            line = state.out_rx.recv() => match line {
                Some((kind, text)) => Some((
                    BackendEvent::Output {
                        category: category_for(kind).to_string(),
                        text,
                    },
                    state,
                )),
                // The output channel closed WITHOUT a prior termination signal (the engine thread
                // ended normally, dropping the sink, and `term_rx`/`drop_rx` have not yet resolved):
                // await the terminate future to produce the real terminal event, then end. This
                // preserves the normal-teardown ordering (the engine's `term_rx` fires on exit).
                None => {
                    let code = (&mut state.terminate).await;
                    state.terminated = true;
                    Some((BackendEvent::Terminated { code }, state))
                }
            },
            code = &mut state.terminate => {
                state.terminated = true;
                Some((BackendEvent::Terminated { code }, state))
            }
        }
    })
    .boxed()
}
