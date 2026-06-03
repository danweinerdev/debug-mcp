//! Stop-waiter tests — mirrors Go `internal/dap/stopwaiter_test.go`.
//!
//! The Go `StopResult{Event|Exited|Terminated}` maps onto the neutral `StopOutcome`:
//! `Deliver` → `Stopped(StopInfo)`, `DeliverExit` → `Exited{code}`, `Cancel` →
//! `Terminated`. Each producer is single-shot and a no-op without a waiter.

use dap_client::{StopWaiter, StoppedBody, StoppedEvent};
use debugger_core::StopOutcome;

fn stopped_event(reason: &str, thread_id: i64) -> StoppedEvent {
    StoppedEvent {
        seq: 1,
        ty: "event".to_string(),
        event: "stopped".to_string(),
        body: StoppedBody {
            reason: reason.to_string(),
            thread_id,
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn register_and_deliver() {
    // Go `TestStopWaiterRegisterAndDeliver`.
    let waiter = StopWaiter::new();
    let rx = waiter.register();
    waiter.deliver(&stopped_event("breakpoint", 1));

    let outcome = rx.await.expect("deliver");
    match outcome {
        StopOutcome::Stopped(info) => {
            assert_eq!(info.reason, "breakpoint");
            assert_eq!(info.thread_id, 1);
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[tokio::test]
async fn register_and_deliver_exit() {
    // Go `TestStopWaiterRegisterAndDeliverExit`.
    let waiter = StopWaiter::new();
    let rx = waiter.register();
    waiter.deliver_exit(42);

    match rx.await.expect("deliver_exit") {
        StopOutcome::Exited { code } => assert_eq!(code, Some(42)),
        other => panic!("expected Exited, got {other:?}"),
    }
}

#[tokio::test]
async fn register_and_cancel() {
    // Go `TestStopWaiterRegisterAndCancel`.
    let waiter = StopWaiter::new();
    let rx = waiter.register();
    waiter.cancel();

    assert!(matches!(rx.await.expect("cancel"), StopOutcome::Terminated));
}

#[tokio::test]
async fn deliver_no_waiter_is_noop() {
    // Go `TestStopWaiterDeliverNoWaiter`: no panic, no block.
    let waiter = StopWaiter::new();
    waiter.deliver(&stopped_event("breakpoint", 0));
}

#[tokio::test]
async fn deliver_exit_no_waiter_is_noop() {
    // Go `TestStopWaiterDeliverExitNoWaiter`.
    let waiter = StopWaiter::new();
    waiter.deliver_exit(1);
}

#[tokio::test]
async fn cancel_no_waiter_is_noop() {
    // Go `TestStopWaiterCancelNoWaiter`.
    let waiter = StopWaiter::new();
    waiter.cancel();
}

#[tokio::test]
async fn register_replaces_prior_waiter() {
    // Register replaces any prior waiter (self-healing slot). The first receiver
    // observes a closed channel; only the second is delivered to.
    let waiter = StopWaiter::new();
    let first = waiter.register();
    let second = waiter.register();
    waiter.deliver_exit(7);

    assert!(first.await.is_err(), "replaced waiter's receiver is closed");
    assert!(matches!(
        second.await.expect("second delivered"),
        StopOutcome::Exited { code: Some(7) }
    ));
}

#[tokio::test]
async fn second_deliver_after_delivery_is_noop() {
    // Single-shot: after one delivery the slot is cleared, so a second producer call is
    // a no-op (the receiver already got its one outcome).
    let waiter = StopWaiter::new();
    let rx = waiter.register();
    waiter.deliver(&stopped_event("step", 1));
    waiter.deliver_exit(0); // no-op: slot already cleared.

    assert!(matches!(
        rx.await.expect("one outcome"),
        StopOutcome::Stopped(_)
    ));
}

#[tokio::test]
async fn deliver_to_dropped_receiver_is_noop() {
    // Design R5: oneshot::Sender::send returns Err when the receiver was dropped
    // (cancelled future). That Err is silently discarded — no panic.
    let waiter = StopWaiter::new();
    let rx = waiter.register();
    drop(rx);
    // Delivering to a dropped receiver must not panic.
    waiter.deliver(&stopped_event("breakpoint", 1));
    // A subsequent register+deliver still works (the slot self-heals).
    let rx2 = waiter.register();
    waiter.cancel();
    assert!(matches!(
        rx2.await.expect("cancel"),
        StopOutcome::Terminated
    ));
}

// Go `TestStopWaiterConcurrent`: concurrent Register + Deliver/DeliverExit/Cancel from
// many tasks must be race-clean (run under TSan for the actual data-race check). Use a
// multi-threaded runtime so the operations genuinely interleave across threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_register_and_producers() {
    use std::sync::Arc;

    let waiter = Arc::new(StopWaiter::new());
    const ITERATIONS: usize = 100;

    // Concurrent Register + Deliver.
    let mut handles = Vec::new();
    for _ in 0..ITERATIONS {
        let w1 = Arc::clone(&waiter);
        handles.push(tokio::spawn(async move {
            w1.register();
        }));
        let w2 = Arc::clone(&waiter);
        handles.push(tokio::spawn(async move {
            w2.deliver(&StoppedEvent {
                seq: 1,
                ty: "event".to_string(),
                event: "stopped".to_string(),
                body: StoppedBody {
                    reason: "step".to_string(),
                    ..Default::default()
                },
            });
        }));
    }
    for h in handles.drain(..) {
        h.await.expect("task");
    }

    // Concurrent Register + DeliverExit + Cancel.
    for _ in 0..ITERATIONS {
        let w1 = Arc::clone(&waiter);
        handles.push(tokio::spawn(async move {
            w1.register();
        }));
        let w2 = Arc::clone(&waiter);
        handles.push(tokio::spawn(async move {
            w2.deliver_exit(0);
        }));
        let w3 = Arc::clone(&waiter);
        handles.push(tokio::spawn(async move {
            w3.cancel();
        }));
    }
    for h in handles {
        h.await.expect("task");
    }
}
