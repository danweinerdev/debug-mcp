//! Subprocess + stderr-ring tests — mirror Go `internal/detect/subprocess_test.go`.
//!
//! The spawn tests use `sh`/`true` (no real lldb-dap needed), exactly like the Go tests.
//! The `--repl-mode=command`-only-when-capable check is observable via the round-trip /
//! exit behavior; capability is carried through `Subprocess::is_lldb_dap`.

use std::sync::Arc;

use lldb_backend::{spawn, StderrBuffer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[test]
fn stderr_basic() {
    // Go `TestStderrBuffer_Basic`: a short write is kept verbatim and returns its len.
    let buf = StderrBuffer::new(4096);
    let data = b"hello, stderr";
    let n = buf.write(data);
    assert_eq!(n, data.len());
    assert_eq!(buf.contents(), "hello, stderr");
}

#[test]
fn stderr_overflow_keeps_last_n() {
    // Go `TestStderrBuffer_Overflow`: a single oversize write keeps only its last N.
    let buf = StderrBuffer::new(10);
    let data = b"abcdefghijklmnop"; // 16 bytes, cap 10
    let n = buf.write(data);
    assert_eq!(n, data.len(), "returns the full input length");
    assert_eq!(buf.contents(), "ghijklmnop");
}

#[test]
fn stderr_multiple_writes_trim_front() {
    // Go `TestStderrBuffer_MultipleWrites`: 3×5 bytes into a cap-10 ring keeps last 10.
    let buf = StderrBuffer::new(10);
    for w in ["abcde", "fghij", "klmno"] {
        let n = buf.write(w.as_bytes());
        assert_eq!(n, w.len());
    }
    assert_eq!(buf.contents(), "fghijklmno");
}

#[test]
fn stderr_default_size_on_nonpositive() {
    // Go `TestStderrBuffer_DefaultSize`: size 0 ⇒ 4096 (Rust usize, so 0 is the only
    // non-positive value).
    assert_eq!(StderrBuffer::new(0).capacity(), 4096);
}

#[tokio::test(flavor = "multi_thread")]
async fn stderr_concurrent_writes() {
    // Go `TestStderrBuffer_Concurrent`: 10 tasks × 100 single-byte writes (1000 ≤ cap),
    // all 'x', total length preserved.
    let buf = Arc::new(StderrBuffer::new(4096));
    let mut handles = Vec::new();
    for _ in 0..10 {
        let b = Arc::clone(&buf);
        handles.push(tokio::spawn(async move {
            for _ in 0..100 {
                b.write(b"x");
            }
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
    let got = buf.contents();
    assert_eq!(got.len(), 1000);
    assert!(got.bytes().all(|c| c == b'x'));
}

#[tokio::test]
async fn spawn_echo_round_trip_and_stderr() {
    // Go `TestSpawnSubprocess`: spawn `sh` (not capable ⇒ no --repl-mode), write a
    // command that echoes to stdout + stderr, read the stdout line, then verify stderr
    // capture after exit.
    let mut sub = spawn(std::path::Path::new("sh"), false).expect("spawn sh");
    assert!(!sub.is_lldb_dap);

    sub.stdin
        .write_all(b"echo hello_stderr >&2\necho hello_stdout\n")
        .await
        .expect("write stdin");
    sub.stdin.flush().await.expect("flush");

    let mut line = String::new();
    sub.stdout.read_line(&mut line).await.expect("read stdout");
    assert_eq!(line.trim_end(), "hello_stdout");

    // Close stdin (EOF) and wait for exit.
    drop(sub.stdin);
    let status = sub.child.wait().await.expect("wait");
    assert!(status.success(), "sh exits cleanly");

    // The background drain has copied stderr by EOF; give it a moment to flush.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        sub.stderr.contents().contains("hello_stderr"),
        "stderr captured: {:?}",
        sub.stderr.contents()
    );
}

#[tokio::test]
async fn spawn_exit_detected_after_stdin_command() {
    // Go `TestSpawnSubprocess_ExitDetected`: tell `sh` to exit 0; Wait reports success.
    let mut sub = spawn(std::path::Path::new("sh"), false).expect("spawn sh");
    sub.stdin.write_all(b"exit 0\n").await.expect("write");
    sub.stdin.flush().await.expect("flush");
    let status = sub.child.wait().await.expect("wait");
    assert!(status.success());
    assert_eq!(status.code(), Some(0));
}

#[tokio::test]
async fn spawn_capable_flag_passes_repl_mode() {
    // Go `TestSpawnSubprocess_IsLLDBDAPFlag`: `true` ignores args; capable=true passes
    // --repl-mode=command (the flag round-trips on the result; `true` exits 0 regardless).
    let mut sub = spawn(std::path::Path::new("true"), true).expect("spawn true");
    assert!(sub.is_lldb_dap, "capable flag carried through");
    let status = sub.child.wait().await.expect("wait");
    assert!(status.success());
}
