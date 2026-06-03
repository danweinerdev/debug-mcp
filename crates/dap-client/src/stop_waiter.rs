//! The single-slot stop waiter (Spec FR-17.8, design §"DAP Client internals").
//!
//! A single caller (an in-flight `cont`/`step`, or `launch`/`attach`'s stop-on-entry
//! wait) registers, then exactly one producer resolves it: [`StopWaiter::deliver`] (a
//! stopped event), [`StopWaiter::deliver_exit`] (the debuggee exited), or
//! [`StopWaiter::cancel`] (terminated / EOF). Each producer is single-shot — it
//! no-ops when no waiter is registered and clears the slot after sending, so exactly
//! one [`StopOutcome`] is delivered per registration.
//!
//! Go origin: `internal/dap/stopwaiter.go` (`StopWaiter` with a buffered(1) channel).
//! The Go `StopResult{Event|Exited|Terminated}` is collapsed here onto the neutral
//! [`StopOutcome`]: a stopped event becomes [`StopOutcome::Stopped`] carrying a
//! [`StopInfo`] built from the DAP body, an exit becomes [`StopOutcome::Exited`], and a
//! cancel becomes [`StopOutcome::Terminated`] (this is where the DAP→neutral mapping
//! the design assigns to the stop waiter happens).

use std::sync::Mutex;

use debugger_core::{StopInfo, StopOutcome};
use tokio::sync::oneshot;

use crate::wire::StoppedEvent;

/// A single-waiter primitive delivering one [`StopOutcome`] per registration.
///
/// `register` replaces any prior slot (self-healing: a cancelled future leaves an
/// orphaned sender that the next register drops — Go `StopWaiter.Register`). The
/// producers ignore a send error from a dropped receiver (the no-waiter / cancelled
/// case is a correct no-op, design risk R5).
#[derive(Debug, Default)]
pub struct StopWaiter {
    slot: Mutex<Option<oneshot::Sender<StopOutcome>>>,
}

impl StopWaiter {
    /// Create an empty stop waiter (no registration).
    pub fn new() -> Self {
        StopWaiter {
            slot: Mutex::new(None),
        }
    }

    /// Register a fresh waiter, returning the receiver that will get exactly one
    /// outcome. Replaces (and drops) any previous registration — the orphaned sender's
    /// receiver, if still held, observes a closed channel.
    pub fn register(&self) -> oneshot::Receiver<StopOutcome> {
        let (tx, rx) = oneshot::channel();
        let mut slot = self.lock();
        *slot = Some(tx);
        rx
    }

    /// Deliver a stopped event (Spec FR-17.8). No-op when no waiter is registered;
    /// clears the slot after sending. The DAP `StoppedEvent` body is translated into a
    /// neutral [`StopInfo`] here.
    pub fn deliver(&self, event: &StoppedEvent) {
        self.send(StopOutcome::Stopped(stop_info_from_event(event)));
    }

    /// Deliver an exit with the given code. No-op without a waiter; clears the slot.
    pub fn deliver_exit(&self, exit_code: i64) {
        self.send(StopOutcome::Exited {
            code: Some(exit_code),
        });
    }

    /// Resolve the waiter as terminated (TerminatedEvent or EOF). No-op without a
    /// waiter; clears the slot.
    pub fn cancel(&self) {
        self.send(StopOutcome::Terminated);
    }

    /// Common single-shot send: take the registered sender (clearing the slot) and
    /// send, discarding a send error from a dropped receiver (Go's "no waiter ⇒
    /// no-op"; design risk R5). The lock is released before `send` so a delivery never
    /// holds the slot lock across the channel hand-off.
    fn send(&self, outcome: StopOutcome) {
        let sender = self.lock().take();
        if let Some(sender) = sender {
            // A dropped receiver (cancelled future) makes this `Err`; that is the
            // intended no-op, exactly matching Go discarding the send when the waiter
            // is gone.
            let _ = sender.send(outcome);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<oneshot::Sender<StopOutcome>>> {
        // The only panic point under this lock is `oneshot::Sender::send`, which does
        // not panic; so the mutex cannot be poisoned in practice. Recover the guard if
        // it ever were, rather than propagating a poison panic into the read loop.
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Translate a DAP `StoppedEvent` body into the neutral [`StopInfo`] the seam speaks.
fn stop_info_from_event(event: &StoppedEvent) -> StopInfo {
    StopInfo {
        reason: event.body.reason.clone(),
        thread_id: event.body.thread_id,
        description: event.body.description.clone(),
        hit_breakpoint_ids: event.body.hit_breakpoint_ids.clone(),
    }
}
