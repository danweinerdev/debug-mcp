//! DbgEng callback objects: the output sink and the event/stop-reason capture.
//!
//! Port of the C++ `OutputCallbacks` and `EventCallbacks` (`callbacks.{h,cpp}` in the
//! windbg-mcp plugin). Two COM objects are registered with the engine's `IDebugClient`:
//!
//! - [`OutputCallbacks`] (`IDebugOutputCallbacks`) — every line DbgEng emits flows through
//!   `Output`; we append it to a captured buffer and, if one is installed, forward it to a
//!   sink closure. This is the analog of `OutputCallbacks::Output` + `GetAndClear`.
//! - [`EventCallbacks`] (`IDebugEventCallbacks`) — the engine reports debuggee events
//!   (breakpoint hit, exception, process exit, module load) here; we record the human-readable
//!   "last stop reason" plus the structured stop fields (bp id/offset, exception code/address,
//!   exit code) the later tasks consume, and return the `DEBUG_STATUS_*` disposition that tells
//!   the engine whether to break or pass through.
//!
//! ## Thread-safety contract
//!
//! DbgEng invokes these callbacks from **its own internal threads**, not necessarily the
//! engine-owning thread. So the shared state ([`CallbackState`]) is behind an
//! `Arc<Mutex<…>>`: both callback objects hold a clone of the `Arc`, and so does the
//! [`crate::Engine`] that reads the captured output / stop info back out. Every method locks
//! the mutex before touching the state. (`Mutex`, never `RefCell` — the access is genuinely
//! cross-thread.)
//!
//! ## Refcount / ownership
//!
//! `client.SetOutputCallbacks`/`SetEventCallbacks` `AddRef` the interface they are handed, so
//! DbgEng keeps the callback objects alive while they are registered. The `Engine` *also*
//! retains the interface objects (so it can later clear them on teardown and so the shared
//! `Arc<Mutex<CallbackState>>` outlives any in-flight callback). Because every interface is a
//! `windows`-crate smart pointer, the C++ manual `delete this`/`Release` footgun does not
//! apply: dropping the retained interfaces `Release`s them.

use std::sync::{Arc, Mutex};

use windows::Win32::System::Diagnostics::Debug::EXCEPTION_RECORD64;
use windows::Win32::System::Diagnostics::Debug::Extensions::{
    DEBUG_EVENT_BREAKPOINT, DEBUG_EVENT_CREATE_PROCESS, DEBUG_EVENT_EXCEPTION,
    DEBUG_EVENT_EXIT_PROCESS, DEBUG_EVENT_LOAD_MODULE, DEBUG_EVENT_SESSION_STATUS,
    DEBUG_OUTPUT_ERROR, DEBUG_OUTPUT_NORMAL, DEBUG_OUTPUT_PROMPT, DEBUG_OUTPUT_WARNING,
    DEBUG_STATUS_BREAK, DEBUG_STATUS_NO_CHANGE, IDebugBreakpoint, IDebugEventCallbacks,
    IDebugEventCallbacks_Impl, IDebugOutputCallbacks, IDebugOutputCallbacks_Impl,
};
use windows::core::{PCSTR, Ref, implement};

/// The Windows initial-process breakpoint exception (`STATUS_BREAKPOINT`, the `int 3` the
/// loader hits): a first-chance occurrence of this is the engine's initial break and must be
/// honored, unlike other first-chance exceptions which we let pass through.
const STATUS_BREAKPOINT: u32 = 0x8000_0003;

/// The category of a DbgEng output line, derived from the `DEBUG_OUTPUT_*` mask passed to
/// `IDebugOutputCallbacks::Output`. Forwarded to the sink so consumers can distinguish normal
/// command output from error/warning text (Phase 3 maps this onto `BackendEvent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// Normal command output (`DEBUG_OUTPUT_NORMAL`).
    Normal,
    /// Error output (`DEBUG_OUTPUT_ERROR`).
    Error,
    /// Warning output (`DEBUG_OUTPUT_WARNING`).
    Warning,
    /// The interactive prompt / prompt-registers text (`DEBUG_OUTPUT_PROMPT`).
    Prompt,
    /// Any other `DEBUG_OUTPUT_*` category we don't special-case; carries the raw mask.
    Other(u32),
}

impl OutputKind {
    /// Classify a raw `DEBUG_OUTPUT_*` mask. The mask is a bitfield but in practice each
    /// `Output` call carries a single category; we test the well-known bits in priority order
    /// and fall back to `Other` with the raw value.
    pub fn from_mask(mask: u32) -> OutputKind {
        if mask & DEBUG_OUTPUT_ERROR != 0 {
            OutputKind::Error
        } else if mask & DEBUG_OUTPUT_WARNING != 0 {
            OutputKind::Warning
        } else if mask & DEBUG_OUTPUT_PROMPT != 0 {
            OutputKind::Prompt
        } else if mask & DEBUG_OUTPUT_NORMAL != 0 {
            OutputKind::Normal
        } else {
            OutputKind::Other(mask)
        }
    }
}

/// An output sink: a closure invoked for every output line DbgEng emits, with the line's
/// [`OutputKind`] and text. Boxed and `Send` because DbgEng may dispatch output from one of its
/// own internal threads. Phase 3 wires this onto a `BackendEvent` channel.
pub type OutputSink = Box<dyn FnMut(OutputKind, &str) + Send>;

/// The shared, cross-thread state the callbacks write and the [`crate::Engine`] reads.
///
/// Lives behind an `Arc<Mutex<…>>` (see the module docs): DbgEng calls the callbacks from its
/// own threads, so this must be `Send + Sync`. The sink is an optional boxed closure invoked
/// for every output line in addition to the captured buffer.
pub struct CallbackState {
    /// The captured output buffer; drained by `Engine::take_output` (the `GetAndClear` analog).
    output: String,
    /// An optional sink invoked for every output line, in addition to buffering.
    sink: Option<OutputSink>,
    /// The id of the most recently hit breakpoint, if any.
    last_breakpoint_id: Option<u32>,
    /// The offset (address) of the most recently hit breakpoint, if any.
    last_breakpoint_offset: Option<u64>,
    /// The exception code of the most recent recorded exception, if any.
    last_exception_code: Option<u32>,
    /// The faulting address of the most recent recorded exception, if any.
    last_exception_address: Option<u64>,
    /// The exit code of the debuggee, once it has exited.
    last_exit_code: Option<u32>,
    /// A human-readable description of the most recent stop, mirroring the C++
    /// `SetLastStopReason` strings.
    last_stop_reason: String,
}

impl CallbackState {
    /// A fresh, empty state.
    pub fn new() -> CallbackState {
        CallbackState {
            output: String::new(),
            sink: None,
            last_breakpoint_id: None,
            last_breakpoint_offset: None,
            last_exception_code: None,
            last_exception_address: None,
            last_exit_code: None,
            last_stop_reason: String::new(),
        }
    }

    /// Install (or replace) the output sink.
    pub fn set_sink(&mut self, sink: OutputSink) {
        self.sink = Some(sink);
    }

    /// Drain the captured output buffer, returning what had accumulated (the `GetAndClear`
    /// analog). Leaves the buffer empty.
    pub fn take_output(&mut self) -> String {
        std::mem::take(&mut self.output)
    }

    /// The most recent stop reason string (empty if nothing has stopped yet).
    pub fn last_stop_reason(&self) -> String {
        self.last_stop_reason.clone()
    }

    /// The id of the most recently hit breakpoint.
    pub fn last_breakpoint_id(&self) -> Option<u32> {
        self.last_breakpoint_id
    }

    /// The offset of the most recently hit breakpoint.
    pub fn last_breakpoint_offset(&self) -> Option<u64> {
        self.last_breakpoint_offset
    }

    /// The code of the most recently recorded exception.
    pub fn last_exception_code(&self) -> Option<u32> {
        self.last_exception_code
    }

    /// The address of the most recently recorded exception.
    pub fn last_exception_address(&self) -> Option<u64> {
        self.last_exception_address
    }

    /// The debuggee's exit code, once it has exited.
    pub fn last_exit_code(&self) -> Option<u32> {
        self.last_exit_code
    }

    /// Append a line to the captured buffer and forward it to the sink (if installed).
    ///
    /// NOTE: the sink runs while the `CallbackState` mutex is held (the caller — `Output` —
    /// locks the state for the whole call). The sink therefore must NOT re-acquire this mutex,
    /// directly or indirectly. Phase 3 wires this to a non-blocking `tokio::mpsc` sender, which
    /// is safe; any future sink must remain non-reentrant w.r.t. `CallbackState`.
    fn record_output(&mut self, kind: OutputKind, text: &str) {
        self.output.push_str(text);
        if let Some(sink) = self.sink.as_mut() {
            sink(kind, text);
        }
    }
}

impl Default for CallbackState {
    fn default() -> CallbackState {
        CallbackState::new()
    }
}

/// Decide whether an exception should break the debuggee or pass through, mirroring the C++
/// `EventCallbacks::Exception` disposition logic.
///
/// Second-chance exceptions always break. First-chance exceptions pass through (return `false`)
/// **except** the initial process breakpoint (`STATUS_BREAKPOINT`, `0x80000003`), which is the
/// engine's initial break and must be honored. Factored out as a pure function so the truth
/// table is unit-testable without a live target.
pub fn exception_breaks(first_chance: bool, code: u32) -> bool {
    !first_chance || code == STATUS_BREAKPOINT
}

/// Read an `IDebugOutputCallbacks`/`IDebugEventCallbacks` ANSI `PCSTR` into an owned `String`,
/// lossily (DbgEng emits OEM/ANSI text; invalid bytes become U+FFFD). Returns an empty string
/// for a null pointer (matching the C++ `if (text)` guard).
fn pcstr_to_string(text: &PCSTR) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: `text` is a non-null, NUL-terminated ANSI string supplied by DbgEng for the
    // duration of the callback. `PCSTR::to_string` walks to the NUL and validates UTF-8; we
    // fall back to a lossy copy of the raw bytes when it is not valid UTF-8 so no output is
    // dropped. `as_bytes()` likewise stops at the NUL.
    unsafe {
        match text.to_string() {
            Ok(s) => s,
            Err(_) => String::from_utf8_lossy(text.as_bytes()).into_owned(),
        }
    }
}

/// The `IDebugOutputCallbacks` implementation: every output line DbgEng emits arrives at
/// [`OutputCallbacks::Output`]. Holds a clone of the shared [`CallbackState`] `Arc`.
#[implement(IDebugOutputCallbacks)]
pub struct OutputCallbacks {
    state: Arc<Mutex<CallbackState>>,
}

impl OutputCallbacks {
    /// Construct an output-callback object sharing `state` with the engine and event callbacks.
    pub fn new(state: Arc<Mutex<CallbackState>>) -> OutputCallbacks {
        OutputCallbacks { state }
    }
}

impl IDebugOutputCallbacks_Impl for OutputCallbacks_Impl {
    fn Output(&self, mask: u32, text: &PCSTR) -> windows::core::Result<()> {
        let s = pcstr_to_string(text);
        if !s.is_empty() {
            let kind = OutputKind::from_mask(mask);
            // The lock is poisoned only if a prior callback panicked while holding it; recover
            // the inner state rather than propagate a panic across the COM/FFI boundary.
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.record_output(kind, &s);
        }
        Ok(())
    }
}

/// The `IDebugEventCallbacks` implementation: DbgEng reports debuggee events here. Holds a clone
/// of the shared [`CallbackState`] `Arc`. Ports `EventCallbacks` from the C++ oracle.
#[implement(IDebugEventCallbacks)]
pub struct EventCallbacks {
    state: Arc<Mutex<CallbackState>>,
}

impl EventCallbacks {
    /// Construct an event-callback object sharing `state` with the engine and output callbacks.
    pub fn new(state: Arc<Mutex<CallbackState>>) -> EventCallbacks {
        EventCallbacks { state }
    }
}

impl EventCallbacks_Impl {
    /// Lock the shared state, recovering from a poisoned mutex (a prior panic) rather than
    /// re-panicking across the COM/FFI boundary.
    fn lock(&self) -> std::sync::MutexGuard<'_, CallbackState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl IDebugEventCallbacks_Impl for EventCallbacks_Impl {
    fn GetInterestMask(&self) -> windows::core::Result<u32> {
        // Mirror the C++ GetInterestMask exactly.
        Ok(DEBUG_EVENT_BREAKPOINT
            | DEBUG_EVENT_EXCEPTION
            | DEBUG_EVENT_CREATE_PROCESS
            | DEBUG_EVENT_EXIT_PROCESS
            | DEBUG_EVENT_LOAD_MODULE
            | DEBUG_EVENT_SESSION_STATUS)
    }

    fn Breakpoint(&self, bp: Ref<'_, IDebugBreakpoint>) -> windows::core::Result<()> {
        // Read id + offset off the breakpoint, defaulting to 0 on a null pointer or a failed
        // accessor (mirrors the C++, which ignores the GetId/GetOffset HRESULTs).
        let (id, offset) = match bp.as_ref() {
            // SAFETY: `bp_ref` is a live `IDebugBreakpoint` borrowed for this callback (the
            // `Ref` is non-null here). `GetId`/`GetOffset` only read out-params by value; no
            // pointers escape and the borrow does not outlive the call.
            Some(bp_ref) => unsafe {
                (bp_ref.GetId().unwrap_or(0), bp_ref.GetOffset().unwrap_or(0))
            },
            None => (0, 0),
        };
        let reason = format!("Breakpoint {id} hit at 0x{offset:X}");
        {
            let mut state = self.lock();
            state.last_breakpoint_id = Some(id);
            state.last_breakpoint_offset = Some(offset);
            state.last_stop_reason = reason;
        }
        // Returning a DEBUG_STATUS_* as an Err(HRESULT(status)) is how this interface conveys
        // the disposition to the engine — DEBUG_STATUS_BREAK requests a stop.
        status(DEBUG_STATUS_BREAK)
    }

    fn Exception(
        &self,
        exception: *const EXCEPTION_RECORD64,
        firstchance: u32,
    ) -> windows::core::Result<()> {
        // SAFETY: `exception` points at an `EXCEPTION_RECORD64` owned by DbgEng and valid for
        // the duration of this callback. We only read the two scalar fields we record; the read
        // does not outlive the call and we never retain the pointer. Guard against a null
        // pointer defensively even though DbgEng documents a valid record here.
        let (code, address) = unsafe {
            match exception.as_ref() {
                Some(rec) => (rec.ExceptionCode.0 as u32, rec.ExceptionAddress),
                None => (0, 0),
            }
        };
        let first = firstchance != 0;
        let reason = format!(
            "{} exception 0x{code:08X} at 0x{address:X}",
            if first {
                "First-chance"
            } else {
                "Second-chance"
            }
        );
        {
            let mut state = self.lock();
            state.last_exception_code = Some(code);
            state.last_exception_address = Some(address);
            state.last_stop_reason = reason;
        }
        if exception_breaks(first, code) {
            status(DEBUG_STATUS_BREAK)
        } else {
            status(DEBUG_STATUS_NO_CHANGE)
        }
    }

    // CreateThread/ExitThread are NOT in `GetInterestMask`, so DbgEng never invokes them; the
    // no-op bodies exist only because `IDebugEventCallbacks` requires them (matches the C++).
    fn CreateThread(
        &self,
        _handle: u64,
        _dataoffset: u64,
        _startoffset: u64,
    ) -> windows::core::Result<()> {
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn ExitThread(&self, _exitcode: u32) -> windows::core::Result<()> {
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn CreateProcessA(
        &self,
        _imagefilehandle: u64,
        _handle: u64,
        _baseoffset: u64,
        _modulesize: u32,
        _modulename: &PCSTR,
        _imagename: &PCSTR,
        _checksum: u32,
        _timedatestamp: u32,
        _initialthreadhandle: u64,
        _threaddataoffset: u64,
        _startoffset: u64,
    ) -> windows::core::Result<()> {
        // C++ logs the module/image names then passes through; we have no log sink here.
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn ExitProcess(&self, exitcode: u32) -> windows::core::Result<()> {
        let reason = format!("Process exited with code {exitcode}");
        {
            let mut state = self.lock();
            state.last_exit_code = Some(exitcode);
            state.last_stop_reason = reason;
        }
        status(DEBUG_STATUS_BREAK)
    }

    fn LoadModule(
        &self,
        _imagefilehandle: u64,
        _baseoffset: u64,
        _modulesize: u32,
        _modulename: &PCSTR,
        _imagename: &PCSTR,
        _checksum: u32,
        _timedatestamp: u32,
    ) -> windows::core::Result<()> {
        // C++ logs the module name then passes through; no-op here.
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn UnloadModule(&self, _imagebasename: &PCSTR, _baseoffset: u64) -> windows::core::Result<()> {
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn SystemError(&self, _error: u32, _level: u32) -> windows::core::Result<()> {
        status(DEBUG_STATUS_NO_CHANGE)
    }

    fn SessionStatus(&self, _status: u32) -> windows::core::Result<()> {
        // C++ logs then returns S_OK (not a DEBUG_STATUS_*).
        Ok(())
    }

    fn ChangeDebuggeeState(&self, _flags: u32, _argument: u64) -> windows::core::Result<()> {
        Ok(())
    }

    fn ChangeEngineState(&self, _flags: u32, _argument: u64) -> windows::core::Result<()> {
        Ok(())
    }

    fn ChangeSymbolState(&self, _flags: u32, _argument: u64) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Convert a `DEBUG_STATUS_*` disposition value into the `Result<()>` the event-callback methods
/// return.
///
/// `IDebugEventCallbacks` methods signal break/go by *returning the `DEBUG_STATUS_*` value as
/// their `HRESULT`* (the C++ `return DEBUG_STATUS_BREAK;`). The windows-crate vtable shim
/// converts our `Result<()>` to an `HRESULT` via `From<Result<()>>`, which yields `HRESULT(0)`
/// for `Ok` and the *error's* `HRESULT` for `Err`. Crucially `DEBUG_STATUS_BREAK` (`6`) is a
/// **success-range** `HRESULT` (non-negative), so we cannot use `HRESULT::ok()` (which would
/// collapse any non-negative value to `Ok(())`/`HRESULT(0)` and lose the `6`). Instead we map
/// `0` (`DEBUG_STATUS_NO_CHANGE` == `S_OK`) to `Ok(())` and carry every non-zero status through
/// an `Err(Error::from_hresult(value))` so the exact value reaches the engine.
///
/// The `if value == 0` branch is the SOLE protection against `from_hresult(HRESULT(0))`, which
/// would otherwise remap to the `S_EMPTY_ERROR` sentinel and lose the value — never call
/// `from_hresult` with zero here. Callers only ever pass `DEBUG_STATUS_*` constants (a small
/// non-negative set: `NO_CHANGE` == 0, `GO` == 1, … `BREAK` == 6), so `value as i32` is always a
/// success-range HRESULT and is preserved verbatim; the debug-assert guards against a future
/// caller passing a negative/error-range value the engine would misinterpret.
fn status(value: u32) -> windows::core::Result<()> {
    if value == 0 {
        Ok(())
    } else {
        debug_assert!(
            (value as i32) >= 0,
            "status() expects a non-negative DEBUG_STATUS_* value, got {value:#x}"
        );
        Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            value as i32,
        )))
    }
}

/// Build both callback objects over a freshly created shared state, returning the state handle
/// (for the engine to read back out) and the two interface objects (already cast to their COM
/// interface types). The engine registers the interfaces with `SetOutputCallbacks` /
/// `SetEventCallbacks` and retains all three.
pub fn build() -> (
    Arc<Mutex<CallbackState>>,
    IDebugOutputCallbacks,
    IDebugEventCallbacks,
) {
    let state = Arc::new(Mutex::new(CallbackState::new()));
    let output: IDebugOutputCallbacks = OutputCallbacks::new(state.clone()).into();
    let event: IDebugEventCallbacks = EventCallbacks::new(state.clone()).into();
    (state, output, event)
}
