//! lldb-dap subprocess spawn + stderr ring buffer (Spec FR-16, task 3.2). Go origin:
//! `internal/detect/subprocess.go` (`SpawnLLDBDAP`, `StderrBuffer`).
//!
//! Spawns the detected binary via `tokio::process`, passing `--repl-mode=command`
//! **only** when capable, with stdin/stdout/stderr piped. A background task drains
//! stderr into a [`StderrBuffer`] (4096-byte keep-last-N ring) to avoid a pipe-buffer
//! deadlock. Spawn does **no** lifecycle management — kill/wait/EOF are driven by the
//! read loop + disconnect (Phases 2/5).

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use debugger_core::BackendError;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Default stderr ring capacity (Go `NewStderrBuffer` non-positive default).
const DEFAULT_STDERR_CAP: usize = 4096;

/// The spawned lldb-dap subprocess and its piped I/O handles. The DAP client writes
/// requests to `stdin` and the read loop reads responses from `stdout`; `stderr` is the
/// captured ring (drained in the background). `is_lldb_dap` carries the capability flag
/// through to the backend. Go origin: `detect.SubprocessResult`.
pub struct Subprocess {
    /// The child handle — its lifecycle (kill/wait) is owned by the backend's
    /// disconnect/teardown, not by spawn (Spec FR-16.4).
    pub child: Child,
    /// DAP request sink (the subprocess's stdin).
    pub stdin: ChildStdin,
    /// DAP response source, wrapped in a [`BufReader`] for the read loop's framed reads.
    pub stdout: BufReader<ChildStdout>,
    /// The captured-stderr ring buffer (the last 4096 bytes).
    pub stderr: Arc<StderrBuffer>,
    /// `true` when the spawned binary is `lldb-dap` (supports `--repl-mode`).
    pub is_lldb_dap: bool,
}

/// A thread-safe keep-last-N ring buffer capturing the tail of stderr (Spec FR-16.3).
/// Go origin: `detect.StderrBuffer`.
///
/// Semantics, matching Go exactly:
/// - constructed with a non-positive size ⇒ default 4096 (see [`StderrBuffer::new`]);
/// - a single write larger than the capacity keeps only its **last** `size` bytes;
/// - otherwise appends and trims from the front to stay within `size`;
/// - [`StderrBuffer::write`] never errors and always reports the full input length.
#[derive(Debug)]
pub struct StderrBuffer {
    inner: Mutex<Vec<u8>>,
    size: usize,
}

impl StderrBuffer {
    /// Create a ring with the given capacity; a non-positive size defaults to 4096
    /// (Go `NewStderrBuffer`). Rust uses `usize`, so "non-positive" is just `0`.
    pub fn new(size: usize) -> Self {
        let size = if size == 0 { DEFAULT_STDERR_CAP } else { size };
        StderrBuffer {
            inner: Mutex::new(Vec::with_capacity(size)),
            size,
        }
    }

    /// The configured capacity (exposed for the default-size parity test).
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Append `data`, keeping only the last `size` bytes. Always returns `data.len()`,
    /// never errors (Go `StderrBuffer.Write`).
    pub fn write(&self, data: &[u8]) -> usize {
        let n = data.len();
        let mut buf = self.lock();

        // A single write at/over capacity keeps only its tail (Go: `len(p) >= b.size`).
        if data.len() >= self.size {
            buf.clear();
            buf.extend_from_slice(&data[data.len() - self.size..]);
            return n;
        }

        buf.extend_from_slice(data);
        if buf.len() > self.size {
            let excess = buf.len() - self.size;
            buf.drain(0..excess);
        }
        n
    }

    /// The captured content as a UTF-8 string (lossy for non-UTF-8 bytes). Go origin:
    /// `StderrBuffer.String`.
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.lock()).into_owned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Spawn `path` with stdin/stdout/stderr piped, passing `--repl-mode=command` iff
/// `is_lldb_dap` (Spec FR-16.1). A background task drains stderr into the returned
/// [`StderrBuffer`]. Go origin: `detect.SpawnLLDBDAP`.
///
/// Spawn failure → [`BackendError::Spawn`] (Go wraps `starting subprocess`).
pub fn spawn(path: &std::path::Path, is_lldb_dap: bool) -> Result<Subprocess, BackendError> {
    let mut command = Command::new(path);
    if is_lldb_dap {
        command.arg("--repl-mode=command");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Don't leak our process group's controlling tty etc.; lldb-dap speaks DAP only
        // over the piped handles. `kill_on_drop` is intentionally NOT set — teardown is
        // explicit (Spec FR-16.4), and the disconnect path drives kill/wait.
        .kill_on_drop(false);

    let mut child = command.spawn().map_err(|e| {
        BackendError::Spawn(format!("starting subprocess {:?}: {e}", path.display()))
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| BackendError::Spawn("creating stdin pipe: no handle".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BackendError::Spawn("creating stdout pipe: no handle".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| BackendError::Spawn("creating stderr pipe: no handle".to_string()))?;

    let stderr_buf = Arc::new(StderrBuffer::new(DEFAULT_STDERR_CAP));
    spawn_stderr_drain(stderr, Arc::clone(&stderr_buf));

    Ok(Subprocess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        stderr: stderr_buf,
        is_lldb_dap,
    })
}

/// Drain `reader` into `buf` in the background until EOF (Go's `go io.Copy(stderrBuf,
/// stderr)`). Read errors end the drain silently — stderr capture is best-effort.
fn spawn_stderr_drain<R>(mut reader: R, buf: Arc<StderrBuffer>)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.write(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
    });
}
