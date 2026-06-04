//! Live engine check (Windows-only): `Engine::create` succeeds on this host (the R1/create
//! verification) and the resulting control interface is wired (a trivial `GetExecutionStatus`).
#![cfg(windows)]

use dbgeng_sys::Engine;
use windows::Win32::System::Diagnostics::Debug::Extensions::DEBUG_STATUS_NO_DEBUGGEE;

#[test]
fn create_succeeds_and_control_is_wired() {
    let mut engine = match Engine::create() {
        Ok(e) => e,
        Err(e) => panic!("Engine::create failed: {e}"),
    };

    // Prove the control interface is live AND returns the documented initial state: a freshly
    // created engine with no target must report DEBUG_STATUS_NO_DEBUGGEE. Asserting the exact
    // value turns this from "it didn't crash" into a behavioral contract.
    let status = engine
        .execution_status()
        .expect("GetExecutionStatus through the control interface");
    assert_eq!(
        status, DEBUG_STATUS_NO_DEBUGGEE,
        "a fresh no-target engine should report DEBUG_STATUS_NO_DEBUGGEE"
    );
}
