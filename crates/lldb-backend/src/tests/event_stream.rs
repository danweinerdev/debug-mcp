//! Event-stream adaptation tests (task 3.4): the read loop's output/terminated channels
//! become a neutral `BackendEvent` stream surfacing `Output{category,text}` and
//! `Terminated{code}`. Needs the crate-internal `factory::build_event_stream`, so it
//! lives here rather than in `tests/`.

use dap_client::{write_message, Client, ReadLoop};
use debugger_core::BackendEvent;
use futures::StreamExt;
use tokio::io::{duplex, BufReader, DuplexStream};

use crate::factory::build_event_stream;

/// Build a client + read loop over a duplex peer, returning the event stream and the
/// peer's writer for injecting output/exited/terminated frames.
fn wire() -> (
    futures::stream::BoxStream<'static, BackendEvent>,
    DuplexStream,
    tokio::task::JoinHandle<()>,
) {
    // req direction is unused (no requests are sent in these tests) but the client needs
    // a writer; give it a throwaway duplex half.
    let (req_client, _req_peer) = duplex(1024);
    let (resp_peer, resp_client) = duplex(64 * 1024);
    let client: Client<DuplexStream> = Client::new(req_client);
    let (read_loop, channels) =
        ReadLoop::new(BufReader::new(resp_client), client.shared_for_read_loop());
    let handle = tokio::spawn(read_loop.run());
    // Keep the client alive for the lifetime of the read loop (it holds the shared state);
    // leak it into the returned closure by dropping here is fine — `shared_for_read_loop`
    // already cloned the Arc into the read loop.
    drop(client);
    let events = build_event_stream(channels);
    (events, resp_peer, handle)
}

#[tokio::test]
async fn output_events_surface_as_output() {
    let (mut events, mut peer, handle) = wire();

    // Inject two output events.
    write_message(
        &mut peer,
        &serde_json::json!({
            "seq": 0, "type": "event", "event": "output",
            "body": {"category": "stdout", "output": "hello\n"}
        }),
    )
    .await
    .unwrap();
    write_message(
        &mut peer,
        &serde_json::json!({
            "seq": 0, "type": "event", "event": "output",
            "body": {"category": "stderr", "output": "warn\n"}
        }),
    )
    .await
    .unwrap();

    let e1 = events.next().await.expect("first event");
    assert_eq!(
        e1,
        BackendEvent::Output {
            category: "stdout".to_string(),
            text: "hello\n".to_string()
        }
    );
    let e2 = events.next().await.expect("second event");
    assert_eq!(
        e2,
        BackendEvent::Output {
            category: "stderr".to_string(),
            text: "warn\n".to_string()
        }
    );

    drop(peer);
    let _ = handle.await;
}

#[tokio::test]
async fn terminated_event_surfaces_as_terminated_with_exit_code() {
    let (mut events, mut peer, handle) = wire();

    // An ExitedEvent records the code; the TerminatedEvent fires the terminated signal,
    // which carries the last exit code.
    write_message(
        &mut peer,
        &serde_json::json!({
            "seq": 0, "type": "event", "event": "exited", "body": {"exitCode": 42}
        }),
    )
    .await
    .unwrap();
    write_message(
        &mut peer,
        &serde_json::json!({"seq": 0, "type": "event", "event": "terminated"}),
    )
    .await
    .unwrap();

    // The Terminated event must surface carrying the recorded exit code. The exited
    // event alone produces no BackendEvent — exit is delivered to the stop waiter, not
    // the async stream — so the first (and only) event here is Terminated. We stop on it
    // rather than draining to stream end (the read loop keeps running until EOF, so its
    // output sender — and thus the stream — stays open past TerminatedEvent).
    let event = events.next().await.expect("a terminated event");
    assert_eq!(event, BackendEvent::Terminated { code: Some(42) });

    drop(peer);
    let _ = handle.await;
}

#[tokio::test]
async fn eof_surfaces_as_terminated_without_code() {
    // Dropping the peer (EOF) runs the read loop's recovery, which fires the terminated
    // signal with no exit code → BackendEvent::Terminated { code: None }.
    let (mut events, peer, handle) = wire();
    drop(peer);

    let mut saw_terminated = None;
    while let Some(e) = events.next().await {
        if let BackendEvent::Terminated { code } = e {
            saw_terminated = Some(code);
        }
    }
    assert_eq!(saw_terminated, Some(None));
    let _ = handle.await;
}
