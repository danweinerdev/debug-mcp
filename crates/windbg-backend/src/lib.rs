//! `windbg-backend` — the WinDbg (DbgEng) backend (Phase 3).
//!
//! Turns the confined, synchronous, **`!Send`** [`dbgeng_sys::Engine`] into an async
//! [`debugger_core::DebuggerBackend`] by owning a **dedicated engine thread** below the seam:
//!
//! - [`thread`] — the engine thread + the [`EngineCmd`](thread::EngineCmd) marshaling channel.
//!   The engine is created and owned on one `std::thread` (it is `!Send`); the thread does MTA
//!   COM init (via [`dbgeng_sys::ComApartment`]) and `Engine::create` on itself, then loops over
//!   commands. The injected **constructor closure** lets a `FakeEngine` be substituted with no
//!   live engine.
//! - [`engine_ops`] — the object-safe [`EngineOps`](engine_ops::EngineOps) trait the thread drives
//!   (mirroring the `Engine` method set), with a trivial pass-through impl for the real engine.
//! - [`backend`] — [`WinDbgBackend`](backend::WinDbgBackend) and the
//!   [`call`](backend::WinDbgBackend::call) marshaling primitive (send an `EngineCmd`, await the
//!   reply, map errors) that the lifecycle / execution / inspection ops (tasks 3.2–3.4) build on.
//! - [`factory`] — [`WinDbgFactory`](factory::WinDbgFactory): spawn the engine thread → await
//!   readiness → wire the output sink → assemble the neutral [`debugger_core::BackendEvent`]
//!   stream → [`debugger_core::Connection`]. `connect()` makes ZERO COM calls on the caller.
//!
//! This is the structural analog of `lldb-backend` (subprocess + DAP read loop), with a thread +
//! command channel in place of the subprocess + transport.
//!
//! No `unsafe`: all COM/FFI `unsafe` is confined to `dbgeng-sys` (design Decision 1).
#![forbid(unsafe_code)]
#![cfg(windows)]

mod backend;
mod engine_ops;
mod error;
mod factory;
mod thread;

#[cfg(test)]
mod tests;

pub use backend::WinDbgBackend;
pub use engine_ops::EngineOps;
pub use error::EngineError;
pub use factory::WinDbgFactory;
pub use thread::{spawn_engine_thread, EngineCmd};
