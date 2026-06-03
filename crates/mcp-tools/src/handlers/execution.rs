//! Execution handlers: `continue`, `step_over`, `step_into`, `step_out`, `pause`, and the
//! shared stop-outcome formatter (Spec FR-8, task 5.3).
//!
//! The resume tools guard `stopped`, resolve the thread id (explicit arg → last-stopped →
//! 1), set state `running`, then await the backend racing the request cancellation token.
//! On a backend send error the state reverts to `stopped`; on cancellation the state is
//! left `running` (Go parity — recover with `pause`). The post-stop transition is applied
//! generation-guarded so a `continue` returning after a concurrent `disconnect` cannot
//! clobber the reset idle state (design Decision 6).

use debugger_core::{BackendError, Granularity, StepKind, StopInfo, StopOutcome};
use mcp_session::State;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::errors::{self, OpError};
use crate::format::format_output_entries;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

/// What kind of resume a handler is performing — selects the backend call, the
/// granularity handling, and the error/timeout wording.
enum Resume {
    Continue,
    Step(StepKind),
}

impl ToolServer {
    /// `continue` (Spec FR-8). Guard stopped → resolve thread → run → format stop.
    pub(crate) async fn handle_continue(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        self.resume(
            args,
            ct,
            Resume::Continue,
            &errors::CONTINUE,
            CONTINUE_TIMEOUT,
        )
        .await
    }

    /// `step_over` (DAP `next`, +granularity).
    pub(crate) async fn handle_step_over(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        self.resume(
            args,
            ct,
            Resume::Step(StepKind::Over),
            &errors::STEP_OVER,
            STEP_OVER_TIMEOUT,
        )
        .await
    }

    /// `step_into` (DAP `stepIn`, +granularity).
    pub(crate) async fn handle_step_into(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        self.resume(
            args,
            ct,
            Resume::Step(StepKind::Into),
            &errors::STEP_INTO,
            STEP_INTO_TIMEOUT,
        )
        .await
    }

    /// `step_out` (DAP `stepOut`, no granularity).
    pub(crate) async fn handle_step_out(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        self.resume(
            args,
            ct,
            Resume::Step(StepKind::Out),
            &errors::STEP_OUT,
            STEP_OUT_TIMEOUT,
        )
        .await
    }

    /// The shared resume path (Go's per-tool `handleContinue`/`handleStep*`).
    async fn resume(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
        resume: Resume,
        op_err: &OpError,
        timeout_msg: &'static str,
    ) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let thread_id = self.resolve_thread_id(args);

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(op_err.request_failed.to_string()),
        };

        // Snapshot the generation before awaiting, so the post-stop transition can be
        // generation-guarded against a concurrent disconnect.
        let generation = self.session.generation();

        // Set state running (the waiter is registered before the send inside the backend).
        self.session.set_state(State::Running);

        let granularity = match &resume {
            Resume::Continue => None,
            Resume::Step(_) => parse_granularity(args),
        };

        // Race the backend call against the request cancellation token. On cancel the
        // backend future is dropped (cancel-safe) and the state is left running.
        let result = tokio::select! {
            r = run_backend(&backend, &resume, thread_id, granularity) => r,
            () = ct.cancelled() => {
                return ToolOutcome::error(timeout_msg);
            }
        };

        match result {
            Ok(outcome) => self.handle_stop_result(outcome, generation),
            Err(e) => {
                // Send error: revert to stopped and return the per-tool request-failed
                // string (Go reverts on a send failure).
                self.session.set_state(State::Stopped);
                ToolOutcome::error(op_err.render(e))
            }
        }
    }

    /// `pause` (Spec FR-8). Guard running → `backend.pause()` → no state change →
    /// `{status:"pause_requested", message:…}`. The blocked `continue`/`step` returns when
    /// the resulting stop arrives.
    pub(crate) async fn handle_pause(&self, _args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Running]) {
            return ToolOutcome::error(e);
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::PAUSE.request_failed.to_string()),
        };

        if let Err(e) = backend.pause().await {
            return ToolOutcome::error(errors::PAUSE.render(e));
        }

        // Pause does NOT change state.
        ToolOutcome::Json(
            RespBuilder::new()
                .set("status", "pause_requested")
                .set(
                    "message",
                    "Pause request sent. The running continue/step operation will return when the process stops.",
                )
                .build(),
        )
    }

    /// Resolve the thread id: explicit numeric `thread_id` arg → last-stopped thread → 1
    /// (Go's `handleContinue` thread resolution).
    fn resolve_thread_id(&self, args: &Args<'_>) -> i64 {
        if let Some(raw) = args.get_raw("thread_id").filter(|v| !v.is_null()) {
            if let Some(tid) = raw.as_f64() {
                return tid as i64;
            }
        }
        self.session
            .last_stopped()
            .map(|e| e.thread_id)
            .unwrap_or(1)
    }

    /// The shared stop-result formatter (Go `handleStopResult`). Applies the post-stop
    /// state transition generation-guarded, drains + merges output, caches last-stopped.
    fn handle_stop_result(&self, outcome: StopOutcome, generation: u64) -> ToolOutcome {
        match outcome {
            StopOutcome::Stopped(info) => {
                self.session
                    .set_state_if_generation(generation, State::Stopped);
                self.session.set_last_stopped(info.clone());

                let entries = self.session.output_buffer().drain();
                let mut builder = stopped_response(&info);
                merge_output(&mut builder, &entries);
                builder.into_outcome()
            }
            StopOutcome::Exited { code } => {
                self.session
                    .set_state_if_generation(generation, State::Terminated);

                let entries = self.session.output_buffer().drain();
                let mut builder = RespBuilder::new().set("status", "exited");
                if let Some(code) = code {
                    builder = builder.set("exit_code", code);
                }
                merge_output(&mut builder, &entries);
                builder.into_outcome()
            }
            StopOutcome::Terminated => {
                self.session
                    .set_state_if_generation(generation, State::Terminated);
                ToolOutcome::Json(
                    RespBuilder::new()
                        .set("status", "terminated")
                        .set("message", "Debug session ended")
                        .build(),
                )
            }
        }
    }
}

/// Build the base `stopped` response object (status/reason/thread_id/description +
/// hit_breakpoint_ids when non-empty). Output is merged afterward by the caller.
fn stopped_response(info: &StopInfo) -> RespBuilder {
    RespBuilder::new()
        .set("status", "stopped")
        .set("reason", info.reason.clone())
        .set("thread_id", info.thread_id)
        .set("description", info.description.clone())
        .set_if(
            !info.hit_breakpoint_ids.is_empty(),
            "hit_breakpoint_ids",
            Value::from(info.hit_breakpoint_ids.clone()),
        )
}

/// Merge the formatted output entries (FR-12) into a response object, exactly as Go's
/// `handleStopResult` ranges `formatOutputEntries(entries)` into the result map.
fn merge_output(builder: &mut RespBuilder, entries: &[mcp_session::OutputEntry]) {
    if let Value::Object(map) = format_output_entries(entries) {
        for (k, v) in map {
            builder.insert(&k, v);
        }
    }
}

/// Dispatch the backend resume call for the given [`Resume`].
async fn run_backend(
    backend: &std::sync::Arc<dyn debugger_core::DebuggerBackend>,
    resume: &Resume,
    thread_id: i64,
    granularity: Option<Granularity>,
) -> Result<StopOutcome, BackendError> {
    match resume {
        Resume::Continue => backend.cont(thread_id).await,
        Resume::Step(kind) => backend.step(*kind, thread_id, granularity).await,
    }
}

/// Parse the optional `granularity` enum (`line`|`instruction`). Applied to the DAP request
/// only when a non-empty value is given (Go's "set granularity only when non-empty").
fn parse_granularity(args: &Args<'_>) -> Option<Granularity> {
    match args.get_string("granularity", "").as_str() {
        "line" => Some(Granularity::Line),
        "instruction" => Some(Granularity::Instruction),
        _ => None,
    }
}

const CONTINUE_TIMEOUT: &str = "continue timed out; process still running, use 'pause' to stop it";
const STEP_OVER_TIMEOUT: &str =
    "step over timed out; process still running, use 'pause' to stop it";
const STEP_INTO_TIMEOUT: &str =
    "step into timed out; process still running, use 'pause' to stop it";
const STEP_OUT_TIMEOUT: &str = "step out timed out; process still running, use 'pause' to stop it";
