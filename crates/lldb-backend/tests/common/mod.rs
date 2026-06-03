//! Scripted-peer harness for the lldb-backend handshake/op tests — the lldb analog of
//! the dap-client `common::Harness`, reusing the same `tokio::io::duplex` pattern.
//!
//! It wires a real [`LldbBackend`] over a `dap-client::Client` whose read loop reads a
//! scripted peer's responses/events, and gives the test the peer's view: read the
//! requests the backend sent, inject scripted responses/events back. A handshake/op
//! method (which blocks on responses) runs concurrently with a peer-scripting future via
//! `tokio::join!`.

use dap_client::{write_message, Client, ReadLoop, Response, StoppedBody, StoppedEvent};
use lldb_backend::LldbBackend;
use serde_json::Value;
use tokio::io::{duplex, BufReader, DuplexStream};

/// The backend under test plus the scripted peer. The two are returned as separate
/// values from [`Harness::new`] so a blocking method (`backend.launch(...)`) and the
/// peer-scripting future borrow disjoint state and can run under one `tokio::join!`.
pub struct Harness {
    /// The peer's view of the backend's request stream.
    pub peer_reads: BufReader<DuplexStream>,
    /// The peer's writer for injecting responses/events into the backend's read loop.
    pub peer_writes: DuplexStream,
    /// Join handle for the spawned read loop task.
    pub read_loop: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// Build the backend + its scripted peer. The backend owns the `dap-client::Client`;
    /// the [`Harness`] owns the peer side and the read-loop task.
    pub fn new(is_lldb_dap: bool) -> (LldbBackend<DuplexStream>, Harness) {
        let (req_client, req_peer) = duplex(64 * 1024);
        let (resp_peer, resp_client) = duplex(64 * 1024);

        let client = Client::new(req_client);
        let (read_loop, channels) =
            ReadLoop::new(BufReader::new(resp_client), client.shared_for_read_loop());
        let handle = tokio::spawn(read_loop.run());
        // The event stream is not needed for handshake/op assertions; drop it (the read
        // loop still drains output into its mpsc, which is fine).
        drop(channels);

        let backend = LldbBackend::new(client, is_lldb_dap, None);

        let harness = Harness {
            peer_reads: BufReader::new(req_peer),
            peer_writes: resp_peer,
            read_loop: handle,
        };
        (backend, harness)
    }

    /// Read the next request and return `(command, seq, arguments)`.
    pub async fn next_request_full(&mut self) -> (String, i64, Value) {
        let raw = read_raw_request(&mut self.peer_reads).await;
        let command = raw
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let seq = raw.get("seq").and_then(Value::as_i64).unwrap_or_default();
        let arguments = raw.get("arguments").cloned().unwrap_or(Value::Null);
        (command, seq, arguments)
    }

    /// Inject a response/event frame into the backend's read loop.
    pub async fn inject<T: serde::Serialize>(&mut self, message: &T) {
        write_message(&mut self.peer_writes, message)
            .await
            .expect("peer inject");
    }

    /// Inject a raw JSON value as a framed message (for events/responses the typed
    /// helpers don't cover, e.g. an `initialized` event or a response with a custom body).
    pub async fn inject_value(&mut self, value: &Value) {
        write_message(&mut self.peer_writes, value)
            .await
            .expect("peer inject value");
    }

    /// Reply with a successful response for the given request `command`/`seq`, carrying
    /// an optional body.
    pub async fn reply_ok(&mut self, command: &str, request_seq: i64, body: Option<Value>) {
        self.inject(&ok_response(command, request_seq, body)).await;
    }

    /// Reply with a `success=false` response carrying `message`.
    pub async fn reply_err(&mut self, command: &str, request_seq: i64, message: &str) {
        self.inject(&err_response(command, request_seq, message))
            .await;
    }

    /// Inject a `stopped` event with the given reason/thread.
    pub async fn inject_stopped(&mut self, reason: &str, thread_id: i64) {
        self.inject(&stopped_event(reason, thread_id)).await;
    }

    /// Drop the peer writer to trigger read-loop EOF, then join the read loop.
    pub async fn close_and_join(self) {
        let Harness {
            peer_reads,
            peer_writes,
            read_loop,
        } = self;
        drop(peer_reads);
        drop(peer_writes);
        let _ = read_loop.await;
    }
}

/// Read a raw request frame as a JSON object (preserving arbitrary fields).
async fn read_raw_request(reader: &mut BufReader<DuplexStream>) -> Value {
    // The dap-client `read_message` only models responses/events/other; for a request we
    // re-read the framed bytes ourselves to inspect arbitrary arguments. Reuse the public
    // framing by reading an envelope then re-serializing is lossy, so frame-read here.
    use tokio::io::AsyncBufReadExt;
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.expect("read header line");
        assert_ne!(n, 0, "unexpected EOF reading request header");
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(v.trim().parse().expect("content length"));
        }
    }
    let len = content_length.expect("Content-Length present");
    use tokio::io::AsyncReadExt;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.expect("read body");
    serde_json::from_slice(&body).expect("request JSON")
}

/// A successful `response` envelope for `command`/`request_seq`, with an optional body.
pub fn ok_response(command: &str, request_seq: i64, body: Option<Value>) -> Response {
    Response {
        seq: 0,
        ty: "response".to_string(),
        request_seq,
        success: true,
        command: command.to_string(),
        message: String::new(),
        body,
    }
}

/// A `success=false` response carrying `message`.
pub fn err_response(command: &str, request_seq: i64, message: &str) -> Response {
    Response {
        seq: 0,
        ty: "response".to_string(),
        request_seq,
        success: false,
        command: command.to_string(),
        message: message.to_string(),
        body: None,
    }
}

/// A `stopped` event.
pub fn stopped_event(reason: &str, thread_id: i64) -> StoppedEvent {
    StoppedEvent {
        seq: 0,
        ty: "event".to_string(),
        event: "stopped".to_string(),
        body: StoppedBody {
            reason: reason.to_string(),
            description: String::new(),
            thread_id,
            text: String::new(),
            all_threads_stopped: true,
            hit_breakpoint_ids: Vec::new(),
        },
    }
}
