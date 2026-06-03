//! The read-loop task: frame-by-frame dispatch + EOF recovery (Spec FR-17.6/17.7,
//! task 2.3, design §"DAP Client internals" / §Error Handling).
//!
//! One task reads framed messages and dispatches by concrete type. Concrete events are
//! matched **before** the generic response case (Go `client.go:187-227`: the event
//! arms come first, only non-event responses fall through to `dispatchResponse`). On
//! EOF / read error the recovery sequence runs in this exact order (Go
//! `client.go:171-185`): record closed-once + the raw error, `cancel_all_pending` with
//! a wrapped "read loop terminated" error, `stop_waiter.cancel()`, fire the terminated
//! signal, then exit.
//!
//! Output and the terminated lifecycle are exposed as tokio channels the backend
//! (Phase 3) adapts into `BackendEvent`s: an `mpsc` of `(category, text)` output pairs
//! and a `oneshot` terminated signal carrying the optional exit code (design
//! Decision 5). The read loop owns no buffering policy (that is the session's
//! `OutputBuffer`, above the seam).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use debugger_core::BackendError;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::client::Shared;
use crate::error::WireError;
use crate::wire::{read_message, DapMessage, Event};

/// One captured program-output chunk forwarded to the backend's output sink
/// (Spec FR-12). Neutral `(category, text)` — no buffering here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub category: String,
    pub text: String,
}

/// The async signals the read loop exposes to the backend. `output` receives every
/// `OutputEvent`; `terminated` fires once when the session ends (TerminatedEvent or
/// EOF), carrying the last-known exit code if an `ExitedEvent` was seen first.
pub struct ReadLoopChannels {
    /// Receiver for program output chunks. Unbounded so the read loop never blocks on a
    /// slow consumer (matching Go's synchronous-but-non-blocking `outputHandler`, whose
    /// session appends to an in-memory buffer).
    pub output: mpsc::UnboundedReceiver<OutputChunk>,
    /// Fires once with the optional exit code when the connection terminates.
    pub terminated: oneshot::Receiver<Option<i64>>,
}

/// The sender side the read loop keeps. Held in one struct so EOF recovery can fire the
/// terminated signal exactly once.
struct Sinks {
    output: mpsc::UnboundedSender<OutputChunk>,
    /// Wrapped so the single terminated send is takeable (fired at most once).
    terminated: Mutex<Option<oneshot::Sender<Option<i64>>>>,
    /// Last exit code seen via an `ExitedEvent`, carried into the terminated signal.
    last_exit_code: Mutex<Option<i64>>,
}

impl Sinks {
    fn fire_terminated(&self) {
        let sender = self
            .terminated
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(sender) = sender {
            let code = *self
                .last_exit_code
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            // A dropped receiver makes this `Err`; that is a no-op (the backend stopped
            // listening), matching Go's best-effort `onTerminated`.
            let _ = sender.send(code);
        }
    }

    fn record_exit(&self, code: i64) {
        *self
            .last_exit_code
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(code);
    }
}

/// Tracks the first read error and that the loop closed exactly once (Go's
/// `closeOnce`/`closeErr`). The recorded error is kept for diagnostics.
#[derive(Default)]
struct CloseState {
    closed: AtomicBool,
    err: Mutex<Option<String>>,
}

impl CloseState {
    /// Record the close exactly once. Returns `true` the first time (so recovery runs
    /// once even if two errors raced — only one can win the `compare_exchange`).
    fn close_once(&self, err: &WireError) -> bool {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *self.err.lock().unwrap_or_else(|p| p.into_inner()) = Some(err.to_string());
            true
        } else {
            false
        }
    }
}

/// The read loop, parameterized over the reader and the writer type of its `Shared`.
pub struct ReadLoop<R, W> {
    reader: R,
    shared: Arc<Shared<W>>,
    sinks: Arc<Sinks>,
    close_state: CloseState,
}

impl<R, W> ReadLoop<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Build the read loop and its channels. Spawn [`ReadLoop::run`] on a task (or await
    /// it directly in tests). Returns the loop plus the [`ReadLoopChannels`] the backend
    /// drains.
    pub fn new(reader: R, shared: Arc<Shared<W>>) -> (Self, ReadLoopChannels) {
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let (term_tx, term_rx) = oneshot::channel();
        let sinks = Arc::new(Sinks {
            output: output_tx,
            terminated: Mutex::new(Some(term_tx)),
            last_exit_code: Mutex::new(None),
        });
        let read_loop = ReadLoop {
            reader,
            shared,
            sinks,
            close_state: CloseState::default(),
        };
        let channels = ReadLoopChannels {
            output: output_rx,
            terminated: term_rx,
        };
        (read_loop, channels)
    }

    /// Run until the transport closes. Dispatches each message (Spec FR-17.6) and runs
    /// EOF recovery on the terminating read (Spec FR-17.7).
    pub async fn run(mut self) {
        loop {
            match read_message(&mut self.reader).await {
                Ok(message) => self.dispatch(message),
                Err(err) => {
                    self.recover(err);
                    return;
                }
            }
        }
    }

    /// Dispatch one decoded message. Concrete events are matched before the generic
    /// response case (Go `client.go:187-227`).
    fn dispatch(&self, message: DapMessage) {
        match message {
            DapMessage::Event(event) => self.dispatch_event(event),
            DapMessage::Response(response) => {
                let request_seq = response.request_seq;
                if !self
                    .shared
                    .dispatch_response(request_seq, DapMessage::Response(response))
                {
                    // No waiter for this seq: log + discard, never panic (Go
                    // `dispatchResponse` "no waiter for response to request seq").
                    log_no_waiter(request_seq);
                }
            }
            DapMessage::Other(envelope) => {
                // A well-formed but unmodeled envelope (e.g. a request from the adapter,
                // or an event we do not model): Go logs it as unhandled (`default`).
                log_unhandled(
                    &envelope.ty,
                    envelope.event.as_deref(),
                    envelope.command.as_deref(),
                );
            }
        }
    }

    fn dispatch_event(&self, event: Event) {
        match event {
            Event::Stopped(stopped) => {
                // Go calls `onStopped` (caches the event) *then* `stopWaiter.Deliver`.
                // The cache hook lives above the seam (Phase 3) and is driven off the
                // stop outcome; here the stop waiter delivery carries the StopInfo, so
                // the ordering is preserved by delivering the translated outcome.
                self.shared.stop_waiter().deliver(&stopped);
            }
            Event::Initialized => {
                // Capacity-1, non-blocking (Go's buffered(1) `initializedChan`).
                self.shared.signal_initialized();
            }
            Event::Output(output) => {
                // Forward to the output sink; unbounded ⇒ never blocks (Spec FR-12).
                let _ = self.sinks.output.send(OutputChunk {
                    category: output.body.category,
                    text: output.body.output,
                });
            }
            Event::Exited(exited) => {
                let code = exited.body.exit_code;
                // Record the exit code (carried into the terminated signal), then
                // deliver an exit to the stop waiter (Go `onExit` then `DeliverExit`).
                self.sinks.record_exit(code);
                self.shared.stop_waiter().deliver_exit(code);
            }
            Event::Terminated => {
                // Go calls `onTerminated` (sets state terminated) then `stopWaiter.Cancel`.
                self.sinks.fire_terminated();
                self.shared.stop_waiter().cancel();
            }
            Event::Thread
            | Event::Breakpoint
            | Event::Process
            | Event::Continued
            | Event::Module
            | Event::Capabilities => {
                // Informational events: log only (Go `client.go:218`).
                log_informational(&event);
            }
        }
    }

    /// EOF / read-error recovery (Spec FR-17.7, Go `client.go:171-185`), in order:
    /// 1. record closed-once + the raw error;
    /// 2. `cancel_all_pending` with a wrapped "read loop terminated" error;
    /// 3. `stop_waiter.cancel()`;
    /// 4. fire the terminated signal;
    /// 5. return (exit the loop).
    fn recover(&self, err: WireError) {
        // (1) close exactly once. If a second error somehow raced, only the first runs
        // recovery; here a single task drives the loop, so this is always the first.
        let _first = self.close_state.close_once(&err);

        // (2) unblock every pending request with a wrapped closed error. The wrapping
        // mirrors Go's `fmt.Errorf("dap.Client: read loop terminated: %w", err)`; the
        // neutral surface is `BackendError::Closed` (the tool layer maps it to the
        // per-op error string).
        self.shared.cancel_all_pending(BackendError::Closed);

        // (3) cancel the stop waiter so a blocked cont/step unblocks as terminated.
        self.shared.stop_waiter().cancel();

        // (4) fire the terminated lifecycle signal so the session transitions to
        // `terminated` even when the subprocess was killed externally (crash recovery).
        self.sinks.fire_terminated();

        // (5) exit (the caller's `run` returns).
    }
}

fn log_no_waiter(request_seq: i64) {
    // Parity with Go's `log.Printf`; kept minimal and dependency-free (eprintln to the
    // server's stderr, where Go's standard logger also writes).
    eprintln!("dap-client: no waiter for response to request seq {request_seq}");
}

fn log_unhandled(ty: &str, event: Option<&str>, command: Option<&str>) {
    match (event, command) {
        (Some(event), _) => eprintln!("dap-client: unhandled event: {event}"),
        (_, Some(command)) => {
            eprintln!("dap-client: unhandled message type {ty} (command {command})")
        }
        _ => eprintln!("dap-client: unhandled message type: {ty}"),
    }
}

fn log_informational(event: &Event) {
    let name = match event {
        Event::Thread => "thread",
        Event::Breakpoint => "breakpoint",
        Event::Process => "process",
        Event::Continued => "continued",
        Event::Module => "module",
        Event::Capabilities => "capabilities",
        _ => "informational",
    };
    eprintln!("dap-client: informational event: {name}");
}
