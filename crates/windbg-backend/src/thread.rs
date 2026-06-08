//! The dedicated engine thread + the [`EngineCmd`] marshaling channel.
//!
//! `dbgeng_sys::Engine` is `!Send` (every DbgEng call must run on one OS thread). So the engine
//! is **created and owned on a dedicated `std::thread`**: the backend never touches it directly,
//! it only sends [`EngineCmd`]s over a `tokio::mpsc` and awaits a per-command `oneshot` reply.
//! This is the analog of `lldb-backend` spawning a subprocess + read loop — here the "transport"
//! is an in-process command channel to a confined thread.
//!
//! ## Thread lifecycle
//!
//! 1. Build the [`ComApartment`](dbgeng_sys::ComApartment) (MTA COM init on *this* thread).
//! 2. Construct the [`EngineOps`] via the injected **constructor closure** (production builds a
//!    real [`Engine`](dbgeng_sys::Engine); tests build a `FakeEngine`). A construction failure is
//!    sent back over the readiness `oneshot` and the thread exits.
//! 3. Mint the [`InterruptHandle`](dbgeng_sys::InterruptHandle) **on-thread** (it is `Send`) and
//!    send `Ok(handle)` over the readiness `oneshot`.
//! 4. Loop: `cmd_rx.blocking_recv()` → match the [`EngineCmd`] → call the engine method → send the
//!    result over the command's reply `oneshot`.
//! 5. On channel close (all command senders dropped) → best-effort `detach` → drop the
//!    `ComApartment` (uninit COM on this thread) → exit.
//!
//! No `unsafe`: the COM init lives inside `dbgeng_sys::ComApartment`; this thread only holds the
//! safe guard.

use dbgeng_sys::{BpLoc, ComApartment, InterruptHandle, LaunchReq, OutputSink};
use debugger_core::{
    BreakpointResult, DumpOutcome, EvalResult, Frame, Instruction, MemoryRead, ModuleInfo,
    StepKind, StopOutcome, ThreadInfo, Variable,
};
use tokio::sync::{mpsc, oneshot};

use crate::engine_ops::EngineOps;
use crate::error::EngineError;

/// A type alias for one command's reply channel: a `oneshot` carrying the op's `Result`.
type Reply<T> = oneshot::Sender<Result<T, EngineError>>;

/// One marshaled engine operation. Each variant carries the op's arguments plus a reply
/// [`oneshot::Sender`] of the op's neutral result type. The engine thread matches on the variant,
/// calls the matching [`EngineOps`] method, and sends the result back over `reply`.
///
/// `set_output_sink` carries no reply (it is fire-and-forget — the sink is installed on the
/// engine and never needs a confirmation), matching `Engine::set_output_sink`'s `()` return.
pub enum EngineCmd {
    Launch {
        req: LaunchReq,
        reply: Reply<StopOutcome>,
    },
    AttachPid {
        pid: u32,
        reply: Reply<StopOutcome>,
    },
    Detach {
        /// Kill the live debuggee (vs. a plain detach). Carried to `EngineOps::detach`; ignored
        /// for a dump session.
        terminate: bool,
        reply: Reply<()>,
    },
    Go {
        timeout_ms: u32,
        reply: Reply<Option<StopOutcome>>,
    },
    Step {
        kind: StepKind,
        reply: Reply<StopOutcome>,
    },
    BreakIn {
        reply: Reply<StopOutcome>,
    },
    SetBreakpoint {
        loc: BpLoc,
        condition: String,
        reply: Reply<BreakpointResult>,
    },
    RemoveBreakpoint {
        id: i64,
        reply: Reply<()>,
    },
    ListBreakpoints {
        reply: Reply<Vec<BreakpointResult>>,
    },
    Threads {
        reply: Reply<Vec<ThreadInfo>>,
    },
    StackTrace {
        thread_id: i64,
        max: i64,
        reply: Reply<Vec<Frame>>,
    },
    Locals {
        frame_index: i64,
        reply: Reply<Vec<Variable>>,
    },
    Evaluate {
        expr: String,
        reply: Reply<EvalResult>,
    },
    ReadMemory {
        address: u64,
        size: usize,
        reply: Reply<MemoryRead>,
    },
    Disassemble {
        address: u64,
        count: i64,
        reply: Reply<Vec<Instruction>>,
    },
    Execute {
        command: String,
        reply: Reply<String>,
    },
    Modules {
        reply: Reply<Vec<ModuleInfo>>,
    },
    CurrentSourceLocation {
        reply: Reply<Option<(String, i64)>>,
    },
    OpenDump {
        path: String,
        reply: Reply<DumpOutcome>,
    },
    AttachKernel {
        connection: String,
        reply: Reply<StopOutcome>,
    },
    /// Install the output sink on the engine (fire-and-forget — no reply). The closure runs on
    /// the engine thread for every output line; per Decision 6 it must be non-blocking.
    SetOutputSink {
        sink: OutputSink,
    },
}

/// Spawn the dedicated engine thread.
///
/// `make_engine` is the **constructor closure** run *on the thread*: production passes
/// `|| Engine::create().map(|e| Box::new(e) as Box<dyn EngineOps>)`; tests pass a closure that
/// builds a `FakeEngine`. Running construction on-thread (rather than passing a built engine in)
/// is mandatory because `Engine` is `!Send` — it can only exist on its owning thread — and it
/// lets a fake be injected with no live engine.
///
/// Returns the [`mpsc::Sender<EngineCmd>`] (the marshaling channel) and a `oneshot` receiver
/// carrying the readiness result: `Ok(InterruptHandle)` once the engine is built, or the
/// construction `EngineError`. The caller awaits the readiness `oneshot`; **no COM call happens
/// on the caller's thread** — all of it (COM init + engine construction) runs on the spawned
/// thread.
///
/// `cmd_buffer` bounds the command channel; a small buffer is plenty since the backend awaits
/// each reply before the next caller proceeds (the channel is effectively a request/reply pipe).
///
/// `terminated` (optional) is fired once, with no exit code, when the engine thread exits its
/// command loop (the channel closed → teardown). The factory wires it into the neutral
/// `BackendEvent::Terminated` stream (the analog of the read loop's terminated signal); tests
/// that only need the round-trip can pass `None`.
pub fn spawn_engine_thread<F>(
    make_engine: F,
    cmd_buffer: usize,
    terminated: Option<oneshot::Sender<Option<i64>>>,
) -> (
    mpsc::Sender<EngineCmd>,
    oneshot::Receiver<Result<InterruptHandle, EngineError>>,
    std::thread::JoinHandle<()>,
)
where
    F: FnOnce() -> Result<Box<dyn EngineOps>, EngineError> + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>(cmd_buffer.max(1));
    let (ready_tx, ready_rx) = oneshot::channel::<Result<InterruptHandle, EngineError>>();

    let join = std::thread::spawn(move || {
        // Run the engine body under `catch_unwind`: a panic on the engine thread (e.g. a future
        // engine op) must NOT silently end the event stream with no terminal signal. Whether the
        // body returns normally or panics, the `terminated` sender fires below so the session's
        // event-pump always sees `Terminated` and leaves no ambiguous state. (`ready_tx` is moved
        // into the body; if the body panics before signaling readiness, dropping `ready_tx`
        // already wakes the awaiting `connect` with an `Err` — handled there as a spawn failure.)
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine_thread_main(make_engine, cmd_rx, ready_tx);
        }));
        if let Some(term) = terminated {
            let _ = term.send(None);
        }
    });

    (cmd_tx, ready_rx, join)
}

/// The engine thread's body: COM init → construct the engine → signal readiness → command loop →
/// detach. Kept private; `spawn_engine_thread` is the entry point and owns the `terminated`
/// signal (fired in its `catch_unwind` "finally" so it covers a panic here too).
fn engine_thread_main<F>(
    make_engine: F,
    mut cmd_rx: mpsc::Receiver<EngineCmd>,
    ready_tx: oneshot::Sender<Result<InterruptHandle, EngineError>>,
) where
    F: FnOnce() -> Result<Box<dyn EngineOps>, EngineError>,
{
    // 1. MTA COM apartment for this thread (the only place the engine's COM init happens — never
    //    on the caller). The guard lives for the whole thread; dropping it at the end uninits COM.
    let _com = match ComApartment::new() {
        Ok(guard) => guard,
        Err(e) => {
            // COM init failed: surface it as the readiness result and exit. A dropped `ready_tx`
            // receiver (the caller went away) is fine — nothing to clean up.
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // 2. Build the engine via the injected constructor (on this thread; `Engine` is `!Send`).
    let mut engine = match make_engine() {
        Ok(engine) => engine,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // 3. Mint the interrupt handle on-thread (it is `Send`) and signal readiness. If the caller
    //    dropped the receiver already, there is nothing to serve — tear down and exit.
    let handle = engine.interrupt_handle();
    if ready_tx.send(Ok(handle)).is_err() {
        // The caller dropped the readiness receiver before we became ready — nothing will ever
        // send a command. Tear down and exit (the spawn wrapper fires `terminated`). Teardown never
        // kills the debuggee — `detach(false)`.
        let _ = engine.detach(false);
        return;
    }

    // 4. Command loop: block on the next command, dispatch it, reply. `blocking_recv` parks this
    //    OS thread (it is a plain `std::thread`, not a tokio worker) until a command arrives or
    //    every sender is dropped.
    while let Some(cmd) = cmd_rx.blocking_recv() {
        dispatch(engine.as_mut(), cmd);
    }

    // 5. Channel closed (the backend and all clones of the sender dropped): best-effort detach so
    //    the target's image is not left locked, then drop `_com` (uninit COM) on the way out. The
    //    spawn wrapper fires the `terminated` signal (the read-loop-ended analog). Teardown never
    //    kills the debuggee — `detach(false)`; an explicit terminate is the disconnect path only.
    let _ = engine.detach(false);
}

/// Dispatch one command to the engine and send the result back over its reply channel. A dropped
/// reply receiver (the caller's future was cancelled/dropped before awaiting) is ignored — the
/// result is simply discarded.
fn dispatch(engine: &mut dyn EngineOps, cmd: EngineCmd) {
    match cmd {
        EngineCmd::Launch { req, reply } => {
            let _ = reply.send(engine.launch(&req));
        }
        EngineCmd::AttachPid { pid, reply } => {
            let _ = reply.send(engine.attach_pid(pid));
        }
        EngineCmd::Detach { terminate, reply } => {
            let _ = reply.send(engine.detach(terminate));
        }
        EngineCmd::Go { timeout_ms, reply } => {
            let _ = reply.send(engine.go(timeout_ms));
        }
        EngineCmd::Step { kind, reply } => {
            let _ = reply.send(engine.step(kind));
        }
        EngineCmd::BreakIn { reply } => {
            let _ = reply.send(engine.break_in());
        }
        EngineCmd::SetBreakpoint {
            loc,
            condition,
            reply,
        } => {
            let _ = reply.send(engine.set_breakpoint(&loc, &condition));
        }
        EngineCmd::RemoveBreakpoint { id, reply } => {
            let _ = reply.send(engine.remove_breakpoint(id));
        }
        EngineCmd::ListBreakpoints { reply } => {
            let _ = reply.send(engine.list_breakpoints());
        }
        EngineCmd::Threads { reply } => {
            let _ = reply.send(engine.threads());
        }
        EngineCmd::StackTrace {
            thread_id,
            max,
            reply,
        } => {
            let _ = reply.send(engine.stack_trace(thread_id, max));
        }
        EngineCmd::Locals { frame_index, reply } => {
            let _ = reply.send(engine.locals(frame_index));
        }
        EngineCmd::Evaluate { expr, reply } => {
            let _ = reply.send(engine.evaluate(&expr));
        }
        EngineCmd::ReadMemory {
            address,
            size,
            reply,
        } => {
            let _ = reply.send(engine.read_memory(address, size));
        }
        EngineCmd::Disassemble {
            address,
            count,
            reply,
        } => {
            let _ = reply.send(engine.disassemble(address, count));
        }
        EngineCmd::Execute { command, reply } => {
            let _ = reply.send(engine.execute(&command));
        }
        EngineCmd::Modules { reply } => {
            let _ = reply.send(engine.modules());
        }
        EngineCmd::CurrentSourceLocation { reply } => {
            let _ = reply.send(engine.current_source_location());
        }
        EngineCmd::OpenDump { path, reply } => {
            let _ = reply.send(engine.open_dump(&path));
        }
        EngineCmd::AttachKernel { connection, reply } => {
            let _ = reply.send(engine.attach_kernel(&connection));
        }
        EngineCmd::SetOutputSink { sink } => {
            // Fire-and-forget: install the sink, no reply.
            engine.set_output_sink(sink);
        }
    }
}
