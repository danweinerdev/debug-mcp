//! `FakeEngine` — a scripted [`EngineOps`] for unit tests, the analog of `lldb-backend`'s
//! `tokio::io::duplex` scripted peer. It holds plain canned data (no DbgEng, no COM) and an
//! `Arc<AtomicBool>` interrupt flag, so the engine thread + marshaling layer can be exercised on
//! any thread with no live engine. `Send` (only plain data + `Arc`s), so it can be moved into the
//! engine thread by the constructor closure.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use dbgeng_sys::{BpLoc, InterruptHandle, LaunchReq, OutputKind, OutputSink};
use debugger_core::{
    BreakpointResult, DumpOutcome, EvalResult, Frame, Instruction, MemoryRead, ModuleInfo,
    StepKind, StopInfo, StopOutcome, ThreadInfo, Variable,
};

use crate::engine_ops::EngineOps;
use crate::error::EngineError;

/// A scripted engine. Each field is the canned reply for the matching [`EngineOps`] method; the
/// defaults are benign so a test only sets the fields it asserts on. `interrupt_flag` is shared
/// with the [`InterruptHandle`] `interrupt_handle()` mints, so a test can observe a `pause()`.
pub struct FakeEngine {
    pub threads: Vec<ThreadInfo>,
    pub modules: Vec<ModuleInfo>,
    pub stop_outcome: StopOutcome,
    /// The interrupt flag the minted [`InterruptHandle`] shares.
    pub interrupt_flag: Arc<AtomicBool>,
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
            interrupt_flag: Arc::new(AtomicBool::new(false)),
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
}

impl EngineOps for FakeEngine {
    fn launch(&mut self, _req: &LaunchReq) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn attach_pid(&mut self, _pid: u32) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn detach(&mut self) -> Result<(), EngineError> {
        Ok(())
    }

    fn go(&mut self, _timeout_ms: u32) -> Result<Option<StopOutcome>, EngineError> {
        Ok(Some(self.stop_outcome.clone()))
    }

    fn step(&mut self, _kind: StepKind) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn break_in(&mut self) -> Result<StopOutcome, EngineError> {
        Ok(self.stop_outcome.clone())
    }

    fn set_breakpoint(
        &mut self,
        _loc: &BpLoc,
        _condition: &str,
    ) -> Result<BreakpointResult, EngineError> {
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
