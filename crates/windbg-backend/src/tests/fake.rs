//! `FakeEngine` — a scripted [`EngineOps`] for unit tests, the analog of `lldb-backend`'s
//! `tokio::io::duplex` scripted peer. It holds plain canned data (no DbgEng, no COM) and an
//! `Arc<AtomicBool>` interrupt flag, so the engine thread + marshaling layer can be exercised on
//! any thread with no live engine. `Send` (only plain data + `Arc`s), so it can be moved into the
//! engine thread by the constructor closure.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use std::sync::atomic::Ordering;

use dbgeng_sys::{BpLoc, InterruptHandle, LaunchReq, OutputKind, OutputSink};
use debugger_core::{
    BreakpointResult, DumpOutcome, EvalResult, Frame, Instruction, MemoryRead, ModuleInfo,
    StepKind, StopInfo, StopOutcome, ThreadInfo, Variable,
};

use crate::engine_ops::EngineOps;
use crate::error::EngineError;

/// A flattened, comparable view of a [`BpLoc`] the recorder stores per `set_breakpoint` call (the
/// real `BpLoc` is neither `Clone` nor `PartialEq`). Lets a test assert exactly which breakpoint
/// locations the backend flushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedBp {
    Address(u64),
    FileLine { file: String, line: u32 },
    Function(String),
}

impl RecordedBp {
    fn from_loc(loc: &BpLoc) -> RecordedBp {
        match loc {
            BpLoc::Address(addr) => RecordedBp::Address(*addr),
            BpLoc::FileLine { file, line } => RecordedBp::FileLine {
                file: file.clone(),
                line: *line,
            },
            BpLoc::Function(name) => RecordedBp::Function(name.clone()),
        }
    }
}

/// What the fake records as the backend drives it, shared with the test via an `Arc<Mutex<…>>`.
/// The fake is moved onto the engine thread, so a test holds a clone of this `Arc` to read back
/// what was marshaled (the analog of inspecting the DAP peer's received frames).
#[derive(Debug, Default)]
pub struct Recorder {
    /// Each `set_breakpoint(loc, condition)` call, in order — proves the launch breakpoint flush.
    pub breakpoints: Vec<(RecordedBp, String)>,
    /// The `terminate` flag of each `detach(terminate)` call, in order — proves disconnect's
    /// kill-vs-detach choice round-trips.
    pub detaches: Vec<bool>,
    /// The `timeout_ms` of each `go(timeout_ms)` call, in order — proves `cont` marshals the
    /// effectively-infinite budget.
    pub gos: Vec<u32>,
    /// Set `true` the instant `go` is *entered* on the engine thread, before it produces its reply.
    /// The cancellation test uses this to confirm the `Go` command actually reached the engine
    /// (the future was polled past its `cmd_tx.send`) BEFORE the abort — so the test exercises a
    /// cont cancelled mid-await, not a never-started future.
    pub go_entered: bool,
    /// The kind of each `step(kind)` call, in order — proves `step` marshals each `StepKind`.
    pub steps: Vec<StepKind>,
    /// The number of `break_in()` calls — proves `cont`'s `None`→break_in fallback ran.
    pub break_ins: u32,
    /// The value of the interrupt flag observed AT THE MOMENT each `detach(terminate)` ran, in
    /// order. `disconnect` trips the interrupt flag *before* marshaling the detach, so a `true`
    /// here proves the ordering (flag-then-detach) directly, not merely the post-condition — a
    /// detach that raced ahead of the flag would record `false`.
    pub interrupt_flag_at_detach: Vec<bool>,
    /// The `(thread_id, max)` of each `stack_trace` call, in order — proves the backend's
    /// fetch-bound (`start + levels`) and thread selection.
    pub stack_traces: Vec<(i64, i64)>,
    /// The `frame_index` of each `locals` call, in order — proves `variables` decoded the frame
    /// index from the scope's `variables_reference` (`reference - 1`).
    pub locals_frames: Vec<i64>,
    /// The `expr` of each `evaluate` call, in order — proves `evaluate(Expression)` forms the
    /// `?? expr` (here the fake's `evaluate` mirrors the real engine, prefixing `?? `).
    pub evaluates: Vec<String>,
    /// The raw `command` of each `execute` call, in order — proves `evaluate(Repl)` marshals the
    /// command VERBATIM (no backtick prefix) through `Execute`.
    pub executes: Vec<String>,
    /// The `(address, size)` of each `read_memory` call, in order — proves the address parse and
    /// the count → size mapping.
    pub read_memories: Vec<(u64, usize)>,
    /// The `(address, count)` of each `disassemble` call, in order — proves the address parse and
    /// that the requested count is honored verbatim.
    pub disassembles: Vec<(u64, i64)>,
}

/// A scripted engine. Each field is the canned reply for the matching [`EngineOps`] method; the
/// defaults are benign so a test only sets the fields it asserts on. `interrupt_flag` is shared
/// with the [`InterruptHandle`] `interrupt_handle()` mints, so a test can observe a `pause()`;
/// `recorder` is shared with the test so it can assert which ops the backend marshaled.
pub struct FakeEngine {
    pub threads: Vec<ThreadInfo>,
    pub modules: Vec<ModuleInfo>,
    pub stop_outcome: StopOutcome,
    /// When true, `go` returns `Ok(None)` (the still-running / unreachable-deadline case) so a test
    /// can drive `cont`'s `None`→`break_in` fallback. When false, `go` returns `Some(stop_outcome)`.
    pub go_returns_none: bool,
    /// The interrupt flag the minted [`InterruptHandle`] shares.
    pub interrupt_flag: Arc<AtomicBool>,
    /// The shared call recorder (breakpoint flush, detach terminate flag, go/step/break_in calls).
    pub recorder: Arc<Mutex<Recorder>>,
    /// Canned frames `stack_trace` returns (before the backend's `start`/`levels` window).
    pub frames: Vec<Frame>,
    /// Canned locals `locals` returns (the inspection translation table).
    pub locals: Vec<Variable>,
    /// Canned `evaluate` output text (the real engine prefixes `?? `; the fake mirrors that).
    pub evaluate_result: String,
    /// Canned `execute` output text (the raw-command result for `evaluate(Repl)`).
    pub execute_result: String,
    /// Canned bytes `read_memory` returns (truncated to the requested `size`).
    pub memory: Vec<u8>,
    /// Canned instructions `disassemble` returns.
    pub instructions: Vec<Instruction>,
}

impl Default for FakeEngine {
    fn default() -> Self {
        FakeEngine {
            threads: Vec::new(),
            modules: Vec::new(),
            stop_outcome: StopOutcome::Stopped(StopInfo {
                reason: "breakpoint".to_string(),
                thread_id: 1,
                description: String::new(),
                hit_breakpoint_ids: Vec::new(),
            }),
            go_returns_none: false,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            recorder: Arc::new(Mutex::new(Recorder::default())),
            frames: Vec::new(),
            locals: Vec::new(),
            evaluate_result: String::new(),
            execute_result: String::new(),
            memory: Vec::new(),
            instructions: Vec::new(),
        }
    }
}

impl FakeEngine {
    /// A fake with a scripted thread list (the round-trip test asserts this comes back).
    pub fn with_threads(threads: Vec<ThreadInfo>) -> FakeEngine {
        FakeEngine {
            threads,
            ..FakeEngine::default()
        }
    }

    /// A fake scripted to return `outcome` from `launch`/`attach`/`go`/`step`, sharing the given
    /// `recorder` so a test can read back the marshaled `set_breakpoint`/`detach` calls.
    pub fn scripted(outcome: StopOutcome, recorder: Arc<Mutex<Recorder>>) -> FakeEngine {
        FakeEngine {
            stop_outcome: outcome,
            recorder,
            ..FakeEngine::default()
        }
    }

    /// Lock the recorder, recovering from a poisoned mutex rather than re-panicking on the engine
    /// thread.
    fn recorder(&self) -> std::sync::MutexGuard<'_, Recorder> {
        self.recorder.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl EngineOps for FakeEngine {
    fn launch(&mut self, _req: &LaunchReq) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn attach_pid(&mut self, _pid: u32) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn detach(&mut self, terminate: bool) -> Result<(), EngineError> {
        // Snapshot the interrupt flag AT detach time before recording the detach, so the ordering
        // test can prove `disconnect` set the flag BEFORE marshaling the detach (a detach that
        // raced ahead of the flag would record `false`).
        let flag_now = self.interrupt_flag.load(Ordering::Acquire);
        let mut rec = self.recorder();
        rec.interrupt_flag_at_detach.push(flag_now);
        rec.detaches.push(terminate);
        Ok(())
    }

    fn go(&mut self, timeout_ms: u32) -> Result<Option<StopOutcome>, EngineError> {
        // Reset the interrupt flag at entry, exactly as the real `go` does — so a `cont` whose
        // future was dropped (cancelled) does not leave a stale `true` that would break the next op.
        self.interrupt_flag.store(false, Ordering::SeqCst);
        {
            let mut rec = self.recorder();
            rec.go_entered = true;
            rec.gos.push(timeout_ms);
        }
        if self.go_returns_none {
            Ok(None)
        } else {
            Ok(Some(self.stop_outcome.clone()))
        }
    }

    fn step(&mut self, kind: StepKind) -> Result<StopOutcome, EngineError> {
        self.recorder().steps.push(kind);
        Ok(self.stop_outcome.clone())
    }

    fn break_in(&mut self) -> Result<StopOutcome, EngineError> {
        self.recorder().break_ins += 1;
        Ok(self.stop_outcome.clone())
    }

    fn set_breakpoint(
        &mut self,
        loc: &BpLoc,
        condition: &str,
    ) -> Result<BreakpointResult, EngineError> {
        self.recorder()
            .breakpoints
            .push((RecordedBp::from_loc(loc), condition.to_string()));
        Ok(BreakpointResult {
            id: 1,
            verified: true,
            line: 0,
            message: String::new(),
        })
    }

    fn remove_breakpoint(&mut self, _id: i64) -> Result<(), EngineError> {
        Ok(())
    }

    fn list_breakpoints(&mut self) -> Result<Vec<BreakpointResult>, EngineError> {
        Ok(Vec::new())
    }

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, EngineError> {
        Ok(self.threads.clone())
    }

    fn stack_trace(&mut self, thread_id: i64, max: i64) -> Result<Vec<Frame>, EngineError> {
        self.recorder().stack_traces.push((thread_id, max));
        Ok(self.frames.clone())
    }

    fn locals(&mut self, frame_index: i64) -> Result<Vec<Variable>, EngineError> {
        self.recorder().locals_frames.push(frame_index);
        Ok(self.locals.clone())
    }

    fn evaluate(&mut self, expr: &str) -> Result<EvalResult, EngineError> {
        // Mirror the real `Engine::evaluate`: it runs `?? <expr>` through Execute. The fake records
        // the FORMED command so a test can assert `evaluate(Expression)` reached the engine as
        // `?? expr` (the C++ plugin's expression-eval form).
        let command = format!("?? {expr}");
        self.recorder().evaluates.push(command);
        Ok(EvalResult {
            result: self.evaluate_result.clone(),
            ty: String::new(),
            variables_reference: 0,
        })
    }

    fn read_memory(&mut self, address: u64, size: usize) -> Result<MemoryRead, EngineError> {
        self.recorder().read_memories.push((address, size));
        // Truncate the canned bytes to the requested size (the engine returns at most `size` bytes).
        let mut data = self.memory.clone();
        data.truncate(size);
        Ok(MemoryRead {
            address: format!("0x{address:016X}"),
            data,
        })
    }

    fn disassemble(&mut self, address: u64, count: i64) -> Result<Vec<Instruction>, EngineError> {
        self.recorder().disassembles.push((address, count));
        Ok(self.instructions.clone())
    }

    fn execute(&mut self, command: &str) -> Result<String, EngineError> {
        // Record the RAW command verbatim so a test can prove `evaluate(Repl)` marshaled it with no
        // backtick prefix (the `??`-wrapping is only the Expression path, handled in `evaluate`).
        self.recorder().executes.push(command.to_string());
        Ok(self.execute_result.clone())
    }

    fn modules(&mut self) -> Result<Vec<ModuleInfo>, EngineError> {
        Ok(self.modules.clone())
    }

    fn current_source_location(&mut self) -> Result<Option<(String, i64)>, EngineError> {
        Ok(None)
    }

    fn open_dump(&mut self, _path: &str) -> Result<DumpOutcome, EngineError> {
        Ok(DumpOutcome {
            stop: None,
            crash_location: None,
        })
    }

    fn attach_kernel(&mut self, _connection: &str) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn set_output_sink(&mut self, mut sink: OutputSink) {
        // Feed the sink one canned line so the wiring is exercised. The closure must be
        // non-blocking (Decision 6); the unbounded send in the factory's sink satisfies that.
        sink(OutputKind::Normal, "");
    }

    fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle::from_flag(Arc::clone(&self.interrupt_flag))
    }
}
