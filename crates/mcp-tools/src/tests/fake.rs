//! A scriptable fake [`DebuggerBackend`] + [`BackendFactory`] for the handler tests.
//!
//! Mirrors the Go `*_test.go` approach of poking the session directly and asserting the
//! tool result shape, but here the fake records the DAP-equivalent backend calls and
//! returns canned outcomes — so guards, response shapes, error strings, output merge, the
//! generation guard, and the pause-during-continue concurrency are all testable WITHOUT a
//! real lldb-dap.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BackendEvent, BackendFactory, BreakpointResult,
    Connection, DebuggerBackend, EvalMode, EvalResult, Frame, FunctionBp, Granularity, Instruction,
    LaunchOutcome, LaunchSpec, MemoryRead, Scope, SourceBp, StepKind, StopOutcome, ThreadInfo,
    Variable,
};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::sync::oneshot;

/// A record of one backend call, for asserting the handler issued the right op.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Launch,
    Attach,
    Disconnect {
        terminate: bool,
    },
    SetSourceBreakpoints {
        file: String,
        count: usize,
    },
    SetFunctionBreakpoints {
        count: usize,
    },
    Cont {
        thread_id: i64,
    },
    Step {
        kind: StepKind,
        thread_id: i64,
        gran: Option<Granularity>,
    },
    Pause,
    Threads,
    StackTrace {
        thread_id: i64,
        start: i64,
        levels: i64,
    },
    Scopes {
        frame_id: i64,
    },
    Variables {
        variables_reference: i64,
    },
    Evaluate {
        expr: String,
        frame_id: Option<i64>,
        mode: EvalMode,
    },
    ReadMemory {
        address: String,
        count: i64,
    },
    Disassemble {
        address: String,
        count: i64,
    },
}

/// Canned responses + a recording of the calls made. Wrap in an `Arc` and share between
/// the test (which reads `calls`/sets responses) and the backend (which the handler holds).
#[derive(Default)]
pub struct FakeState {
    pub calls: Vec<Call>,

    pub launch_outcome: Option<Result<LaunchOutcome, BackendError>>,
    pub attach_outcome: Option<Result<AttachOutcome, BackendError>>,
    pub source_bp_result: Option<Result<Vec<BreakpointResult>, BackendError>>,
    pub function_bp_result: Option<Result<Vec<BreakpointResult>, BackendError>>,
    pub cont_result: Option<Result<StopOutcome, BackendError>>,
    pub step_result: Option<Result<StopOutcome, BackendError>>,
    pub pause_result: Option<Result<(), BackendError>>,
    pub threads_result: Option<Result<Vec<ThreadInfo>, BackendError>>,
    pub stack_trace_result: Option<Result<(Vec<Frame>, i64), BackendError>>,
    pub scopes_result: Option<Result<Vec<Scope>, BackendError>>,
    pub variables_result: Option<Result<Vec<Variable>, BackendError>>,
    pub evaluate_result: Option<Result<EvalResult, BackendError>>,
    pub read_memory_result: Option<Result<MemoryRead, BackendError>>,
    pub disassemble_result: Option<Result<Vec<Instruction>, BackendError>>,

    /// When set, the `cont` call awaits this receiver before returning — lets a test hold a
    /// `continue` blocked while another op (`pause`) runs (the concurrency test).
    pub cont_gate: Option<oneshot::Receiver<StopOutcome>>,
    pub repl_capable: bool,
    /// The pid the fake backend reports via `debugger_pid()` (the lldb-dap subprocess pid in
    /// production). `None` ⇒ no spawned child, as in the scripted-peer unit tests.
    pub debugger_pid: Option<i64>,
}

/// The fake backend over a shared [`FakeState`].
pub struct FakeBackend {
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeBackend {
    pub fn new(state: Arc<Mutex<FakeState>>) -> Self {
        FakeBackend { state }
    }

    fn record(&self, call: Call) {
        self.state.lock().unwrap().calls.push(call);
    }
}

#[async_trait]
impl DebuggerBackend for FakeBackend {
    async fn launch(&self, _spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        self.record(Call::Launch);
        self.state
            .lock()
            .unwrap()
            .launch_outcome
            .take()
            .unwrap_or(Ok(LaunchOutcome::Running))
    }

    async fn attach(&self, _spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        self.record(Call::Attach);
        self.state
            .lock()
            .unwrap()
            .attach_outcome
            .take()
            .unwrap_or(Ok(AttachOutcome::Stopped(debugger_core::StopInfo {
                reason: "entry".to_string(),
                thread_id: 1,
                description: String::new(),
                hit_breakpoint_ids: Vec::new(),
            })))
    }

    async fn disconnect(&self, terminate: bool) {
        self.record(Call::Disconnect { terminate });
    }

    async fn set_source_breakpoints(
        &self,
        file: &str,
        bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        self.record(Call::SetSourceBreakpoints {
            file: file.to_string(),
            count: bps.len(),
        });
        self.state
            .lock()
            .unwrap()
            .source_bp_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn set_function_breakpoints(
        &self,
        bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        self.record(Call::SetFunctionBreakpoints { count: bps.len() });
        self.state
            .lock()
            .unwrap()
            .function_bp_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn cont(&self, thread_id: i64) -> Result<StopOutcome, BackendError> {
        self.record(Call::Cont { thread_id });
        // If gated, await the gate (the concurrency test holds a continue blocked here).
        let gate = self.state.lock().unwrap().cont_gate.take();
        if let Some(rx) = gate {
            return rx.await.map_err(|_| BackendError::Closed);
        }
        self.state
            .lock()
            .unwrap()
            .cont_result
            .take()
            .unwrap_or(Ok(StopOutcome::Stopped(debugger_core::StopInfo {
                reason: "step".to_string(),
                thread_id,
                description: String::new(),
                hit_breakpoint_ids: Vec::new(),
            })))
    }

    async fn step(
        &self,
        kind: StepKind,
        thread_id: i64,
        gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError> {
        self.record(Call::Step {
            kind,
            thread_id,
            gran,
        });
        self.state
            .lock()
            .unwrap()
            .step_result
            .take()
            .unwrap_or(Ok(StopOutcome::Stopped(debugger_core::StopInfo {
                reason: "step".to_string(),
                thread_id,
                description: String::new(),
                hit_breakpoint_ids: Vec::new(),
            })))
    }

    async fn pause(&self) -> Result<(), BackendError> {
        self.record(Call::Pause);
        self.state
            .lock()
            .unwrap()
            .pause_result
            .take()
            .unwrap_or(Ok(()))
    }

    async fn threads(&self) -> Result<Vec<ThreadInfo>, BackendError> {
        self.record(Call::Threads);
        self.state
            .lock()
            .unwrap()
            .threads_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn stack_trace(
        &self,
        thread_id: i64,
        start: i64,
        levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError> {
        self.record(Call::StackTrace {
            thread_id,
            start,
            levels,
        });
        self.state
            .lock()
            .unwrap()
            .stack_trace_result
            .take()
            .unwrap_or(Ok((Vec::new(), 0)))
    }

    async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, BackendError> {
        self.record(Call::Scopes { frame_id });
        self.state
            .lock()
            .unwrap()
            .scopes_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        self.record(Call::Variables {
            variables_reference,
        });
        self.state
            .lock()
            .unwrap()
            .variables_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    async fn evaluate(
        &self,
        expr: &str,
        frame_id: Option<i64>,
        mode: EvalMode,
    ) -> Result<EvalResult, BackendError> {
        self.record(Call::Evaluate {
            expr: expr.to_string(),
            frame_id,
            mode,
        });
        self.state
            .lock()
            .unwrap()
            .evaluate_result
            .take()
            .unwrap_or(Ok(EvalResult {
                result: String::new(),
                ty: String::new(),
                variables_reference: 0,
            }))
    }

    async fn read_memory(&self, address: &str, count: i64) -> Result<MemoryRead, BackendError> {
        self.record(Call::ReadMemory {
            address: address.to_string(),
            count,
        });
        self.state
            .lock()
            .unwrap()
            .read_memory_result
            .take()
            .unwrap_or(Ok(MemoryRead {
                address: address.to_string(),
                data: Vec::new(),
            }))
    }

    async fn disassemble(
        &self,
        address: &str,
        count: i64,
    ) -> Result<Vec<Instruction>, BackendError> {
        self.record(Call::Disassemble {
            address: address.to_string(),
            count,
        });
        self.state
            .lock()
            .unwrap()
            .disassemble_result
            .take()
            .unwrap_or(Ok(Vec::new()))
    }

    fn supports_command_repl_mode(&self) -> bool {
        self.state.lock().unwrap().repl_capable
    }

    fn debugger_pid(&self) -> Option<i64> {
        self.state.lock().unwrap().debugger_pid
    }
}

/// A factory whose `connect()` returns a [`FakeBackend`] over the given shared state, plus
/// an empty (immediately-ending) event stream. A `connect_error` makes it fail.
pub struct FakeFactory {
    pub state: Arc<Mutex<FakeState>>,
    pub connect_error: Mutex<Option<BackendError>>,
}

impl FakeFactory {
    pub fn new(state: Arc<Mutex<FakeState>>) -> Self {
        FakeFactory {
            state,
            connect_error: Mutex::new(None),
        }
    }

    pub fn with_connect_error(state: Arc<Mutex<FakeState>>, err: BackendError) -> Self {
        FakeFactory {
            state,
            connect_error: Mutex::new(Some(err)),
        }
    }
}

#[async_trait]
impl BackendFactory for FakeFactory {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn connect(&self) -> Result<Connection, BackendError> {
        if let Some(err) = self.connect_error.lock().unwrap().take() {
            return Err(err);
        }
        let backend: Arc<dyn DebuggerBackend> = Arc::new(FakeBackend::new(Arc::clone(&self.state)));
        let events: BoxStream<'static, BackendEvent> = stream::empty().boxed();
        Ok(Connection { backend, events })
    }
}
