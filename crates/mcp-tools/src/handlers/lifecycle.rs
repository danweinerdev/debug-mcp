//! Lifecycle handlers: `launch`, `attach`, `disconnect` (Spec FR-4/FR-5/FR-6, task 5.3).
//!
//! These own the connect→pump→launch ordering (the event-pump is spawned **before**
//! `backend.launch`, so a `Terminated` during the handshake reaches the session), the
//! pid-precedence validation, and disconnect's two sequential 5 s timeouts. The DAP
//! handshake itself lives below the seam in the backend's `launch`/`attach`.

use std::sync::Arc;
use std::time::Duration;

use debugger_core::{AttachOutcome, AttachSpec, BackendError, LaunchOutcome, LaunchSpec};
use mcp_session::{spawn_event_pump, State};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

impl ToolServer {
    /// `launch` (Spec FR-4). Guard idle → parse → flush pending breakpoints into the
    /// `LaunchSpec` → `factory.connect()` → store backend + spawn the event-pump **before**
    /// `backend.launch(spec)` → map the outcome.
    pub(crate) async fn handle_launch(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle]) {
            return ToolOutcome::error(e);
        }

        let program = match args.require_string("program") {
            Ok(p) => p,
            Err(e) => return ToolOutcome::error(e),
        };
        let program_args = match args.parse_json_array("args") {
            Ok(a) => a,
            Err(e) => return ToolOutcome::error(e),
        };
        let cwd = args.get_string("cwd", "");
        let env = match args.parse_json_object("env") {
            Ok(e) => e,
            Err(e) => return ToolOutcome::error(e),
        };
        let stop_on_entry = args.get_bool("stop_on_entry", true);

        // Set state configuring (Go launch.go:76).
        self.session.set_state(State::Configuring);

        // Flush pending breakpoints into the spec (launch flushes; attach does not).
        let (source_map, function_bps) = self.session.flush_pending_breakpoints();
        let source_breakpoints: Vec<_> = source_map.into_iter().collect();

        let spec = LaunchSpec {
            program: program.clone(),
            args: program_args,
            cwd: if cwd.is_empty() { None } else { Some(cwd) },
            env,
            stop_on_entry,
            source_breakpoints,
            function_breakpoints: function_bps,
        };

        // Connect (detect + spawn + read loop). Failure → Go find/spawn strings + reset.
        let connection = match self.factory.connect().await {
            Ok(c) => c,
            Err(e) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(connect_error(e));
            }
        };

        // Store the backend and spawn the event-pump BEFORE awaiting backend.launch, so a
        // Terminated event during the handshake reaches the session (not dropped). The
        // pump captures the current generation; a later disconnect bumps it, so a stale
        // Terminated cannot clobber the reset idle state (design Decision 6).
        self.set_backend(Arc::clone(&connection.backend)).await;
        let generation = self.session.generation();
        spawn_event_pump(connection.events, Arc::clone(&self.session), generation);

        // Run the launch handshake, racing the request cancellation token. On cancel the
        // backend future is dropped (cancel-safe) and we return the Go timeout string. The
        // distinct cancellation messages mirror Go: the whole-launch timeout vs the
        // stop-on-entry wait timeout are the same observable "timed out" path here (the
        // coarse trait collapses the handshake), so we use the stop-on-entry wording when
        // stop_on_entry, else the launch wording.
        let outcome = tokio::select! {
            r = connection.backend.launch(spec) => r,
            () = ct.cancelled() => {
                self.cleanup_after_cancel().await;
                let msg = if stop_on_entry {
                    "timed out waiting for stop on entry: context canceled"
                } else {
                    "launch timed out: context canceled"
                };
                return ToolOutcome::error(msg);
            }
        };

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(launch_error(e));
            }
        };

        // Record program + pid. Go records the lldb-dap subprocess pid here
        // (`SetPID(sub.Cmd.Process.Pid)`); the backend surfaces it via `debugger_pid()`.
        self.session.set_program(program.clone());
        if let Some(pid) = connection.backend.debugger_pid() {
            self.session.set_pid(pid);
        }
        let pid = self.session.pid();

        match outcome {
            LaunchOutcome::Stopped(info) => {
                self.session.set_state(State::Stopped);
                let reason = info.reason.clone();
                let thread_id = info.thread_id;
                self.session.set_last_stopped(info);
                ToolOutcome::Json(
                    RespBuilder::new()
                        .set("status", "launched")
                        .set("program", program)
                        .set("pid", pid)
                        .set("state", "stopped")
                        .set("stop_reason", reason)
                        .set("stopped_thread_id", thread_id)
                        .build(),
                )
            }
            LaunchOutcome::Running => {
                self.session.set_state(State::Running);
                ToolOutcome::Json(
                    RespBuilder::new()
                        .set("status", "launched")
                        .set("program", program)
                        .set("pid", pid)
                        .set("state", "running")
                        .build(),
                )
            }
            LaunchOutcome::Exited { .. } => {
                self.session.set_state(State::Terminated);
                ToolOutcome::text("Program exited during launch")
            }
        }
    }

    /// `attach` (Spec FR-5). Guard idle → pid-precedence validation → connect → store +
    /// pump → `backend.attach(spec)` → map the outcome. No breakpoint flush.
    pub(crate) async fn handle_attach(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle]) {
            return ToolOutcome::error(e);
        }

        // pid (number) takes precedence over wait_for (string); at least one required.
        let pid_present = args.get_raw("pid").is_some_and(|v| !v.is_null());
        let wait_for_raw = args.get_raw("wait_for");

        let (spec, program_label, explicit_pid) = if pid_present {
            let pid_value = args.get_raw("pid").expect("present");
            let pid = match pid_value.as_f64() {
                Some(f) => f as i64,
                None => return ToolOutcome::error("'pid' must be a number"),
            };
            if pid <= 0 {
                return ToolOutcome::error("'pid' must be a positive integer");
            }
            (
                AttachSpec {
                    pid: Some(pid),
                    wait_for: None,
                },
                format!("pid:{pid}"),
                Some(pid),
            )
        } else if let Some(raw) = wait_for_raw.filter(|v| !v.is_null()) {
            let name = raw.as_str().unwrap_or("");
            if name.is_empty() {
                return ToolOutcome::error("'wait_for' must be a non-empty string");
            }
            (
                AttachSpec {
                    pid: None,
                    wait_for: Some(name.to_string()),
                },
                name.to_string(),
                None,
            )
        } else {
            return ToolOutcome::error("either 'pid' or 'wait_for' must be provided");
        };

        self.session.set_state(State::Configuring);

        let connection = match self.factory.connect().await {
            Ok(c) => c,
            Err(e) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(connect_error(e));
            }
        };

        self.set_backend(Arc::clone(&connection.backend)).await;
        let generation = self.session.generation();
        spawn_event_pump(connection.events, Arc::clone(&self.session), generation);

        let outcome = tokio::select! {
            r = connection.backend.attach(spec) => r,
            () = ct.cancelled() => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error("timed out waiting for stop on entry: context canceled");
            }
        };

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(attach_error(e));
            }
        };

        // PID precedence (Spec FR-5.6): the supplied target pid when attaching by pid,
        // otherwise the lldb-dap subprocess pid (attach-by-wait_for) via `debugger_pid()`.
        self.session.set_program(program_label.clone());
        match explicit_pid {
            Some(pid) => self.session.set_pid(pid),
            None => {
                if let Some(pid) = connection.backend.debugger_pid() {
                    self.session.set_pid(pid);
                }
            }
        }
        let pid = self.session.pid();

        match outcome {
            AttachOutcome::Stopped(info) => {
                self.session.set_state(State::Stopped);
                let reason = info.reason.clone();
                let thread_id = info.thread_id;
                self.session.set_last_stopped(info);
                ToolOutcome::Json(
                    RespBuilder::new()
                        .set("status", "attached")
                        .set("program", program_label)
                        .set("pid", pid)
                        .set("state", "stopped")
                        .set("stop_reason", reason)
                        .set("stopped_thread_id", thread_id)
                        .build(),
                )
            }
            AttachOutcome::Exited { .. } | AttachOutcome::Terminated => {
                self.session.set_state(State::Terminated);
                ToolOutcome::text("Process exited during attach")
            }
        }
    }

    /// `disconnect` (Spec FR-6). Guard non-idle → best-effort `backend.disconnect` under
    /// 5 s (errors ignored) → drop the backend (graceful exit under 5 s, then kill) →
    /// reset → always `{"status":"disconnected"}`.
    pub(crate) async fn handle_disconnect(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[
            State::Configuring,
            State::Stopped,
            State::Running,
            State::Terminated,
        ]) {
            return ToolOutcome::error(e);
        }

        let terminate = args.get_bool("terminate", true);

        // First 5 s timeout: the best-effort DAP DisconnectRequest (errors ignored).
        if let Some(backend) = self.current_backend().await {
            let _ =
                tokio::time::timeout(Duration::from_secs(5), backend.disconnect(terminate)).await;
        }

        // Second 5 s timeout: drop the backend (the last Arc tears down the subprocess and
        // ends the event-pump). Dropping happens on a blocking task bounded at 5 s; if it
        // overruns we proceed anyway (the backend's own disconnect already force-killed the
        // child, so the drop is fast). Resetting always succeeds past the guard.
        self.drop_backend_bounded().await;

        self.session.reset();
        ToolOutcome::Json(RespBuilder::new().set("status", "disconnected").build())
    }

    /// Drop the connected backend within a 5 s bound. The Arc is moved onto a blocking
    /// drop; if the drop overruns the bound we abandon the wait (the subprocess was already
    /// killed by `backend.disconnect`), still clearing the slot so a fresh `launch`
    /// connects anew (session reuse, Spec FR-6).
    async fn drop_backend_bounded(&self) {
        let backend = self.backend.write().await.take();
        if let Some(backend) = backend {
            let (tx, rx) = oneshot::channel::<()>();
            tokio::spawn(async move {
                drop(backend);
                let _ = tx.send(());
            });
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }

    /// Cleanup after a cancelled or failed launch/attach: drop the backend and reset to
    /// idle (Go `cleanupSubprocess`). The backend's drop kills the subprocess.
    async fn cleanup_after_cancel(&self) {
        self.clear_backend().await;
        self.session.reset();
    }
}

/// Map a `connect()` failure to the Go find/spawn strings (Spec FR-4.4.2/4.4.3).
fn connect_error(err: BackendError) -> String {
    match err {
        BackendError::Detect(m) => format!("failed to find lldb-dap: {m}"),
        BackendError::Spawn(m) => format!("failed to spawn lldb-dap: {m}"),
        // connect() only produces Detect/Spawn; anything else surfaces verbatim.
        other => other.to_string(),
    }
}

/// Map a `backend.launch` failure. The backend already phrased the per-step Go string in
/// the `Dap` message (`initialize failed: …`, `launch failed: …`, etc.); render it
/// verbatim. A `Protocol`/transport error surfaces with the neutral cause.
fn launch_error(err: BackendError) -> String {
    handshake_error(err)
}

/// Map a `backend.attach` failure (same shape as launch — the backend carries the Go
/// per-step wording in the message).
fn attach_error(err: BackendError) -> String {
    handshake_error(err)
}

fn handshake_error(err: BackendError) -> String {
    match err {
        // The backend already built the full Go string (`initialize failed: …`,
        // `launch failed: …`, `setExceptionBreakpoints failed: …`, …).
        BackendError::Dap { message } => message,
        // A wrong-typed handshake response: `unexpected <op> response type: <type>`. The
        // backend stamped `"<op>:<label>"`; recover `<op>` and `<label>`.
        BackendError::Protocol { ty } => match ty.split_once(':') {
            Some((op, label)) => format!("unexpected {op} response type: {label}"),
            None => format!("unexpected response type: {ty}"),
        },
        BackendError::Closed => "connection closed".to_string(),
        other => other.to_string(),
    }
}
