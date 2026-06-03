//! Shared scripted-peer harness for the client/read-loop tests — the `tokio::io::duplex`
//! analog of the Go tests' `io.Pipe` fakes.
//!
//! Two duplex pairs model the two directions: one carries the client's *requests* to
//! the peer, the other carries the peer's *responses/events* to the client's read loop.
//! The peer half lets a test read the requests the client wrote and inject scripted
//! responses/events, making every ordering deterministic.

use dap_client::{read_message, write_message, Client, DapMessage, ReadLoop, ReadLoopChannels};
use tokio::io::{duplex, BufReader, DuplexStream};

/// One end of a scripted DAP peer wired to a [`Client`] + its read loop.
pub struct Harness {
    /// The client under test (clone-able handle to the shared state).
    pub client: Client<DuplexStream>,
    /// The read-loop channels (`output` / `terminated`), for assertions.
    pub channels: ReadLoopChannels,
    /// The peer's view of the client's request stream (read requests the client sent).
    pub peer_reads: BufReader<DuplexStream>,
    /// The peer's writer for injecting responses/events into the client's read loop.
    pub peer_writes: DuplexStream,
    /// Join handle for the spawned read loop task.
    pub read_loop: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Build the harness and spawn the read loop. The read loop runs until the response
    /// channel closes (drop [`Harness::peer_writes`] to trigger EOF).
    pub fn new() -> Self {
        // Client writes requests into `req_client`; the peer reads them from `req_peer`.
        let (req_client, req_peer) = duplex(64 * 1024);
        // The peer writes responses/events into `resp_peer`; the read loop reads them
        // from `resp_client`.
        let (resp_peer, resp_client) = duplex(64 * 1024);

        let client = Client::new(req_client);
        let (read_loop, channels) =
            ReadLoop::new(BufReader::new(resp_client), client.shared_for_read_loop());
        let handle = tokio::spawn(read_loop.run());

        Harness {
            client,
            channels,
            peer_reads: BufReader::new(req_peer),
            peer_writes: resp_peer,
            read_loop: handle,
        }
    }

    /// Read the next request the client sent (decoded as an `Other` envelope, since
    /// requests are not modeled as concrete by the client).
    pub async fn next_request(&mut self) -> DapMessage {
        read_message(&mut self.peer_reads)
            .await
            .expect("peer read request")
    }

    /// Inject a response/event frame into the client's read loop.
    pub async fn inject<T: serde::Serialize>(&mut self, message: &T) {
        write_message(&mut self.peer_writes, message)
            .await
            .expect("peer inject");
    }

    /// Trigger EOF on the read loop by dropping the peer's response writer, then wait
    /// for the read loop task to finish its recovery and exit.
    pub async fn close_and_join(self) {
        // Name every field in the destructure (no `..`) so a test binary that never
        // reads `channels`/`client`/`peer_reads` still counts them as used here — this
        // keeps the harness warning-clean across every binary without an `#[allow]`.
        let Harness {
            client,
            channels,
            peer_reads,
            peer_writes,
            read_loop,
        } = self;
        drop(client);
        drop(channels);
        drop(peer_reads);
        drop(peer_writes);
        read_loop.await.expect("read loop joins after EOF");
    }
}
