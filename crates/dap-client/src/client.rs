//! The DAP client core (Spec FR-17.2–17.5, design Decision 4, task 2.2).
//!
//! Owns the write half (behind a `tokio::sync::Mutex`, serializing frame writes —
//! Go's `writeMu`), the sequence counter (`AtomicI64`, pre-increment, first seq `1`),
//! and the pending map (`HashMap<i64, oneshot::Sender<Result<DapMessage,
//! BackendError>>>` keyed by request seq). [`Client::send`]/[`Client::send_async`]
//! correlate a response to its request by `request_seq`; [`Client::send_and_await_both`]
//! awaits a response and the `InitializedEvent` concurrently, order-independent (the
//! launch/attach dual-await, Spec FR-17.5).
//!
//! Cancel-safety: dropping a [`Client::send`] future removes its pending entry via an
//! [`AbortGuard`] (design risk R5) — idempotent, and it never holds the pending-map
//! lock across its body, so it cannot deadlock against [`Shared::cancel_all_pending`].
//!
//! Go origin: `internal/dap/client.go` (`nextSeq`, `Send`, `SendAsync`,
//! `dispatchResponse`, `cancelAllPending`, `InitializedChan`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use debugger_core::BackendError;
use tokio::io::AsyncWrite;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::error::WireError;
use crate::stop_waiter::StopWaiter;
use crate::wire::{write_message, DapMessage, Request};

/// The result delivered to a pending waiter: the correlated response, or a transport
/// error (write failure rollback never reaches the channel; EOF delivers
/// [`BackendError::Closed`]).
type PendingResult = Result<DapMessage, BackendError>;

/// Shared client state, held by an `Arc` so the read-loop task and every in-flight
/// send share one view of the pending map, seq, stop waiter, and initialized signal.
///
/// The writer `W` is the subprocess stdin half (or a test peer). It is generic so tests
/// can drive a `tokio::io::duplex` peer; production uses `tokio::process::ChildStdin`.
///
/// Obtain the `Arc<Shared<W>>` for the read loop via [`Client::shared_for_read_loop`].
pub struct Shared<W> {
    writer: AsyncMutex<W>,
    seq: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<PendingResult>>>,
    stop_waiter: StopWaiter,
    /// Capacity-1 signal for the `InitializedEvent` (Spec FR-17.6): a second signal is
    /// dropped, matching Go's buffered(1) `initializedChan` + non-blocking send.
    initialized: Mutex<InitializedSignal>,
}

/// Single-slot, capacity-1 `InitializedEvent` signal. The read loop sets `fired` and
/// wakes a parked waiter; a second fire is a no-op (drop), matching Go's non-blocking
/// send onto a buffered(1) channel.
#[derive(Default)]
struct InitializedSignal {
    fired: bool,
    waiter: Option<oneshot::Sender<()>>,
}

impl<W> Shared<W> {
    fn new(writer: W) -> Self {
        Shared {
            writer: AsyncMutex::new(writer),
            seq: AtomicI64::new(0),
            pending: Mutex::new(HashMap::new()),
            stop_waiter: StopWaiter::new(),
            initialized: Mutex::new(InitializedSignal::default()),
        }
    }

    /// Pre-increment the sequence counter; the first value returned is `1`
    /// (Spec FR-17.2). Go origin: `client.go:nextSeq` (`c.seq++; return c.seq`).
    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Register a pending waiter for `seq`, returning its receiver.
    fn register_pending(&self, seq: i64) -> oneshot::Receiver<PendingResult> {
        let (tx, rx) = oneshot::channel();
        self.lock_pending().insert(seq, tx);
        rx
    }

    /// Remove a pending entry (idempotent). Used by [`AbortGuard`] on cancel and by
    /// [`Shared::cancel_all_pending`] indirectly. Returns the sender if present.
    fn remove_pending(&self, seq: i64) -> Option<oneshot::Sender<PendingResult>> {
        self.lock_pending().remove(&seq)
    }

    /// Dispatch a response to its pending waiter, keyed by `request_seq`. No waiter ⇒
    /// no-op (returns `false`, logged by the read loop), never panics (Go
    /// `dispatchResponse`).
    pub fn dispatch_response(&self, seq: i64, message: DapMessage) -> bool {
        if let Some(tx) = self.remove_pending(seq) {
            // A dropped receiver (caller's future cancelled between dispatch and the
            // AbortGuard winning the lock) makes this `Err`; discard it — the entry is
            // already gone from the caller's perspective.
            let _ = tx.send(Ok(message));
            true
        } else {
            false
        }
    }

    /// Fail every pending request with the given error and clear the map (Go
    /// `cancelAllPending`). Called on EOF/read error. Idempotent against a concurrent
    /// [`AbortGuard`] drop: the guard's `remove_pending` and this both go through the
    /// same lock, so each entry is taken exactly once.
    pub fn cancel_all_pending(&self, err: BackendError) {
        // Drain under the lock, then send outside it so a slow receiver can't hold the
        // pending lock. `BackendError` is not `Clone`, so re-create the closed error
        // per waiter (every cancel uses the same closed cause).
        let drained: Vec<oneshot::Sender<PendingResult>> = {
            let mut pending = self.lock_pending();
            pending.drain().map(|(_, tx)| tx).collect()
        };
        for tx in drained {
            let _ = tx.send(Err(clone_backend_error(&err)));
        }
    }

    /// The stop waiter (Spec FR-17.8), shared with the read loop.
    pub(crate) fn stop_waiter(&self) -> &StopWaiter {
        &self.stop_waiter
    }

    /// Fire the `InitializedEvent` signal (capacity-1, non-blocking; a second fire is
    /// dropped). Go origin: the non-blocking send onto `initializedChan` in the read
    /// loop's `InitializedEvent` case.
    pub(crate) fn signal_initialized(&self) {
        let mut sig = self.lock_initialized();
        sig.fired = true;
        if let Some(waiter) = sig.waiter.take() {
            let _ = waiter.send(());
        }
    }

    /// Await the `InitializedEvent`. Resolves immediately if it already fired (the
    /// capacity-1 latch), otherwise parks until [`Shared::signal_initialized`].
    ///
    /// A prior parked waiter is replaced (its receiver, if dropped, simply never
    /// resolves); only the launch/attach dual-await uses this, and it registers at most
    /// one waiter at a time.
    async fn wait_initialized(&self) {
        let rx = {
            let mut sig = self.lock_initialized();
            if sig.fired {
                return;
            }
            let (tx, rx) = oneshot::channel();
            sig.waiter = Some(tx);
            rx
        };
        // If the sender is dropped without firing (cannot happen in the dual-await,
        // which holds the Arc), treat it as never-resolving by awaiting the receiver;
        // a closed channel yields `Err`, which we map to "resolved" so the caller's
        // `select!` is not left dangling on a logic bug.
        let _ = rx.await;
    }

    /// The number of currently-registered pending requests. Mirrors the Go tests'
    /// direct `len(client.pending)` assertions (`TestCancelAllPending`).
    pub fn pending_len(&self) -> usize {
        self.lock_pending().len()
    }

    /// True if a waiter is registered for `seq`. Mirrors the Go tests' direct pending-map
    /// lookup after cancellation (`TestSendContextCancellationViaSend`).
    pub fn has_pending(&self, seq: i64) -> bool {
        self.lock_pending().contains_key(&seq)
    }

    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<PendingResult>>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_initialized(&self) -> std::sync::MutexGuard<'_, InitializedSignal> {
        self.initialized
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<W> Shared<W>
where
    W: AsyncWrite + Unpin,
{
    /// Write a framed request onto the wire, serializing concurrent writes via the
    /// write mutex (Go's `writeMu`).
    async fn write_request(&self, request: &Request) -> Result<(), WireError> {
        let mut writer = self.writer.lock().await;
        write_message(&mut *writer, request).await
    }
}

/// A DAP protocol client correlating requests to responses by sequence number
/// (Spec FR-17). Construct with [`Client::new`], then drive the read side with a
/// [`crate::ReadLoop`] on a spawned task.
pub struct Client<W> {
    shared: Arc<Shared<W>>,
}

impl<W> Clone for Client<W> {
    fn clone(&self) -> Self {
        Client {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<W> Client<W>
where
    W: AsyncWrite + Unpin,
{
    /// Create a client over the given writer (the subprocess stdin or a test peer).
    /// The matching reader is consumed by the read loop separately.
    pub fn new(writer: W) -> Self {
        Client {
            shared: Arc::new(Shared::new(writer)),
        }
    }

    /// The shared state to hand to [`crate::ReadLoop::new`]. The read loop and the
    /// client share one `Shared` via the `Arc` so dispatch reaches the right pending
    /// waiters and stop waiter. Tests also use it to assert pending-map size / dispatch
    /// directly (mirroring the Go tests that poke `client.pending`).
    pub fn shared_for_read_loop(&self) -> Arc<Shared<W>> {
        Arc::clone(&self.shared)
    }

    /// The stop waiter (Spec FR-17.8). Callers register *before* sending a resume
    /// request (Go `launch.go:304`).
    pub fn stop_waiter(&self) -> &StopWaiter {
        self.shared.stop_waiter()
    }

    /// The next sequence number that would be assigned (pre-increment, first `1`).
    /// Exposed for the parity test mirroring `TestNextSeqIncrementing`.
    pub fn next_seq(&self) -> i64 {
        self.shared.next_seq()
    }

    /// Await the `InitializedEvent` (the capacity-1 latch). Resolves immediately if it
    /// already fired, otherwise parks until the read loop signals it (Go's
    /// `InitializedChan()`).
    ///
    /// This is the decoupled half of [`Client::send_and_await_both`]: some adapters (real
    /// lldb-dap) defer the launch/attach **response** until *after* `configurationDone`,
    /// while delivering `initialized` right after the request — so a backend that must
    /// send `configurationDone` to unblock the launch response cannot block on that
    /// response first. It instead sends the request with [`Client::send_async`], awaits
    /// `initialized` here (which gates configuration), runs `configurationDone`, then
    /// awaits the deferred response. (`send_and_await_both` remains for adapters that
    /// answer the request before `configurationDone`.)
    pub async fn wait_initialized(&self) {
        self.shared.wait_initialized().await;
    }

    /// Send a request without blocking on the response: assign a seq, register the
    /// pending waiter, write the frame, and return the receiver. On a write error the
    /// pending entry is rolled back (Go `SendAsync`). The caller stamps nothing — the
    /// client owns seq assignment.
    pub async fn send_async(
        &self,
        request: Request,
    ) -> Result<oneshot::Receiver<PendingResult>, BackendError> {
        let (rx, _seq) = self.send_async_with_seq(request).await?;
        Ok(rx)
    }

    /// Like [`Client::send_async`] but also returns the assigned seq, so a caller that
    /// needs cancel-safety (the dual-await) can arm a precise [`AbortGuard`].
    async fn send_async_with_seq(
        &self,
        mut request: Request,
    ) -> Result<(oneshot::Receiver<PendingResult>, i64), BackendError> {
        let seq = self.shared.next_seq();
        request.set_seq(seq);

        let rx = self.shared.register_pending(seq);

        if let Err(err) = self.shared.write_request(&request).await {
            // Roll back the pending entry on write failure (Go `SendAsync` delete).
            self.shared.remove_pending(seq);
            return Err(BackendError::Send(err.to_string()));
        }

        Ok((rx, seq))
    }

    /// Send a request and block until the correlated response (or a transport error).
    /// There is no internal timeout — the caller's `select!`/cancellation bounds it
    /// (Spec FR-17.4). Dropping this future removes the pending entry via [`AbortGuard`].
    pub async fn send(&self, request: Request) -> Result<DapMessage, BackendError> {
        let seq = self.shared.next_seq();
        let mut request = request;
        request.set_seq(seq);

        let rx = self.shared.register_pending(seq);

        // Arm the abort guard *before* the write: if this future is dropped after the
        // entry is registered but before/while writing, the guard removes it.
        let guard = AbortGuard::new(Arc::clone(&self.shared), seq);

        if let Err(err) = self.shared.write_request(&request).await {
            // The guard removes the entry on drop; explicitly disarm-then-remove is the
            // same effect, but let the guard handle it for one code path.
            drop(guard);
            return Err(BackendError::Send(err.to_string()));
        }

        // Await the response. If this future is dropped here (cancellation), the guard's
        // Drop removes the still-registered pending entry. On normal resolution we
        // disarm the guard so it does not redundantly touch the (now-removed) entry.
        let result = match rx.await {
            Ok(result) => result,
            // The sender was dropped without sending. This happens only if the shared
            // state is being torn down; surface it as a closed transport.
            Err(_) => Err(BackendError::Closed),
        };
        guard.disarm();
        result
    }

    /// Send a request and await **both** its response and the `InitializedEvent`,
    /// order-independent (Spec FR-17.5 — the launch/attach dual-await). Returns the
    /// response; the initialized signal is consumed as a side effect.
    ///
    /// Go origin: `launch.go`/`attach.go` selecting over the response channel and
    /// `InitializedChan()`. Both must arrive; order is version-dependent.
    pub async fn send_and_await_both(&self, request: Request) -> Result<DapMessage, BackendError> {
        let (rx, seq) = self.send_async_with_seq(request).await?;

        // Guard the pending entry for the duration of the dual-await: if this future is
        // dropped, remove the still-registered entry.
        let guard = AbortGuard::new(Arc::clone(&self.shared), seq);

        let response = {
            let init_fut = self.shared.wait_initialized();
            let resp_fut = async {
                match rx.await {
                    Ok(result) => result,
                    Err(_) => Err(BackendError::Closed),
                }
            };
            tokio::pin!(init_fut);
            tokio::pin!(resp_fut);

            // Drive both to completion in any order. We need the response value and we
            // need initialized to have fired; loop until both are satisfied.
            let mut response: Option<PendingResult> = None;
            let mut initialized_done = false;
            while response.is_none() || !initialized_done {
                tokio::select! {
                    r = &mut resp_fut, if response.is_none() => {
                        response = Some(r);
                    }
                    () = &mut init_fut, if !initialized_done => {
                        initialized_done = true;
                    }
                }
            }
            response.expect("loop exits only once response is Some")
        };

        guard.disarm();
        response
    }
}

/// Removes a pending entry when dropped, giving [`Client::send`] cancel-safety: a
/// dropped send future cleans up its registration so it is not delivered to later.
/// Idempotent (the entry may already be gone via `cancel_all_pending` or dispatch).
///
/// Design risk R5: the guard is `Send`, never holds the pending-map lock across its
/// own body (it acquires it briefly inside `remove_pending`), and tolerates the entry
/// already being absent. Disarming via [`AbortGuard::disarm`] suppresses the removal on
/// the success path so a delivered response is not double-handled.
struct AbortGuard<W> {
    shared: Arc<Shared<W>>,
    seq: i64,
    armed: bool,
}

impl<W> AbortGuard<W> {
    fn new(shared: Arc<Shared<W>>, seq: i64) -> Self {
        AbortGuard {
            shared,
            seq,
            armed: true,
        }
    }

    /// Suppress the on-drop removal (the request resolved normally).
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl<W> Drop for AbortGuard<W> {
    fn drop(&mut self) {
        if self.armed {
            // Idempotent: `remove_pending` returns `None` if `cancel_all_pending` or a
            // dispatch already took the entry. No double-free, no panic (design R5).
            self.shared.remove_pending(self.seq);
        }
    }
}

/// `BackendError` is not `Clone`, but `cancel_all_pending` must hand the same logical
/// cause to every waiter. Re-create the closed/transport cause per waiter. Only the
/// closed-transport path uses this, so reproducing [`BackendError::Closed`] (and a
/// faithful copy of the other transport variants, for completeness) is sufficient.
fn clone_backend_error(err: &BackendError) -> BackendError {
    match err {
        BackendError::Closed => BackendError::Closed,
        BackendError::Timeout => BackendError::Timeout,
        BackendError::Detect(m) => BackendError::Detect(m.clone()),
        BackendError::Spawn(m) => BackendError::Spawn(m.clone()),
        BackendError::Send(m) => BackendError::Send(m.clone()),
        BackendError::Protocol { ty } => BackendError::Protocol { ty: ty.clone() },
        BackendError::Dap { message } => BackendError::Dap {
            message: message.clone(),
        },
    }
}
