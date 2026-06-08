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

    fn stack_trace(&mut self, _thread_id: i64, _max: i64) -> Result<Vec<Frame>, EngineError> {
        Ok(Vec::new())
    }

    fn locals(&mut self, _frame_index: i64) -> Result<Vec<Variable>, EngineError> {
        Ok(Vec::new())
    }

    fn evaluate(&mut self, _expr: &str) -> Result<EvalResult, EngineError> {
        Ok(EvalResult {
            result: String::new(),
            ty: String::new(),
            variables_reference: 0,
        })
    }

    fn read_memory(&mut self, address: u64, _size: usize) -> Result<MemoryRead, EngineError> {
        Ok(MemoryRead {
            address: format!("0x{address:016X}"),
            data: Vec::new(),
        })
    }

    fn disassemble(&mut self, _address: u64, _count: i64) -> Result<Vec<Instruction>, EngineError> {
        Ok(Vec::new())
    }

    fn execute(&mut self, _command: &str) -> Result<String, EngineError> {
        Ok(String::new())
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
