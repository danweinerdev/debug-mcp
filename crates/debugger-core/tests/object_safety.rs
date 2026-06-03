//! Proves the `DebuggerBackend` and `BackendFactory` traits are implementable and
//! object-safe by building a stub backend/factory and storing them as trait objects
//! (`Arc<dyn DebuggerBackend>`, `Box<dyn BackendFactory>`). This is the gate that lets
//! Phases 2/3 and Phase 4 code against the seam in parallel.

use std::sync::Arc;

use async_trait::async_trait;
use debugger_core::{
    AttachOutcome, AttachSpec, BackendError, BackendEvent, BackendFactory, BreakpointResult,
    Connection, DebuggerBackend, EvalMode, EvalResult, Frame, FunctionBp, Granularity, Instruction,
    LaunchOutcome, LaunchSpec, MemoryRead, Scope, SourceBp, StepKind, StopOutcome, ThreadInfo,
    Variable,
};
use futures::executor::block_on;
use futures::stream::{self, StreamExt};

/// A do-nothing backend used only to exercise the trait surface.
struct NullBackend;

#[async_trait]
impl DebuggerBackend for NullBackend {
    async fn launch(&self, _spec: LaunchSpec) -> Result<LaunchOutcome, BackendError> {
        Ok(LaunchOutcome::Running)
    }

    async fn attach(&self, _spec: AttachSpec) -> Result<AttachOutcome, BackendError> {
        Err(BackendError::Detect(
            "null backend cannot attach".to_string(),
        ))
    }

    async fn disconnect(&self, _terminate: bool) {}

    async fn set_source_breakpoints(
        &self,
        _file: &str,
        _bps: &[SourceBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        Ok(Vec::new())
    }

    async fn set_function_breakpoints(
        &self,
        _bps: &[FunctionBp],
    ) -> Result<Vec<BreakpointResult>, BackendError> {
        Ok(Vec::new())
    }

    async fn cont(&self, _thread_id: i64) -> Result<StopOutcome, BackendError> {
        Ok(StopOutcome::Terminated)
    }

    async fn step(
        &self,
        _kind: StepKind,
        _thread_id: i64,
        _gran: Option<Granularity>,
    ) -> Result<StopOutcome, BackendError> {
        Ok(StopOutcome::Terminated)
    }

    async fn pause(&self) -> Result<(), BackendError> {
        Ok(())
    }

    async fn threads(&self) -> Result<Vec<ThreadInfo>, BackendError> {
        Ok(Vec::new())
    }

    async fn stack_trace(
        &self,
        _thread_id: i64,
        _start: i64,
        _levels: i64,
    ) -> Result<(Vec<Frame>, i64), BackendError> {
        Ok((Vec::new(), 0))
    }

    async fn scopes(&self, _frame_id: i64) -> Result<Vec<Scope>, BackendError> {
        Ok(Vec::new())
    }

    async fn variables(&self, _variables_reference: i64) -> Result<Vec<Variable>, BackendError> {
        Ok(Vec::new())
    }

    async fn evaluate(
        &self,
        _expr: &str,
        _frame_id: Option<i64>,
        _mode: EvalMode,
    ) -> Result<EvalResult, BackendError> {
        Err(BackendError::Dap {
            message: "null backend cannot evaluate".to_string(),
        })
    }

    async fn read_memory(&self, _address: &str, _count: i64) -> Result<MemoryRead, BackendError> {
        Err(BackendError::Closed)
    }

    async fn disassemble(
        &self,
        _address: &str,
        _count: i64,
    ) -> Result<Vec<Instruction>, BackendError> {
        Err(BackendError::Timeout)
    }

    fn supports_command_repl_mode(&self) -> bool {
        false
    }
}

/// A stub factory that hands back a [`NullBackend`] plus a single-event stream.
struct NullFactory;

#[async_trait]
impl BackendFactory for NullFactory {
    fn name(&self) -> &'static str {
        "null"
    }

    async fn connect(&self) -> Result<Connection, BackendError> {
        let backend: Arc<dyn DebuggerBackend> = Arc::new(NullBackend);
        let events = stream::once(async { BackendEvent::Terminated { code: Some(0) } }).boxed();
        Ok(Connection { backend, events })
    }
}

#[test]
fn debugger_backend_is_object_safe() {
    let backend: Arc<dyn DebuggerBackend> = Arc::new(NullBackend);
    assert!(!backend.supports_command_repl_mode());
}

#[test]
fn backend_factory_is_object_safe() {
    let factory: Box<dyn BackendFactory> = Box::new(NullFactory);
    assert_eq!(factory.name(), "null");
}

// Driven with `futures::executor::block_on` (not `#[tokio::test]`) so this crate keeps
// zero `tokio` dependency in *any* section — the seam invariant (Spec FR-18). A real
// backend supplies its own runtime below the seam.
#[test]
fn factory_connect_yields_a_usable_connection() {
    block_on(async {
        let factory: Box<dyn BackendFactory> = Box::new(NullFactory);
        let mut conn = factory.connect().await.expect("connect");

        let outcome = conn
            .backend
            .launch(sample_launch_spec())
            .await
            .expect("launch");
        assert!(matches!(outcome, LaunchOutcome::Running));

        let event = conn.events.next().await.expect("one event");
        assert_eq!(event, BackendEvent::Terminated { code: Some(0) });
        assert!(conn.events.next().await.is_none(), "stream exhausted");
    });
}

#[test]
fn backend_error_variants_surface_through_the_trait() {
    block_on(async {
        let backend: Arc<dyn DebuggerBackend> = Arc::new(NullBackend);

        assert!(matches!(
            backend
                .attach(AttachSpec {
                    pid: Some(1),
                    wait_for: None
                })
                .await,
            Err(BackendError::Detect(_))
        ));
        assert!(matches!(
            backend.read_memory("0x0", 4).await,
            Err(BackendError::Closed)
        ));
        assert!(matches!(
            backend.disassemble("0x0", 4).await,
            Err(BackendError::Timeout)
        ));
    });
}

fn sample_launch_spec() -> LaunchSpec {
    LaunchSpec {
        program: "/bin/true".to_string(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
        stop_on_entry: false,
        source_breakpoints: Vec::new(),
        function_breakpoints: Vec::new(),
    }
}
