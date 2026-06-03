//! Wire-framing tests — mirrors Go `internal/dap/types_test.go`.
//!
//! Round-trips each exercised message through `write_message` → `read_message` over an
//! in-memory `tokio::io::duplex` peer, and asserts the four malformed-input cases the
//! Go `TestMalformedInput*` tests pin: truncated body, invalid JSON body, missing
//! `Content-Length` header, and an empty reader.

use std::io::Cursor;

use dap_client::{
    read_message, write_message, DapMessage, ExitedEvent, OutputEvent, Request, Response,
    StoppedEvent,
};
use serde_json::json;
use tokio::io::BufReader;

/// Serialize `message` to a Content-Length frame, then read it back through the public
/// `read_message`. Drives the bytes through an in-memory buffer (the Go test's
/// `bytes.Buffer` analog).
async fn write_then_read<T: serde::Serialize>(message: &T) -> DapMessage {
    let mut framed: Vec<u8> = Vec::new();
    write_message(&mut framed, message).await.expect("write");
    let mut reader = BufReader::new(Cursor::new(framed));
    read_message(&mut reader).await.expect("read")
}

// --- Round-trips (mirror TestRoundTrip* + TestAttachArgsRoundTrip). ---

#[tokio::test]
async fn round_trip_initialize_request() {
    // Go `TestRoundTripInitializeRequest`: a request with raw arguments round-trips,
    // preserving seq/type/command/arguments. (Our requests carry args as raw JSON.)
    let mut req = Request::new(
        "initialize",
        Some(json!({
            "clientID": "lldb-debug-mcp",
            "adapterID": "lldb-dap",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
        })),
    );
    req.set_seq(1);

    // A request is `type:"request"` → decodes to `Other` (the client only models
    // responses+events as concrete; outgoing requests are not re-read by the client).
    // Assert the envelope round-trips its discriminators by reading it back raw.
    let mut framed = Vec::new();
    write_message(&mut framed, &req).await.expect("write");
    let mut reader = BufReader::new(Cursor::new(framed));
    let decoded = read_message(&mut reader).await.expect("read");
    match decoded {
        DapMessage::Other(env) => {
            assert_eq!(env.seq, 1);
            assert_eq!(env.ty, "request");
            assert_eq!(env.command.as_deref(), Some("initialize"));
        }
        other => panic!("expected Other(request), got {other:?}"),
    }
}

#[tokio::test]
async fn round_trip_launch_request_arguments_preserved() {
    // Go `TestRoundTripLaunchRequest`: the launch arguments survive the round-trip. We
    // carry arguments as raw JSON, so assert the raw object is byte-faithful.
    let args = json!({
        "program": "/path/to/exe",
        "args": ["--flag", "value"],
        "cwd": "/working/dir",
        "env": {"FOO": "bar"},
        "stopOnEntry": true,
        "initCommands": ["settings set target.x86-disassembly-flavor intel"],
    });
    let mut req = Request::new("launch", Some(args.clone()));
    req.set_seq(2);

    let mut framed = Vec::new();
    write_message(&mut framed, &req).await.expect("write");
    let mut reader = BufReader::new(Cursor::new(framed));
    let decoded = read_message(&mut reader).await.expect("read");
    let DapMessage::Other(env) = decoded else {
        panic!("expected Other");
    };
    assert_eq!(env.seq, 2);
    assert_eq!(env.command.as_deref(), Some("launch"));
    // Re-parse the whole frame to confirm the arguments are intact.
    // (The Other envelope only keeps discriminators; re-read the raw bytes.)
    let mut framed2 = Vec::new();
    write_message(&mut framed2, &req).await.expect("write");
    let value: serde_json::Value = {
        // strip the Content-Length header to get the JSON body.
        let text = String::from_utf8(framed2).unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str(body).unwrap()
    };
    assert_eq!(value["arguments"], args);
}

#[tokio::test]
async fn round_trip_continue_request() {
    // Go `TestRoundTripContinueRequest`.
    let mut req = Request::new(
        "continue",
        Some(json!({"threadId": 42, "singleThread": true})),
    );
    req.set_seq(3);
    let decoded = write_then_read(&req).await;
    let DapMessage::Other(env) = decoded else {
        panic!("expected Other");
    };
    assert_eq!(env.seq, 3);
    assert_eq!(env.command.as_deref(), Some("continue"));
}

#[tokio::test]
async fn round_trip_evaluate_request() {
    // Go `TestRoundTripEvaluateRequest`.
    let mut req = Request::new(
        "evaluate",
        Some(json!({"expression": "x + y", "frameId": 7, "context": "watch"})),
    );
    req.set_seq(4);
    let decoded = write_then_read(&req).await;
    let DapMessage::Other(env) = decoded else {
        panic!("expected Other");
    };
    assert_eq!(env.seq, 4);
    assert_eq!(env.command.as_deref(), Some("evaluate"));
}

#[tokio::test]
async fn round_trip_response_message() {
    // The client *reads* responses as concrete `Response`s (correlation key
    // `request_seq`). Analog of the InitializeResponse the Go client tests synthesize.
    let resp = Response {
        seq: 1,
        ty: "response".to_string(),
        request_seq: 7,
        success: true,
        command: "initialize".to_string(),
        message: String::new(),
        body: Some(json!({"supportsConfigurationDoneRequest": true})),
    };
    let decoded = write_then_read(&resp).await;
    match decoded {
        DapMessage::Response(got) => {
            assert_eq!(got.request_seq, 7);
            assert!(got.success);
            assert_eq!(got.command, "initialize");
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

#[tokio::test]
async fn round_trip_stopped_event() {
    let event = StoppedEvent {
        seq: 5,
        ty: "event".to_string(),
        event: "stopped".to_string(),
        body: dap_client::StoppedBody {
            reason: "breakpoint".to_string(),
            thread_id: 1,
            hit_breakpoint_ids: vec![3],
            ..Default::default()
        },
    };
    let decoded = write_then_read(&event).await;
    match decoded {
        DapMessage::Event(dap_client::Event::Stopped(got)) => {
            assert_eq!(got.body.reason, "breakpoint");
            assert_eq!(got.body.thread_id, 1);
            assert_eq!(got.body.hit_breakpoint_ids, vec![3]);
        }
        other => panic!("expected Stopped event, got {other:?}"),
    }
}

#[tokio::test]
async fn round_trip_output_event() {
    let event = OutputEvent {
        seq: 6,
        ty: "event".to_string(),
        event: "output".to_string(),
        body: dap_client::OutputBody {
            category: "stdout".to_string(),
            output: "hello world\n".to_string(),
        },
    };
    let decoded = write_then_read(&event).await;
    match decoded {
        DapMessage::Event(dap_client::Event::Output(got)) => {
            assert_eq!(got.body.category, "stdout");
            assert_eq!(got.body.output, "hello world\n");
        }
        other => panic!("expected Output event, got {other:?}"),
    }
}

#[tokio::test]
async fn round_trip_exited_event() {
    let event = ExitedEvent {
        seq: 7,
        ty: "event".to_string(),
        event: "exited".to_string(),
        body: dap_client::ExitedBody { exit_code: 42 },
    };
    let decoded = write_then_read(&event).await;
    match decoded {
        DapMessage::Event(dap_client::Event::Exited(got)) => {
            assert_eq!(got.body.exit_code, 42);
        }
        other => panic!("expected Exited event, got {other:?}"),
    }
}

#[tokio::test]
async fn initialized_and_terminated_events_decode() {
    // Bare events (no body) decode to their unit variants.
    let initialized = json!({"seq": 1, "type": "event", "event": "initialized"});
    let terminated = json!({"seq": 2, "type": "event", "event": "terminated"});
    assert!(matches!(
        write_then_read(&initialized).await,
        DapMessage::Event(dap_client::Event::Initialized)
    ));
    assert!(matches!(
        write_then_read(&terminated).await,
        DapMessage::Event(dap_client::Event::Terminated)
    ));
}

#[tokio::test]
async fn informational_events_decode_to_their_variants() {
    for (name, want) in [
        ("thread", "thread"),
        ("breakpoint", "breakpoint"),
        ("process", "process"),
        ("continued", "continued"),
    ] {
        let event = json!({"seq": 1, "type": "event", "event": name});
        let decoded = write_then_read(&event).await;
        let DapMessage::Event(ev) = decoded else {
            panic!("expected event for {want}");
        };
        let matched = matches!(
            (name, &ev),
            ("thread", dap_client::Event::Thread)
                | ("breakpoint", dap_client::Event::Breakpoint)
                | ("process", dap_client::Event::Process)
                | ("continued", dap_client::Event::Continued)
        );
        assert!(matched, "event {name} decoded to {ev:?}");
    }
}

#[tokio::test]
async fn unmodeled_event_decodes_to_other() {
    // An event we do not model falls through to Other (the read loop logs it as
    // unhandled — Go's `default`). `loadedSource` is a real DAP event the backend has no
    // interest in, so it stays unmodeled.
    let event = json!({"seq": 1, "type": "event", "event": "loadedSource"});
    let decoded = write_then_read(&event).await;
    match decoded {
        DapMessage::Other(env) => assert_eq!(env.event.as_deref(), Some("loadedSource")),
        other => panic!("expected Other, got {other:?}"),
    }
}

// --- Malformed input (mirror TestMalformedInput*). ---

#[tokio::test]
async fn malformed_truncated_body() {
    // Go `TestMalformedInputTruncatedMessage`: a valid header but a body shorter than
    // Content-Length. read_exact hits EOF mid-body → error.
    let input = b"Content-Length: 100\r\n\r\n{\"seq\":1".to_vec();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader)
        .await
        .expect_err("truncated body must error");
    // Truncation surfaces as an IO (UnexpectedEof), not a clean EOF.
    assert!(matches!(err, dap_client::WireError::Io(_)), "got {err:?}");
}

#[tokio::test]
async fn malformed_bad_json() {
    // Go `TestMalformedInputBadJSON`: header length matches a non-JSON body.
    let bad = "{not valid json!!}";
    let input = format!("Content-Length: {}\r\n\r\n{bad}", bad.len()).into_bytes();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader)
        .await
        .expect_err("bad json must error");
    assert!(matches!(err, dap_client::WireError::Json(_)), "got {err:?}");
}

#[tokio::test]
async fn malformed_no_header() {
    // Go `TestMalformedInputNoHeader`: bare JSON with no Content-Length header. The
    // header block ends at EOF (or at a blank line) without a Content-Length → error.
    let input = br#"{"seq":1,"type":"request","command":"initialize"}"#.to_vec();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader)
        .await
        .expect_err("missing header must error");
    // The single bare line has no Content-Length and no terminating blank line, so the
    // reader hits EOF after a partial header → UnexpectedEof (truncated header).
    assert!(
        matches!(
            err,
            dap_client::WireError::Io(_) | dap_client::WireError::MissingContentLength
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn malformed_no_header_with_blank_line() {
    // A header block that terminates cleanly (blank line) but never carried a
    // Content-Length must be MissingContentLength specifically.
    let input = b"X-Other: 1\r\n\r\n{}".to_vec();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader).await.expect_err("must error");
    assert!(
        matches!(err, dap_client::WireError::MissingContentLength),
        "got {err:?}"
    );
}

#[tokio::test]
async fn malformed_empty_reader() {
    // Go `TestMalformedInputEmptyReader`: an empty reader. The first read_line returns
    // 0 → clean EOF.
    let input: Vec<u8> = Vec::new();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader)
        .await
        .expect_err("empty reader must error");
    assert!(matches!(err, dap_client::WireError::Eof), "got {err:?}");
}

#[tokio::test]
async fn malformed_invalid_content_length() {
    // A non-numeric Content-Length is rejected distinctly.
    let input = b"Content-Length: notanumber\r\n\r\n{}".to_vec();
    let mut reader = BufReader::new(Cursor::new(input));
    let err = read_message(&mut reader).await.expect_err("must error");
    assert!(
        matches!(err, dap_client::WireError::InvalidContentLength(_)),
        "got {err:?}"
    );
}
