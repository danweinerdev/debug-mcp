//! Capability-gated WinDbg handlers: `open_crash_dump`, `attach_kernel`, `analyze_crash`,
//! `get_modules` (design §"Tool surface (additions)", task 1.4).
//!
//! `open_crash_dump`/`attach_kernel` are full **connect points** — they mirror the
//! `launch`/`attach` flow (guard idle → force-select the `windbg` factory → `connect()` →
//! store backend + spawn the event-pump before the backend call → map the outcome → reset
//! on failure). Unlike `launch`/`attach` they never read a `backend` arg: the backend is
//! hardcoded to `"windbg"`, so on a platform without a registered windbg factory they
//! resolve to a clear "not available" tool-error (the intended Phase-1 behavior).
//!
//! `analyze_crash`/`get_modules` operate on the **already-connected** backend (no connect):
//! they guard `Stopped`, require a live backend, and map `Unsupported` to the standard
//! `"<tool> is not supported by the <backend> backend"` string (lldb in practice today).

use std::sync::Arc;

use debugger_core::{AttachOutcome, BackendError, DumpOutcome};
use mcp_session::{State, spawn_event_pump};
use tokio_util::sync::CancellationToken;

use crate::Args;
use crate::handlers::lifecycle::connect_error;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;

/// The hardcoded backend name the two crash/kernel connect-point tools force-select. These
/// verbs are WinDbg-only, so they never honor a `backend` arg.
const WINDBG: &str = "windbg";

impl ToolServer {
    /// `open_crash_dump` (caps.crash_dump). A connect point: guard idle → require
    /// `dump_path` → force-select the windbg factory → connect → store + pump → load the
    /// dump (raced against cancellation) → map the [`DumpOutcome`]. Resets to idle on any
    /// failure (connect-failure cleanup).
    pub(crate) async fn handle_open_crash_dump(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle]) {
            return ToolOutcome::error(e);
        }

        let dump_path = match args.require_string("dump_path") {
            Ok(p) => p,
            Err(e) => return ToolOutcome::error(e),
        };

        self.session.set_state(State::Configuring);

        // Force-select windbg (this verb never honors a `backend` arg). On a platform with
        // no windbg factory the registry reports "unknown backend"; wrap it as a clear
        // not-available message rather than surfacing the raw registry string.
        let factory = match self.registry.select(Some(WINDBG)) {
            Ok(f) => f,
            Err(_) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(not_available("open_crash_dump"));
            }
        };

        let connection = match factory.connect().await {
            Ok(c) => c,
            Err(e) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(connect_error(factory.name(), e));
            }
        };

        // Store the backend and spawn the event-pump BEFORE awaiting the dump load (same
        // ordering as launch, so a Terminated during loading reaches the session).
        self.set_backend(Arc::clone(&connection.backend), factory.name())
            .await;
        let generation = self.session.generation();
        spawn_event_pump(connection.events, Arc::clone(&self.session), generation);

        let outcome = tokio::select! {
            r = connection.backend.open_dump(&dump_path) => r,
            () = ct.cancelled() => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error("open_crash_dump timed out: context canceled");
            }
        };

        let outcome: DumpOutcome = match outcome {
            Ok(o) => o,
            Err(BackendError::Unsupported(tool)) => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(unsupported(tool, factory.name()));
            }
            Err(e) => {
                // Post-connect failure (Dap/Protocol/Send/Closed/Timeout) of the dump load —
                // NOT a connect-phase Detect/Spawn, so use the `request failed` wording rather
                // than `connect_error` (which only renders Detect/Spawn).
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(format!("open_crash_dump request failed: {e}"));
            }
        };

        // A dump session is inherently "stopped". `stop` may be `None` (the dump yielded no
        // faulting-thread context); that is valid — the response simply omits stop_reason and
        // a later `backtrace` falls back to the default thread. Phase 4 + the is_dump flag
        // (task 1.4b) refine dump-thread handling.
        self.session.set_state(State::Stopped);
        // Freeze the dump-session contract (task 1.4b): a dump session is Stopped but cannot
        // be resumed — the execution handlers reject continue/step_* while this flag holds.
        self.session.set_dump(true);

        let mut builder = RespBuilder::new()
            .set("status", "dump_loaded")
            .set("program", dump_path);
        if let Some(info) = outcome.stop {
            let reason = info.reason.clone();
            let thread_id = info.thread_id;
            self.session.set_last_stopped(info);
            builder = builder
                .set("stop_reason", reason)
                .set("stopped_thread_id", thread_id);
        }
        if let Some(loc) = outcome.crash_location {
            builder = builder.set("crash_location", loc);
        }
        builder.into_outcome()
    }

    /// `attach_kernel` (caps.kernel). A connect point of the same shape as
    /// `open_crash_dump`: guard idle → require + validate the KDNET `connection` →
    /// force-select windbg → connect → store + pump → attach (raced against cancellation) →
    /// map the [`AttachOutcome`].
    pub(crate) async fn handle_attach_kernel(
        &self,
        args: &Args<'_>,
        ct: &CancellationToken,
    ) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Idle]) {
            return ToolOutcome::error(e);
        }

        let connection_str = match args.require_string("connection") {
            Ok(c) => c,
            Err(e) => return ToolOutcome::error(e),
        };
        if !connection_str.starts_with("net:") {
            return ToolOutcome::error(
                "'connection' must be a KDNET connection string starting with 'net:'",
            );
        }

        self.session.set_state(State::Configuring);

        let factory = match self.registry.select(Some(WINDBG)) {
            Ok(f) => f,
            Err(_) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(not_available("attach_kernel"));
            }
        };

        let connection = match factory.connect().await {
            Ok(c) => c,
            Err(e) => {
                self.session.reset();
                self.clear_backend().await;
                return ToolOutcome::error(connect_error(factory.name(), e));
            }
        };

        self.set_backend(Arc::clone(&connection.backend), factory.name())
            .await;
        let generation = self.session.generation();
        spawn_event_pump(connection.events, Arc::clone(&self.session), generation);

        let outcome = tokio::select! {
            r = connection.backend.attach_kernel(&connection_str) => r,
            () = ct.cancelled() => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error("attach_kernel timed out: context canceled");
            }
        };

        let outcome: AttachOutcome = match outcome {
            Ok(o) => o,
            Err(BackendError::Unsupported(tool)) => {
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(unsupported(tool, factory.name()));
            }
            Err(e) => {
                // Post-connect failure of the kernel attach — use the `request failed` wording
                // (connect-phase Detect/Spawn is handled by the `factory.connect()` arm above).
                self.cleanup_after_cancel().await;
                return ToolOutcome::error(format!("attach_kernel request failed: {e}"));
            }
        };

        match outcome {
            AttachOutcome::Stopped(info) => {
                self.session.set_state(State::Stopped);
                let reason = info.reason.clone();
                let thread_id = info.thread_id;
                self.session.set_last_stopped(info);
                ToolOutcome::Json(
                    RespBuilder::new()
                        .set("status", "kernel_attached")
                        .set("connection", connection_str)
                        .set("state", "stopped")
                        .set("stop_reason", reason)
                        .set("stopped_thread_id", thread_id)
                        .build(),
                )
            }
            AttachOutcome::Exited { .. } | AttachOutcome::Terminated => {
                // The kernel target died/was unreachable mid-handshake (mirrors `handle_attach`).
                // The backend stays in the slot; the agent must `disconnect` to recover to idle.
                self.session.set_state(State::Terminated);
                ToolOutcome::text("Kernel session ended during attach")
            }
        }
    }

    /// `analyze_crash` (caps.analyze). Operates on the active backend (NOT a connect point):
    /// guard `Stopped` + a live backend, run the backend's `analyze()`, and return its raw
    /// report under `{"analysis": <text>}`. `Unsupported` maps to the standard string.
    pub(crate) async fn handle_analyze_crash(&self) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error("analyze_crash request failed: connection closed"),
        };

        match backend.analyze().await {
            Ok(text) => ToolOutcome::Json(RespBuilder::new().set("analysis", text).build()),
            Err(BackendError::Unsupported(tool)) => {
                ToolOutcome::error(self.unsupported_active(tool))
            }
            Err(e) => ToolOutcome::error(format!("analyze_crash request failed: {e}")),
        }
    }

    /// `get_modules` (caps.modules). Operates on the active backend (NOT a connect point):
    /// guard `Stopped` + a live backend, list modules, and return `{"modules": [...]}`.
    /// `Unsupported` maps to the standard string.
    pub(crate) async fn handle_get_modules(&self) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error("get_modules request failed: connection closed"),
        };

        match backend.modules().await {
            Ok(modules) => {
                let items = serde_json::to_value(&modules)
                    .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
                ToolOutcome::Json(RespBuilder::new().set("modules", items).build())
            }
            Err(BackendError::Unsupported(tool)) => {
                ToolOutcome::error(self.unsupported_active(tool))
            }
            Err(e) => ToolOutcome::error(format!("get_modules request failed: {e}")),
        }
    }

    /// The `Unsupported` tool-error string for an active-backend tool, keyed on the active
    /// backend's recorded name (lldb in practice). Falls back to the registry default name
    /// if somehow no backend name is recorded.
    fn unsupported_active(&self, tool: &'static str) -> String {
        let backend = self
            .active_backend_name()
            .unwrap_or_else(|| self.registry.default_name());
        unsupported(tool, backend)
    }
}

/// The "not available" tool-error for a force-windbg verb when the windbg factory is not
/// registered on this platform. The raw registry "unknown backend" string is intentionally
/// NOT surfaced for these hardcoded-windbg tools — it is wrapped here.
fn not_available(tool: &str) -> String {
    format!("{tool} is not available: the windbg backend is not registered on this platform")
}

/// The standard `Unsupported` tool-error: a backend method returned
/// [`BackendError::Unsupported`] for `tool` while the active/selected backend is `backend`.
fn unsupported(tool: &str, backend: &str) -> String {
    format!("{tool} is not supported by the {backend} backend")
}
