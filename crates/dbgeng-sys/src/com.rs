//! `ComApartment` — a safe RAII guard for an MTA (multithreaded) COM apartment.
//!
//! DbgEng must be driven from a thread that has initialized COM. The C++ `windbg-mcp` plugin
//! did `CoInitializeEx(COINIT_MULTITHREADED)` once on its engine thread (`main.cpp`); the
//! Phase-3 `windbg-backend` runs the engine on a dedicated thread and needs to do the same
//! without itself containing `unsafe`. This module confines that `unsafe` here (design
//! Decision 1) and hands the backend a safe guard whose `Drop` balances the init.
//!
//! `CoInitializeEx` is **per-thread**: each thread that calls it must balance it with a
//! `CoUninitialize` on the *same* thread. So [`ComApartment`] is `!Send`/`!Sync` (it holds a
//! `PhantomData<*const ()>`), which makes the type system enforce that the guard is dropped on
//! the thread that created it — exactly the thread the engine runs on.

use std::marker::PhantomData;

use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

use crate::error::EngineError;

/// An initialized MTA COM apartment for the current thread. Constructing one calls
/// `CoInitializeEx(COINIT_MULTITHREADED)`; dropping one calls `CoUninitialize` (only when the
/// init actually succeeded — see [`ComApartment::new`]).
///
/// `!Send`/`!Sync`: a COM apartment is thread-affine, so the guard must be created and dropped
/// on the same thread (the engine thread). The `PhantomData<*const ()>` removes the auto
/// `Send`/`Sync` impls without holding any real data.
pub struct ComApartment {
    /// Whether *this* guard owns a successful `CoInitializeEx` that `Drop` must balance with
    /// `CoUninitialize`. `S_OK`/`S_FALSE` (already initialized on this thread, compatibly) both
    /// own an uninit; an error path (which never constructs a `ComApartment`) does not.
    initialized: bool,
    /// Pin the guard to its creating thread: COM init/uninit are per-thread, so the type must
    /// be `!Send`/`!Sync`.
    _not_send: PhantomData<*const ()>,
}

impl ComApartment {
    /// Initialize the current thread's COM apartment as multithreaded (MTA).
    ///
    /// `CoInitializeEx` returns:
    /// - `S_OK` — first successful init on this thread; we own a matching `CoUninitialize`.
    /// - `S_FALSE` — COM was already initialized on this thread with a *compatible* (MTA)
    ///   concurrency model; the call still incremented the per-thread init count, so we still
    ///   own a balancing `CoUninitialize`.
    /// - `RPC_E_CHANGED_MODE` — COM was already initialized on this thread with an
    ///   *incompatible* (STA) model. This is a real error: we did **not** acquire an init, so we
    ///   return `Err` and `Drop` must not uninitialize.
    ///
    /// Both success codes are treated as success; `RPC_E_CHANGED_MODE` (and any other failing
    /// `HRESULT`) becomes an [`EngineError`].
    pub fn new() -> Result<ComApartment, EngineError> {
        // SAFETY: `CoInitializeEx` is the documented COM entry point. We pass a null reserved
        // pointer (`None`) and the `COINIT_MULTITHREADED` flag, exactly as the C++ engine thread
        // did. It only touches the *current* thread's COM state and returns an `HRESULT` by
        // value — no pointers cross the boundary. The returned `initialized` flag records whether
        // we acquired an init that `Drop` (below, on this same thread) must balance with
        // `CoUninitialize`; the `RPC_E_CHANGED_MODE`/error paths acquire nothing and `Drop` skips
        // the uninit accordingly.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        if hr == S_OK || hr == S_FALSE {
            // First successful init (`S_OK`) or a compatible re-init that still bumped the
            // per-thread count (`S_FALSE`): we own a balancing `CoUninitialize`.
            Ok(ComApartment {
                initialized: true,
                _not_send: PhantomData,
            })
        } else {
            // `RPC_E_CHANGED_MODE` (already initialized STA on this thread — incompatible) or any
            // other failing `HRESULT`: we acquired no init, so `Drop` must not uninitialize.
            Err(EngineError::op(
                "CoInitializeEx",
                windows::core::Error::from_hresult(hr),
            ))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful `CoInitializeEx` this guard performed. `Drop` runs
            // on the same thread that constructed the guard (the type is `!Send`/`!Sync`, so it
            // cannot have moved threads), which is the per-thread pairing `CoUninitialize`
            // requires. It takes no arguments and returns nothing; no pointers are involved.
            unsafe { CoUninitialize() };
        }
    }
}
