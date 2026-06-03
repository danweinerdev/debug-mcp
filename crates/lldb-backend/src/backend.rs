//! `LldbBackend` — the lldb-dap implementation of [`DebuggerBackend`] (tasks 3.3/3.4).
//!
//! Orchestrates `dap-client`: it owns the launch/attach handshake (including the
//! order-independent `InitializedEvent` wait via [`Client::send_and_await_both`] and the
//! **stop-waiter placement asymmetry** — register-before-configurationDone on launch,
//! register-after on attach), the lldb-dap arg shapes, the repl-mode/backtick decision,
//! and the DAP→neutral translation for every op. No response *formatting* lives here
//! (hex dump / variable flatten / JSON shaping are Phase 5).
//!
//! Handshake failures are surfaced as [`BackendError::Dap`] carrying the **exact Go
//! string** (e.g. `initialize failed: <msg>`, `setBreakpoints failed for <file>: <err>`)
//! so the Phase 5 launch/attach handler can render them verbatim; the coarse trait
//! returns a single `BackendError`, so the per-step prefix must travel in the message.
//! Go origin: `internal/tools/launch.go`, `internal/tools/attach.go`, and the
//! DAP-issuing parts of the other tool files.
//!
//! **Empty-message fallback (deviation from the Go oracle).** lldb-dap often reports a
//! `success=false` handshake response with an *empty* `message`, writing the real cause
//! (bad program path, permission denied) only to stderr. The Go server surfaced the bare
//! `launch failed: ` in that case; here [`LldbBackend::diagnostic`] folds the captured
//! stderr ring into the error instead, so the user-facing string is never empty. A
//! non-empty DAP message is always preferred and used verbatim (full Go parity on that
//! path).

use std::sync::Arc;

use async_trait::async_trait;
use dap_client::{Client, DapMessage, Request, Response};
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BreakpointResult, DebuggerBackend, EvalMode,
    EvalResult, Frame, FunctionBp, Granularity, Instruction, LaunchOutcome, LaunchSpec, MemoryRead,
    Scope, SourceBp, StepKind, StopOutcome, ThreadInfo, Variable,
};
use tokio::io::AsyncWrite;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::args::{attach_args_to_value, launch_args_to_value, LldbAttachArgs, LldbLaunchArgs};
use crate::subprocess::StderrBuffer;
use crate::{body, requests};

/// The lldb-dap backend over a `dap-client::Client`. Generic over the writer `W` (the
/// subprocess stdin in production, a `tokio::io::duplex` peer in tests).
pub struct LldbBackend<W> {
    client: Client<W>,
    /// Whether `--repl-mode=command` was passed (Spec FR-15 capability flag). Drives
    /// [`DebuggerBackend::supports_command_repl_mode`] and the `run_command` backtick.
    is_lldb_dap: bool,
    /// The child handle, taken on `disconnect` to kill/wait the subprocess. `None` in
    /// scripted-peer tests (there is no real child).
    child: Mutex<Option<Child>>,
    /// The lldb-dap subprocess OS pid, captured at construction (the child's pid stays
    /// valid until reaped, but `Child::id()` returns `None` after the child is `wait`ed,
    /// so we snapshot it up front). `None` for scripted-peer tests with no real child.
    /// Surfaced via [`DebuggerBackend::debugger_pid`] so the launch/attach handler can
    /// record it on the session (Go `SetPID(sub.Cmd.Process.Pid)`).
    pid: Option<i64>,
    /// The lldb-dap stderr ring (last 4 KB). When a handshake fails with an empty DAP
    /// `message` (lldb-dap commonly reports the real cause — bad program path, missing
    /// permissions — only on stderr), this is folded into the error so the user-facing
    /// string is never the bare `launch failed: ` / `attach failed: `. `None` in
    /// scripted-peer tests with no real subprocess.
    stderr: Option<Arc<StderrBuffer>>,
}

impl<W> LldbBackend<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Build a backend over an existing client + capability flag, holding the child for
    /// teardown. Used by the factory after spawn + read-loop start. The child's pid is
    /// snapshotted now (before any `wait`, which would invalidate `Child::id()`).
    pub fn new(client: Client<W>, is_lldb_dap: bool, child: Option<Child>) -> Self {
        Self::with_stderr(client, is_lldb_dap, child, None)
    }

    /// Like [`Self::new`] but also wires the lldb-dap stderr ring, so an empty-message
    /// handshake failure can fall back to the captured stderr (the factory uses this; the
    /// scripted-peer tests construct one directly to exercise the fallback).
    pub fn with_stderr(
        client: Client<W>,
        is_lldb_dap: bool,
        child: Option<Child>,
        stderr: Option<Arc<StderrBuffer>>,
    ) -> Self {
        let pid = child.as_ref().and_then(|c| c.id()).map(i64::from);
        LldbBackend {
            client,
            is_lldb_dap,
            child: Mutex::new(child),
            pid,
            stderr,
        }
    }

    /// The captured lldb-dap stderr (trimmed), or empty when unavailable. Used as the
    /// fallback diagnostic when a handshake response carries an empty `message`.
    fn stderr_snapshot(&self) -> String {
        self.stderr
            .as_ref()
            .map(|b| b.contents().trim().to_string())
            .unwrap_or_default()
    }

    /// Send a request and check it resolved to a successful [`Response`] for `op`,
    /// returning the response. Reproduces Go's per-op error triad: `<op> request failed:
    /// <err>` (send), `unexpected <op> response type: <type>` (wrong message), and
    /// `<op> failed: <message>` (`success=false`). `op` is the Go error-string verb (e.g.
    /// `setBreakpoints`), used in all three forms.
    async fn send_checked(&self, op: &str, request: Request) -> Result<Response, BackendError> {
        let message = self
            .client
            .send(request)
            .await
            .map_err(|e| dap_err(format!("{op} request failed: {e}")))?;
        self.check_response(op, op, message)
    }

    /// Check a handshake/op response for `op`. `op` is the request verb (for the
    /// `unexpected <op> response type`) and `failed_op` the `<failed_op> failed` verb. On a
    /// `success=false` response with an **empty** `message`, the captured lldb-dap stderr is
    /// folded in so the error is never the bare `<failed_op> failed: ` (review finding 3);
    /// a non-empty DAP message is preferred and used verbatim.
    fn check_response(
        &self,
        op: &str,
        failed_op: &str,
        message: DapMessage,
    ) -> Result<Response, BackendError> {
        match message {
            DapMessage::Response(resp) => {
                if resp.success {
                    Ok(resp)
                } else {
                    Err(dap_err(format!(
                        "{failed_op} failed: {}",
                        self.diagnostic(&resp.message)
                    )))
                }
            }
            // A non-response (or wrong-typed) message: Go's `unexpected <op> response type:
            // <%T>`. The neutral surface is `Protocol{ty}`; Phase 5 renders the wrapping.
            other => Err(BackendError::Protocol {
                ty: format!("{op}:{}", message_type_label(&other)),
            }),
        }
    }

    /// Like [`Self::check_response`] but discards the response body (used where the caller
    /// does not consume it).
    fn check_response_msg(
        &self,
        op: &str,
        failed_op: &str,
        message: DapMessage,
    ) -> Result<(), BackendError> {
        self.check_response(op, failed_op, message).map(|_| ())
    }

    /// Pick the diagnostic text for a `success=false` handshake/op response: the DAP
    /// `message` when present, else the trimmed lldb-dap stderr ring (so the user-facing
    /// error is never empty). When both are empty, falls back to a literal so the wording
    /// still parses as `<op> failed: <text>`.
    fn diagnostic(&self, dap_message: &str) -> String {
        if !dap_message.is_empty() {
            return dap_message.to_string();
        }
        let stderr = self.stderr_snapshot();
        if !stderr.is_empty() {
            return stderr;
        }
        "no error message provided by lldb-dap".to_string()
    }

    /// Run the launch handshake (Go `launch.go` steps 9–18).
    ///
    /// **Response/`initialized` decoupling.** Real lldb-dap defers the *launch response*
    /// until after `configurationDone`, while emitting `initialized` right after the
    /// launch request. The handshake therefore (a) sends launch with `send_async` (no
    /// block), (b) awaits `initialized` — which gates configuration, (c) flushes
    /// breakpoints + exception filters + `configurationDone`, then (d) awaits the deferred
    /// launch response. An adapter that answers launch *before* `configurationDone` is
    /// equally handled: the response receiver simply already holds the value at step (d).
    /// This is the order-independent `InitializedEvent`/response handling the spec
    /// mandates (Spec FR-4.4.9), made robust to the deferred-response variant.
    ///
    /// **Stop-waiter placement.** When `stop_on_entry`, the stop waiter is registered
    /// **before** `configurationDone` (Go `launch.go:304`) so the StoppedEvent that fires
    /// immediately after `configurationDone` is not lost — load-bearing, see the timing
    /// test.
    async fn run_launch(&self, spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        self.initialize().await?;

        // Send launch (do NOT block on the response — it may be deferred to after
        // configurationDone). Map a write failure to the Go `launch request failed: <err>`.
        let launch_args = LldbLaunchArgs::new(
            spec.program.clone(),
            spec.args.clone(),
            spec.cwd.clone(),
            spec.env.clone(),
            spec.stop_on_entry,
        );
        let launch_value = launch_args_to_value(&launch_args);
        let launch_rx = self
            .client
            .send_async(requests::launch(launch_value))
            .await
            .map_err(|e| dap_err(format!("launch request failed: {e}")))?;

        // Await `initialized` (the read loop latched it; resolves immediately if already
        // fired). This gates configuration, exactly as the DAP base protocol prescribes.
        self.client.wait_initialized().await;

        // Flush pending breakpoints (launch only — attach does not).
        for (file, bps) in &spec.source_breakpoints {
            self.send_checked_breakpoints_for_file(file, bps).await?;
        }
        if !spec.function_breakpoints.is_empty() {
            let msg = self
                .client
                .send(requests::set_function_breakpoints(
                    &spec.function_breakpoints,
                ))
                .await
                .map_err(|e| dap_err(format!("setFunctionBreakpoints failed: {e}")))?;
            self.check_response_msg("setFunctionBreakpoints", "setFunctionBreakpoints", msg)?;
        }

        // setExceptionBreakpoints (empty filters) — send only, errors mapped, no
        // type/success check (Go discards `_, err`).
        self.client
            .send(requests::set_exception_breakpoints())
            .await
            .map_err(|e| dap_err(format!("setExceptionBreakpoints failed: {e}")))?;

        // Register the stop waiter BEFORE configurationDone when stop_on_entry, so the
        // StoppedEvent fired immediately after configurationDone is not lost to a race
        // (Go `launch.go:304-307`). This placement is load-bearing — see the timing test.
        let waiter = if spec.stop_on_entry {
            Some(self.client.stop_waiter().register())
        } else {
            None
        };

        // configurationDone — send only, error mapped, no type/success check.
        self.client
            .send(requests::configuration_done())
            .await
            .map_err(|e| dap_err(format!("configurationDone failed: {e}")))?;

        // Now await the (possibly deferred) launch response and check it
        // (`launch failed: <message>` / `unexpected launch response type: <type>`).
        let launch_msg = match launch_rx.await {
            Ok(result) => result.map_err(|e| dap_err(format!("launch request failed: {e}")))?,
            Err(_) => return Err(BackendError::Closed),
        };
        self.check_response("launch", "launch", launch_msg)?;

        // Handle stop_on_entry.
        match waiter {
            Some(rx) => match rx.await {
                Ok(StopOutcome::Stopped(info)) => Ok(LaunchOutcome::Stopped(info)),
                Ok(StopOutcome::Exited { code }) => Ok(LaunchOutcome::Exited { code }),
                // Terminated during launch → "Program exited during launch" upstream;
                // the neutral outcome is Exited{None} (matches Go treating exit||terminated
                // as the early-exit branch).
                Ok(StopOutcome::Terminated) => Ok(LaunchOutcome::Exited { code: None }),
                // The stop-waiter sender dropped without delivering: the connection tore
                // down. Treat as exited-during-launch.
                Err(_) => Ok(LaunchOutcome::Exited { code: None }),
            },
            None => Ok(LaunchOutcome::Running),
        }
    }

    /// Run the attach handshake (Go `attach.go` steps 9–16).
    ///
    /// Same response/`initialized` decoupling as [`Self::run_launch`] (the attach response
    /// may be deferred to after `configurationDone`): send attach with `send_async`, await
    /// `initialized`, run setExceptionBreakpoints + `configurationDone`, await the deferred
    /// attach response. No breakpoint flush.
    ///
    /// **Stop-waiter placement.** Registered **after** `configurationDone` (Go
    /// `attach.go:219`) — the inverse of launch.
    async fn run_attach(&self, spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        self.initialize().await?;

        let attach_args = LldbAttachArgs::new(spec.pid, spec.wait_for.clone());
        let attach_value = attach_args_to_value(&attach_args);
        let attach_rx = self
            .client
            .send_async(requests::attach(attach_value))
            .await
            .map_err(|e| dap_err(format!("attach request failed: {e}")))?;

        self.client.wait_initialized().await;

        // setExceptionBreakpoints (empty filters) → configurationDone, send-only checks.
        self.client
            .send(requests::set_exception_breakpoints())
            .await
            .map_err(|e| dap_err(format!("setExceptionBreakpoints failed: {e}")))?;

        self.client
            .send(requests::configuration_done())
            .await
            .map_err(|e| dap_err(format!("configurationDone failed: {e}")))?;

        // Await the (possibly deferred) attach response and check it.
        let attach_msg = match attach_rx.await {
            Ok(result) => result.map_err(|e| dap_err(format!("attach request failed: {e}")))?,
            Err(_) => return Err(BackendError::Closed),
        };
        self.check_response("attach", "attach", attach_msg)?;

        // Register the stop waiter AFTER configurationDone (attach asymmetry).
        let rx = self.client.stop_waiter().register();
        match rx.await {
            Ok(StopOutcome::Stopped(info)) => Ok(AttachOutcome::Stopped(info)),
            Ok(StopOutcome::Exited { code }) => Ok(AttachOutcome::Exited { code }),
            Ok(StopOutcome::Terminated) => Ok(AttachOutcome::Terminated),
            Err(_) => Ok(AttachOutcome::Terminated),
        }
    }

    /// initialize: send + check (shared by launch/attach). Go strings:
    /// `initialize request failed: <err>` / `unexpected initialize response type: <type>`
    /// / `initialize failed: <message>`.
    async fn initialize(&self) -> Result<(), BackendError> {
        let msg = self
            .client
            .send(requests::initialize())
            .await
            .map_err(|e| dap_err(format!("initialize request failed: {e}")))?;
        self.check_response("initialize", "initialize", msg)?;
        Ok(())
    }

    /// One file's `setBreakpoints` during the launch flush, with the
    /// `setBreakpoints failed for <file>: <…>` error wording (Go `launch.go:223/233`).
    async fn send_checked_breakpoints_for_file(
        &self,
        file: &str,
        bps: &[SourceBp],
    ) -> Result<(), BackendError> {
        let msg = self
            .client
            .send(requests::set_breakpoints(file, bps))
            .await
            .map_err(|e| dap_err(format!("setBreakpoints failed for {file}: {e}")))?;
        match msg {
            DapMessage::Response(resp) if resp.command == "setBreakpoints" => {
                if resp.success {
                    Ok(())
                } else {
                    Err(dap_err(format!(
                        "setBreakpoints failed for {file}: {}",
                        self.diagnostic(&resp.message)
                    )))
                }
            }
            DapMessage::Response(resp) => Err(BackendError::Protocol {
                ty: response_type_label(&resp),
            }),
            other => Err(BackendError::Protocol {
                ty: message_type_label(&other),
            }),
        }
    }

    /// Resolve a step `StepKind` to the DAP command name (Go's distinct handlers).
    fn step_command(kind: StepKind) -> &'static str {
        match kind {
            StepKind::Over => "next",
            StepKind::Into => "stepIn",
            StepKind::Out => "stepOut",
        }
    }

    /// Map a neutral granularity to the DAP `granularity` string (only `step_over`/
    /// `step_into` carry it; `step_out` passes `None`).
    fn granularity_str(g: Granularity) -> &'static str {
        match g {
            Granularity::Line => "line",
            Granularity::Instruction => "instruction",
        }
    }

    /// Register the stop waiter, send a resume request, and await the next outcome
    /// (cont/step). The waiter is registered **before** the send (Spec FR-8.3). On a
    /// send error the waiter is simply dropped (its receiver is discarded) and the send
    /// error is returned; the session reverts state above the seam.
    async fn resume_and_wait(&self, request: Request) -> Result<StopOutcome, BackendError> {
        let rx = self.client.stop_waiter().register();
        self.client
            .send(request)
            .await
            .map_err(|e| BackendError::Send(e.to_string()))?;
        match rx.await {
            Ok(outcome) => Ok(outcome),
            // The connection tore down before any outcome was delivered.
            Err(_) => Ok(StopOutcome::Terminated),
        }
    }
}

#[async_trait]
impl<W> DebuggerBackend for LldbBackend<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    async fn launch(&self, spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        self.run_launch(spec).await
    }

    async fn attach(&self, spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        self.run_attach(spec).await
    }

    async fn disconnect(&self, terminate: bool) {
        // Best-effort DisconnectRequest (errors ignored, Spec FR-6). The 5-second bounds
        // are applied by the Phase 5 handler / disconnect logic; here we issue the
        // request and tear the child down.
        let _ = self.client.send(requests::disconnect(terminate)).await;

        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            // Close stdin is implicit via the dropped client writer on backend drop; here
            // we kill + reap to avoid a zombie if the process is still alive.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    async fn set_source_breakpoints(
        &self,
        file: &str,
        bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        let resp = self
            .send_checked("setBreakpoints", requests::set_breakpoints(file, bps))
            .await?;
        body::breakpoints(&resp.body)
    }

    async fn set_function_breakpoints(
        &self,
        bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        let resp = self
            .send_checked(
                "setFunctionBreakpoints",
                requests::set_function_breakpoints(bps),
            )
            .await?;
        body::breakpoints(&resp.body)
    }

    async fn cont(&self, thread_id: i64) -> Result<StopOutcome, BackendError> {
        self.resume_and_wait(requests::cont(thread_id)).await
    }

    async fn step(
        &self,
        kind: StepKind,
        thread_id: i64,
        gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError> {
        let command = Self::step_command(kind);
        // step_out never carries a granularity (Go has no granularity param on step_out).
        let gran = match kind {
            StepKind::Out => None,
            _ => gran.map(Self::granularity_str),
        };
        self.resume_and_wait(requests::step(command, thread_id, gran))
            .await
    }

    async fn pause(&self) -> Result<(), BackendError> {
        // Go checks the pause response type/success; reproduce that (the error surfaces
        // as `pause failed: <message>` / `unexpected pause response type` upstream).
        self.send_checked("pause", requests::pause()).await?;
        Ok(())
    }

    async fn threads(&self) -> Result<Vec<ThreadInfo>, BackendError> {
        let resp = self.send_checked("threads", requests::threads()).await?;
        body::threads(&resp.body)
    }

    async fn stack_trace(
        &self,
        thread_id: i64,
        start: i64,
        levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError> {
        let resp = self
            .send_checked(
                "stackTrace",
                requests::stack_trace(thread_id, start, levels),
            )
            .await?;
        body::stack_trace(&resp.body)
    }

    async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, BackendError> {
        let resp = self
            .send_checked("scopes", requests::scopes(frame_id))
            .await?;
        body::scopes(&resp.body)
    }

    async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        let resp = self
            .send_checked("variables", requests::variables(variables_reference))
            .await?;
        body::variables(&resp.body)
    }

    async fn evaluate(
        &self,
        expr: &str,
        frame_id: Option<i64>,
        mode: EvalMode,
    ) -> Result<EvalResult, BackendError> {
        let request = match mode {
            // expression evaluation: context="variables" with the resolved frame id.
            EvalMode::Expression => requests::evaluate(expr, frame_id, "variables"),
            // repl/command: context="repl", NO frame id; prepend a backtick iff the
            // backend does not support command repl mode (legacy lldb-vscode). The
            // handler passes the raw command (Spec FR-14.2 — the backtick lives here).
            EvalMode::Repl => {
                let expr = if self.is_lldb_dap {
                    expr.to_string()
                } else {
                    format!("`{expr}")
                };
                requests::evaluate(&expr, None, "repl")
            }
        };
        let resp = self.send_checked("evaluate", request).await?;
        body::evaluate(&resp.body)
    }

    async fn read_memory(&self, address: &str, count: i64) -> Result<MemoryRead, BackendError> {
        let resp = self
            .send_checked("readMemory", requests::read_memory(address, count))
            .await?;
        body::read_memory(&resp.body)
    }

    async fn disassemble(
        &self,
        address: &str,
        count: i64,
    ) -> Result<Vec<Instruction>, BackendError> {
        let resp = self
            .send_checked("disassemble", requests::disassemble(address, count))
            .await?;
        body::disassemble(&resp.body)
    }

    fn supports_command_repl_mode(&self) -> bool {
        self.is_lldb_dap
    }

    fn debugger_pid(&self) -> Option<i64> {
        self.pid
    }
}

/// Build a [`BackendError::Dap`] carrying a full Go error string (handshake-internal
/// failures travel in the message; see the module doc).
fn dap_err(message: String) -> BackendError {
    BackendError::Dap { message }
}

/// A human label for a non-response message's "type", standing in for Go's `%T`.
fn message_type_label(message: &DapMessage) -> String {
    match message {
        DapMessage::Response(resp) => response_type_label(resp),
        DapMessage::Event(_) => "event".to_string(),
        DapMessage::Other(env) => {
            if let Some(event) = &env.event {
                format!("event:{event}")
            } else if let Some(command) = &env.command {
                format!("{}:{command}", env.ty)
            } else {
                env.ty.clone()
            }
        }
    }
}

fn response_type_label(resp: &Response) -> String {
    format!("response:{}", resp.command)
}
