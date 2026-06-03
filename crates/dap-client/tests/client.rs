//! Client-core tests — mirrors Go `internal/dap/client_test.go` (the send/correlate set).
//!
//! Driven over the scripted `tokio::io::duplex` peer in `common`. Covers send/await,
//! concurrent sends answered in reversed order (correlation by `request_seq`), the
//! async send, `next_seq` 1..N, drop-on-cancel removing the pending entry (AbortGuard),
//! the AbortGuard-vs-`cancel_all_pending` race, and dispatch-to-no-waiter (no panic).

mod common;

use common::Harness;
use dap_client::{Client, DapMessage, Request, Response};
use debugger_core::BackendError;
use serde_json::json;
use tokio::io::duplex;

/// Build an `initialize` response for a given request seq, mirroring the Go
/// `makeInitializeResponse`.
fn initialize_response(seq: i64, request_seq: i64) -> Response {
    Response {
        seq,
        ty: "response".to_string(),
        request_seq,
        success: true,
        command: "initialize".to_string(),
        message: String::new(),
        body: None,
    }
}

fn initialize_request() -> Request {
    Request::new(
        "initialize",
        Some(json!({"clientID": "test", "adapterID": "lldb-dap"})),
    )
}

/// Extract the request seq from a decoded request frame.
fn request_seq(msg: &DapMessage) -> i64 {
    match msg {
        DapMessage::Other(env) => env.seq,
        other => panic!("expected request envelope, got {other:?}"),
    }
}

#[tokio::test]
async fn send_receive() {
    // Go `TestSendReceive`: Send returns the correlated response.
    let mut h = Harness::new();
    let client = h.client.clone();

    // Drive the peer inline: send concurrently with the peer read/reply.
    let send = client.send(initialize_request());
    let serve = async {
        let req = h.next_request().await;
        let seq = request_seq(&req);
        h.inject(&initialize_response(1, seq)).await;
    };
    let (result, ()) = tokio::join!(send, serve);

    let msg = result.expect("send ok");
    match msg {
        DapMessage::Response(resp) => {
            assert!(resp.success);
            assert_eq!(resp.command, "initialize");
        }
        other => panic!("expected Response, got {other:?}"),
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn send_multiple_concurrent_reversed() {
    // Go `TestSendMultipleConcurrent`: 3 concurrent sends, answered in reverse order,
    // each get their own response (correlation by request_seq).
    let mut h = Harness::new();
    const N: usize = 3;

    let client = h.client.clone();
    let senders = tokio::spawn(async move {
        let mut handles = Vec::new();
        for _ in 0..N {
            let c = client.clone();
            handles.push(tokio::spawn(
                async move { c.send(initialize_request()).await },
            ));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.expect("join"));
        }
        results
    });

    // Peer: read all N requests, collect their seqs, then respond in reverse order.
    let mut seqs = Vec::new();
    for _ in 0..N {
        let req = h.next_request().await;
        seqs.push(request_seq(&req));
    }
    for (i, seq) in seqs.iter().enumerate().rev() {
        h.inject(&initialize_response(100 + i as i64, *seq)).await;
    }

    let results = senders.await.expect("senders join");
    assert_eq!(results.len(), N);
    for r in results {
        let msg = r.expect("send ok");
        assert!(matches!(msg, DapMessage::Response(resp) if resp.success));
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn send_async_resolves() {
    // Go `TestSendAsync`: send_async returns a receiver that gets the response.
    let mut h = Harness::new();

    let rx = h
        .client
        .send_async(initialize_request())
        .await
        .expect("send_async");
    let req = h.next_request().await;
    let seq = request_seq(&req);
    h.inject(&initialize_response(1, seq)).await;

    let result = rx.await.expect("receiver");
    let msg = result.expect("ok");
    assert!(matches!(msg, DapMessage::Response(resp) if resp.success));
    h.close_and_join().await;
}

#[tokio::test]
async fn next_seq_increments_from_one() {
    // Go `TestNextSeqIncrementing`: next_seq yields 1..N.
    let (writer, _peer) = duplex(1024);
    let client = Client::new(writer);
    for i in 1..=10 {
        assert_eq!(client.next_seq(), i);
    }
}

#[tokio::test]
async fn dispatch_response_no_waiter_does_not_panic() {
    // Go `TestDispatchResponseNoWaiter`: dispatch with no pending waiter is a no-op.
    let (writer, _peer) = duplex(1024);
    let client = Client::new(writer);
    let shared = client.shared_for_read_loop();
    // No pending entry for seq 999.
    let dispatched =
        shared.dispatch_response(999, DapMessage::Response(initialize_response(1, 999)));
    assert!(!dispatched, "no waiter ⇒ dispatch returns false (no panic)");
}

#[tokio::test]
async fn cancel_all_pending_fails_every_waiter() {
    // Go `TestCancelAllPending`: 3 pending requests, cancel_all_pending fails them all
    // and clears the map.
    let mut h = Harness::new();
    const N: usize = 3;

    let mut receivers = Vec::new();
    for _ in 0..N {
        let rx = h
            .client
            .send_async(initialize_request())
            .await
            .expect("send_async");
        receivers.push(rx);
        // Drain the request the client wrote so the peer pipe doesn't back-pressure.
        let _ = h.next_request().await;
    }

    let shared = h.client.shared_for_read_loop();
    assert_eq!(shared.pending_len(), N, "all pending registered");

    shared.cancel_all_pending(BackendError::Closed);
    assert_eq!(shared.pending_len(), 0, "map cleared");

    for rx in receivers {
        let result = rx.await.expect("receiver");
        assert!(matches!(result, Err(BackendError::Closed)));
    }
    h.close_and_join().await;
}

#[tokio::test]
async fn dropped_send_removes_pending_entry() {
    // Go `TestSendContextCancellationViaSend`: cancelling/dropping a Send removes its
    // pending entry (the AbortGuard). Spawn the send on a task, wait until the peer has
    // read the request (which only happens after the write + pending registration),
    // then abort the task — dropping the future fires the AbortGuard.
    let mut h = Harness::new();
    let shared = h.client.shared_for_read_loop();
    let client = h.client.clone();

    let send_task = tokio::spawn(async move { client.send(initialize_request()).await });

    // The peer reads the request only after the client wrote it, so by the time this
    // returns the pending entry is registered.
    let _ = h.next_request().await;
    assert_eq!(shared.pending_len(), 1, "entry registered while awaiting");

    // Abort the task: its future is dropped, firing the AbortGuard.
    send_task.abort();
    let _ = send_task.await; // Joins (Err(Cancelled)); the drop has run.

    // The drop fires synchronously on abort completion; poll until the map empties to
    // tolerate the abort/drop scheduling window deterministically.
    wait_until_pending_empty(&shared).await;
    assert_eq!(shared.pending_len(), 0, "pending entry removed on drop");
    h.close_and_join().await;
}

#[tokio::test]
async fn abort_guard_drop_racing_cancel_all_pending_is_safe() {
    // Design R5: an AbortGuard drop racing cancel_all_pending must neither panic nor
    // double-free (idempotent delete). Run many iterations to shake the interleaving;
    // run under TSan for the actual data-race verdict.
    for _ in 0..200 {
        let mut h = Harness::new();
        let shared = h.client.shared_for_read_loop();
        let client = h.client.clone();

        let send_task = tokio::spawn(async move { client.send(initialize_request()).await });
        let _ = h.next_request().await; // Pending entry now registered.

        // Race: cancel_all_pending draining the entry vs the AbortGuard firing when the
        // send future is dropped on abort.
        let shared2 = shared.clone();
        let canceller = tokio::spawn(async move {
            shared2.cancel_all_pending(BackendError::Closed);
        });
        send_task.abort();
        let _ = send_task.await;
        canceller.await.expect("cancel task");

        wait_until_pending_empty(&shared).await;
        assert_eq!(shared.pending_len(), 0, "entry gone exactly once");
        h.close_and_join().await;
    }
}

/// Spin (yielding) until the pending map is empty. Bounded so a real leak still fails
/// the test rather than hanging.
async fn wait_until_pending_empty<W>(shared: &std::sync::Arc<dap_client::Shared<W>>) {
    for _ in 0..1000 {
        if shared.pending_len() == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
}
