//! [`EngineOps`] — the object-safe trait the engine thread drives, mirroring the
//! `dbgeng_sys::Engine` method set the backend marshals onto.
//!
//! The production impl ([`impl EngineOps for dbgeng_sys::Engine`]) is a trivial pass-through;
//! a scripted [`FakeEngine`](crate::fake::FakeEngine) impl (test-only) lets tasks 3.2–3.4
//! unit-test marshaling/translation with no live engine — the analog of `lldb-backend`'s
//! `tokio::io::duplex` scripted peer.
//!
//! Every method takes `&mut self` (the engine is single-thread-confined and not re-entrant, so
//! the owning thread serializes calls behind `&mut`), and the trait is object-safe so the thread
//! can own a `Box<dyn EngineOps>` chosen by the injected constructor closure.

use dbgeng_sys::{BpLoc, Engine, InterruptHandle, LaunchReq, OutputSink};
use debugger_core::{
    BreakpointResult, DumpOutcome, EvalResult, Frame, Instruction, MemoryRead, ModuleInfo,
    StepKind, StopOutcome, ThreadInfo, Variable,
};

use crate::error::EngineError;

/// The DbgEng operation set the backend marshals over the engine thread. Object-safe (every
/// method is dispatchable on `dyn EngineOps`) so the thread can hold a `Box<dyn EngineOps>` that
/// is either the real [`Engine`] or a scripted fake.
///
/// The signatures mirror [`dbgeng_sys::Engine`] one-to-one and return the neutral
/// `debugger_core` types directly (the `Engine` already produces them), so the impl is a
/// pass-through and the marshaling layer (`EngineCmd` + `WinDbgBackend::call`) carries the
/// neutral types end-to-end.
pub trait EngineOps {
    fn launch(&mut self, req: &LaunchReq) -> Result<StopOutcome, EngineError>;
    fn attach_pid(&mut self, pid: u32) -> Result<StopOutcome, EngineError>;
    /// End the session. `terminate` kills the live debuggee (vs. a plain detach); ignored for a
    /// dump session. See [`dbgeng_sys::Engine::detach`].
    fn detach(&mut self, terminate: bool) -> Result<(), EngineError>;
    fn go(&mut self, timeout_ms: u32) -> Result<Option<StopOutcome>, EngineError>;
    fn step(&mut self, kind: StepKind) -> Result<StopOutcome, EngineError>;
    fn break_in(&mut self) -> Result<StopOutcome, EngineError>;
    fn set_breakpoint(
        &mut self,
        loc: &BpLoc,
        condition: &str,
    ) -> Result<BreakpointResult, EngineError>;
    fn remove_breakpoint(&mut self, id: i64) -> Result<(), EngineError>;
    fn list_breakpoints(&mut self) -> Result<Vec<BreakpointResult>, EngineError>;
    fn threads(&mut self) -> Result<Vec<ThreadInfo>, EngineError>;
    fn stack_trace(&mut self, thread_id: i64, max: i64) -> Result<Vec<Frame>, EngineError>;
    fn locals(&mut self, frame_index: i64) -> Result<Vec<Variable>, EngineError>;
    fn evaluate(&mut self, expr: &str) -> Result<EvalResult, EngineError>;
    fn read_memory(&mut self, address: u64, size: usize) -> Result<MemoryRead, EngineError>;
    fn disassemble(&mut self, address: u64, count: i64) -> Result<Vec<Instruction>, EngineError>;
    fn execute(&mut self, command: &str) -> Result<String, EngineError>;
    fn modules(&mut self) -> Result<Vec<ModuleInfo>, EngineError>;
    fn current_source_location(&mut self) -> Result<Option<(String, i64)>, EngineError>;
    fn open_dump(&mut self, path: &str) -> Result<DumpOutcome, EngineError>;
    fn attach_kernel(&mut self, connection: &str) -> Result<StopOutcome, EngineError>;
    fn analyze(&mut self) -> Result<String, EngineError>;
    fn set_output_sink(&mut self, sink: OutputSink);
    /// Mint a `Send` interrupt handle. `&self` — it only clones an `Arc<AtomicBool>` and mutates
    /// nothing (matches `Engine::interrupt_handle`).
    fn interrupt_handle(&self) -> InterruptHandle;
}

impl EngineOps for Engine {
    fn launch(&mut self, req: &LaunchReq) -> Result<StopOutcome, EngineError> {
        Engine::launch(self, req)
    }

    fn attach_pid(&mut self, pid: u32) -> Result<StopOutcome, EngineError> {
        Engine::attach_pid(self, pid)
    }

    fn detach(&mut self, terminate: bool) -> Result<(), EngineError> {
        Engine::detach(self, terminate)
    }

    fn go(&mut self, timeout_ms: u32) -> Result<Option<StopOutcome>, EngineError> {
        Engine::go(self, timeout_ms)
    }

    fn step(&mut self, kind: StepKind) -> Result<StopOutcome, EngineError> {
        Engine::step(self, kind)
    }

    fn break_in(&mut self) -> Result<StopOutcome, EngineError> {
        Engine::break_in(self)
    }

    fn set_breakpoint(
        &mut self,
        loc: &BpLoc,
        condition: &str,
    ) -> Result<BreakpointResult, EngineError> {
        Engine::set_breakpoint(self, loc, condition)
    }

    fn remove_breakpoint(&mut self, id: i64) -> Result<(), EngineError> {
        Engine::remove_breakpoint(self, id)
    }

    fn list_breakpoints(&mut self) -> Result<Vec<BreakpointResult>, EngineError> {
        Engine::list_breakpoints(self)
    }

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, EngineError> {
        Engine::threads(self)
    }

    fn stack_trace(&mut self, thread_id: i64, max: i64) -> Result<Vec<Frame>, EngineError> {
        Engine::stack_trace(self, thread_id, max)
    }

    fn locals(&mut self, frame_index: i64) -> Result<Vec<Variable>, EngineError> {
        Engine::locals(self, frame_index)
    }

    fn evaluate(&mut self, expr: &str) -> Result<EvalResult, EngineError> {
        Engine::evaluate(self, expr)
    }

    fn read_memory(&mut self, address: u64, size: usize) -> Result<MemoryRead, EngineError> {
        Engine::read_memory(self, address, size)
    }

    fn disassemble(&mut self, address: u64, count: i64) -> Result<Vec<Instruction>, EngineError> {
        Engine::disassemble(self, address, count)
    }

    fn execute(&mut self, command: &str) -> Result<String, EngineError> {
        Engine::execute(self, command)
    }

    fn modules(&mut self) -> Result<Vec<ModuleInfo>, EngineError> {
        Engine::modules(self)
    }

    fn current_source_location(&mut self) -> Result<Option<(String, i64)>, EngineError> {
        Engine::current_source_location(self)
    }

    fn open_dump(&mut self, path: &str) -> Result<DumpOutcome, EngineError> {
        Engine::open_dump(self, path)
    }

    fn attach_kernel(&mut self, connection: &str) -> Result<StopOutcome, EngineError> {
        Engine::attach_kernel(self, connection)
    }

    fn analyze(&mut self) -> Result<String, EngineError> {
        Engine::analyze(self)
    }

    fn set_output_sink(&mut self, sink: OutputSink) {
        Engine::set_output_sink(self, sink)
    }

    fn interrupt_handle(&self) -> InterruptHandle {
        Engine::interrupt_handle(self)
    }
}
