//! DAP wire layer: Content-Length framing + the ~20 DAP message types the client
//! exchanges (Spec FR-17.1, design §"DAP Client internals", task 2.1).
//!
//! Go origin: `internal/dap/types.go` plus the `google/go-dap` framing that
//! `internal/dap/client.go` drives (`ReadProtocolMessage`/`WriteProtocolMessage`).
//! These local structs resolve design risk **R3** — we own the wire shape rather
//! than pulling a DAP crate. They reproduce the base-protocol envelope go-dap
//! produces: a single `Content-Length: <byte-count>\r\n\r\n` header followed by
//! exactly that many bytes of UTF-8 JSON, and the `seq`/`type`/`command`/`event`/
//! `request_seq`/`success`/`message`/`body`/`arguments` field names.
//!
//! Request `arguments` for launch/attach are carried as raw JSON
//! ([`serde_json::Value`]): the lldb-specific arg shapes live in Phase 3, so this
//! crate stays debugger-agnostic.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::WireError;

/// Maximum `Content-Length` we will honor. go-dap imposes no cap, but an unbounded
/// length lets a hostile/corrupt peer request an arbitrary allocation. 64 MiB is far
/// above any real DAP message yet bounds the blast radius of a bad header.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// A decoded DAP message. The read loop matches on this to dispatch (Spec FR-17.6);
/// the variant is chosen by the `type` field plus the `command`/`event` discriminator.
///
/// Only the message types this client actually exchanges are modeled. Anything that
/// decodes as a well-formed envelope but is not a recognized event/response lands in
/// [`DapMessage::Other`] (the read loop logs it as unhandled, matching Go's `default`).
#[derive(Debug, Clone, PartialEq)]
pub enum DapMessage {
    /// A `type:"response"` message (any command). Correlated by `request_seq`.
    Response(Response),
    /// A recognized `type:"event"` message.
    Event(Event),
    /// A well-formed envelope that is neither a response nor a recognized event
    /// (e.g. an unmodeled event, or a `type:"request"` from the adapter). Logged as
    /// unhandled by the read loop.
    Other(Envelope),
}

impl DapMessage {
    /// Decode framed JSON bytes into a typed message via the `type` discriminator.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, WireError> {
        let envelope: Envelope = serde_json::from_slice(bytes).map_err(WireError::Json)?;
        match envelope.ty.as_str() {
            "response" => {
                let response: Response = serde_json::from_slice(bytes).map_err(WireError::Json)?;
                Ok(DapMessage::Response(response))
            }
            "event" => match Event::from_envelope(bytes, &envelope)? {
                Some(event) => Ok(DapMessage::Event(event)),
                None => Ok(DapMessage::Other(envelope)),
            },
            _ => Ok(DapMessage::Other(envelope)),
        }
    }
}

/// The DAP base-protocol envelope (`ProtocolMessage`): `seq` + `type`, plus the
/// discriminator fields used to route. All routing decisions read only this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    /// Present on requests and responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Present on events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

/// A DAP response (`type:"response"`). Correlation key is [`Response::request_seq`].
/// `body` is left as raw JSON: the backend (Phase 3) owns the per-command decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub request_seq: i64,
    pub success: bool,
    pub command: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// The recognized DAP events. Concrete variants for everything the read loop acts on;
/// the informational ones (Thread/Breakpoint/Process/Continued) are matched so the
/// loop can log them, matching Go's informational-event case (`client.go:218`).
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Initialized,
    Stopped(StoppedEvent),
    Output(OutputEvent),
    Exited(ExitedEvent),
    Terminated,
    /// `type:"event"`, `event:"thread"` — informational, log only.
    Thread,
    /// `type:"event"`, `event:"breakpoint"` — informational, log only.
    Breakpoint,
    /// `type:"event"`, `event:"process"` — informational, log only.
    Process,
    /// `type:"event"`, `event:"continued"` — informational, log only.
    Continued,
    /// `type:"event"`, `event:"module"` — informational, log only. lldb-dap emits one
    /// per loaded shared library during launch; modeled so it is logged as informational
    /// (not "unhandled event") to keep stderr quiet.
    Module,
    /// `type:"event"`, `event:"capabilities"` — informational, log only. lldb-dap may
    /// emit this to advertise late capabilities; logged as informational like `module`.
    Capabilities,
}

impl Event {
    /// Decode an event from its raw bytes + already-parsed envelope. Returns `Ok(None)`
    /// for an event whose `event` name we do not model (the caller maps that to
    /// [`DapMessage::Other`], i.e. "unhandled").
    fn from_envelope(bytes: &[u8], envelope: &Envelope) -> Result<Option<Self>, WireError> {
        let name = match envelope.event.as_deref() {
            Some(name) => name,
            None => return Ok(None),
        };
        let event = match name {
            "initialized" => Event::Initialized,
            "stopped" => {
                let e: StoppedEvent = serde_json::from_slice(bytes).map_err(WireError::Json)?;
                Event::Stopped(e)
            }
            "output" => {
                let e: OutputEvent = serde_json::from_slice(bytes).map_err(WireError::Json)?;
                Event::Output(e)
            }
            "exited" => {
                let e: ExitedEvent = serde_json::from_slice(bytes).map_err(WireError::Json)?;
                Event::Exited(e)
            }
            "terminated" => Event::Terminated,
            "thread" => Event::Thread,
            "breakpoint" => Event::Breakpoint,
            "process" => Event::Process,
            "continued" => Event::Continued,
            "module" => Event::Module,
            "capabilities" => Event::Capabilities,
            _ => return Ok(None),
        };
        Ok(Some(event))
    }
}

/// `StoppedEvent` body fields the client/backend consume (Spec FR-17.6, FR-8.4).
/// Go origin: `godap.StoppedEventBody` as read in `client.go` / `handleStopResult`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StoppedBody {
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "threadId", default)]
    pub thread_id: i64,
    #[serde(default)]
    pub text: String,
    #[serde(rename = "allThreadsStopped", default)]
    pub all_threads_stopped: bool,
    #[serde(rename = "hitBreakpointIds", default)]
    pub hit_breakpoint_ids: Vec<i64>,
}

/// `type:"event"`, `event:"stopped"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoppedEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub event: String,
    pub body: StoppedBody,
}

/// `OutputEvent` body (Spec FR-12). Go origin: `godap.OutputEventBody`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OutputBody {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub output: String,
}

/// `type:"event"`, `event:"output"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub event: String,
    pub body: OutputBody,
}

/// `ExitedEvent` body (Spec FR-17.6). Go origin: `godap.ExitedEventBody`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExitedBody {
    #[serde(rename = "exitCode", default)]
    pub exit_code: i64,
}

/// `type:"event"`, `event:"exited"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitedEvent {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub event: String,
    pub body: ExitedBody,
}

/// A request the client serializes onto the wire. `arguments` is raw JSON so this crate
/// stays debugger-agnostic (the lldb arg shapes live in Phase 3). `seq` is stamped by
/// the client just before writing (Spec FR-17.2/17.3). Go origin: the `godap.*Request`
/// values assembled by `internal/tools/*.go` and written via `WriteProtocolMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

impl Request {
    /// Build a request with `type:"request"`, the given command, and optional raw
    /// arguments. `seq` starts at `0`; the client overwrites it via [`Request::set_seq`]
    /// before the frame is written.
    pub fn new(command: impl Into<String>, arguments: Option<Value>) -> Self {
        Request {
            seq: 0,
            ty: "request".to_string(),
            command: command.into(),
            arguments,
        }
    }

    /// Stamp the sequence number assigned by the client (Spec FR-17.3).
    pub fn set_seq(&mut self, seq: i64) {
        self.seq = seq;
    }
}

/// Write a DAP message as a Content-Length-framed frame: the JSON body is serialized
/// first so the byte count in the header is exact (Spec FR-17.1). Matches go-dap's
/// `WriteProtocolMessage` (single header line + CRLF + CRLF + body, no trailing bytes).
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message).map_err(WireError::Json)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(WireError::Io)?;
    writer.write_all(&body).await.map_err(WireError::Io)?;
    writer.flush().await.map_err(WireError::Io)?;
    Ok(())
}

/// Read one Content-Length-framed DAP message and decode it (Spec FR-17.1). Mirrors
/// go-dap's `ReadProtocolMessage`: consume header lines until a blank line, require a
/// `Content-Length` header, then read exactly that many body bytes.
///
/// Errors map to [`WireError`]: a clean EOF before any header byte is
/// [`WireError::Eof`]; a truncated body (EOF mid-frame) is [`WireError::Io`]
/// (`UnexpectedEof`); a missing/invalid header is [`WireError::MissingContentLength`]
/// / [`WireError::InvalidContentLength`]; an unparseable body is [`WireError::Json`].
pub async fn read_message<R>(reader: &mut R) -> Result<DapMessage, WireError>
where
    R: AsyncBufRead + Unpin,
{
    let content_length = read_header(reader).await?;
    let mut body = vec![0u8; content_length];
    read_exact_body(reader, &mut body).await?;
    DapMessage::from_slice(&body)
}

/// Read header lines up to the blank separator line, returning the `Content-Length`.
async fn read_header<R>(reader: &mut R) -> Result<usize, WireError>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut first_line = true;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(WireError::Io)?;
        if n == 0 {
            // EOF. A clean EOF before any header byte means the peer closed the
            // stream between messages (the normal shutdown path → `Eof`). EOF after
            // partial header bytes is a truncated frame.
            if first_line {
                return Err(WireError::Eof);
            }
            return Err(WireError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF in the middle of a DAP header",
            )));
        }
        first_line = false;

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line ends the header block.
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let parsed: usize = value
                .trim()
                .parse()
                .map_err(|_| WireError::InvalidContentLength(value.trim().to_string()))?;
            if parsed > MAX_CONTENT_LENGTH {
                return Err(WireError::InvalidContentLength(value.trim().to_string()));
            }
            content_length = Some(parsed);
        }
        // Other header fields (none are produced by go-dap, but tolerate them) are
        // ignored, matching go-dap's "only Content-Length is consulted" behavior.
    }
    content_length.ok_or(WireError::MissingContentLength)
}

/// Read exactly `body.len()` bytes; EOF before the body is complete is a truncated
/// frame (`UnexpectedEof`), matching go-dap reading a short body.
async fn read_exact_body<R>(reader: &mut R, body: &mut [u8]) -> Result<(), WireError>
where
    R: AsyncBufRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    reader.read_exact(body).await.map_err(WireError::Io)?;
    Ok(())
}
