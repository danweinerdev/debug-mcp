//! R2 — event-stream tests driving [`build_event_stream`](crate::factory::build_event_stream)
//! directly over hand-made channels (no live engine, no COM, no KDNET). These prove the
//! orphaned-kernel-thread pump fix: the merged stream emits Output until a terminate trigger, then
//! exactly ONE `Terminated`, then ENDS — where the trigger is the FIRST of the engine `term_rx`
//! (real exit code) or the backend-drop `drop_rx` (synthetic `Terminated { None }`).
//!
//! A live KDNET orphan test is NOT feasible (no VM; it would hang in `WaitForEvent(INFINITE)`), so
//! these channel-level tests are the proof of the pump fix. They use only tokio channels, so they
//! exercise the exact production stream logic with no engine.

use dbgeng_sys::OutputKind;
use debugger_core::BackendEvent;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use crate::factory::build_event_stream;

/// R2 core proof — synthetic Terminated on backend drop. Simulate the orphaned engine thread: the
/// engine `term_rx` NEVER fires AND `out_rx` stays open (the orphan still owns the output sink). Then
/// DROP the `drop_tx` (the backend's `_drop_signal` dropping). The stream must yield
/// `Terminated { code: None }` and then END — without this the session's event-pump would hang
/// forever waiting for a stream end that the orphan never produces.
#[tokio::test]
async fn drop_signal_forces_synthetic_terminated_and_ends() {
    let (_out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
    let (_term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut events = build_event_stream(out_rx, term_rx, drop_rx);

    // The orphan is stuck: neither `term_rx` fires nor `out_rx` closes. Drop the backend's drop
    // signal (its `_drop_signal` going away) — this is the only thing that can end the pump.
    // `_out_tx` and `_term_tx` are intentionally kept alive (orphan still owns the sink / never
    // fires term) to model the stuck thread precisely.
    drop(drop_tx);

    let ev = events.next().await.expect("a synthetic Terminated on drop");
    assert_eq!(
        ev,
        BackendEvent::Terminated { code: None },
        "dropping the backend forces a synthetic Terminated with no exit code"
    );
    assert!(
        events.next().await.is_none(),
        "the stream ENDS after the synthetic Terminated (the pump completes)"
    );
}

/// Normal-path regression — the engine fires `term_rx` with a real exit code. The stream must yield
/// exactly ONE `Terminated { code: Some(0) }` then end, and a SUBSEQUENT `drop_tx` drop must NOT
/// produce a second Terminated (no double-Terminated on the normal teardown path).
#[tokio::test]
async fn term_rx_real_code_then_drop_does_not_double_terminate() {
    let (_out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
    let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut events = build_event_stream(out_rx, term_rx, drop_rx);

    // The engine reports a clean exit code 0 (the normal teardown winning the race).
    term_tx.send(Some(0)).expect("send real exit code");

    let ev = events.next().await.expect("the real Terminated");
    assert_eq!(
        ev,
        BackendEvent::Terminated { code: Some(0) },
        "the engine's real exit code wins the race"
    );

    // The backend drops slightly later (clean disconnect path). This must be harmless: the stream
    // already ended, so there is NO second Terminated.
    drop(drop_tx);
    assert!(
        events.next().await.is_none(),
        "the stream already ended; a later backend drop yields NO second Terminated"
    );
}

/// Output-then-terminate ordering — a couple of Output events flow through `out_rx`, then `term_rx`
/// fires. The stream must yield the Output events in order, then exactly one Terminated, then end.
#[tokio::test]
async fn output_events_then_terminate() {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
    let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (_drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut events = build_event_stream(out_rx, term_rx, drop_rx);

    out_tx
        .send((OutputKind::Normal, "hello\n".to_string()))
        .expect("send output 1");
    out_tx
        .send((OutputKind::Error, "warn\n".to_string()))
        .expect("send output 2");

    let e1 = events.next().await.expect("first output");
    assert_eq!(
        e1,
        BackendEvent::Output {
            category: "stdout".to_string(),
            text: "hello\n".to_string(),
        }
    );
    let e2 = events.next().await.expect("second output");
    assert_eq!(
        e2,
        BackendEvent::Output {
            category: "stderr".to_string(),
            text: "warn\n".to_string(),
        }
    );

    // Now the engine terminates with exit code 3.
    term_tx.send(Some(3)).expect("send exit code");

    let term = events.next().await.expect("the Terminated event");
    assert_eq!(
        term,
        BackendEvent::Terminated { code: Some(3) },
        "after the outputs, exactly one Terminated with the real code"
    );
    assert!(
        events.next().await.is_none(),
        "the stream ends after the single Terminated"
    );
}

/// Fix 2 (biased drain) — buffered output is drained BEFORE Terminated when both are ready. We
/// pre-load `out_rx` with output AND fire `term_rx` BEFORE the first poll, so the unfold's
/// `select!` sees both branches ready on the very first poll. With `biased;` (output checked
/// first) every buffered line must be yielded before the single Terminated, so NO buffered output
/// is dropped in favor of the terminal event. Without `biased`, `select!`'s pseudo-random fairness
/// could emit Terminated first and lose the buffered lines.
#[tokio::test]
async fn buffered_output_drains_before_terminated_when_both_ready() {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
    let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (_drop_tx, drop_rx) = oneshot::channel::<()>();

    // Buffer two output lines and fire termination, all BEFORE building/polling the stream — so the
    // first poll of each `select!` arm finds BOTH `out_rx.recv()` and `term_rx` ready.
    out_tx
        .send((OutputKind::Normal, "line1\n".to_string()))
        .expect("buffer output 1");
    out_tx
        .send((OutputKind::Normal, "line2\n".to_string()))
        .expect("buffer output 2");
    term_tx.send(Some(7)).expect("fire termination");

    let mut events = build_event_stream(out_rx, term_rx, drop_rx);

    // Both buffered lines come out first (biased toward `out_rx`), in order.
    assert_eq!(
        events.next().await.expect("first buffered output"),
        BackendEvent::Output {
            category: "stdout".to_string(),
            text: "line1\n".to_string(),
        },
        "the first buffered line is drained, not dropped for Terminated"
    );
    assert_eq!(
        events.next().await.expect("second buffered output"),
        BackendEvent::Output {
            category: "stdout".to_string(),
            text: "line2\n".to_string(),
        },
        "the second buffered line is drained before Terminated"
    );

    // Only AFTER the buffer is empty does the single Terminated (with the real code) appear.
    assert_eq!(
        events.next().await.expect("Terminated after the drain"),
        BackendEvent::Terminated { code: Some(7) },
        "Terminated is emitted only once the buffered output is fully drained"
    );
    assert!(
        events.next().await.is_none(),
        "the stream ends after the single Terminated"
    );
}

/// A dropped `term_rx` SENDER (the engine thread exited without an explicit code) with `out_rx` also
/// closed resolves the terminate race with `None` — the normal channel-close teardown. Proves the
/// stream still ends with a single Terminated when neither an explicit code nor a backend drop fires
/// but the engine thread's senders all go away (the clean spawn-wrapper teardown path).
#[tokio::test]
async fn dropped_term_sender_and_closed_output_terminate_with_none() {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<(OutputKind, String)>();
    let (term_tx, term_rx) = oneshot::channel::<Option<i64>>();
    let (_drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut events = build_event_stream(out_rx, term_rx, drop_rx);

    // The engine thread ends: it drops the output sink (closing out_rx) and the term sender without
    // a value (the spawn wrapper fires `term.send(None)` in production; a bare drop is the harsher
    // case and must still terminate).
    drop(out_tx);
    drop(term_tx);

    let ev = events.next().await.expect("a Terminated on clean teardown");
    assert_eq!(
        ev,
        BackendEvent::Terminated { code: None },
        "a dropped term sender + closed output ends the stream with a None-code Terminated"
    );
    assert!(
        events.next().await.is_none(),
        "the stream ends after the single Terminated"
    );
}
