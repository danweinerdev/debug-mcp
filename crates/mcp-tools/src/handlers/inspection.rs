//! Inspection handlers: `status`, `backtrace`, `threads`, `variables`, `evaluate`
//! (Spec FR-9/FR-10, task 5.4).
//!
//! `status` reads only cached session data (no live DAP). The others guard `stopped` and
//! call the backend; `variables`/`evaluate` resolve the frame id via
//! [`resolve_frame_id`](crate::frame::resolve_frame_id) (implicit `stack_trace(levels=20)`
//! on a frame-map miss).

use std::collections::HashMap;

use debugger_core::EvalMode;
use mcp_session::State;
use serde_json::{Map, Value};

use crate::errors;
use crate::flatten::flatten_variables;
use crate::frame::resolve_frame_id;
use crate::response::{RespBuilder, ToolOutcome};
use crate::server::ToolServer;
use crate::Args;

/// The 100-variable hard cap the `variables` flattening applies (Spec FR-10.3).
const VARIABLE_CAP: usize = 100;

impl ToolServer {
    /// `status` (Spec FR-9). No guard, no live DAP — per-state fields from cached data.
    pub(crate) fn handle_status(&self) -> ToolOutcome {
        let state = self.session.state();
        let mut builder = RespBuilder::new().set("state", state.to_string());

        match state {
            State::Idle => {
                builder = builder.set("message", "No active debug session");
            }
            State::Configuring => {
                builder = builder.set("message", "Debug session is being configured");
            }
            State::Stopped => {
                builder = builder
                    .set("program", self.session.program())
                    .set("pid", self.session.pid());
                if let Some(event) = self.session.last_stopped() {
                    builder = builder
                        .set("stop_reason", event.reason.clone())
                        .set("stopped_thread_id", event.thread_id);
                    if !event.description.is_empty() {
                        builder = builder.set("stop_description", event.description.clone());
                    }
                    if !event.hit_breakpoint_ids.is_empty() {
                        builder = builder.set(
                            "hit_breakpoint_ids",
                            Value::from(event.hit_breakpoint_ids.clone()),
                        );
                    }
                }
            }
            State::Running => {
                builder = builder
                    .set("program", self.session.program())
                    .set("pid", self.session.pid());
            }
            State::Terminated => {
                builder = builder.set("program", self.session.program());
                if let Some(code) = self.session.exit_code() {
                    builder = builder.set("exit_code", code);
                }
            }
        }

        builder.into_outcome()
    }

    /// `backtrace` (Spec FR-10.1). Guard stopped → resolve thread → `stack_trace` → rebuild
    /// + store the frame map → format frames.
    pub(crate) async fn handle_backtrace(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let thread_id = self.resolve_backtrace_thread_id(args);

        // levels default 20, overridden only when present and > 0.
        let mut levels = 20;
        if let Some(l) = args.get_f64("levels") {
            if l > 0.0 {
                levels = l as i64;
            }
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::STACK_TRACE.request_failed.to_string()),
        };

        let (frames, total_frames) = match backend.stack_trace(thread_id, 0, levels).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(errors::STACK_TRACE.render(e)),
        };

        // Rebuild and store the frame mapping (frame index → frame id) for all frames.
        let mut frame_mapping = HashMap::with_capacity(frames.len());
        for frame in &frames {
            frame_mapping.insert(frame.index, frame.id);
        }
        self.session.set_frame_mapping(frame_mapping);

        let frame_items: Vec<Value> = frames
            .iter()
            .map(|frame| {
                let mut entry = Map::new();
                entry.insert("index".to_string(), Value::from(frame.index));
                entry.insert("name".to_string(), Value::from(frame.name.clone()));
                entry.insert("id".to_string(), Value::from(frame.id));
                if let Some(path) = &frame.source_path {
                    if !path.is_empty() {
                        entry.insert("file".to_string(), Value::from(path.clone()));
                        entry.insert("line".to_string(), Value::from(frame.line));
                    }
                }
                if let Some(ip) = &frame.instruction_pointer {
                    if !ip.is_empty() {
                        entry.insert("address".to_string(), Value::from(ip.clone()));
                    }
                }
                Value::Object(entry)
            })
            .collect();

        ToolOutcome::Json(
            RespBuilder::new()
                .set("frames", Value::Array(frame_items))
                .set("total_frames", total_frames)
                .set("thread_id", thread_id)
                .build(),
        )
    }

    /// `threads` (Spec FR-10.2). Guard stopped → `threads` → mark stopped/current thread →
    /// `stopped_thread_id` when matched.
    pub(crate) async fn handle_threads(&self, _args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error(errors::THREADS.request_failed.to_string()),
        };

        let threads = match backend.threads().await {
            Ok(t) => t,
            Err(e) => return ToolOutcome::error(errors::THREADS.render(e)),
        };

        let last_stopped_tid = self.session.last_stopped().map(|e| e.thread_id);

        let mut stopped_thread_id: Option<i64> = None;
        let thread_items: Vec<Value> = threads
            .iter()
            .map(|th| {
                let mut entry = Map::new();
                entry.insert("id".to_string(), Value::from(th.id));
                entry.insert("name".to_string(), Value::from(th.name.clone()));
                if Some(th.id) == last_stopped_tid {
                    entry.insert("is_stopped".to_string(), Value::from(true));
                    entry.insert("is_current".to_string(), Value::from(true));
                    stopped_thread_id = Some(th.id);
                }
                Value::Object(entry)
            })
            .collect();

        let count = thread_items.len();
        let builder = RespBuilder::new()
            .set("threads", Value::Array(thread_items))
            .set("count", count)
            .set_opt("stopped_thread_id", stopped_thread_id);
        builder.into_outcome()
    }

    /// `variables` (Spec FR-10.3). Guard stopped → parse frame/scope/depth/filter → resolve
    /// frame id → `scopes` → match scope (case-insensitive) → `flatten_variables(…,100)`.
    pub(crate) async fn handle_variables(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let frame_index = args.get_f64("frame_index").map(|f| f as i64).unwrap_or(0);

        let scope = {
            let s = args.get_string("scope", "");
            if s.is_empty() {
                "local".to_string()
            } else {
                s
            }
        };

        // Default depth: 2, except 1 for global. An explicit depth >= 0 overrides.
        let mut depth = if scope == "global" { 1 } else { 2 };
        if let Some(d) = args.get_f64("depth") {
            if d >= 0.0 {
                depth = d as i64;
            }
        }

        let filter = args.get_string("filter", "");

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error("failed to resolve frame: connection closed"),
        };

        let frame_id = match resolve_frame_id(&backend, &self.session, frame_index).await {
            Ok(id) => id,
            Err(e) => return ToolOutcome::error(format!("failed to resolve frame: {e}")),
        };

        let scopes = match backend.scopes(frame_id).await {
            Ok(s) => s,
            Err(e) => return ToolOutcome::error(errors::SCOPES.render(e)),
        };

        // Case-insensitive scope name match (Locals/Local, Globals/Global,
        // Registers/Register).
        let target = scopes.iter().find(|s| scope_matches(&scope, &s.name));
        let target = match target {
            Some(s) => s,
            None => {
                return ToolOutcome::error(format!(
                    "scope '{scope}' not found in frame {frame_index}"
                ))
            }
        };

        let (vars, truncated) = match flatten_variables(
            backend.as_ref(),
            target.variables_reference,
            depth,
            VARIABLE_CAP,
            &filter,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return ToolOutcome::error(format!(
                    "failed to fetch variables: {}",
                    errors::VARIABLES.render(e)
                ))
            }
        };

        let count = vars.len();
        let vars_value = serde_json::to_value(&vars).unwrap_or(Value::Array(Vec::new()));
        ToolOutcome::Json(
            RespBuilder::new()
                .set("variables", vars_value)
                .set("count", count)
                .set("scope", scope)
                .set("truncated", truncated)
                .build(),
        )
    }

    /// `evaluate` (Spec FR-10.4). Guard stopped → resolve frame id → `evaluate(context=
    /// variables)` → `{result,type}` + has_children/variables_reference when ref > 0.
    pub(crate) async fn handle_evaluate(&self, args: &Args<'_>) -> ToolOutcome {
        if let Err(e) = self.session.check_state(&[State::Stopped]) {
            return ToolOutcome::error(e);
        }

        let expression = match args.require_string("expression") {
            Ok(e) => e,
            Err(e) => return ToolOutcome::error(e),
        };

        let frame_index = args.get_f64("frame_index").map(|f| f as i64).unwrap_or(0);

        let backend = match self.current_backend().await {
            Some(b) => b,
            None => return ToolOutcome::error("failed to resolve frame: connection closed"),
        };

        let frame_id = match resolve_frame_id(&backend, &self.session, frame_index).await {
            Ok(id) => id,
            Err(e) => return ToolOutcome::error(format!("failed to resolve frame: {e}")),
        };

        let result = match backend
            .evaluate(&expression, Some(frame_id), EvalMode::Expression)
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolOutcome::error(errors::EVALUATE.render(e)),
        };

        let mut builder = RespBuilder::new()
            .set("result", result.result)
            .set("type", result.ty);
        if result.variables_reference > 0 {
            builder = builder
                .set("has_children", true)
                .set("variables_reference", result.variables_reference);
        }
        builder.into_outcome()
    }

    /// Resolve the backtrace thread id: explicit `thread_id` → last-stopped → 1.
    fn resolve_backtrace_thread_id(&self, args: &Args<'_>) -> i64 {
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
}

/// Case-insensitive scope-name match (Spec FR-10.3): `local`→`Locals`/`Local`,
/// `global`→`Globals`/`Global`, `register`→`Registers`/`Register`.
fn scope_matches(requested: &str, name: &str) -> bool {
    let lower = name.to_lowercase();
    match requested {
        "local" => lower == "locals" || lower == "local",
        "global" => lower == "globals" || lower == "global",
        "register" => lower == "registers" || lower == "register",
        _ => false,
    }
}
