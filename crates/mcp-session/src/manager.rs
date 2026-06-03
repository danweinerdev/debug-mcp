//! The neutral `SessionManager` (Spec FR-4/FR-7/FR-12, design §`SessionManager`).
//!
//! One `RwLock`-guarded `Inner` plus a separately-locked [`OutputBuffer`]. Holds the
//! state machine, the generation epoch, the breakpoint tracking, the frame-map cache,
//! and the last-stop / exit-code caches. Depends only on `debugger-core` — it cannot
//! name a DAP or lldb type (the seam).
//!
//! Go origin: `internal/session/session.go` `SessionManager`. Two intentional
//! divergences from the Go oracle, both per the design:
//! - The `replModeCommand` flag is **not** tracked here — the backend owns that decision
//!   (`supports_command_repl_mode`), so there is no session-level flag to keep in sync.
//! - A `generation` epoch (design Decision 6) guards the post-call state write against a
//!   concurrent `disconnect`; the Go worker pool serialized calls instead.

use std::collections::HashMap;
use std::sync::RwLock;

use debugger_core::{FunctionBp, SourceBp, StopInfo};

use crate::breakpoint::BreakpointInfo;
use crate::output_buffer::OutputBuffer;
use crate::state::State;

#[derive(Debug)]
struct Inner {
    state: State,
    /// Bumped on every `reset()` (and thus on `disconnect`). A blocking handler snapshots
    /// this before awaiting the backend and applies its state transition only if the
    /// value is unchanged — so a `continue` returning after a concurrent `disconnect`
    /// cannot clobber the reset `idle` state (design Decision 6).
    generation: u64,

    program: String,
    pid: i64,
    exit_code: Option<i64>,
    last_stopped: Option<StopInfo>,

    /// Frame index → debugger frame id.
    frame_mapping: HashMap<i64, i64>,

    source_bps: HashMap<String, Vec<SourceBp>>,
    function_bps: Vec<FunctionBp>,
    /// Resolved breakpoint metadata, keyed by debugger-assigned id.
    bp_responses: HashMap<i64, BreakpointInfo>,

    /// Pending breakpoints, set before launch and flushed during configuration.
    pending_source_bps: HashMap<String, Vec<SourceBp>>,
    pending_function_bps: Vec<FunctionBp>,
}

impl Inner {
    fn new() -> Self {
        Inner {
            state: State::Idle,
            generation: 0,
            program: String::new(),
            pid: 0,
            exit_code: None,
            last_stopped: None,
            frame_mapping: HashMap::new(),
            source_bps: HashMap::new(),
            function_bps: Vec::new(),
            bp_responses: HashMap::new(),
            pending_source_bps: HashMap::new(),
            pending_function_bps: Vec::new(),
        }
    }
}

/// Thread-safe holder of all state for the active debug session. The public surface is
/// the seam between the tool handlers (Phase 5) and the neutral session model.
#[derive(Debug)]
pub struct SessionManager {
    inner: RwLock<Inner>,
    output: OutputBuffer,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create a session in the idle state with all maps initialized.
    pub fn new() -> Self {
        SessionManager {
            inner: RwLock::new(Inner::new()),
            output: OutputBuffer::new(),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().expect("session lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().expect("session lock poisoned")
    }

    // --- state machine + guards (Spec FR-4) ---

    /// The current session state.
    pub fn state(&self) -> State {
        self.read().state
    }

    /// Unconditionally set the session state (no transition validation — Go parity).
    pub fn set_state(&self, state: State) {
        self.write().state = state;
    }

    /// Apply a state transition **only if** `generation` still matches the current epoch,
    /// under one write lock (so a concurrent `reset()` cannot slip between the check and
    /// the write). Returns whether it was applied. Used by the execution handlers to write
    /// the post-stop `stopped`/`terminated` transition without clobbering a state a
    /// concurrent `disconnect` reset (design Decision 6 — the in-flight-call counterpart of
    /// [`SessionManager::terminate_if_generation`]).
    pub fn set_state_if_generation(&self, generation: u64, state: State) -> bool {
        let mut inner = self.write();
        if inner.generation != generation {
            return false;
        }
        inner.state = state;
        true
    }

    /// The current generation epoch (snapshot before a blocking backend call;
    /// design Decision 6).
    pub fn generation(&self) -> u64 {
        self.read().generation
    }

    /// Read-only guard (Spec FR-4.2): `Ok(())` when the current state is one of
    /// `allowed`, else the exact Go error string for the current state.
    pub fn check_state(&self, allowed: &[State]) -> Result<(), String> {
        let current = self.state();
        if allowed.contains(&current) {
            return Ok(());
        }

        match current {
            State::Idle => {
                Err("no debug session active. Use 'launch' or 'attach' first.".to_string())
            }
            State::Running => Err("process is running. Use 'pause' first.".to_string()),
            _ => {
                let names = allowed
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "invalid state: {current}, expected one of: {names}"
                ))
            }
        }
    }

    /// Clear all session state, return to idle, and bump the generation epoch
    /// (design Decision 6). A fresh output buffer is installed (Go reinstantiates it).
    pub fn reset(&self) {
        let mut inner = self.write();
        inner.state = State::Idle;
        inner.generation = inner.generation.wrapping_add(1);
        inner.program = String::new();
        inner.pid = 0;
        inner.exit_code = None;
        inner.last_stopped = None;
        inner.frame_mapping = HashMap::new();
        inner.source_bps = HashMap::new();
        inner.function_bps = Vec::new();
        inner.bp_responses = HashMap::new();
        inner.pending_source_bps = HashMap::new();
        inner.pending_function_bps = Vec::new();
        drop(inner);

        // The output buffer has its own lock; clear it independently (Go installs a new
        // buffer, observably equivalent to draining the existing one).
        self.output.drain();
    }

    // --- process info accessors ---

    pub fn program(&self) -> String {
        self.read().program.clone()
    }

    pub fn set_program(&self, program: String) {
        self.write().program = program;
    }

    pub fn pid(&self) -> i64 {
        self.read().pid
    }

    pub fn set_pid(&self, pid: i64) {
        self.write().pid = pid;
    }

    pub fn exit_code(&self) -> Option<i64> {
        self.read().exit_code
    }

    pub fn set_exit_code(&self, code: i64) {
        self.write().exit_code = Some(code);
    }

    /// Atomically apply the async-termination transition (record the exit code, set state
    /// `terminated`) **only if** `generation` still matches the session's current epoch.
    /// Returns whether the transition was applied. The whole check-and-act runs under one
    /// write lock so a concurrent `reset()` cannot slip between the guard and the writes
    /// (design Decision 6 — the event-pump's backstop for the no-call-in-flight case).
    pub fn terminate_if_generation(&self, generation: u64, code: Option<i64>) -> bool {
        let mut inner = self.write();
        if inner.generation != generation {
            return false;
        }
        if let Some(code) = code {
            inner.exit_code = Some(code);
        }
        inner.state = State::Terminated;
        true
    }

    // --- last-stop cache (Spec FR-4, used by status/handleStopResult) ---

    /// A clone of the most recent stop info, or `None`.
    pub fn last_stopped(&self) -> Option<StopInfo> {
        self.read().last_stopped.clone()
    }

    pub fn set_last_stopped(&self, info: StopInfo) {
        self.write().last_stopped = Some(info);
    }

    // --- frame-map cache (Spec FR-4) ---

    /// A clone of the frame index → frame id map (never the live map — Spec Appendix A).
    pub fn frame_mapping(&self) -> HashMap<i64, i64> {
        self.read().frame_mapping.clone()
    }

    /// Replace the frame-map cache.
    pub fn set_frame_mapping(&self, mapping: HashMap<i64, i64>) {
        self.write().frame_mapping = mapping;
    }

    // --- output buffer (Spec FR-12) ---

    /// The session's output buffer (its own lock, separate from the session `RwLock`).
    pub fn output_buffer(&self) -> &OutputBuffer {
        &self.output
    }

    // --- breakpoint tracking (Spec FR-7) ---

    /// Append a source breakpoint to the active list for `file`; return the created
    /// breakpoint.
    pub fn add_source_breakpoint(&self, file: &str, line: i64, condition: &str) -> SourceBp {
        let bp = SourceBp {
            line,
            condition: condition.to_string(),
        };
        self.write()
            .source_bps
            .entry(file.to_string())
            .or_default()
            .push(bp.clone());
        bp
    }

    /// Append a function breakpoint to the active list; return the created breakpoint.
    pub fn add_function_breakpoint(&self, name: &str, condition: &str) -> FunctionBp {
        let bp = FunctionBp {
            name: name.to_string(),
            condition: condition.to_string(),
        };
        self.write().function_bps.push(bp.clone());
        bp
    }

    /// Store resolved breakpoint metadata, keyed by debugger-assigned id.
    pub fn add_breakpoint_response(&self, info: BreakpointInfo) {
        self.write().bp_responses.insert(info.id, info);
    }

    /// A read-only clone of the tracked breakpoint metadata for `id`, or `None` when no
    /// breakpoint with that id is tracked. Lets a handler compute a proposed
    /// breakpoint-list change (and pick the file/kind) *before* committing the session
    /// mutation, so the mutation can be deferred until the DAP call succeeds (Spec FR-7.3,
    /// transactional update).
    pub fn breakpoint_info(&self, id: i64) -> Option<BreakpointInfo> {
        self.read().bp_responses.get(&id).cloned()
    }

    /// Remove a tracked breakpoint by debugger id (Spec FR-7.3). Returns
    /// `(file_path, was_function)`: source breakpoints are matched by **line only**,
    /// function breakpoints by **name only**, taking the first match in the active
    /// tracking list. The response-tracking entry is then deleted. An unknown id errors
    /// `breakpoint ID <id> not found`.
    pub fn remove_breakpoint_by_id(&self, id: i64) -> Result<(String, bool), String> {
        let mut inner = self.write();

        let info = match inner.bp_responses.get(&id) {
            Some(info) => info.clone(),
            None => return Err(format!("breakpoint ID {id} not found")),
        };

        let mut file_path = String::new();
        let mut was_function = false;

        match info.ty.as_str() {
            "source" => {
                file_path = info.file.clone();
                if let Some(bps) = inner.source_bps.get_mut(&file_path) {
                    if let Some(pos) = bps.iter().position(|bp| bp.line == info.line) {
                        bps.remove(pos);
                    }
                }
            }
            "function" => {
                was_function = true;
                if let Some(pos) = inner
                    .function_bps
                    .iter()
                    .position(|bp| bp.name == info.function)
                {
                    inner.function_bps.remove(pos);
                }
            }
            _ => {}
        }

        inner.bp_responses.remove(&id);
        Ok((file_path, was_function))
    }

    /// All tracked breakpoint metadata, sorted ascending by id (Spec FR-7.4).
    pub fn list_breakpoints(&self) -> Vec<BreakpointInfo> {
        let inner = self.read();
        let mut result: Vec<BreakpointInfo> = inner.bp_responses.values().cloned().collect();
        result.sort_by_key(|info| info.id);
        result
    }

    /// A defensive copy of the active source breakpoints for `file` (empty when none).
    pub fn source_breakpoints_for_file(&self, file: &str) -> Vec<SourceBp> {
        self.read()
            .source_bps
            .get(file)
            .cloned()
            .unwrap_or_default()
    }

    /// A defensive copy of the active function-breakpoint list.
    pub fn all_function_breakpoints(&self) -> Vec<FunctionBp> {
        self.read().function_bps.clone()
    }

    // --- pending breakpoints (flushed only by launch — Spec FR-7.5) ---

    /// Buffer a source breakpoint for flush at launch.
    pub fn add_pending_source_breakpoint(&self, file: &str, line: i64, condition: &str) {
        let bp = SourceBp {
            line,
            condition: condition.to_string(),
        };
        self.write()
            .pending_source_bps
            .entry(file.to_string())
            .or_default()
            .push(bp);
    }

    /// Buffer a function breakpoint for flush at launch.
    pub fn add_pending_function_breakpoint(&self, name: &str, condition: &str) {
        let bp = FunctionBp {
            name: name.to_string(),
            condition: condition.to_string(),
        };
        self.write().pending_function_bps.push(bp);
    }

    /// Move pending breakpoints into the active tracking structures and return them so
    /// the caller can build the `LaunchSpec` (Spec FR-7.5). Pending buffers are cleared,
    /// so a second flush returns empty and does not duplicate active breakpoints
    /// (idempotent).
    pub fn flush_pending_breakpoints(&self) -> (HashMap<String, Vec<SourceBp>>, Vec<FunctionBp>) {
        let mut inner = self.write();

        let source_files = std::mem::take(&mut inner.pending_source_bps);
        let func_bps = std::mem::take(&mut inner.pending_function_bps);

        for (file, bps) in &source_files {
            inner
                .source_bps
                .entry(file.clone())
                .or_default()
                .extend(bps.iter().cloned());
        }
        inner.function_bps.extend(func_bps.iter().cloned());

        (source_files, func_bps)
    }
}
